// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {
    IERC20
} from "../lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import {
    IERC20Metadata
} from "../lib/openzeppelin-contracts/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {FractionMath} from "../src/libraries/FractionMath.sol";
import {IPartyInfo} from "../src/liquidityparty/IPartyInfo.sol";
import {IPartyPlanner} from "../src/liquidityparty/IPartyPlanner.sol";
import {IPartyPool} from "../src/liquidityparty/IPartyPool.sol";
import {
    LiquidityPartySwapAdapter
} from "../src/liquidityparty/LiquidityPartySwapAdapter.sol";
import {AdapterTest} from "./AdapterTest.sol";

contract LiquidityPartyFunctionTest is AdapterTest {
    using FractionMath for Fraction;

    IPartyPlanner internal constant PLANNER =
        IPartyPlanner(0x5E9DB9fa66aeA7f254d4A6783b1a6180C4B8AAe3);
    IPartyInfo internal constant INFO =
        IPartyInfo(0xefF3Ed388D3887e7C9F375B7f1ad8A0B77C05643);
    address internal constant EXTRA_IMPL1 =
        0xAcb7089D62b67545299842bc7133582f8bE9eB86;
    address internal constant EXTRA_IMPL2 =
        0x669a9B0cBdEFb31380b912C1dF3b77fA7C18D821;
    IPartyPool internal constant POOL =
        IPartyPool(0x1270Da05Cf1d047763CEEfDe25a4a5438b26fdA6);
    bytes32 internal constant POOL_ID = bytes32(bytes20(address(POOL)));
    uint256 internal constant FORK_BLOCK = 25301915;

    LiquidityPartySwapAdapter internal adapter;
    uint256 internal constant TEST_ITERATIONS = 10;

    address[] internal tokens;
    address internal constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address internal constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address internal constant AAVE = 0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9;

    address private constant INPUT_TOKEN = WETH;
    uint8 private constant INPUT_INDEX = 1;
    address private constant OUTPUT_TOKEN = AAVE;
    uint8 private constant OUTPUT_INDEX = 2;

    function setUp() public {
        tokens = new address[](3);
        tokens[0] = USDC;
        tokens[1] = WETH;
        tokens[2] = AAVE;

        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);

        adapter = new LiquidityPartySwapAdapter(PLANNER, INFO);

        vm.label(address(PLANNER), "PartyPlanner");
        vm.label(address(INFO), "PartyInfo");
        vm.label(address(EXTRA_IMPL1), "PartyPoolExtraImpl1");
        vm.label(address(EXTRA_IMPL2), "PartyPoolExtraImpl2");
        vm.label(address(POOL), "PartyPool");
        vm.label(address(adapter), "LiquidityPartySwapAdapter");
        for (uint256 i = 0; i < tokens.length; i++) {
            vm.label(address(tokens[i]), IERC20Metadata(tokens[i]).symbol());
        }
    }

    function testPrice() public view {
        uint256[] memory amounts = new uint256[](3);
        uint256 balance = IERC20(INPUT_TOKEN).balanceOf(address(POOL));
        // cannot use 1: the fee will round up and take
        // everything, resulting in a zero-output reversion
        amounts[0] = 2;
        amounts[1] = balance;
        amounts[2] = balance * 2;

        Fraction[] memory prices =
            adapter.price(POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN, amounts);

        for (uint256 i = 0; i < prices.length; i++) {
            assertGt(prices[i].numerator, 0);
            assertGt(prices[i].denominator, 0);
        }
    }

    function testPriceDecreasing() public view {
        uint256[] memory limits =
            adapter.getLimits(POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN);

        uint256[] memory amounts = new uint256[](TEST_ITERATIONS);

        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            // The first entry will be a zero amount which returns the current
            // marginal price.
            amounts[i] = limits[0] * i / (TEST_ITERATIONS - 1);
        }

        Fraction[] memory prices =
            adapter.price(POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN, amounts);

        for (uint256 i = 0; i < TEST_ITERATIONS - 1; i++) {
            assertEq(prices[i].compareFractions(prices[i + 1]), 1);
        }
    }

    function testSwapFuzz(uint256 amount) public {
        uint256[] memory limits =
            adapter.getLimits(POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN);
        // 1 will not work because we take fee-on-input
        // and round up, leaving nothing to trade
        vm.assume(amount > 1);
        vm.assume(amount <= limits[0]);

        deal(INPUT_TOKEN, address(this), amount);
        IERC20(INPUT_TOKEN).approve(address(adapter), amount);

        uint256 usdtBalance = IERC20(INPUT_TOKEN).balanceOf(address(this));
        uint256 wethBalance = IERC20(OUTPUT_TOKEN).balanceOf(address(this));

        Trade memory trade = adapter.swap(
            POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN, OrderSide.Sell, amount
        );

        if (trade.calculatedAmount > 0) {
            assertEq(
                amount,
                usdtBalance - IERC20(INPUT_TOKEN).balanceOf(address(this))
            );
            assertEq(
                trade.calculatedAmount,
                IERC20(OUTPUT_TOKEN).balanceOf(address(this)) - wethBalance
            );
        }
    }

    function testSwapSellIncreasing() public {
        uint256[] memory limits =
            adapter.getLimits(POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN);
        uint256[] memory amounts = new uint256[](TEST_ITERATIONS);
        Trade[] memory trades = new Trade[](TEST_ITERATIONS);

        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            amounts[i] = limits[0] * (i + 1) / (TEST_ITERATIONS - 1);

            uint256 beforeSwap = vm.snapshot();

            deal(INPUT_TOKEN, address(this), amounts[i]);
            IERC20(INPUT_TOKEN).approve(address(adapter), amounts[i]);
            trades[i] = adapter.swap(
                POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN, OrderSide.Sell, amounts[i]
            );

            vm.revertTo(beforeSwap);
        }

        for (uint256 i = 0; i < TEST_ITERATIONS - 1; i++) {
            assertLe(trades[i].calculatedAmount, trades[i + 1].calculatedAmount);
            // Marginal price (output-per-input) does not increase as the sell
            // size grows. INFO.price is input-per-output, which the adapter
            // inverts, so the fractions no longer share a constant denominator
            // basis — compare them directly instead of per-field.
            assertGe(
                int256(trades[i].price.compareFractions(trades[i + 1].price)), 0
            );
        }
    }

    function testGetLimits() public view {
        uint256[] memory limits =
            adapter.getLimits(POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN);

        assert(limits.length == 2);
        assert(limits[0] > 0);
        assert(limits[1] > 0);
    }

    function testGetTokens() public view {
        address[] memory adapterTokens = adapter.getTokens(POOL_ID);
        for (uint256 i = 0; i < tokens.length; i++) {
            assertEq(adapterTokens[i], tokens[i]);
        }
    }

    function testGetPoolIds() public view {
        uint256 offset = 0;
        uint256 limit = 10;
        bytes32[] memory poolIds = adapter.getPoolIds(offset, limit);

        assertLe(
            poolIds.length,
            limit,
            "Number of pool IDs should be less than or equal to limit"
        );
        if (poolIds.length > 0) {
            assertGt(uint256(poolIds[0]), 0, "Pool ID should be greater than 0");
        }
    }

    // Use WETH/AAVE pair — USDC's 6 decimals cause precision failures at tiny
    // pool sizes (~$1) with large relative trade sizes.
    function testLiquidityPartyPoolBehaviour() public {
        IERC20(WETH).approve(address(adapter), type(uint256).max);
        IERC20(AAVE).approve(address(adapter), type(uint256).max);
        testPricesForPair(adapter, POOL_ID, WETH, AAVE, true);
        testPricesForPair(adapter, POOL_ID, AAVE, WETH, true);
    }

    // Exact-output (buy) swap: specify the desired output amount and confirm
    // the adapter quotes the required input via swapAmountsForExactOutput and
    // the
    // pool delivers at least the requested output.
    function testSwapBuy() public {
        // A small, feasible amount of the output token relative to the pool's
        // inventory.
        uint256 buyAmount = IERC20(OUTPUT_TOKEN).balanceOf(address(POOL)) / 100;
        assertGt(buyAmount, 0);

        deal(INPUT_TOKEN, address(this), type(uint128).max);
        IERC20(INPUT_TOKEN).approve(address(adapter), type(uint256).max);

        uint256 inBefore = IERC20(INPUT_TOKEN).balanceOf(address(this));
        uint256 outBefore = IERC20(OUTPUT_TOKEN).balanceOf(address(this));

        Trade memory trade = adapter.swap(
            POOL_ID, INPUT_TOKEN, OUTPUT_TOKEN, OrderSide.Buy, buyAmount
        );

        uint256 spent = inBefore - IERC20(INPUT_TOKEN).balanceOf(address(this));
        uint256 received =
            IERC20(OUTPUT_TOKEN).balanceOf(address(this)) - outBefore;

        // For a buy, the calculated amount is the input spent.
        assertEq(trade.calculatedAmount, spent);
        // We received at least the requested output.
        assertGe(received, buyAmount);
    }
}
