// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    MetricFallbackExecutor
} from "../../src/executors/MetricFallbackExecutor.sol";
import {
    TychoFallbackRouter,
    TychoFallbackRouter__NotSelf
} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    MetricFallbackRouter,
    MetricFallbackRouter__InvalidDataLength
} from "../../src/fallback/MetricFallbackRouter.sol";

/// @notice Metric's v1 periphery lens; see
/// https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/smart-contracts-reference/get-quote
interface IMetricOmmSwapQuoterV1 {
    function quoteLiveExactInSingle(
        address pool,
        address sender,
        bool zeroForOne,
        uint128 amountIn,
        uint128 priceLimitX64
    ) external returns (uint256 amountIn_, uint256 amountOut);
}

interface IMetricOmmSwapCallback {
    function metricOmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

/// @notice Fills `swapOut`. Reverts unless the callback paid exactly `amountIn`.
contract MockMetricOmmPool {
    IERC20 public immutable tokenIn;
    IERC20 public immutable tokenOut;
    uint256 public swapOut;
    bool public broken;

    constructor(address tokenIn_, address tokenOut_) {
        tokenIn = IERC20(tokenIn_);
        tokenOut = IERC20(tokenOut_);
    }

    function set(uint256 swapOut_, bool broken_) external {
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

contract MetricFallbackRouterTest is FallbackRouterAssertions {
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
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER)
        );
        executor = new MetricFallbackExecutor(address(router));
        pool = new MockMetricOmmPool(USDC_ADDR, WETH_ADDR);
        deal(WETH_ADDR, address(pool), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);
    }

    /// The pool is paid once: the simulated swap rolls back.
    function testMetricFillsWhenItQuotesHigher() public {
        pool.set(4 ether, false);

        vm.recordLogs();
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 4 ether);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), USDC_IN);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    function testFallsBackWhenMetricQuotesLower() public {
        pool.set(3 ether, false);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(pool)), 100 ether);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// A pool that reverts in the simulated swap quotes zero.
    function testFallsBackWhenMetricReverts() public {
        pool.set(4 ether, true);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), 0);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    function testFallsBackWhenMetricPaysNothing() public {
        pool.set(0, false);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(bytes(hex"01"));

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
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

    function testSimulatePrimaryRejectsExternalCaller() public {
        vm.expectRevert(TychoFallbackRouter__NotSelf.selector);
        router.simulatePrimary(_swapStruct(), address(pool), hex"01");
    }

    function testExecutorSwap() public {
        pool.set(4 ether, false);

        executor.swap(USDC_IN, _executorData(), BOB);

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 4 ether);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
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
        _expectFallbackSwap(
            address(router),
            address(pool),
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            reason
        );
    }
}

/// @notice Runs the router against a live Metric v1 pool, with the chain's Uniswap V3 pool as
/// fallback. Swaps 1 WETH for USDC; WETH is the pool's token0 on both chains.
abstract contract MetricFallbackRouterForkTest is FallbackRouterAssertions {
    /// Metric's v1 quoter. It returns a quote only to an `eth_call` from `address(0)`.
    IMetricOmmSwapQuoterV1 constant METRIC_QUOTER =
        IMetricOmmSwapQuoterV1(0x803Dd787ef9734c34696877ca6F20194fBcBFbF8);
    uint256 constant WETH_IN = 1 ether;
    bytes constant ZERO_FOR_ONE = hex"01";

    MetricFallbackRouter router;
    MetricFallbackExecutor executor;

    /// @dev Forks a block where the pool's oracle is fresh and deploys `router`.
    function _forkAndDeploy() internal virtual;

    function _metricPool() internal view virtual returns (address);

    function _uniswapV3Pool() internal view virtual returns (address);

    function _weth() internal view virtual returns (address);

    function _usdc() internal view virtual returns (address);

    function setUp() public {
        _forkAndDeploy();
        executor = new MetricFallbackExecutor(address(router));
        deal(_weth(), address(router), WETH_IN);
    }

    /// Metric quotes above the Uniswap V3 pool at the fork block, so Metric fills.
    function testPaysTheHigherQuote() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(_uniswapV3Pool());
        uint256 metricOut = _metricQuote();
        assertGt(metricOut, _quoteFallback(v3));

        vm.recordLogs();
        router.swap(_swapStruct(), _metricPool(), ZERO_FOR_ONE, v3);

        assertEq(IERC20(_usdc()).balanceOf(BOB), metricOut);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), _weth(), _usdc());
    }

    /// The quote is the fill.
    function testMetricFillsTheQuotedAmount() public {
        uint256 metricOut = _metricQuote();
        uint256 poolWethBefore = IERC20(_weth()).balanceOf(_metricPool());

        router.swap(
            _swapStruct(),
            _metricPool(),
            ZERO_FOR_ONE,
            FallbackSwaps.uniswapV3(address(0xdead))
        );

        assertEq(IERC20(_usdc()).balanceOf(BOB), metricOut);
        assertEq(
            IERC20(_weth()).balanceOf(_metricPool()) - poolWethBefore, WETH_IN
        );
        assertEq(IERC20(_weth()).balanceOf(address(router)), 0);
    }

    /// A day later the pool's oracle is stale.
    function testStaleMetricFallsBack() public {
        vm.warp(block.timestamp + 1 days);
        bytes memory v3 = FallbackSwaps.uniswapV3(_uniswapV3Pool());
        uint256 v3Out = _quoteFallback(v3);

        _expectFallbackSwap(
            address(router),
            _metricPool(),
            _weth(),
            _usdc(),
            WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(_swapStruct(), _metricPool(), ZERO_FOR_ONE, v3);

        assertEq(IERC20(_usdc()).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), _weth(), _usdc());
    }

    function _quoteFallback(bytes memory fallbackSwap)
        internal
        returns (uint256)
    {
        vm.prank(address(router));
        return router.quoteFallback(_swapStruct(), fallbackSwap);
    }

    /// Metric's own quote, read as an off-chain `eth_call` reads it.
    function _metricQuote() internal returns (uint256 amountOut) {
        vm.prank(address(0), address(0));
        (, amountOut) = METRIC_QUOTER.quoteLiveExactInSingle(
            _metricPool(), address(router), true, uint128(WETH_IN), 0
        );
    }

    function _swapStruct()
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        return FallbackSwaps.swap(_weth(), _usdc(), WETH_IN, BOB);
    }
}

