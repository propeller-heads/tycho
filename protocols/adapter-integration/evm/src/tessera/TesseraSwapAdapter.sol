// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.27;
import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";

interface ITesseraSwap {
    function tesseraSwapViewAmounts(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified
    ) external view returns (uint256, uint256);
    function tesseraSwapWithAllowances(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256 amountCheck,
        address recipient,
        bytes calldata swapData
    ) external;
}

/// The Engine and Pair contracts are unverified. Their interfaces and field
/// names follow the ABI in the SDK Tessera provides to integrators.
interface ITesseraEngine {
    /// `(true, pair)` for a registered token pair, in either order.
    function getTesseraPool(address tokenA, address tokenB)
        external
        view
        returns (bool registered, address pair);
    /// The quote TesseraSwap gives `sender`. With `sender` = address(0) it
    /// skips the caller-code screen, which an unflagged caller such as today's
    /// TychoRouter also passes, so the output is the same at a fraction of
    /// the gas. Exact output when `amountSpecified` is negative.
    function swapAmountView(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        address sender
    ) external view returns (uint256 amountIn, uint256 amountOut);
    /// While block.number <= fastPullBlock, every Pair skips its first
    /// fastPullLevel orders.
    function fastPullBlock() external view returns (uint256);
}

interface ITesseraPair {
    struct Order {
        uint160 amount;
        uint64 priceMultiplierPpm;
        bool isDead;
    }

    struct StalenessWidenParams {
        uint64 cutoffBlock;
        uint64 widenStartBlock;
        uint32 widenCoeff;
        uint32 maxWidenPpm;
    }

    struct CurveState {
        Order[20] baseToQuoteOrders;
        Order[20] quoteToBaseOrders;
    }

    /// The venue fills a side's orders from the first, or from fastPullLevel
    /// while fast pull is active, up to the first dead one. Fills since the
    /// last ladder post, in input units, are deducted from that ladder; with
    /// reduceAccumulator set, opposite-side fills converted at the mid price
    /// are added back.
    struct State {
        uint256 cumulativeQuoteAmountInSinceUpd;
        uint256 cumulativeBaseAmountInSinceUpd;
        uint32 widenPpm;
        uint32 cumulativeSwapAmountMultiplierPpm;
        uint8 fastPullLevel;
        bool reduceAccumulator;
        StalenessWidenParams stalenessWidenParams;
        uint32 widenlistedPpm;
        bool enforceWhitelist;
        uint32 prioFeeThresholdFactor;
        uint32 widenPrioFeePpm;
        CurveState curveState;
    }
    function baseToken() external view returns (address);
    function quoteToken() external view returns (address);
    function poolState() external view returns (State memory);
}

