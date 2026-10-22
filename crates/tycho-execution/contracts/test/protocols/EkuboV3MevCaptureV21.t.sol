pragma solidity ^0.8.26;

import "./EkuboV3.t.sol";
import {PoolId} from "@ekubo-v3/types/poolId.sol";
import {toPoolId} from "@ekubo-v3/types/poolKey.sol";

address constant MEV_CAPTURE_V21 = 0x51f831c23fa4051862881Ef8EfC0c3C37905Ce49;
uint256 constant MEV_CAPTURE_V21_FORK_BLOCK = 26250000;
address constant MEV_CAPTURE_V21_TOKEN0 =
    0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48; // USDC
address constant MEV_CAPTURE_V21_TOKEN1 =
    0xdAC17F958D2ee523a2206206994597C13D831ec7; // USDT

/// USDC/USDT with a 0.05% pool fee and tick spacing 1000.
function mevCaptureV21PoolKey() pure returns (PoolKey memory) {
    return PoolKey({
        token0: MEV_CAPTURE_V21_TOKEN0,
        token1: MEV_CAPTURE_V21_TOKEN1,
        config: createConcentratedPoolConfig({
            _fee: 0x0020c49ba5e353f7,
            _tickSpacing: 1000,
            _extension: MEV_CAPTURE_V21
        })
    });
}

/// Creates the pool at tick 0 and adds its positions. `helper` must hold
/// enough of both tokens.
function seedMevCaptureV21Pool(LiquidityHelper helper) {
    PoolKey memory poolKey = mevCaptureV21PoolKey();
    CORE.initializePool(poolKey, 0);

    int128 l = 40_000_000_000_000;
    helper.provide(poolKey, -50_000, 50_000, l);
    helper.provide(poolKey, -3000, -1000, l);
    helper.provide(poolKey, 1000, 3000, l);
    // Tick 4000 stays initialized with a zero net liquidity delta.
    helper.provide(poolKey, 2000, 4000, l / 2);
    helper.provide(poolKey, 4000, 6000, l / 2);
}

