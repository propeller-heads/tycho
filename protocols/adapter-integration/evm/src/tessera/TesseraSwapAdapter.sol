// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.27;
import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IERC20Metadata
} from "openzeppelin-contracts/contracts/token/ERC20/extensions/IERC20Metadata.sol";

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

interface ITesseraPair {
    struct Order {
        uint160 amount;
        uint64 priceMultiplierPpm;
        bool isDead;
    }

    struct Staleness {
        uint64 cutoffBlock;
        uint64 widenStartBlock;
        uint32 widenCoeff;
        uint32 maxWidenPpm;
    }

    struct Curve {
        Order[20] baseToQuoteOrders;
        Order[20] quoteToBaseOrders;
    }

    struct State {
        uint256 cumulativeQuote;
        uint256 cumulativeBase;
        uint32 widenPpm;
        uint32 accumulatorMultiplier;
        uint8 fastPullLevel;
        bool reduceAccumulator;
        Staleness staleness;
        uint32 widenlistedPpm;
        bool enforceWhitelist;
        uint32 priorityFactor;
        uint32 priorityWiden;
        Curve curve;
    }
    function baseToken() external view returns (address);
    function quoteToken() external view returns (address);
    function poolState() external view returns (State memory);
}

/// Real venue execution supplies pricing and post-swap storage. No SDK math is
/// embedded.
contract TesseraSwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;
    ITesseraSwap public immutable tesseraSwap;

    constructor(address venue) {
        tesseraSwap = ITesseraSwap(venue);
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
        try tesseraSwap.tesseraSwapViewAmounts(
            sell, buy, int256(amount)
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
        try tesseraSwap.tesseraSwapViewAmounts(
            sell, buy, -int256(output)
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
        // Choose the step through the venue, accounting for output token
        // precision. A fixed one-micro-USDC step rounds to zero when buying
        // cbBTC.
        uint256 target = 10 ** IERC20Metadata(buy).decimals() / 1e6;
        if (target < 1000) target = 1000;
        (uint256 step,) = _quoteInput(sell, buy, target);
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
        }
    }

    function getLimits(bytes32 poolId, address sell, address buy)
        external
        view
        returns (uint256[] memory limits)
    {
        ITesseraPair pair = _pair(poolId, sell, buy);
        ITesseraPair.State memory state = pair.poolState();
        ITesseraPair.Order[20] memory orders = sell == pair.baseToken()
            ? state.curve.baseToQuoteOrders
            : state.curve.quoteToBaseOrders;
        uint256 upper;
        // Sum is only a search bound. Selection, accumulators, widening and
        // stale-price rejection remain in venue bytecode, including dead and
        // fast-pull orders.
        for (uint256 i; i < 20; ++i) {
            upper += orders[i].amount;
        }
        limits = new uint256[](2);
        // Seed by an exact-output quote so low-decimal outputs do not round to
        // zero. This is a conservative hint: if no larger probe succeeds, the
        // output bound is the seed's exact-output amount.
        uint256 target = 10 ** IERC20Metadata(buy).decimals() / 1e6;
        if (target == 0) target = 1;
        (uint256 good, uint256 out) = _quoteInput(sell, buy, target);
        if (good == 0 || good > upper || out == 0) return limits;
        limits[1] = out;
        // Three venue probes fit the 3M budget at the pinned block. This is a
        // conservative hint, with up to 25% of the initial interval unresolved.
        for (uint256 i; i < 2 && upper > good + 1; ++i) {
            uint256 mid = good + (upper - good) / 2;
            out = _quote(sell, buy, mid);
            if (out > 0) {
                good = mid;
                limits[1] = out;
            } else {
                upper = mid;
            }
        }
        limits[0] = good;
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
            tesseraSwap.tesseraSwapViewAmounts(sell, buy, signedAmount);
        if (amountIn == 0 || amountOut == 0) {
            revert Unavailable("Unquotable amount");
        }
        IERC20(sell).safeTransferFrom(msg.sender, address(this), amountIn);
        IERC20(sell).forceApprove(address(tesseraSwap), amountIn);
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
        trade.gasUsed = gasBefore - gasleft();
        trade.calculatedAmount = side == OrderSide.Sell ? amountOut : amountIn;
        trade.price = _price(sell, buy, 0);
    }
}
