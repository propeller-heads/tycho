pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {
    IAccessControl
} from "@openzeppelin/contracts/access/IAccessControl.sol";
import {MockPropAMM} from "./PropAMM.t.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {
    FallbackExecutor,
    FallbackExecutor__AddressZero,
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    TychoFallbackRouter,
    TychoFallbackRouter__AddressZero,
    TychoFallbackRouter__InvalidSwapLength,
    TychoFallbackRouter__InvalidCallback,
    TychoFallbackRouter__NotPoolManager,
    TychoFallbackRouter__UnknownVenue,
    TychoFallbackRouter__NotSelf
} from "../../src/fallback/TychoFallbackRouter.sol";

/// @notice Builds the venue entries `TychoFallbackRouter` decodes.
library FallbackSwaps {
    function uniswapV2(address pair, uint8 feeBps)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(uint8(0), pair, feeBps);
    }

    function uniswapV3(address pool) internal pure returns (bytes memory) {
        return abi.encodePacked(uint8(1), pool);
    }

    function uniswapV4(
        uint24 fee,
        int24 tickSpacing,
        address hook,
        bytes memory hookData
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(
            uint8(2), bytes3(fee), tickSpacing, hook, hookData
        );
    }

    function curve(address pool, uint8 poolType, uint8 i, uint8 j)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(uint8(3), pool, poolType, i, j);
    }

    function fluidV1(address dex, bool zero2one)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(uint8(4), dex, zero2one);
    }

    /// Executor swap data: `[tokenIn][tokenOut][primaryLen][primarySwap][fallback]`.
    function swapData(
        address tokenIn,
        address tokenOut,
        address pamm,
        bytes memory fallbackSwap
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(tokenIn, tokenOut, pamm, fallbackSwap);
    }
}

/// @notice Accepts `tokenIn` and reports success without paying anything.
contract SilentVenue {
    function swap(
        address, /* tokenIn */
        address, /* tokenOut */
        uint256, /* amountIn */
        uint256, /* minAmountOut */
        address, /* recipient */
        uint256 /* deadline */
    )
        external
        pure
        returns (uint256 amountOut)
    {
        return 0;
    }
}

