// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title DollarStoreAdapter
/// @notice Adapter for DollarStore, a singleton that swaps listed stablecoins
/// 1:1 in normalized 6-decimal units, with no fee.
/// @dev The component model, the pool id and what is indexed are described in
/// the ethereum-dollarstore package README. Every call checks that the tokens
/// hash to the pool id.
///
/// Quotes and limits come from the store's `getSwapQuote`; its doc lists when
/// it returns 0. Swaps use `swapExactInput`, which fills fully or reverts and
/// never queues, so the adapter never owns a queue position.
contract DollarStoreAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    IDollarStore public immutable store;

    constructor(address store_) {
        store = IDollarStore(store_);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev The price is constant: one normalized unit for one, so the
    /// marginal price in raw amounts is `buyScaling / sellScaling` at every
    /// amount up to the sell limit. It is returned even when the pair
    /// cannot trade now (limit 0), for amount 0.
    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external view override returns (Fraction[] memory prices) {
        _checkPoolId(poolId, sellToken, buyToken);
        (uint64 sellScaling, uint64 buyScaling) = _scalings(sellToken, buyToken);
        uint256 limit = _sellLimit(sellToken, buyToken, sellScaling, buyScaling);
        uint256 largest = 0;
        prices = new Fraction[](specifiedAmounts.length);
        for (uint256 i = 0; i < specifiedAmounts.length; i++) {
            if (specifiedAmounts[i] > largest) largest = specifiedAmounts[i];
            prices[i] = Fraction(buyScaling, sellScaling);
        }
        if (largest > limit) revert LimitExceeded(limit);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Reads nothing from the store before the swap, so the measured gas
    /// is the swap's cost with the store's accounts and slots cold, as on
    /// chain. The store takes whole normalized units: an amount past the sell
    /// limit by less than one unit still fills, and the sub-unit remainder
    /// goes back to the caller. A zero amount returns an empty trade at the
    /// constant price without calling the store, which rejects it:
    /// tycho-simulation swaps zero on a direction whose limit is zero.
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (side == OrderSide.Buy) {
            revert NotImplemented("DollarStore swaps are exact-input only");
        }
        _checkPoolId(poolId, sellToken, buyToken);
        if (specifiedAmount == 0) {
            (uint64 sell, uint64 buy) = _scalings(sellToken, buyToken);
            trade.price = Fraction(buy, sell);
            return trade;
        }
        IERC20(sellToken)
            .safeTransferFrom(msg.sender, address(this), specifiedAmount);
        IERC20(sellToken).forceApprove(address(store), specifiedAmount);

        uint256 gasBefore = gasleft();
        try store.swapExactInput(
            sellToken, buyToken, specifiedAmount, 0, msg.sender, block.timestamp
        ) returns (
            uint256 amountIn, uint256 amountOut
        ) {
            trade.gasUsed = gasBefore - gasleft();
            trade.calculatedAmount = amountOut;
            if (amountIn < specifiedAmount) {
                IERC20(sellToken)
                    .safeTransfer(msg.sender, specifiedAmount - amountIn);
            }
        } catch (bytes memory reason) {
            _refuse(reason, sellToken, buyToken);
        }
        (uint64 sellScaling, uint64 buyScaling) = _scalings(sellToken, buyToken);
        trade.price = Fraction(buyScaling, sellScaling);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev `getSwapQuote` with an unbounded input returns the most the store
    /// can pay out now, in raw buy-token units, assuming every queued payout
    /// succeeds. A payout the token refuses is parked on chain and the store
    /// fills less, so the limit can then be too high. The sell limit is the
    /// same number of normalized units in the sell token.
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        _checkPoolId(poolId, sellToken, buyToken);
        (uint64 sellScaling, uint64 buyScaling) = _scalings(sellToken, buyToken);
        limits = new uint256[](2);
        limits[0] = _sellLimit(sellToken, buyToken, sellScaling, buyScaling);
        // Exact: limits[0] is a whole number of units.
        // forge-lint: disable-next-line(divide-before-multiply)
        limits[1] = limits[0] / sellScaling * buyScaling;
    }

    /// @inheritdoc ISwapAdapter
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](5);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.PriceFunction;
        capabilities[2] = Capability.ConstantPrice;
        capabilities[3] = Capability.HardLimits;
        capabilities[4] = Capability.MarginalPrice;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        address[2][] memory pairs = _pairs();
        for (uint256 i = 0; i < pairs.length; i++) {
            if (poolIdOf(pairs[i][0], pairs[i][1]) == poolId) {
                tokens = new address[](2);
                (tokens[0], tokens[1]) = _sorted(pairs[i][0], pairs[i][1]);
                return tokens;
            }
        }
        revert InvalidOrder("Unknown pool");
    }

    /// @inheritdoc ISwapAdapter
    function getPoolIds(uint256 offset, uint256 limit)
        external
        view
        override
        returns (bytes32[] memory ids)
    {
        address[2][] memory pairs = _pairs();
        if (offset >= pairs.length) return new bytes32[](0);
        uint256 end = offset + limit;
        if (end > pairs.length) end = pairs.length;
        ids = new bytes32[](end - offset);
        for (uint256 i = 0; i < ids.length; i++) {
            ids[i] = poolIdOf(pairs[offset + i][0], pairs[offset + i][1]);
        }
    }

    /// @notice The pool id of a pair, in either token order.
    function poolIdOf(address tokenA, address tokenB)
        public
        view
        returns (bytes32)
    {
        (address token0, address token1) = _sorted(tokenA, tokenB);
        return keccak256(abi.encodePacked(address(store), token0, token1));
    }

    function _checkPoolId(bytes32 poolId, address sellToken, address buyToken)
        internal
        view
    {
        if (poolIdOf(sellToken, buyToken) != poolId) {
            revert InvalidOrder("Pool/token mismatch");
        }
    }

    /// @dev The scaling factors (10 ** (decimals - 6)) of the two tokens. The
    /// store keeps an asset's scaling factor after it is delisted, so it is
    /// non-zero for every token a component was created for.
    function _scalings(address sellToken, address buyToken)
        internal
        view
        returns (uint64 sellScaling, uint64 buyScaling)
    {
        sellScaling = store.assetScalingFactor(sellToken);
        buyScaling = store.assetScalingFactor(buyToken);
        if (sellScaling == 0 || buyScaling == 0) {
            revert InvalidOrder("Token never listed");
        }
    }

    /// @dev Re-raises a store refusal as the adapter error for it: below one
    /// normalized unit (ZeroAmount) is TooSmall, more than the store can fill
    /// (InsufficientLiquidity) is LimitExceeded. Anything else is re-raised
    /// unchanged.
    function _refuse(bytes memory reason, address sellToken, address buyToken)
        internal
        view
    {
        // The first four bytes of the revert data are the error selector.
        // forge-lint: disable-next-line(unsafe-typecast)
        bytes4 selector = bytes4(reason);
        (uint64 sellScaling, uint64 buyScaling) = _scalings(sellToken, buyToken);
        if (selector == IDollarStore.ZeroAmount.selector) {
            revert TooSmall(sellScaling);
        }
        if (selector == IDollarStore.InsufficientLiquidity.selector) {
            revert LimitExceeded(_sellLimit(
                    sellToken, buyToken, sellScaling, buyScaling
                ));
        }
        // Re-raise the store's revert data as it is.
        assembly ("memory-safe") {
            revert(add(reason, 0x20), mload(reason))
        }
    }

    function _sellLimit(
        address sellToken,
        address buyToken,
        uint64 sellScaling,
        uint64 buyScaling
    ) internal view returns (uint256) {
        uint256 maxOut = store.getSwapQuote(
            sellToken, buyToken, type(uint256).max
        );
        // Exact: the quote is a whole number of units.
        // forge-lint: disable-next-line(divide-before-multiply)
        return maxOut / buyScaling * sellScaling;
    }

    /// @dev Every pair the store routes in one leg now: hub-hub and
    /// hub-spoke, for listed assets. Store pool indexes run from 0 to
    /// poolCount() - 1; index 0 is the hub.
    function _pairs() internal view returns (address[2][] memory pairs) {
        address[] memory hub = store.getPoolAssets(0);
        uint256 poolCount = store.poolCount();
        address[] memory spokeAssets = new address[](0);
        for (uint256 p = 1; p < poolCount; p++) {
            // Pool indexes are uint16 on chain, so poolCount() fits. The
            // loop is bounded by the pools governance created.
            // forge-lint: disable-next-line(unsafe-typecast,calls-loop)
            address[] memory assets = store.getPoolAssets(uint16(p));
            for (uint256 a = 0; a < assets.length; a++) {
                // A removed pool keeps its asset list; an asset relisted in
                // another pool is counted there.
                // forge-lint: disable-next-line(calls-loop)
                bool listed = store.isAssetListed(assets[a]);
                // forge-lint: disable-next-line(calls-loop)
                if (listed && store.assetPoolId(assets[a]) == p) {
                    spokeAssets = _append(spokeAssets, assets[a]);
                }
            }
        }
        uint256 hubPairs =
            hub.length < 2 ? 0 : hub.length * (hub.length - 1) / 2;
        pairs = new address[2][](hubPairs + spokeAssets.length * hub.length);
        uint256 k = 0;
        for (uint256 i = 0; i < hub.length; i++) {
            for (uint256 j = i + 1; j < hub.length; j++) {
                pairs[k++] = [hub[i], hub[j]];
            }
        }
        for (uint256 s = 0; s < spokeAssets.length; s++) {
            for (uint256 h = 0; h < hub.length; h++) {
                pairs[k++] = [spokeAssets[s], hub[h]];
            }
        }
    }

    function _append(address[] memory list, address item)
        internal
        pure
        returns (address[] memory out)
    {
        out = new address[](list.length + 1);
        for (uint256 i = 0; i < list.length; i++) {
            out[i] = list[i];
        }
        out[list.length] = item;
    }

    function _sorted(address a, address b)
        internal
        pure
        returns (address, address)
    {
        return a < b ? (a, b) : (b, a);
    }
}

interface IDollarStore {
    function getSwapQuote(address offerAsset, address wantAsset, uint256 amount)
        external
        view
        returns (uint256);

    function swapExactInput(
        address offerAsset,
        address wantAsset,
        uint256 amount,
        uint256 minAmountOut,
        address recipient,
        uint256 deadline
    ) external returns (uint256 amountIn, uint256 amountOut);

    function assetScalingFactor(address asset) external view returns (uint64);

    error ZeroAmount();
    error InsufficientLiquidity(
        address wantAsset, uint256 fillableAmountOut, uint256 requestedAmountOut
    );

    function isAssetListed(address asset) external view returns (bool);

    function assetPoolId(address asset) external view returns (uint16);

    function poolCount() external view returns (uint256);

    function getPoolAssets(uint16 poolId)
        external
        view
        returns (address[] memory);
}
