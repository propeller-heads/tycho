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
/// @dev Each pool is a Tessera pair: a dedicated contract (an EIP-1967 proxy)
/// holding that pair's price state and identity, registered on the pricing
/// engine. The pool id is the pair's contract address (high 20 bytes of the
/// bytes32), matching the substreams component id; the pair's tokens are read
/// from its own `baseToken()`/`quoteToken()` getters. A single treasury holds
/// all inventory and settles by allowance through the verified `TesseraSwap`
/// entrypoint. Quotes are a pure function of tracked storage plus a freshness
/// gate on the age of the last price post relative to `block.number`;
/// simulating with the indexed block's environment always passes the gate
/// because prices post every block.
contract TesseraSwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    /// @dev Bisection rounds in getLimits; halves the search interval each
    /// round.
    uint256 constant LIMIT_BISECTION_ROUNDS = 32;
    /// @dev Decade-scan rounds when searching for the smallest quotable
    /// amount (dust inputs round the quote to zero output).
    uint256 constant MIN_QUOTABLE_SCAN_ROUNDS = 13;

    ITesseraSwap public immutable tesseraSwap;

    constructor(address tesseraSwap_) {
        tesseraSwap = ITesseraSwap(tesseraSwap_);
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
    /// above the pair's quote-ladder capacity the venue returns a zero output
    /// instead of reverting, and a disabled or stale pair quotes zero at every
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
    /// @dev The pool id is the pair's contract address; the pair exposes its
    /// own token getters.
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        ITesseraPair pair = _pair(poolId);
        tokens = new address[](2);
        tokens[0] = pair.baseToken();
        tokens[1] = pair.quoteToken();
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Tessera exposes no on-chain enumeration from tokens to pair
    /// contracts (the public helper returns token pairs only, and the
    /// engine's registry is a mapping without a getter). Pool ids come from
    /// the substreams component ids.
    function getPoolIds(uint256, uint256)
        external
        pure
        override
        returns (bytes32[] memory)
    {
        revert NotImplemented("TesseraSwapAdapter.getPoolIds");
    }

    function _pair(bytes32 poolId) internal pure returns (ITesseraPair) {
        return ITesseraPair(address(bytes20(poolId)));
    }

    function _validatePoolTokens(
        bytes32 poolId,
        address sellToken,
        address buyToken
    ) internal view {
        ITesseraPair pair = _pair(poolId);
        address base = pair.baseToken();
        address quote = pair.quoteToken();
        bool validPair = (sellToken == base && buyToken == quote)
            || (sellToken == quote && buyToken == base);
        if (!validPair) {
            revert InvalidOrder("Pool/token mismatch");
        }
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
        uint256 amount = 10 ** IERC20Metadata(sellToken).decimals() / 1e6;
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

interface ITesseraPair {
    function baseToken() external view returns (address);

    function quoteToken() external view returns (address);
}