/// @notice The claim the contract exists for: a reverting pAMM still delivers `tokenOut`, through
/// a venue an executor could never reach.
contract TychoFallbackRouterTest is Constants, TestUtils {
    using FallbackSwaps for bytes;

    TychoFallbackRouter router;
    MockPropAMM pamm;

    uint256 constant USDC_IN = 10_000e6;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 22689128);
        router = new TychoFallbackRouter(
            ADMIN, IPoolManager(POOL_MANAGER), FLUIDV1_LIQUIDITY
        );
        pamm = new MockPropAMM();
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(TychoFallbackRouter__AddressZero.selector);
        new TychoFallbackRouter(
            address(0), IPoolManager(POOL_MANAGER), FLUIDV1_LIQUIDITY
        );

        vm.expectRevert(TychoFallbackRouter__AddressZero.selector);
        new TychoFallbackRouter(
            ADMIN, IPoolManager(address(0)), FLUIDV1_LIQUIDITY
        );

        vm.expectRevert(TychoFallbackRouter__AddressZero.selector);
        new TychoFallbackRouter(ADMIN, IPoolManager(POOL_MANAGER), address(0));
    }

    /// A live pAMM fills and the fallback is never touched.
    function testPropAMMFillsPrimary() public {
        // 1 WETH for the whole 10 000 USDC, far off the Uniswap V3 price of roughly 4 WETH, so
        // the asserted amount can only have come from the pAMM.
        pamm.setPrice(USDC_ADDR, WETH_ADDR, 1e26);
        deal(WETH_ADDR, address(pamm), 100 ether);
        _fundRouter(USDC_ADDR, USDC_IN);

        uint256 amountOut = router.swap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            BOB,
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(amountOut, 1 ether);
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 1 ether);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), USDC_IN);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// The pAMM has no price, so `quote` reverts. Uniswap V3 pays inside its callback, reachable
    /// only because this contract still holds the USDC.
    function testFallsBackToUniswapV3() public {
        _fundRouter(USDC_ADDR, USDC_IN);

        uint256 amountOut = router.swap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            BOB,
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertGt(amountOut, 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), amountOut);
        // The failed primary's transfer reverted with it.
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// The fallback starts from the full `amountIn`, whatever the primarySwap consumed.
    function testFallsBackToUniswapV2() public {
        _fundRouter(USDC_ADDR, USDC_IN);

        uint256 amountOut = router.swap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            BOB,
            address(pamm),
            FallbackSwaps.uniswapV2(USDC_WETH_USV2, 30)
        );

        assertGt(amountOut, 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), amountOut);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// Curve pays the caller, so the swap forwards the output itself.
    function testFallsBackToCurve() public {
        uint256 amountIn = 1000e18;
        _fundRouter(DAI_ADDR, amountIn);

        uint256 amountOut = router.swap(
            DAI_ADDR,
            USDC_ADDR,
            amountIn,
            BOB,
            address(pamm),
            FallbackSwaps.curve(TRIPOOL, 1, 0, 1)
        );

        assertGt(amountOut, 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), amountOut);
        _assertRouterDrained(DAI_ADDR, USDC_ADDR);
        assertEq(IERC20(DAI_ADDR).allowance(address(router), TRIPOOL), 0);
    }

    /// V4 runs inside `unlockCallback`, where this contract syncs, transfers and settles.
    function testFallsBackToUniswapV4() public {
        uint256 amountIn = 100 ether;
        _fundRouter(USDE_ADDR, amountIn);

        uint256 amountOut = router.swap(
            USDE_ADDR,
            USDT_ADDR,
            amountIn,
            BOB,
            address(pamm),
            FallbackSwaps.uniswapV4(100, 1, address(0), bytes(""))
        );

        assertGt(amountOut, 0);
        assertEq(IERC20(USDT_ADDR).balanceOf(BOB), amountOut);
        _assertRouterDrained(USDE_ADDR, USDT_ADDR);
    }

    /// Zero output counts as a failure and takes back the `tokenIn` already sent.
    function testVenuePayingNothingFallsThrough() public {
        SilentVenue silent = new SilentVenue();
        _fundRouter(USDC_ADDR, USDC_IN);

        uint256 amountOut = router.swap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            BOB,
            address(silent),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertGt(amountOut, 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(silent)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// A failing fallback reverts the swap. There is no third attempt.
    function testFallbackFailureReverts() public {
        _fundRouter(USDC_ADDR, USDC_IN);

        vm.expectRevert();
        router.swap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            BOB,
            address(pamm),
            FallbackSwaps.uniswapV3(makeAddr("not a pool"))
        );
    }

    function testUnknownFallbackVenueReverts() public {
        _fundRouter(USDC_ADDR, USDC_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__UnknownVenue.selector, uint8(9)
            )
        );
        router.swap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            BOB,
            address(pamm),
            abi.encodePacked(uint8(9), USDC_WETH_USV3)
        );
    }

    function testEmptyFallbackReverts() public {
        _fundRouter(USDC_ADDR, USDC_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__InvalidSwapLength.selector, uint256(0)
            )
        );
        router.swap(
            USDC_ADDR, WETH_ADDR, USDC_IN, BOB, address(pamm), bytes("")
        );
    }

    /// `executePropAMM` is external only so `swap` can wrap it in try/catch.
    function testExecutePropAMMRejectsExternalCaller() public {
        vm.expectRevert(TychoFallbackRouter__NotSelf.selector);
        router.executePropAMM(USDC_ADDR, WETH_ADDR, USDC_IN, BOB, address(pamm));
    }

    /// No swap is running, so there is no venue that may be paid.
    function testUniswapV3CallbackRejectsStranger() public {
        vm.expectRevert(TychoFallbackRouter__InvalidCallback.selector);
        router.uniswapV3SwapCallback(1, -1, bytes(""));
    }

    function testDexCallbackRejectsStranger() public {
        vm.expectRevert(TychoFallbackRouter__InvalidCallback.selector);
        router.dexCallback(USDC_ADDR, USDC_IN);
    }

    function testUnlockCallbackRejectsStranger() public {
        vm.expectRevert(TychoFallbackRouter__NotPoolManager.selector);
        router.unlockCallback(bytes(""));
    }

    function testRescueRequiresAdmin() public {
        deal(USDC_ADDR, address(router), 1e6);

        vm.prank(BOB);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector,
                BOB,
                bytes32(0)
            )
        );
        router.rescue(USDC_ADDR, BOB, 1e6);

        vm.prank(ADMIN);
        router.rescue(USDC_ADDR, BOB, 1e6);
        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), 1e6);
    }

    function _fundRouter(address token, uint256 amount) internal {
        deal(token, address(router), amount);
    }

    /// Holds no funds once a leg is done.
    function _assertRouterDrained(address tokenIn, address tokenOut)
        internal
        view
    {
        assertEq(IERC20(tokenIn).balanceOf(address(router)), 0);
        assertEq(IERC20(tokenOut).balanceOf(address(router)), 0);
    }
}

