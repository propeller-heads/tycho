// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {
    IERC20
} from "../../lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "../../lib/openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {ISwapAdapter} from "../interfaces/ISwapAdapter.sol";
import {Funding} from "./Funding.sol";
import {IPartyInfo} from "./IPartyInfo.sol";
import {IPartyPlanner} from "./IPartyPlanner.sol";
import {IPartyPool} from "./IPartyPool.sol";

contract LiquidityPartySwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    // Forge lint wants immutables to be all caps. Slither wants them to be
    // mixed case. Why do we care about pedantic linters? The Solidity style
    // guide mentions "constants" but never "immutables." Faced with an
    // irresolvable linter conflict, I chose to disable the slither linter,
    // since its detection of immutables as constants seems to be broken.
    // slither-disable-next-line naming-convention
    IPartyPlanner public immutable PLANNER;
    // slither-disable-next-line naming-convention
    IPartyInfo public immutable INFO;

    constructor(IPartyPlanner planner, IPartyInfo info) {
        PLANNER = planner;
        INFO = info;
    }

    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external view override returns (Fraction[] memory prices) {
        IPartyPool pool = _poolFromId(poolId);
        (uint256 indexIn, uint256 indexOut) =
            _tokenIndexes(pool, sellToken, buyToken);
        prices = new Fraction[](specifiedAmounts.length);
        for (uint256 i = 0; i < specifiedAmounts.length; i++) {
            uint256 amount = specifiedAmounts[i];
            if (amount == 0) {
                // Marginal price support
                prices[i] = _marginalPrice(pool, indexIn, indexOut);
            } else {
                // Regular slippage calculation.
                // slither-disable-next-line unused-return calls-loop
                (
                    /*uint256 amountIn*/,
                    uint256 amountOut, /*uint256 inFee*/
                ) = INFO.swapAmounts(pool, indexIn, indexOut, amount);
                prices[i].numerator = amountOut;
                // Use `amount` (not amountIn) as denominator, because excess
                // dust from PREFUNDING is kept by the pool.
                prices[i].denominator = amount;
            }
        }
    }

    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external returns (Trade memory trade) {
        // Setup
        address swapper = msg.sender;
        IPartyPool pool = _poolFromId(poolId);
        (uint256 indexIn, uint256 indexOut) =
            _tokenIndexes(pool, sellToken, buyToken);

        // The pool only performs exact-input swaps. For a sell, the specified
        // amount is the input. For a buy, the specified amount is the desired
        // net output, so we first quote the required input via the exact-output
        // quoter and constrain the swap with minAmountOut = the desired output.
        uint256 amountIn;
        uint256 minAmountOut;
        if (side == OrderSide.Sell) {
            amountIn = specifiedAmount;
            minAmountOut = 0;
        } else {
            // slither-disable-next-line unused-return
            (amountIn,) = INFO.swapAmountsForExactOutput(
                pool, indexIn, indexOut, specifiedAmount
            );
            minAmountOut = specifiedAmount;
        }

        // Transfer and Swap
        uint256 startingGas = gasleft();
        IERC20(sellToken).safeTransferFrom(swapper, address(pool), amountIn);
        // slither-disable-next-line unused-return
        try pool.swap(
            address(this), // pool requires msg.sender == payer for PREFUNDING
            Funding.PREFUNDING,
            swapper,
            indexIn,
            indexOut,
            amountIn,
            minAmountOut,
            0,
            false,
            ""
        ) returns (
            uint256, uint256 amountOut, uint256
        ) {
            uint256 endingGas = gasleft();
            uint256 gasUsed = startingGas - endingGas;
            Fraction memory poolPrice = _marginalPrice(pool, indexIn, indexOut);
            // For a sell the calculated amount is the output received; for a
            // buy it is the input spent.
            uint256 calculatedAmount =
                side == OrderSide.Sell ? amountOut : amountIn;
            // forge-lint: disable-next-line(named-struct-fields)
            return Trade(calculatedAmount, gasUsed, poolPrice);
        } catch Error(string memory reason) {
            bytes32 hash = keccak256(bytes(reason));
            if (hash == keccak256("too small")) {
                revert TooSmall(0);
            } else if (
                hash == keccak256("too large")
                    || hash == keccak256("swap: transfer exceeds max")
            ) {
                revert LimitExceeded(0); // max size is not easily computable
            } else if (hash == keccak256("killed")) {
                // This condition should have already be detected by
                revert Unavailable("pool has been permanently killed");
            } else if (hash == keccak256("LMSR: size metric zero")) {
                revert Unavailable("pool currently has no LP assets");
            } else if (hash == keccak256("slippage control")) {
                // A buy quote that the pool could not fill at the requested
                // output (e.g. rounding at the feasibility boundary).
                revert LimitExceeded(0);
            } else {
                // re-raise
                revert(string(abi.encodePacked("unhandled: ", reason)));
            }
        }
        // Unreachable
    }

    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        returns (uint256[] memory limits)
    {
        // We arbitrarily limit the amounts like Uniswap V2 does, to make the
        // test cases work. There is no theoretical limit on the input amount.
        // forge-lint: disable-next-line(unsafe-typecast)
        address pool = address(bytes20(poolId));
        limits = new uint256[](2);

        // input token limit: Theoretically unlimited, but artificially limited
        // here to practical ranges. Instead of estimating actual
        // input limits based on a maximum target slippage, we merely return a
        // fixed fraction of the input token's current inventory as a practical
        // limit.
        limits[0] = IERC20(sellToken).balanceOf(pool);

        // output token limit: the pool's current balance
        limits[1] = IERC20(buyToken).balanceOf(pool);
    }

    function getCapabilities(bytes32, address, address)
        external
        pure
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](4);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.BuyOrder;
        capabilities[2] = Capability.PriceFunction;
        capabilities[3] = Capability.MarginalPrice;
        return capabilities;
    }

    function getTokens(bytes32 poolId)
        external
        view
        returns (address[] memory tokens)
    {
        IPartyPool pool = _poolFromId(poolId);
        return pool.allTokens();
    }

    /// @dev This returns all pools even if they have been killed() and put into
    /// withdrawal-only mode. Make sure to check pool.killed() before trying to
    /// swap with that pool.
    function getPoolIds(uint256 offset, uint256 limit)
        external
        view
        returns (bytes32[] memory ids)
    {
        IPartyPool[] memory pools = PLANNER.getAllPools(offset, limit);
        ids = new bytes32[](pools.length);
        for (uint256 i = 0; i < pools.length; i++) {
            ids[i] = bytes32(uint256(uint160(address(pools[i]))));
        }
    }

    //
    // Internal Helpers
    //

    uint256 private constant NONE = type(uint256).max;

    /// @dev Liquidity Party pools identify tokens by index rather than address,
    /// saving 5200 gas per swap.
    function _tokenIndexes(IPartyPool pool, address sellToken, address buyToken)
        internal
        view
        returns (uint256 indexIn, uint256 indexOut)
    {
        indexIn = NONE;
        indexOut = NONE;
        address[] memory tokens = pool.allTokens();
        uint256 numTokens = tokens.length;
        for (uint256 i = 0; i < numTokens; i++) {
            if (tokens[i] == sellToken) {
                indexIn = i;
            } else if (tokens[i] == buyToken) {
                indexOut = i;
            }
        }
        // This should never happen if the token metadata was correctly loaded
        // by substreams
        require(indexIn != NONE && indexOut != NONE, "tokens not in pool");
    }

    function _marginalPrice(IPartyPool pool, uint256 indexIn, uint256 indexOut)
        internal
        view
        returns (Fraction memory poolPrice)
    {
        // INFO.price() returns the Q128.128 *input-per-output* (BUY-convention)
        // marginal price. It is base-adjusted but NOT token-decimal-adjusted,
        // so it is already in the same raw-token-unit basis as the
        // finite-amount
        // branch of price() (which returns amountOut/amount). This adapter
        // reports *output-per-input* (SELL convention), so we only take the
        // reciprocal and apply the fee:
        //
        //   rawOutPerIn = (2^128 / price) * (1 - fee)
        //
        // i.e. numerator/denominator = (2^128 * netPpm) / (price * 1e6).
        // slither-disable-next-line calls-loop
        uint256 price128x128 = INFO.price(pool, indexIn, indexOut);
        // slither-disable-next-line calls-loop
        uint256[] memory poolFees = INFO.fees(pool);
        uint256 netPpm = 1_000_000 - (poolFees[indexIn] + poolFees[indexOut]);
        // forge-lint: disable-next-line(named-struct-fields)
        return Fraction((uint256(1) << 128) * netPpm, price128x128 * 1_000_000);
    }

    function _poolFromId(bytes32 poolId) internal pure returns (IPartyPool) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return IPartyPool(address(bytes20(poolId)));
    }
}