/// Swaps on a MEVCapture v2.1 pool through the executor against the extension
/// deployed on Ethereum. The pool is created on the fork, so its state is
/// known; the expected amounts are the tycho-simulation quotes of the same
/// state (`ekubo_v3::pool::mev_capture_v21::tests::test_fork_route_quotes`).
contract EkuboV3MevCaptureV21Test is Constants, TestUtils {
    using CoreLib for ICore;

    EkuboV3ExecutorStandalone immutable executor =
        new EkuboV3ExecutorStandalone(EKUBO_V3_SIGNED_EXCLUSIVE_SWAP);
    LiquidityHelper immutable liquidityHelper = new LiquidityHelper();

    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 USDT = IERC20(USDT_ADDR);

    constructor() {
        vm.makePersistent(address(executor));
        vm.makePersistent(address(liquidityHelper));
    }

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), MEV_CAPTURE_V21_FORK_BLOCK);
        // TODO: remove once Foundry stable includes the Fusaka hardfork mapping.
        address(vm)
            .call(abi.encodeWithSignature("setEvmVersion(string)", "osaka"));

        deal(USDC_ADDR, address(liquidityHelper), 10_000_000e6);
        deal(USDT_ADDR, address(liquidityHelper), 10_000_000e6);
        seedMevCaptureV21Pool(liquidityHelper);
    }

    function testSwapsMatchQuotes() public {
        // The state the simulation test quotes from.
        _assertState(
            39614081261743854815199363072,
            0,
            40_000_000_000_000,
            0x6ada581f6ada581f6ada581f00006ada57a70000000000000000000000000000
        );

        // Next block: the first swap updates the anchor.
        _nextBlock();
        // Away from the anchor, decreasing price (several Core calls).
        _swap(USDC_ADDR, USDT_ADDR, 50_000e6, 49_868_947_729, "away_dec");
        // Toward the anchor, increasing price, same timestamp.
        _swap(USDT_ADDR, USDC_ADDR, 10_000e6, 10_011_242_024, "toward_inc");
        _assertState(
            39599244835603686790750605543,
            -1499,
            80_000_000_000_000,
            0x6ada582b6ada582b6ada581f01006ada582b0000000000000000000000000000
        );

        _nextBlock();
        // Toward then away from the anchor, increasing price, crossing ticks
        // including one with a zero net liquidity delta.
        _swap(USDT_ADDR, USDC_ADDR, 150_000e6, 149_559_555_721, "away_inc");
        // Toward the anchor, decreasing price.
        _swap(USDC_ADDR, USDT_ADDR, 5_000e6, 5_012_253_996, "toward_dec");
        _assertState(
            39614081261750541708812399991,
            2897,
            100_000_000_000_000,
            0x6ada58376ada58376ada581f01006ada582b000000000000ffffffffff9b9dcf
        );
    }

    function _nextBlock() internal {
        vm.roll(block.number + 1);
        vm.warp(block.timestamp + 12);
    }

    function _swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 expectedOut,
        string memory label
    ) internal returns (uint256 amountOut) {
        deal(tokenIn, address(executor), amountIn);
        uint256 outBefore = IERC20(tokenOut).balanceOf(address(executor));

        bytes memory data = abi.encodePacked(
            tokenIn, tokenOut, PoolConfig.unwrap(mevCaptureV21PoolKey().config)
        );
        uint256 gasBefore = gasleft();
        executor.swap(amountIn, data, address(executor));
        uint256 gasUsed = gasBefore - gasleft();

        amountOut = IERC20(tokenOut).balanceOf(address(executor)) - outBefore;
        console.log(label, amountOut, "gas", gasUsed);
        assertEq(amountOut, expectedOut, label);
        assertEq(IERC20(tokenIn).balanceOf(address(executor)), 0);
    }

    /// Core's pool state (raw sqrt ratio) and the extension's state word.
    function _assertState(
        uint96 sqrtRatio,
        int32 tick,
        uint128 liquidity,
        bytes32 extensionState
    ) internal view {
        PoolId poolId = toPoolId(mevCaptureV21PoolKey());
        PoolState state = CORE.poolState(poolId);
        assertEq(SqrtRatio.unwrap(state.sqrtRatio()), sqrtRatio, "sqrtRatio");
        assertEq(state.tick(), tick, "tick");
        assertEq(state.liquidity(), liquidity, "liquidity");
        assertEq(
            vm.load(MEV_CAPTURE_V21, PoolId.unwrap(poolId)),
            extensionState,
            "extension state"
        );
    }
}

/// The route the Rust encoder builds for the first swap above, executed through
/// TychoRouter with the quote as the minimum amount out.
contract TychoRouterForEkuboV3MevCaptureV21Test is TychoRouterTestSetup {
    LiquidityHelper liquidityHelper;

    function getForkBlock() public pure override returns (uint256) {
        return MEV_CAPTURE_V21_FORK_BLOCK;
    }

    function setUp() public virtual override {
        super.setUp();

        // TODO: remove once Foundry stable includes the Fusaka hardfork mapping.
        address(vm)
            .call(abi.encodeWithSignature("setEvmVersion(string)", "osaka"));

        // Remove delegations
        vm.etch(ALICE, "");

        liquidityHelper = new LiquidityHelper();
        deal(USDC_ADDR, address(liquidityHelper), 10_000_000e6);
        deal(USDT_ADDR, address(liquidityHelper), 10_000_000e6);
        seedMevCaptureV21Pool(liquidityHelper);
    }

    function testSingleEkuboV3MevCaptureV21Integration() public {
        // The quote is for the first swap of the next block.
        vm.roll(block.number + 1);
        vm.warp(block.timestamp + 12);

        deal(USDC_ADDR, ALICE, 50_000e6);
        uint256 balanceBefore = IERC20(USDT_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_ekubo_v3_mev_capture_v21"
            )
        );
        vm.stopPrank();

        assertTrue(success, "call failed");
        assertEq(
            IERC20(USDT_ADDR).balanceOf(ALICE) - balanceBefore, 49_868_947_729
        );
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
    }
}
