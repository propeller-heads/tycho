// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {IERC20Metadata} from
    "openzeppelin-contracts/contracts/token/ERC20/extensions/IERC20Metadata.sol";

/// @title BopAMMAdapter
/// @notice Adapter for swapping tokens on BopAMM (Bebop's on-chain PMM).
/// @dev Each pool is an `asset/USDC` book keyed by a small assetId on the
/// pricing module. The pool id packs `settlement (20 bytes) | assetId (12
/// bytes)`, matching the substreams component id. Quotes are priced from
/// operator-committed lanes in the update registry and are only valid when
/// `block.timestamp` equals the book's committed update timestamp (the
/// registry reverts `StaleUpdate()` otherwise); simulation pins the timestamp
/// via the `override_block_timestamp` component attribute.
contract BopAMMAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    /// @dev Upper bound on enumerable asset ids; matches the substreams
    /// implementation's bound.
    uint256 constant MAX_ASSET_ID = 64;
    /// @dev Bisection rounds in getLimits; halves the search interval each
    /// round.
    uint256 constant LIMIT_BISECTION_ROUNDS = 32;
    /// @dev Decade-scan rounds when searching for the smallest quotable
    /// amount (quotes below the book's minimum size revert).
    uint256 constant MIN_QUOTABLE_SCAN_ROUNDS = 13;

    IBopAmmV2 public immutable settlement;
    IBopAmmPricing public immutable pricing;
    address public immutable usdc;

    constructor(address settlement_) {
        settlement = IBopAmmV2(settlement_);
        pricing = IBopAmmPricing(settlement.pricing());
        usdc = settlement.usdc();
    }

    /// @inheritdoc ISwapAdapter
    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external view override returns (Fraction[] memory prices) {
        _validatePoolTokens(poolId, sellToken, buyToken);
        prices = new Fraction[](specifiedAmounts.length);

        for (uint256 i = 0; i < specifiedAmounts.length; i++) {
            uint256 amountOut =
                settlement.quote(sellToken, buyToken, specifiedAmounts[i]);
            if (amountOut == 0) {
                revert TooSmall(0);
            }
            prices[i] = Fraction(amountOut, specifiedAmounts[i]);
        }
    }

    /// @inheritdoc ISwapAdapter
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (side == OrderSide.Buy) {
            revert NotImplemented("BopAMM quotes are exact-input only");
        }
        if (specifiedAmount == 0) {
            return trade;
        }
        _validatePoolTokens(poolId, sellToken, buyToken);

        IERC20(sellToken).safeTransferFrom(
            msg.sender, address(this), specifiedAmount
        );
        IERC20(sellToken).forceApprove(address(settlement), specifiedAmount);

        uint256 buyBalanceBefore = IERC20(buyToken).balanceOf(msg.sender);
        uint256 gasBefore = gasleft();
        settlement.swap(
            sellToken, buyToken, specifiedAmount, 0, block.timestamp, msg.sender
        );
        trade.gasUsed = gasBefore - gasleft();
        trade.calculatedAmount =
            IERC20(buyToken).balanceOf(msg.sender) - buyBalanceBefore;
        if (trade.calculatedAmount == 0) {
            revert TooSmall(0);
        }
        trade.price =
            _marginalPriceAfterSwap(sellToken, buyToken, specifiedAmount);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev The maximum sellable amount is found by probing `quote()`: the
    /// venue reverts `InsufficientLiquidity()` above the committed lane size.
    /// The maker's inventory is not checked by `quote()`, so the limit can
    /// overestimate what `swap()` can settle if the maker is underfunded —
    /// the interface prefers overestimation, and the operator sizes lanes to
    /// inventory in practice.
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        _validatePoolTokens(poolId, sellToken, buyToken);
        limits = new uint256[](2);
        if (settlement.paused()) {
            return limits;
        }

        uint256 good = _smallestQuotable(sellToken, buyToken);
        if (good == 0) {
            return limits;
        }

        uint256 bad = good * 2;
        while (
            bad < type(uint256).max / 2
                && _quoteSucceeds(sellToken, buyToken, bad)
        ) {
            good = bad;
            bad *= 2;
        }
        for (uint256 i = 0; i < LIMIT_BISECTION_ROUNDS; i++) {
            uint256 mid = good + (bad - good) / 2;
            if (_quoteSucceeds(sellToken, buyToken, mid)) {
                good = mid;
            } else {
                bad = mid;
            }
        }

        limits[0] = good;
        limits[1] = settlement.quote(sellToken, buyToken, good);
    }

    /// @inheritdoc ISwapAdapter
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](4);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.PriceFunction;
        capabilities[2] = Capability.ConstantPrice;
        capabilities[3] = Capability.HardLimits;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        (address asset,,,,) = pricing.getAssetConfig(_assetId(poolId));
        if (asset == address(0)) {
            revert InvalidOrder("Unknown pool");
        }
        tokens = new address[](2);
        tokens[0] = asset;
        tokens[1] = usdc;
    }

    /// @inheritdoc ISwapAdapter
    function getPoolIds(uint256 offset, uint256 limit)
        external
        view
        override
        returns (bytes32[] memory ids)
    {
        bytes32[] memory configured = new bytes32[](MAX_ASSET_ID);
        uint256 count = 0;
        for (uint256 i = 0; i < MAX_ASSET_ID; i++) {
            (address asset,,,,) = pricing.getAssetConfig(uint8(i));
            if (asset == address(0)) {
                continue;
            }
            configured[count] = _poolId(uint8(i));
            count++;
        }

        if (offset >= count) {
            return new bytes32[](0);
        }
        uint256 end = offset + limit;
        if (end > count) {
            end = count;
        }
        ids = new bytes32[](end - offset);
        for (uint256 i = 0; i < ids.length; i++) {
            ids[i] = configured[offset + i];
        }
    }

    /// @notice Pool id for a book: `settlement (20 bytes) | assetId (12
    /// bytes)`.
    function _poolId(uint8 assetId) internal view returns (bytes32) {
        return bytes32(bytes20(address(settlement))) | bytes32(uint256(assetId));
    }

    function _assetId(bytes32 poolId) internal view returns (uint8) {
        if (address(bytes20(poolId)) != address(settlement)) {
            revert InvalidOrder("Pool id settlement mismatch");
        }
        uint256 assetId = uint256(poolId) & type(uint96).max;
        if (assetId >= MAX_ASSET_ID) {
            revert InvalidOrder("Asset id out of range");
        }
        // Bounded by MAX_ASSET_ID above.
        // forge-lint: disable-next-line(unsafe-typecast)
        return uint8(assetId);
    }

    function _validatePoolTokens(
        bytes32 poolId,
        address sellToken,
        address buyToken
    ) internal view {
        (address asset,,,,) = pricing.getAssetConfig(_assetId(poolId));
        if (asset == address(0)) {
            revert InvalidOrder("Unknown pool");
        }
        bool validPair = (sellToken == asset && buyToken == usdc)
            || (sellToken == usdc && buyToken == asset);
        if (!validPair) {
            revert InvalidOrder("Pool/token mismatch");
        }
    }

    /// @dev Smallest amount the venue will quote, found by scanning upward in
    /// decades from 1e-6 of one sell-token unit.
    function _smallestQuotable(address sellToken, address buyToken)
        internal
        view
        returns (uint256)
    {
        uint256 amount = 10 ** IERC20Metadata(sellToken).decimals() / 1e6;
        if (amount == 0) {
            amount = 1;
        }
        for (uint256 i = 0; i < MIN_QUOTABLE_SCAN_ROUNDS; i++) {
            if (_quoteSucceeds(sellToken, buyToken, amount)) {
                return amount;
            }
            amount *= 10;
        }
        return 0;
    }

    function _quoteSucceeds(address sellToken, address buyToken, uint256 amount)
        internal
        view
        returns (bool)
    {
        try settlement.quote(sellToken, buyToken, amount) returns (
            uint256 amountOut
        ) {
            return amountOut > 0;
        } catch {
            return false;
        }
    }

    /// @dev Marginal price after the trade; `Fraction(0, 1)` when the trade
    /// consumed the remaining quotable size.
    function _marginalPriceAfterSwap(
        address sellToken,
        address buyToken,
        uint256 amount
    ) internal view returns (Fraction memory) {
        try settlement.quote(sellToken, buyToken, amount) returns (
            uint256 amountOut
        ) {
            if (amountOut > 0) {
                return Fraction(amountOut, amount);
            }
        } catch {}
        return Fraction(0, 1);
    }
}

interface IBopAmmV2 {
    function pricing() external view returns (address);

    function usdc() external view returns (address);

    function paused() external view returns (bool);

    function quote(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut);

    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        uint256 expiry,
        address recipient
    ) external payable;
}

interface IBopAmmPricing {
    function getAssetConfig(uint8 assetId)
        external
        view
        returns (
            address token,
            uint256 decimals,
            uint256 param,
            uint256 minSize,
            uint256 reserved
        );
}