contract MetricFallbackRouterEthereumTest is MetricFallbackRouterForkTest {
    /// The feed was stale in the blocks around this one.
    uint256 constant FORK_BLOCK = 26_105_909;
    address constant METRIC_WETH_USDC_POOL =
        0xF85AfbADeCC7F23Dd173ed706f07d9C7e2473e24;

    function _forkAndDeploy() internal override {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        router = new MetricFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER)
        );
    }

    function _metricPool() internal pure override returns (address) {
        return METRIC_WETH_USDC_POOL;
    }

    function _uniswapV3Pool() internal view override returns (address) {
        return USDC_WETH_USV3;
    }

    function _weth() internal view override returns (address) {
        return WETH_ADDR;
    }

    function _usdc() internal view override returns (address) {
        return USDC_ADDR;
    }
}

contract MetricFallbackRouterBaseTest is
    MetricFallbackRouterForkTest,
    TestUtils
{
    uint256 constant FORK_BLOCK = 52_085_032;
    address constant METRIC_WETH_USDC_POOL =
        0x258bE4EA05f674e0B26AA71dfC08E0c87c499Fb0;
    address constant BASE_USDC_WETH_USV3 =
        0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant BASE_POOL_MANAGER =
        0x498581fF718922c3f8e6A244956aF099B2652b2b;
    address constant BASE_STATIC_QUOTER =
        0x28aF629a9F3ECE3c8D9F0b7cCf6349708CeC8cFb;

    function _forkAndDeploy() internal override {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        router = new MetricFallbackRouter(
            IPoolManager(BASE_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(BASE_STATIC_QUOTER)
        );
    }

    /// The swap data comes from the Rust encoder's
    /// `test_encode_metric_fallback_for_solidity`.
    function testExecutorSwapsRustEncodedData() public {
        uint256 metricOut = _metricQuote();

        executor.swap(
            WETH_IN,
            loadCallDataFromFile("test_encode_metric_fallback_for_solidity"),
            BOB
        );

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), metricOut);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    function _metricPool() internal pure override returns (address) {
        return METRIC_WETH_USDC_POOL;
    }

    function _uniswapV3Pool() internal pure override returns (address) {
        return BASE_USDC_WETH_USV3;
    }

    function _weth() internal pure override returns (address) {
        return BASE_WETH;
    }

    function _usdc() internal pure override returns (address) {
        return BASE_USDC;
    }
}

interface IPancakePair {
    function getReserves()
        external
        view
        returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
}

