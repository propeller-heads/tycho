// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Vm} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackSwaps} from "./Fallback.t.sol";
import {
    FallbackExecutor__AddressZero,
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    MetricFallbackExecutor
} from "../../src/executors/MetricFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    IMetricOmmSwapQuoter,
    MetricFallbackRouter,
    MetricFallbackRouter__AddressZero,
    MetricFallbackRouter__InvalidDataLength
} from "../../src/fallback/MetricFallbackRouter.sol";

interface IMetricOmmSwapCallback {
    function metricOmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

/// @notice Quotes `quoteOut` and fills `swapOut`, so a test can make the two disagree. Reverts
/// unless the callback paid exactly `amountIn`.
contract MockMetricOmmPool {
    IERC20 public immutable tokenIn;
    IERC20 public immutable tokenOut;
    uint256 public quoteOut;
    uint256 public swapOut;
    bool public broken;

    constructor(address tokenIn_, address tokenOut_) {
        tokenIn = IERC20(tokenIn_);
        tokenOut = IERC20(tokenOut_);
    }

    function set(uint256 quoteOut_, uint256 swapOut_, bool broken_) external {
        quoteOut = quoteOut_;
        swapOut = swapOut_;
        broken = broken_;
    }

    function swap(
        address recipient,
        bool, /* zeroForOne */
        int128 amountSpecified,
        uint128, /* priceLimitX64 */
        bytes calldata, /* callbackData */
        bytes calldata /* extensionData */
    ) external returns (int128, int128) {
        require(!broken, "MockMetricOmmPool: broken");
        uint256 amountIn = uint256(uint128(amountSpecified));
        uint256 balanceBefore = tokenIn.balanceOf(address(this));
        IMetricOmmSwapCallback(msg.sender)
            .metricOmmSwapCallback(int256(amountIn), -int256(swapOut), "");
        require(
            tokenIn.balanceOf(address(this)) - balanceBefore == amountIn,
            "MockMetricOmmPool: unpaid"
        );
        tokenOut.transfer(recipient, swapOut);
        return (0, 0);
    }
}

/// @notice Reverts for a pool quoting zero, as the real quoter does for a pool that cannot fill.
contract MockMetricOmmSwapQuoter is IMetricOmmSwapQuoter {
    function quoteLiveExactInSingle(
        address pool,
        bool, /* zeroForOne */
        uint128 amountIn,
        uint128 /* priceLimitX64 */
    ) external view returns (uint256, uint256) {
        uint256 quoteOut = MockMetricOmmPool(pool).quoteOut();
        require(quoteOut > 0, "MockMetricOmmSwapQuoter: no quote");
        return (amountIn, quoteOut);
    }
}

contract MetricFallbackRouterTest is Constants {
    uint256 constant FORK_BLOCK = 22_689_128;
    uint256 constant USDC_IN = 10_000e6;
    /// The Uniswap V3 fallback's fill for `USDC_IN` at `FORK_BLOCK`.
    uint256 constant V3_WETH_OUT = 3_611_998_638_539_827_447;

    MetricFallbackRouter router;
    MetricFallbackExecutor executor;
    MockMetricOmmPool pool;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        router = new MetricFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            new MockMetricOmmSwapQuoter()
        );
        executor = new MetricFallbackExecutor(address(router));
        pool = new MockMetricOmmPool(USDC_ADDR, WETH_ADDR);
        deal(WETH_ADDR, address(pool), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);
    }

    function testMetricFillsWhenItQuotesHigher() public {
        pool.set(4 ether, 4 ether, false);

        vm.recordLogs();
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 4 ether);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), USDC_IN);
        _assertNoFallbackSwap(vm.getRecordedLogs());
        _assertRouterDrained();
    }

    function testFallsBackWhenMetricQuotesLower() public {
        pool.set(3 ether, 3 ether, false);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(pool)), 100 ether);
        _assertRouterDrained();
    }

    /// The pool is not paid, since its swap is never attempted.
    function testFallsBackWhenQuoterReverts() public {
        pool.set(0, 4 ether, false);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), 0);
        _assertRouterDrained();
    }

    function testFallsBackWhenMetricRevertsAfterWinningQuote() public {
        pool.set(4 ether, 4 ether, true);

        _expectFallbackSwap(TychoFallbackRouter.FallbackReason.PrimaryFailed);
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), 0);
        _assertRouterDrained();
    }

    function testFallsBackWhenMetricPaysNothing() public {
        pool.set(4 ether, 0, false);

        _expectFallbackSwap(TychoFallbackRouter.FallbackReason.PrimaryFailed);
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        _assertRouterDrained();
    }

    function testRejectsMalformedMetricData() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                MetricFallbackRouter__InvalidDataLength.selector, 0
            )
        );
        _swap(bytes(""));

        vm.expectRevert(
            abi.encodeWithSelector(
                MetricFallbackRouter__InvalidDataLength.selector, 2
            )
        );
        _swap(bytes(hex"0100"));
    }

    function testConstructorRejectsZeroQuoter() public {
        vm.expectRevert(MetricFallbackRouter__AddressZero.selector);
        new MetricFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            IMetricOmmSwapQuoter(address(0))
        );
    }

    function testExecutorSwap() public {
        pool.set(4 ether, 4 ether, false);

        executor.swap(USDC_IN, _executorData(), BOB);

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 4 ether);
        _assertRouterDrained();
    }

    function testExecutorGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(_executorData());

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(router));
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertFalse(outputToRouter);
        assertEq(
            executor.fundsExpectedAddress(_executorData()), address(router)
        );
    }

    /// 61 bytes carry the pool and direction but no fallback.
    function testExecutorRejectsDataWithoutFallback() public {
        bytes memory data =
            abi.encodePacked(USDC_ADDR, WETH_ADDR, address(pool), uint8(1));
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, 61
            )
        );
        executor.getTransferData(data);
    }

    function testExecutorRejectsZeroRouter() public {
        vm.expectRevert(FallbackExecutor__AddressZero.selector);
        new MetricFallbackExecutor(address(0));
    }

    function _swap(bytes memory metricData) internal {
        router.swap(
            _swapStruct(),
            address(pool),
            metricData,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _swapStruct()
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        return FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB);
    }

    function _executorData() internal view returns (bytes memory) {
        return abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            address(pool),
            uint8(1),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _expectFallbackSwap(TychoFallbackRouter.FallbackReason reason)
        internal
    {
        vm.expectEmit(address(router));
        emit TychoFallbackRouter.FallbackSwap(
            address(pool),
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            reason
        );
    }

    function _assertNoFallbackSwap(Vm.Log[] memory logs) internal view {
        for (uint256 i = 0; i < logs.length; i++) {
            assertFalse(
                logs[i].emitter == address(router)
                    && logs[i].topics[0]
                        == TychoFallbackRouter.FallbackSwap.selector
            );
        }
    }

    function _assertRouterDrained() internal view {
        assertEq(IERC20(USDC_ADDR).balanceOf(address(router)), 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(router)), 0);
    }
}