/// @notice Fluid pulls `tokenIn` through `dexCallback`. Forked where the dex is live.
contract TychoFallbackRouterFluidTest is Constants, TestUtils {
    address constant FLUID_DEX = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
    address constant SUSDE_ADDR = 0x9D39A5DE30e57443BfF2A8307A4256c8797A3497;

    TychoFallbackRouter router;
    MockPropAMM pamm;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23748828);
        router = new TychoFallbackRouter(
            ADMIN, IPoolManager(POOL_MANAGER), FLUIDV1_LIQUIDITY
        );
        pamm = new MockPropAMM();
    }

    function testFallsBackToFluidV1() public {
        uint256 amountIn = 10e18;
        deal(SUSDE_ADDR, address(router), amountIn);

        uint256 amountOut = router.swap(
            SUSDE_ADDR,
            USDT_ADDR,
            amountIn,
            BOB,
            address(pamm),
            FallbackSwaps.fluidV1(FLUID_DEX, true)
        );

        assertGt(amountOut, 0);
        assertEq(IERC20(USDT_ADDR).balanceOf(BOB), amountOut);
        assertEq(IERC20(SUSDE_ADDR).balanceOf(address(router)), 0);
    }
}

/// @notice The same claim through the whole TychoRouter: the leg's input lands at the fallback
/// router, not at a pool, which is what makes the retry fundable.
contract FallbackExecutorTest is TychoRouterTestSetup {
    MockPropAMM pamm;

    function getForkBlock() public pure override returns (uint256) {
        return 22689128;
    }

    function setUp() public override {
        super.setUp();
        pamm = new MockPropAMM();
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = fallbackExecutor.getTransferData(_swapData());

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(fallbackRouter));
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            fallbackExecutor.fundsExpectedAddress(_swapData()),
            address(fallbackRouter)
        );
    }

    function testInvalidDataLength() public {
        vm.expectRevert(FallbackExecutor__InvalidDataLength.selector);
        fallbackExecutor.getTransferData(abi.encodePacked(USDC_ADDR, WETH_ADDR));
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(FallbackExecutor__AddressZero.selector);
        new FallbackExecutor(address(0));
    }

    /// The whole leg: a dead pAMM still settles, at the Uniswap V3 price.
    function testSingleSwap() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1 ether,
            1 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), _swapData())
        );
        vm.stopPrank();

        assertGt(amountOut, 1 ether);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// The TychoRouter's `minAmountOut` is the leg's only price check.
    function testSingleSwapMinAmountOutBinds() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectRevert();
        tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1000 ether,
            1000 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), _swapData())
        );
        vm.stopPrank();
    }

    /// The fallback leg funds the next hop's pool directly.
    function testSequentialSwap() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(address(fallbackExecutor), _swapData());
        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            USDC_ADDR,
            DAI_ADDR,
            1000e18,
            1000e18,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertGt(amountOut, 1000e18);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// A pAMM with no price, then a Uniswap V3 retry.
    function _swapData() internal view returns (bytes memory) {
        return FallbackSwaps.swapData(
            USDC_ADDR,
            WETH_ADDR,
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }
}