/// @notice Runs the router as `deploy-fallback-router.js` deploys it on BSC: a Uniswap V4
/// PoolManager, no Fluid liquidity layer and no Uniswap V3 static quoter. The Metric pool is a
/// mock; the fallbacks are the live PancakeSwap WBNB/USDT pools. Swaps 1 WBNB for USDT.
contract MetricFallbackRouterBscTest is FallbackRouterAssertions {
    uint256 constant FORK_BLOCK = 127_900_000;
    uint256 constant WBNB_IN = 1 ether;
    /// WBNB is token1 of the WBNB/USDT pools.
    bytes constant ONE_FOR_ZERO = hex"00";
    address constant BSC_USDT = 0x55d398326f99059fF775485246999027B3197955;
    address constant BSC_POOL_MANAGER =
        0x28e2Ea090877bF75740558f6BFB36A5ffeE9e9dF;
    address constant PANCAKE_V2_WBNB_USDT =
        0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE;
    /// The 0.01% pool.
    address constant PANCAKE_V3_WBNB_USDT =
        0x172fcD41E0913e95784454622d1c3724f546f849;
    uint8 constant PANCAKE_V2_FEE_BPS = 25;

    MetricFallbackRouter router;
    MockMetricOmmPool pool;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("bsc"), FORK_BLOCK);
        router = new MetricFallbackRouter(
            IPoolManager(BSC_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(address(0))
        );
        pool = new MockMetricOmmPool(BSC_WBNB, BSC_USDT);
        deal(BSC_USDT, address(pool), 10_000 ether);
        deal(BSC_WBNB, address(router), WBNB_IN);
    }

    /// The pair's own K check accepts the output only at Pancake's 25 bps fee or above.
    function testFallsBackToPancakeswapV2() public {
        pool.set(0, true);
        uint256 expectedOut = _pancakeV2AmountOut();

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackProtocol.UniswapV2,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(
            _swapStruct(),
            address(pool),
            ONE_FOR_ZERO,
            FallbackSwaps.uniswapV2(PANCAKE_V2_WBNB_USDT, PANCAKE_V2_FEE_BPS)
        );

        assertEq(IERC20(BSC_USDT).balanceOf(BOB), expectedOut);
        _assertRouterDrained(address(router), BSC_WBNB, BSC_USDT);
    }

    /// Without a static quoter the pool is quoted by simulation, and pays its input through
    /// `pancakeV3SwapCallback`.
    function testFallsBackToPancakeswapV3() public {
        pool.set(0, true);
        bytes memory v3 = FallbackSwaps.uniswapV3(PANCAKE_V3_WBNB_USDT);
        uint256 v3Out = _quoteFallback(v3);
        assertGt(v3Out, 0);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(_swapStruct(), address(pool), ONE_FOR_ZERO, v3);

        assertEq(IERC20(BSC_USDT).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BSC_WBNB, BSC_USDT);
    }

    function testMetricFillsWhenItBeatsPancakeswapV3() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(PANCAKE_V3_WBNB_USDT);
        uint256 metricOut = _quoteFallback(v3) + 1;
        pool.set(metricOut, false);

        vm.recordLogs();
        router.swap(_swapStruct(), address(pool), ONE_FOR_ZERO, v3);

        assertEq(IERC20(BSC_USDT).balanceOf(BOB), metricOut);
        assertEq(IERC20(BSC_WBNB).balanceOf(address(pool)), WBNB_IN);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), BSC_WBNB, BSC_USDT);
    }

    /// PancakeSwap V2's `getAmountOut` at a 25 bps fee, with WBNB as token1.
    function _pancakeV2AmountOut() internal view returns (uint256) {
        (uint112 usdtReserve, uint112 wbnbReserve,) =
            IPancakePair(PANCAKE_V2_WBNB_USDT).getReserves();
        uint256 amountInWithFee = WBNB_IN * 9975;
        return amountInWithFee * usdtReserve
            / (uint256(wbnbReserve) * 10_000 + amountInWithFee);
    }

    function _quoteFallback(bytes memory fallbackSwap)
        internal
        returns (uint256)
    {
        vm.prank(address(router));
        return router.quoteFallback(_swapStruct(), fallbackSwap);
    }

    function _swapStruct()
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        return FallbackSwaps.swap(BSC_WBNB, BSC_USDT, WBNB_IN, BOB);
    }

    function _expectFallbackSwap(
        TychoFallbackRouter.FallbackProtocol protocol,
        TychoFallbackRouter.FallbackReason reason
    ) internal {
        _expectFallbackSwap(
            address(router),
            address(pool),
            BSC_WBNB,
            BSC_USDT,
            WBNB_IN,
            protocol,
            reason
        );
    }
}

