// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IERC20Metadata
} from "openzeppelin-contracts/contracts/token/ERC20/extensions/IERC20Metadata.sol";

/// @title TesseraSwapAdapter
/// @notice Adapter for swapping tokens on Tessera V (Wintermute's propAMM).
/// @dev Each pool is a `base/USDC` book backed by a per-book price store; a
/// single treasury holds all inventory and settles by allowance through the
/// verified `TesseraSwap` entrypoint. The pool id packs
/// `tesseraswap (20 bytes) | base token low 12 bytes`, matching the substreams
/// component id. Quotes are a pure function of tracked storage (engine +
/// per-book store) plus a freshness gate on the age of the last price post
/// relative to `block.number`; simulating with the indexed block's environment
/// always passes the gate because prices post every block.
contract TesseraSwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    /// @dev Bisection rounds in getLimits; halves the search interval each
    /// round.
    uint256 constant LIMIT_BISECTION_ROUNDS = 32;
    /// @dev Decade-scan rounds when searching for the smallest quotable
    /// amount (dust inputs round the quote to zero output).
    uint256 constant MIN_QUOTABLE_SCAN_ROUNDS = 13;

    ITesseraSwap public immutable tesseraSwap;
    ITesseraPairs public immutable pairHelper;
    address public immutable usdc;

    constructor(address tesseraSwap_, address pairHelper_, address usdc_) {
        tesseraSwap = ITesseraSwap(tesseraSwap_);
        pairHelper = ITesseraPairs(pairHelper_);
        usdc = usdc_;
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
                _quoteExactIn(sellToken, buyToken, specifiedAmounts[i]);
            if (amountOut == 0) {
                revert TooSmall(0);
            }
            prices[i] = Fraction(amountOut, specifiedAmounts[i]);
        }
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Both order sides settle through `tesseraSwapWithAllowances`: a
    /// positive `amountSpecified` is exact-input, a negative one exact-output.
    /// The settled amounts are taken from the view quote rather than a
    /// recipient balance-diff — the view path is byte-identical to the
    /// executable path at a pinned block, and the off-chain simulation engine
    /// does not model the recipient's output-token balance.
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (specifiedAmount == 0) {
            return trade;
        }
        _validatePoolTokens(poolId, sellToken, buyToken);

        uint256 amountIn;
        uint256 amountOut;
        uint256 amountCheck;
        int256 amountSpecified;
        if (specifiedAmount > uint256(type(int256).max)) {
            revert Unavailable("Amount exceeds int256");
        }
        if (side == OrderSide.Sell) {
            // Bounded by the int256 guard above.
            // forge-lint: disable-next-line(unsafe-typecast)
            amountSpecified = int256(specifiedAmount);
            // Exact-in: the venue checks `amountOut >= amountCheck`.
            amountCheck = 0;
        } else {
            // Bounded by the int256 guard above.
            // forge-lint: disable-next-line(unsafe-typecast)
            amountSpecified = -int256(specifiedAmount);
            // Exact-out: the venue checks `amountIn <= amountCheck`.
            amountCheck = type(uint256).max;
        }
        (amountIn, amountOut) = _quoteView(sellToken, buyToken, amountSpecified);
        if (amountOut == 0 || amountIn == 0) {
            revert TooSmall(0);
        }

        IERC20(sellToken).safeTransferFrom(msg.sender, address(this), amountIn);
        IERC20(sellToken).forceApprove(address(tesseraSwap), amountIn);

        uint256 gasBefore = gasleft();
        tesseraSwap.tesseraSwapWithAllowances(
            sellToken, buyToken, amountSpecified, amountCheck, msg.sender, ""
        );
        trade.gasUsed = gasBefore - gasleft();
        trade.calculatedAmount = side == OrderSide.Sell ? amountOut : amountIn;
        trade.price = _marginalPriceAfterSwap(sellToken, buyToken);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev The maximum sellable amount is found by bisecting the view quote:
    /// above the book's quote-ladder capacity the venue returns a zero output
    /// instead of reverting, and a disabled or stale book quotes zero at every
    /// size (so its limits read zero and it drops out of routing on its own).
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        _validatePoolTokens(poolId, sellToken, buyToken);
        limits = new uint256[](2);

        uint256 good = _smallestQuotable(sellToken, buyToken);
        if (good == 0) {
            return limits;
        }

        uint256 bad = good * 2;
        while (
            bad < type(uint256).max / 2
                && _quoteExactIn(sellToken, buyToken, bad) > 0
        ) {
            good = bad;
            bad *= 2;
        }
        for (uint256 i = 0; i < LIMIT_BISECTION_ROUNDS; i++) {
            uint256 mid = good + (bad - good) / 2;
            if (_quoteExactIn(sellToken, buyToken, mid) > 0) {
                good = mid;
            } else {
                bad = mid;
            }
        }

        limits[0] = good;
        limits[1] = _quoteExactIn(sellToken, buyToken, good);
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
        capabilities[1] = Capability.BuyOrder;
        capabilities[2] = Capability.PriceFunction;
        capabilities[3] = Capability.HardLimits;
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Resolved through the pair-list helper: the pool id carries only
    /// the base token's low 12 bytes, which cannot be expanded to an address
    /// on-chain. Only used for testing; the hot paths (price/swap/limits)
    /// receive full token addresses and never touch the helper.
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        address base = _baseTokenFromHelper(poolId);
        if (base == address(0)) {
            revert InvalidOrder("Unknown pool");
        }
        tokens = new address[](2);
        tokens[0] = base;
        tokens[1] = usdc;
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Enumerates the live pair list from the helper. Delisted books
    /// (which still exist as components but quote zero) are not returned.
    function getPoolIds(uint256 offset, uint256 limit)
        external
        view
        override
        returns (bytes32[] memory ids)
    {
        address[][] memory pairs = pairHelper.getTesseraPairs();
        if (offset >= pairs.length) {
            return new bytes32[](0);
        }
        uint256 end = offset + limit;
        if (end > pairs.length) {
            end = pairs.length;
        }
        ids = new bytes32[](end - offset);
        for (uint256 i = 0; i < ids.length; i++) {
            ids[i] = _poolId(pairs[offset + i][0]);
        }
    }

    /// @notice Pool id for a book: `tesseraswap (20 bytes) | base token low 12
    /// bytes`.
    function _poolId(address baseToken) internal view returns (bytes32) {
        return bytes32(bytes20(address(tesseraSwap)))
            | bytes32(uint256(uint160(baseToken)) & type(uint96).max);
    }

    function _validatePoolTokens(
        bytes32 poolId,
        address sellToken,
        address buyToken
    ) internal view {
        if (address(bytes20(poolId)) != address(tesseraSwap)) {
            revert InvalidOrder("Pool id entrypoint mismatch");
        }
        address base;
        if (sellToken == usdc) {
            base = buyToken;
        } else if (buyToken == usdc) {
            base = sellToken;
        } else {
            revert InvalidOrder("One side must be the quote token");
        }
        uint256 suffix = uint256(poolId) & type(uint96).max;
        if (uint256(uint160(base)) & type(uint96).max != suffix) {
            revert InvalidOrder("Pool/token mismatch");
        }
    }

    /// @dev Base token for a pool id, from the helper's live pair list.
    function _baseTokenFromHelper(bytes32 poolId)
        internal
        view
        returns (address)
    {
        uint256 suffix = uint256(poolId) & type(uint96).max;
        address[][] memory pairs = pairHelper.getTesseraPairs();
        for (uint256 i = 0; i < pairs.length; i++) {
            address base = pairs[i][0];
            if (uint256(uint160(base)) & type(uint96).max == suffix) {
                return base;
            }
        }
        return address(0);
    }

    function _quoteView(address sellToken, address buyToken, int256 amount)
        internal
        view
        returns (uint256 amountIn, uint256 amountOut)
    {
        (amountIn, amountOut) =
            tesseraSwap.tesseraSwapViewAmounts(sellToken, buyToken, amount);
    }

    function _quoteExactIn(address sellToken, address buyToken, uint256 amount)
        internal
        view
        returns (uint256 amountOut)
    {
        if (amount > uint256(type(int256).max)) {
            return 0;
        }
        // Bounded by the int256 guard above.
        // forge-lint: disable-next-line(unsafe-typecast)
        (, amountOut) = _quoteView(sellToken, buyToken, int256(amount));
    }

    /// @dev Smallest amount the venue quotes a non-zero output for, found by
    /// scanning upward in decades from 1e-6 of one sell-token unit (dust
    /// inputs round the output to zero).
    function _smallestQuotable(address sellToken, address buyToken)
        internal
        view
        returns (uint256)
    {
        uint256 amount =
            10 ** IERC20Metadata(sellToken).decimals() / 1e6;
        if (amount == 0) {
            amount = 1;
        }
        for (uint256 i = 0; i < MIN_QUOTABLE_SCAN_ROUNDS; i++) {
            if (_quoteExactIn(sellToken, buyToken, amount) > 0) {
                return amount;
            }
            amount *= 10;
        }
        return 0;
    }

    /// @dev Marginal price after the trade; `Fraction(0, 1)` when the trade
    /// consumed the remaining quotable size.
    function _marginalPriceAfterSwap(address sellToken, address buyToken)
        internal
        view
        returns (Fraction memory)
    {
        uint256 probe = _smallestQuotable(sellToken, buyToken);
        if (probe > 0) {
            uint256 amountOut = _quoteExactIn(sellToken, buyToken, probe);
            if (amountOut > 0) {
                return Fraction(amountOut, probe);
            }
        }
        return Fraction(0, 1);
    }
}

interface ITesseraSwap {
    function tesseraSwapViewAmounts(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified
    ) external view returns (uint256 amountIn, uint256 amountOut);

    function tesseraSwapWithAllowances(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256 amountCheck,
        address recipient,
        bytes calldata swapData
    ) external;
}

interface ITesseraPairs {
    function getTesseraPairs() external view returns (address[][] memory);
}