contract MetricFallbackRouterBaseTest is Constants {
    /// The block `TychoRouterForMetricTest` forks at, where the pool's oracle is fresh.
    uint256 constant FORK_BLOCK = 48_957_697;
    address constant METRIC_WETH_USDC_POOL =
        0x600668566fc5E9d471A1A235937221e39aC0ed04;
    address constant BASE_USDC_WETH_USV3 =
        0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant BASE_POOL_MANAGER =
        0x498581fF718922c3f8e6A244956aF099B2652b2b;
    address constant BASE_STATIC_QUOTER =
        0x28aF629a9F3ECE3c8D9F0b7cCf6349708CeC8cFb;
    /// The quoter of the factory that created `METRIC_WETH_USDC_POOL`.
    IMetricOmmSwapQuoter constant METRIC_QUOTER =
        IMetricOmmSwapQuoter(0xaB6C48D981B943F62A23bb4EB2db125182E6753c);
    uint256 constant WETH_IN = 1 ether;
    /// WETH is the pool's token0.
    bytes constant ZERO_FOR_ONE = hex"01";

    MetricFallbackRouter router;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        router = new MetricFallbackRouter(
            IPoolManager(BASE_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(BASE_STATIC_QUOTER),
            METRIC_QUOTER
        );
        deal(BASE_WETH, address(router), WETH_IN);
    }

    function testPaysTheHigherQuote() public {
        TychoFallbackRouter.Swap memory swap_ = _swapStruct();
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);

        vm.prank(address(router));
        uint256 v3Out = router.quoteFallback(swap_, v3);
        uint256 metricOut = _metricQuote();
        assertGt(metricOut, 0);
        assertGt(v3Out, 0);

        router.swap(swap_, METRIC_WETH_USDC_POOL, ZERO_FOR_ONE, v3);

        uint256 expected = metricOut >= v3Out ? metricOut : v3Out;
        assertEq(IERC20(BASE_USDC).balanceOf(BOB), expected);
        assertEq(IERC20(BASE_WETH).balanceOf(address(router)), 0);
    }

    /// The quote is the fill.
    function testMetricFillsTheQuotedAmount() public {
        uint256 metricOut = _metricQuote();
        uint256 poolWethBefore =
            IERC20(BASE_WETH).balanceOf(METRIC_WETH_USDC_POOL);

        router.swap(
            _swapStruct(),
            METRIC_WETH_USDC_POOL,
            ZERO_FOR_ONE,
            FallbackSwaps.uniswapV3(address(0xdead))
        );

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), metricOut);
        assertEq(
            IERC20(BASE_WETH).balanceOf(METRIC_WETH_USDC_POOL) - poolWethBefore,
            WETH_IN
        );
        assertEq(IERC20(BASE_WETH).balanceOf(address(router)), 0);
    }

    /// A day later the pool's oracle is stale.
    function testStaleMetricFallsBack() public {
        vm.warp(block.timestamp + 1 days);

        vm.expectEmit(address(router));
        emit TychoFallbackRouter.FallbackSwap(
            METRIC_WETH_USDC_POOL,
            BASE_WETH,
            BASE_USDC,
            WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(
            _swapStruct(),
            METRIC_WETH_USDC_POOL,
            ZERO_FOR_ONE,
            FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3)
        );

        assertGt(IERC20(BASE_USDC).balanceOf(BOB), 0);
        assertEq(IERC20(BASE_WETH).balanceOf(address(router)), 0);
    }

    function _metricQuote() internal returns (uint256 amountOut) {
        (, amountOut) = METRIC_QUOTER.quoteLiveExactInSingle(
            METRIC_WETH_USDC_POOL, true, uint128(WETH_IN), 0
        );
    }

    function _swapStruct()
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        return FallbackSwaps.swap(BASE_WETH, BASE_USDC, WETH_IN, BOB);
    }
}