/// Prices, limits and the quote before settlement come from the Engine's view
/// quote; swaps settle through TesseraSwap itself, so post-swap storage is the
/// venue's own.
///
/// Tessera prices by caller. A caller its code screen flags pays the Pair's
/// widenlistedPpm on top of the spread, the Engine can reject a sender or
/// tx.origin by category, and with enforceWhitelist set only whitelisted
/// callers trade. Quotes here assume an unflagged, permitted caller, as
/// today's TychoRouter is; simulation does not show a flagged or rejected
/// caller, and caller-aware quoting is left as a follow-up.
///
/// Settlement also widens the price by the Pair's widenPrioFeePpm once the
/// priority fee reaches the Engine's global threshold scaled by the Pair's
/// prioFeeThresholdFactor. The Tycho VM simulates at zero priority fee, so
/// every quote and limit here is the unwidened price.
contract TesseraSwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;
    /// Raw output units the price step buys. The view rounds output down, so
    /// the measured price is low by under one unit in this many. Fixed rather
    /// than derived from decimals(), which reads 0 in the Tycho VM.
    uint256 internal constant PRICE_STEP_OUTPUT = 1e6;

    /// Bisection rounds when the computed limit does not quote.
    uint256 internal constant FALLBACK_PROBES = 8;

    /// Gas the TychoRouter at 0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67
    /// pays to settle beyond the settlement this adapter measures. TesseraSwap
    /// scans its caller's whole code on settlement, about 79 gas per byte, so
    /// the router's 22,390 bytes cost about 1.23M more than this adapter's
    /// 6,883. The rest is storage that the adapter's quote has already warmed
    /// and the router reads cold. Measured on a Base fork at block
    /// 52,890,608; re-measure when the router is redeployed or this adapter's
    /// code size changes.
    uint256 internal constant ROUTER_SETTLEMENT_EXTRA_GAS = 1_360_000;

    ITesseraSwap public immutable tesseraSwap;
    /// TesseraSwap exposes neither its Engine nor its treasury, so both are
    /// fixed per deployment, like the Substreams configuration. A treasury
    /// rotation leaves the old treasury's balance capping the limits until the
    /// adapter is redeployed.
    ITesseraEngine public immutable engine;
    address public immutable treasury;

    constructor(address venue, address engine_, address treasury_) {
        tesseraSwap = ITesseraSwap(venue);
        engine = ITesseraEngine(engine_);
        treasury = treasury_;
    }

    function _pair(bytes32 poolId, address sell, address buy)
        internal
        view
        returns (ITesseraPair pair)
    {
        pair = ITesseraPair(address(bytes20(poolId)));
        address base = pair.baseToken();
        address quote = pair.quoteToken();
        if (!((sell == base && buy == quote) || (sell == quote && buy == base)))
        {
            revert InvalidOrder("Pool/token mismatch");
        }
        // TesseraSwap routes by token pair, not pool id: a Pair the registry
        // no longer points at would quote and settle against its successor.
        (bool registered, address current) = engine.getTesseraPool(sell, buy);
        if (!registered || current != address(pair)) {
            revert Unavailable("Pair not registered");
        }
    }

    function getTokens(bytes32 poolId)
        external
        view
        returns (address[] memory tokens)
    {
        ITesseraPair pair = ITesseraPair(address(bytes20(poolId)));
        tokens = new address[](2);
        tokens[0] = pair.baseToken();
        tokens[1] = pair.quoteToken();
    }

    function getPoolIds(uint256, uint256)
        external
        pure
        returns (bytes32[] memory)
    {
        revert NotImplemented("No pair enumeration");
    }

    function getCapabilities(bytes32, address, address)
        external
        pure
        returns (Capability[] memory caps)
    {
        caps = new Capability[](4);
        caps[0] = Capability.SellOrder;
        caps[1] = Capability.BuyOrder;
        caps[2] = Capability.PriceFunction;
        caps[3] = Capability.MarginalPrice;
    }

    function _quote(address sell, address buy, uint256 amount)
        internal
        view
        returns (uint256 out)
    {
        if (amount > uint256(type(int256).max)) return 0;
        try engine.swapAmountView(
            sell, buy, int256(amount), address(0)
        ) returns (
            uint256, uint256 output
        ) {
            return output;
        } catch {
            return 0;
        }
    }

    function _quoteInput(address sell, address buy, uint256 output)
        internal
        view
        returns (uint256 input, uint256 quotedOutput)
    {
        if (output > uint256(type(int256).max)) return (0, 0);
        try engine.swapAmountView(
            sell, buy, -int256(output), address(0)
        ) returns (
            uint256 amountIn, uint256 amountOut
        ) {
            return (amountIn, amountOut);
        } catch {
            return (0, 0);
        }
    }

    // Finite difference of the venue's view quote; do not substitute average
    // execution price.
    function _price(address sell, address buy, uint256 amount)
        internal
        view
        returns (Fraction memory)
    {
        (uint256 step,) = _quoteInput(sell, buy, PRICE_STEP_OUTPUT);
        if (step == 0) return Fraction(0, 1);
        uint256 beforeOut = amount == 0 ? 0 : _quote(sell, buy, amount);
        uint256 afterOut = _quote(sell, buy, amount + step);
        return Fraction(afterOut > beforeOut ? afterOut - beforeOut : 0, step);
    }

    function price(
        bytes32 poolId,
        address sell,
        address buy,
        uint256[] memory amounts
    ) external view returns (Fraction[] memory prices) {
        _pair(poolId, sell, buy);
        prices = new Fraction[](amounts.length);
        for (uint256 i; i < amounts.length; ++i) {
            prices[i] = _price(sell, buy, amounts[i]);
            // A Pair that cannot quote (stale or dead ladder) has no price; a
            // zero price would read as a free token.
            if (prices[i].numerator == 0) revert Unavailable("No price");
        }
    }

    /// The largest sell amount that fills in full, computed from the Pair's
    /// ladder rather than searched for:
    /// 1. the sell side's orders up to the first dead one, which the venue
    ///    never fills past;
    /// 2. minus fills since the operator last posted the ladder, net of fills
    ///    in the opposite direction, which give capacity back;
    /// 3. capped so the output fits the treasury's balance of the buy token,
    ///    which the view quote ignores but settlement needs.
    /// The `poolState()` field meanings are inferred, not documented. If the
    /// computed amount does not quote, a stale price or a changed layout, the
    /// limit falls back to a conservative search below it.
    function getLimits(bytes32 poolId, address sell, address buy)
        external
        view
        returns (uint256[] memory limits)
    {
        ITesseraPair pair = _pair(poolId, sell, buy);
        limits = new uint256[](2);
        uint256 amount = _ladderBound(pair, sell, buy);
        if (amount == 0) return limits;
        uint256 output = _quote(sell, buy, amount);
        if (output == 0) (amount, output) = _searchBelow(sell, buy, amount);
        if (output == 0) return limits;
        uint256 inventory = IERC20(buy).balanceOf(treasury);
        if (output > inventory) {
            // The exact-output input rounds up, so one unit less fits.
            (uint256 needed,) = _quoteInput(sell, buy, inventory);
            if (needed <= 1) return limits;
            amount = needed - 1;
            output = _quote(sell, buy, amount);
        }
        limits[0] = amount;
        limits[1] = output;
    }

    function _ladderBound(ITesseraPair pair, address sell, address buy)
        internal
        view
        returns (uint256 bound)
    {
        ITesseraPair.State memory state = pair.poolState();
        bool sellBase = sell == pair.baseToken();
        ITesseraPair.Order[20] memory orders = sellBase
            ? state.curveState.baseToQuoteOrders
            : state.curveState.quoteToBaseOrders;
        uint256 first =
            block.number <= engine.fastPullBlock() ? state.fastPullLevel : 0;
        if (first >= 20) return 0;
        for (uint256 i; i < first; ++i) {
            if (orders[i].isDead) return 0;
        }
        for (uint256 i = first; i < 20 && !orders[i].isDead; ++i) {
            bound += orders[i].amount;
        }
        uint256 filled = sellBase
            ? state.cumulativeBaseAmountInSinceUpd
            : state.cumulativeQuoteAmountInSinceUpd;
        if (state.reduceAccumulator) {
            uint256 opposite = sellBase
                ? state.cumulativeQuoteAmountInSinceUpd
                : state.cumulativeBaseAmountInSinceUpd;
            // The venue converts at the mid price. Selling the opposite fills
            // back pays the spread, so this returns slightly less and the limit
            // stays on the safe side, without decoding the packed mid price.
            uint256 returned = opposite == 0 ? 0 : _quote(buy, sell, opposite);
            filled = filled > returned ? filled - returned : 0;
        }
        bound = bound > filled ? bound - filled : 0;
    }

    /// The largest amount found by bisection between a one-step quote and
    /// `upper`, with its output, or zeros when even the step does not quote.
    function _searchBelow(address sell, address buy, uint256 upper)
        internal
        view
        returns (uint256 amount, uint256 output)
    {
        (amount,) = _quoteInput(sell, buy, PRICE_STEP_OUTPUT);
        if (amount == 0 || amount >= upper) return (0, 0);
        output = _quote(sell, buy, amount);
        if (output == 0) return (0, 0);
        for (uint256 i; i < FALLBACK_PROBES && upper > amount + 1; ++i) {
            uint256 mid = amount + (upper - amount) / 2;
            uint256 midOutput = _quote(sell, buy, mid);
            if (midOutput > 0) {
                (amount, output) = (mid, midOutput);
            } else {
                upper = mid;
            }
        }
    }

    function swap(
        bytes32 poolId,
        address sell,
        address buy,
        OrderSide side,
        uint256 specified
    ) external returns (Trade memory trade) {
        _pair(poolId, sell, buy);
        if (specified == 0 || specified > uint256(type(int256).max)) {
            revert InvalidOrder("Invalid amount");
        }
        int256 signedAmount =
            side == OrderSide.Sell ? int256(specified) : -int256(specified);
        (uint256 amountIn, uint256 amountOut) =
            engine.swapAmountView(sell, buy, signedAmount, address(0));
        if (amountIn == 0 || amountOut == 0) {
            revert Unavailable("Unquotable amount");
        }
        IERC20(sell).safeTransferFrom(msg.sender, address(this), amountIn);
        IERC20(sell).forceApprove(address(tesseraSwap), amountIn);
        uint256 outputBefore = IERC20(buy).balanceOf(msg.sender);
        uint256 gasBefore = gasleft();
        // Empty data selects integrator tag 0: view and settlement agree while
        // A[0] == 0.
        tesseraSwap.tesseraSwapWithAllowances(
            sell,
            buy,
            signedAmount,
            side == OrderSide.Sell ? 0 : amountIn,
            msg.sender,
            ""
        );
        trade.gasUsed = gasBefore - gasleft() + ROUTER_SETTLEMENT_EXTRA_GAS;
        // Report what the venue settled, not its view quote, so any gap
        // between the two shows up in simulated output.
        uint256 received = IERC20(buy).balanceOf(msg.sender) - outputBefore;
        if (side == OrderSide.Buy && received < specified) {
            revert Unavailable("Exact output not settled");
        }
        trade.calculatedAmount = side == OrderSide.Sell ? received : amountIn;
        trade.price = _price(sell, buy, 0);
    }
}
