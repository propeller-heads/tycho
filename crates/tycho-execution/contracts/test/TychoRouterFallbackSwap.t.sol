pragma solidity ^0.8.26;

import {TychoRouterV3, ClientFeeParams} from "@src/TychoRouterV3.sol";
import {Dispatcher__OnlySelf} from "@src/Dispatcher.sol";
import {LibSwap} from "../lib/LibSwap.sol";
import "./TychoRouterTestSetup.sol";

/// @dev Mock executor whose `swap` always reverts. Stands in for a primary
/// executor hitting a protocol revert (e.g. a stale PropAMM quote).
/// `getTransferData` decodes tokenIn/tokenOut from the first 40 bytes of
/// `data` so the Dispatcher's single-hop-cycle check passes.
contract AlwaysRevertingExecutor {
    function getTransferData(bytes calldata data)
        external
        pure
        returns (TransferManager.TransferType, address, address, address, bool)
    {
        address tokenIn = address(bytes20(data[0:20]));
        address tokenOut = address(bytes20(data[20:40]));
        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    function swap(uint256, bytes calldata, address) external payable {
        revert("mock executor reverted");
    }

    function fundsExpectedAddress(bytes calldata)
        external
        view
        returns (address)
    {
        return address(this);
    }
}

contract TychoRouterFallbackSwapTest is TychoRouterTestSetup {
    AlwaysRevertingExecutor revertingExecutor;

    function setUp() public override {
        super.setUp();
        revertingExecutor = new AlwaysRevertingExecutor();
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        address[] memory executors = new address[](1);
        executors[0] = address(revertingExecutor);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    /// @dev Encodes the payload that follows `LibSwap.FALLBACK_MARKER` in a
    /// hop's executor slot: `uint16 primaryLength || primary || fallback`.
    function encodeFallbackData(
        address primaryExecutor,
        bytes memory primaryData,
        address fallbackExecutor,
        bytes memory fallbackData
    ) internal pure returns (bytes memory) {
        bytes memory primary = abi.encodePacked(primaryExecutor, primaryData);
        return abi.encodePacked(
            uint16(primary.length), primary, fallbackExecutor, fallbackData
        );
    }

    function testSingleSwapFallbackPrimaryReverts() public {
        // Trade 1 WETH for DAI. The primary executor reverts, so the hop
        // falls back to the Uniswap V2 executor.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory swap = encodeSingleSwap(
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                address(revertingExecutor),
                abi.encodePacked(WETH_ADDR, DAI_ADDR),
                address(usv2Executor),
                encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
            )
        );

        uint256 expectedAmountOut = 2018817438608734439722;
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            expectedAmountOut * 9900 / 10000,
            ALICE,
            noClientFee(),
            swap
        );

        assertEq(amountOut, expectedAmountOut);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), expectedAmountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);
        vm.stopPrank();
    }

    function testSingleSwapFallbackPrimarySucceeds() public {
        // The primary executor succeeds, so the fallback executor (which
        // would revert if called) is never used.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory swap = encodeSingleSwap(
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                address(usv2Executor),
                encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR),
                address(revertingExecutor),
                abi.encodePacked(WETH_ADDR, DAI_ADDR)
            )
        );

        uint256 expectedAmountOut = 2018817438608734439722;
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            expectedAmountOut * 9900 / 10000,
            ALICE,
            noClientFee(),
            swap
        );

        assertEq(amountOut, expectedAmountOut);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), expectedAmountOut);
        vm.stopPrank();
    }

    function testSingleSwapFallbackBothRevert() public {
        // Both the primary and the fallback executor revert. The fallback's
        // revert bubbles up to the caller.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory swap = encodeSingleSwap(
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                address(revertingExecutor),
                abi.encodePacked(WETH_ADDR, DAI_ADDR),
                address(revertingExecutor),
                abi.encodePacked(WETH_ADDR, DAI_ADDR)
            )
        );

        vm.expectRevert(bytes("mock executor reverted"));
        tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1000 ether,
            1000 ether * 9900 / 10000,
            ALICE,
            noClientFee(),
            swap
        );
        vm.stopPrank();
    }

    function testSingleSwapFallbackPrimaryUnapproved() public {
        // The primary executor is not approved on the Dispatcher. Its
        // validation revert is caught like any other primary failure and
        // the fallback executes.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory swap = encodeSingleSwap(
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                makeAddr("unapprovedExecutor"),
                abi.encodePacked(WETH_ADDR, DAI_ADDR),
                address(usv2Executor),
                encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
            )
        );

        uint256 expectedAmountOut = 2018817438608734439722;
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            expectedAmountOut * 9900 / 10000,
            ALICE,
            noClientFee(),
            swap
        );

        assertEq(amountOut, expectedAmountOut);
        vm.stopPrank();
    }

    function testSequentialSwapFallbackSecondHop() public {
        // Trade 1 WETH for USDC through DAI. The second hop carries a
        // fallback: its primary reverts, the Uniswap V2 executor completes
        // it. The first hop's output stays at the router (never
        // pre-positioned at the primary pool) and the fallback transfers it
        // to the pool itself.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        swaps[1] = encodeSequentialSwap(
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                address(revertingExecutor),
                abi.encodePacked(DAI_ADDR, USDC_ADDR),
                address(usv2Executor),
                encodeUniswapV2Swap(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR)
            )
        );

        tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 2005810530);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        vm.stopPrank();
    }

    function testSequentialSwapFallbackFirstHop() public {
        // The first hop carries a fallback. The primary's revert rolls back
        // any user-fund transfer, and the fallback re-pulls the input from
        // the user's wallet.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                address(revertingExecutor),
                abi.encodePacked(WETH_ADDR, DAI_ADDR),
                address(usv2Executor),
                encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
            )
        );
        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR)
        );

        tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 2005810530);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);
        vm.stopPrank();
    }

    function testSplitSwapFallbackLastHop() public {
        // Same route as _getSplitSwaps in TychoRouterSplitSwap.t.sol, with
        // the DAI -> USDC hop wrapped in a fallback bundle whose primary
        // reverts.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = new bytes[](4);
        // WETH -> WBTC (60%)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100,
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );
        // WBTC -> USDC
        swaps[1] = encodeSplitSwap(
            uint8(1),
            uint8(3),
            uint24(0),
            address(usv2Executor),
            encodeUniswapV2Swap(USDC_WBTC_POOL, WBTC_ADDR, USDC_ADDR)
        );
        // WETH -> DAI (remaining 40%)
        swaps[2] = encodeSplitSwap(
            uint8(0),
            uint8(2),
            uint24(0),
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        // DAI -> USDC with fallback
        swaps[3] = encodeSplitSwap(
            uint8(2),
            uint8(3),
            uint24(0),
            LibSwap.FALLBACK_MARKER,
            encodeFallbackData(
                address(revertingExecutor),
                abi.encodePacked(DAI_ADDR, USDC_ADDR),
                address(usv2Executor),
                encodeUniswapV2Swap(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR)
            )
        );

        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            4,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 1989737355);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        vm.stopPrank();
    }

    function testTrySwapOnExecutorOnlySelf() public {
        vm.expectRevert(abi.encodeWithSelector(Dispatcher__OnlySelf.selector));
        tychoRouter.trySwapOnExecutor(
            address(usv2Executor), 1 ether, hex"", true, false, ALICE
        );
    }
}