/// @notice Runs the router as `deploy-fallback-router.js` deploys it on Polygon: a Uniswap V4
/// PoolManager, no Fluid liquidity layer and Eden's Uniswap V3 static quoter. The Metric pool is
/// a mock; the fallbacks are the live QuickSwap V2 and Uniswap V3 WETH/USDC pools. Swaps 0.1 WETH
/// for USDC.
contract MetricFallbackRouterPolygonTest is FallbackRouterAssertions {
    uint256 constant FORK_BLOCK = 95_800_000;
    uint256 constant WETH_IN = 0.1 ether;
    /// WETH is token1 of the WETH/USDC pools.
    bytes constant ONE_FOR_ZERO = hex"00";
    address constant POLYGON_POOL_MANAGER =
        0x67366782805870060151383F4BbFF9daB53e5cD6;
    address constant POLYGON_UNISWAP_V3_STATIC_QUOTER =
        0x7637Aaeb5BD58269B782726680d83f72C651aE74;
    address constant QUICKSWAP_V2_WETH_USDC =
        0x7bAF833f82BB1971f99A5a5d84bED1d5D0dEDD70;
    /// The 0.05% pool.
    address constant UNISWAP_V3_WETH_USDC =
        0xA4D8c89f0c20efbe54cBa9e7e7a7E509056228D9;
    uint8 constant QUICKSWAP_V2_FEE_BPS = 30;

    MetricFallbackRouter router;
    MockMetricOmmPool pool;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("polygon"), FORK_BLOCK);
        router = new MetricFallbackRouter(
            IPoolManager(POLYGON_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(POLYGON_UNISWAP_V3_STATIC_QUOTER)
        );
        pool = new MockMetricOmmPool(POLYGON_WETH, POLYGON_USDC);
        deal(POLYGON_USDC, address(pool), 1_000_000e6);
        deal(POLYGON_WETH, address(router), WETH_IN);
    }

    function testFallsBackToQuickswapV2() public {
        pool.set(0, true);
        uint256 expectedOut = _quickswapV2AmountOut();

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackProtocol.UniswapV2,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(
            _swapStruct(),
            address(pool),
            ONE_FOR_ZERO,
            FallbackSwaps.uniswapV2(
                QUICKSWAP_V2_WETH_USDC, QUICKSWAP_V2_FEE_BPS
            )
        );

        assertEq(IERC20(POLYGON_USDC).balanceOf(BOB), expectedOut);
        _assertRouterDrained(address(router), POLYGON_WETH, POLYGON_USDC);
    }

    /// The static quoter's quote is what the pool pays.
    function testFallsBackToUniswapV3() public {
        pool.set(0, true);
        bytes memory v3 = FallbackSwaps.uniswapV3(UNISWAP_V3_WETH_USDC);
        uint256 v3Out = _quoteFallback(v3);
        assertGt(v3Out, 0);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(_swapStruct(), address(pool), ONE_FOR_ZERO, v3);

        assertEq(IERC20(POLYGON_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), POLYGON_WETH, POLYGON_USDC);
    }

    function testMetricFillsWhenItBeatsUniswapV3() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(UNISWAP_V3_WETH_USDC);
        uint256 metricOut = _quoteFallback(v3) + 1;
        pool.set(metricOut, false);

        vm.recordLogs();
        router.swap(_swapStruct(), address(pool), ONE_FOR_ZERO, v3);

        assertEq(IERC20(POLYGON_USDC).balanceOf(BOB), metricOut);
        assertEq(IERC20(POLYGON_WETH).balanceOf(address(pool)), WETH_IN);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), POLYGON_WETH, POLYGON_USDC);
    }

    /// QuickSwap V2's `getAmountOut` at its 30 bps fee, with WETH as token1.
    function _quickswapV2AmountOut() internal view returns (uint256) {
        (uint112 usdcReserve, uint112 wethReserve,) =
            IUniswapV2Pair(QUICKSWAP_V2_WETH_USDC).getReserves();
        uint256 amountInWithFee = WETH_IN * 9970;
        return amountInWithFee * usdcReserve
            / (uint256(wethReserve) * 10_000 + amountInWithFee);
    }

    function _quoteFallback(bytes memory fallbackSwap)
        internal
        returns (uint256)
    {
        vm.prank(address(router));
        return router.quoteFallback(_swapStruct(), fallbackSwap);
    }

    function _swapStruct()
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        return FallbackSwaps.swap(POLYGON_WETH, POLYGON_USDC, WETH_IN, BOB);
    }

    function _expectFallbackSwap(
        TychoFallbackRouter.FallbackProtocol protocol,
        TychoFallbackRouter.FallbackReason reason
    ) internal {
        _expectFallbackSwap(
            address(router),
            address(pool),
            POLYGON_WETH,
            POLYGON_USDC,
            WETH_IN,
            protocol,
            reason
        );
    }
}
