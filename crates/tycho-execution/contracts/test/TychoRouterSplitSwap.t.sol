pragma solidity ^0.8.26;

import {
    TychoRouterV3,
    TransferManager,
    ClientFeeParams
} from "@src/TychoRouterV3.sol";
import "./TychoRouterTestSetup.sol";
import {Vault__UnexpectedNonZeroCount} from "@src/Vault.sol";

import {
    TransferManager__ExceededTransferFromAllowance
} from "@src/TransferManager.sol";

contract HackedCallbackDataPool is Constants {
    // A hacked USV3-compatible pool. When called via swap(), it triggers a
    // USV3 callback with PEPE as the owed token instead of the real input token. The
    // real UniswapV3Executor honestly relays whatever the pool reports, so the
    // TransferManager ends up transferring PEPE.
    // Matches IUniswapV3Pool.swap signature
    function swap(address, bool, int256, uint160, bytes calldata)
        external
        returns (int256, int256)
    {
        // Craft callback data with stolenToken as tokenIn.
        // The UniV3 executor reads tokenIn from
        // callbackData[0:20].
        bytes memory cbData = abi.encodePacked(
            bytes20(PEPE_ADDR), // tokenIn
            bytes20(address(0)), //  tokenOut
            bytes3(uint24(0)) //     fee
        );
        uint256 stolenAmount = 1000 ether;

        // slither-disable-next-line low-level-calls
        (bool ok,) = msg.sender
            .call(
                abi.encodeWithSignature(
                    "uniswapV3SwapCallback(int256,int256,bytes)",
                    int256(stolenAmount), // amount0Delta (owed)
                    int256(0), //            amount1Delta
                    cbData
                )
            );
        require(ok, "Callback failed");

        // First value is the stolen PEPE amount
        return (int256(stolenAmount), int256(0));
    }
}

contract TychoRouterSplitSwapTest is TychoRouterTestSetup {
    function _getSplitSwaps() private view returns (bytes[] memory) {
        // Trade 1 WETH for USDC through DAI and WBTC with 4 swaps on Uniswap V2
        //          ->   DAI   ->
        // 1 WETH                   USDC
        //          ->   WBTC  ->
        //       (univ2)     (univ2)
        bytes[] memory swaps = new bytes[](4);

        // WETH -> WBTC (60%)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
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
        // WETH -> DAI
        swaps[2] = encodeSplitSwap(
            uint8(0),
            uint8(2),
            uint24(0),
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        // DAI -> USDC
        swaps[3] = encodeSplitSwap(
            uint8(2),
            uint8(3),
            uint24(0),
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR)
        );

        return swaps;
    }

    function testSplitSwapInternalMethod() public {
        // Trade 1 WETH for USDC through DAI and WBTC - see _getSplitSwaps for more info

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        bytes[] memory swaps = _getSplitSwaps();
        // Set transient storage to allow transferFrom from ALICE
        tychoRouter.tstoreExposed(WETH_ADDR, amountIn, false, false);
        tychoRouter.exposedSplitSwap(
            amountIn, 4, pleEncode(swaps), tychoRouterAddr, false
        );
        vm.stopPrank();

        uint256 usdcBalance = IERC20(USDC_ADDR).balanceOf(tychoRouterAddr);
        assertEq(usdcBalance, 1989737355);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);
    }

    function testSplitSwapPermit2() public {
        // Trade 1 WETH for USDC through DAI and WBTC - see _getSplitSwaps for more info

        uint256 amountIn = 1 ether;
        uint256 existingVaultBalance = 1.5 ether;
        deal(WETH_ADDR, ALICE, amountIn + existingVaultBalance);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, existingVaultBalance);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, tychoRouterAddr, amountIn);

        bytes[] memory swaps = _getSplitSwaps();

        // Alice has an existing Vault balance which should not be used.
        tychoRouter.deposit(WETH_ADDR, existingVaultBalance);
        tychoRouter.splitSwapPermit2(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1, // expected amount out
            1, // min amount out
            4,
            ALICE,
            noClientFee(),
            permitSingle,
            signature,
            pleEncode(swaps)
        );

        uint256 usdcBalance = IERC20(USDC_ADDR).balanceOf(ALICE);
        assertEq(usdcBalance, 1989737355);
        assertEq(
            IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), existingVaultBalance
        );
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);
        // Check that ALICE's Vault balance was not affected.
        assertEq(
            tychoRouter.balanceOf(ALICE, uint256(uint160(WETH_ADDR))),
            existingVaultBalance
        );
    }

    function testSplitSwapTransferFrom() public {
        // Trade 1 WETH for USDC through DAI and WBTC - see _getSplitSwaps for more info
        uint256 amountIn = 1 ether;
        uint256 existingVaultBalance = 1.5 ether;
        deal(WETH_ADDR, ALICE, amountIn + existingVaultBalance);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR)
            .approve(tychoRouterAddr, amountIn + existingVaultBalance);

        bytes[] memory swaps = _getSplitSwaps();

        // Alice has an existing Vault balance which should not be used.
        tychoRouter.deposit(WETH_ADDR, existingVaultBalance);
        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1000_000000, // expected amount out
            1000_000000 * 9800 / 10000, // min amount out
            4,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        uint256 usdcBalance = IERC20(USDC_ADDR).balanceOf(ALICE);
        assertEq(usdcBalance, 1989737355);
        assertEq(
            IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), existingVaultBalance
        );
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);
        // Check that ALICE's Vault balance was not affected.
        assertEq(
            tychoRouter.balanceOf(ALICE, uint256(uint160(WETH_ADDR))),
            existingVaultBalance
        );
    }

    function testSplitSwapUndefinedMinAmount() public {
        // Min amount should always be non-zero. If zero, swap attempt should revert.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(tychoRouterAddr), amountIn);

        bytes[] memory swaps = _getSplitSwaps();

        vm.expectRevert(TychoRouter__AmountOutZero.selector);
        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            0, // expected amount out
            0, // min amount out
            4,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();
    }

    function testSplitSwapZeroInput() public {
        bytes[] memory swaps = _getSplitSwaps();

        vm.expectRevert(TychoRouter__ZeroInput.selector);
        tychoRouter.splitSwap(
            0,
            WETH_ADDR,
            USDC_ADDR,
            1,
            1,
            4,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
    }

    function testSplitSwapInsufficientApproval() public {
        // Trade 1 WETH for USDC through DAI and WBTC - see _getSplitSwaps for more info
        uint256 amountIn = 1 ether;

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        // Approve less than the amountIn
        IERC20(WETH_ADDR).approve(address(tychoRouterAddr), amountIn - 1);
        bytes[] memory swaps = _getSplitSwaps();

        vm.expectRevert();
        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1000_000000, // expected amount out
            1000_000000 * 9800 / 10000, // min amount out
            2,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        vm.stopPrank();
    }

    function testTransferFromExceedsRestriction() public {
        // A maliciously encoded split swap attempts to take more than the input amount
        // from the user's wallet. The user has accidentally allowed MAX - REVERT

        //          ->   WBTC
        // 1 WETH
        //          ->   WBTC
        //       (univ2)
        bytes[] memory swaps = new bytes[](2);

        // WETH -> WBTC (60%)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24((0xffffff * 60) / 100), // 60%
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );
        // WETH -> WBTC (60%)
        swaps[1] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24((0xffffff * 60) / 100), // 60%
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );

        uint256 amountIn = 100 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        // Alice's mistake - too high approval. She should still be protected by our
        // router.
        IERC20(WETH_ADDR).approve(tychoRouterAddr, UINT256_MAX);

        vm.expectRevert(
            abi.encodeWithSelector(
                TransferManager__ExceededTransferFromAllowance.selector,
                40000000000000000000,
                60000000000000000000
            )
        );
        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            WBTC_ADDR,
            200_000000, // expected amount out (2 WBTC)
            200_000000 * 9800 / 10000, // min amount out
            4,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
    }

    function testSplitSwapNegativeSlippageFailure() public {
        // Trade 1 WETH for USDC through DAI and WBTC - see _getSplitSwaps for more info

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, tychoRouterAddr, amountIn);

        bytes[] memory swaps = _getSplitSwaps();

        uint256 minAmountOut = 3000 * 1e18;

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NegativeSlippage.selector,
                1989737355, // actual amountOut
                minAmountOut
            )
        );
        tychoRouter.splitSwapPermit2(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            minAmountOut,
            minAmountOut,
            4,
            ALICE,
            noClientFee(),
            permitSingle,
            signature,
            pleEncode(swaps)
        );
        vm.stopPrank();
    }

    function testEmptySwapsRevert() public {
        uint256 amountIn = 10 ** 18;
        bytes memory swaps = "";
        vm.expectRevert(TychoRouter__EmptySwaps.selector);
        tychoRouter.exposedSplitSwap(amountIn, 2, swaps, tychoRouterAddr, false);
    }

    function testSplitInputCyclicSwapInternalMethod() public {
        // This test has start and end tokens that are the same
        // The flow is:
        //            ┌─ (USV3, 60% split) ──> WETH ─┐
        //            │                              │
        // USDC ──────┤                              ├──(USV2)──> USDC
        //            │                              │
        //            └─ (USV3, 40% split) ──> WETH ─┘
        uint256 amountIn = 100 * 10 ** 6;

        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory usdcWethV3Pool1ZeroOneData =
            encodeUniswapV3Swap(USDC_ADDR, WETH_ADDR, USDC_WETH_USV3, true);

        bytes memory usdcWethV3Pool2ZeroOneData =
            encodeUniswapV3Swap(USDC_ADDR, WETH_ADDR, USDC_WETH_USV3_2, true);

        bytes memory wethUsdcV2OneZeroData =
            encodeUniswapV2Swap(USDC_WETH_USV2, WETH_ADDR, USDC_ADDR);

        bytes[] memory swaps = new bytes[](3);
        // USDC -> WETH (60% split)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
            address(usv3Executor),
            usdcWethV3Pool1ZeroOneData
        );
        // USDC -> WETH (40% remainder)
        swaps[1] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24(0),
            address(usv3Executor),
            usdcWethV3Pool2ZeroOneData
        );
        // WETH -> USDC
        swaps[2] = encodeSplitSwap(
            uint8(1),
            uint8(0),
            uint24(0),
            address(usv2Executor),
            wethUsdcV2OneZeroData
        );
        // Set transient storage to allow transferFrom from ALICE
        tychoRouter.tstoreExposed(USDC_ADDR, amountIn, false, false);
        tychoRouter.exposedSplitSwap(
            amountIn, 2, pleEncode(swaps), tychoRouterAddr, false
        );
        vm.stopPrank();
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 99654537);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
    }

    function testSplitMultipleTransferFromProtocolDebit() public {
        // This test attempts to perform multiple `transferFrom`s - which is not
        // permitted by the TychoRouterV3.
        //
        // The flow is:
        //            ┌─ (BALANCER V2, 60% split) ──┐
        //            │                             │
        // WETH ──────┤                             ├────> BAL
        //            │                             │
        //            └─ (BALANCER V2, 40% split) ──┘
        uint256 amountIn = 1 ether;
        uint256 existingRouterBalance = 3 ether;
        bytes32 WETH_BAL_POOL_ID =
            0x5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014;

        deal(WETH_ADDR, ALICE, amountIn + existingRouterBalance);
        vm.startPrank(ALICE);

        IERC20(WETH_ADDR)
            .approve(tychoRouterAddr, amountIn + existingRouterBalance);

        // Simulate funds already in the router in Alice's vault - we must make sure
        // these are untouched after our swap.
        tychoRouter.deposit(WETH_ADDR, existingRouterBalance);

        // For simplicity, just use the same protocol data for both swap legs
        bytes memory protocolData =
            abi.encodePacked(WETH_ADDR, BAL_ADDR, WETH_BAL_POOL_ID);

        bytes[] memory swaps = new bytes[](2);
        // WETH -> BAL (60% split)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
            address(balancerv2Executor),
            protocolData
        );

        // WETH -> BAL (40% remainder)
        swaps[1] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24(0), // remaining 40%
            address(balancerv2Executor),
            protocolData
        );
        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            BAL_ADDR,
            1, // expected amount out
            1, // min amount out
            2, // number of tokens
            ALICE, // receiver
            noClientFee(),
            pleEncode(swaps)
        );
        assertEq(IERC20(BAL_ADDR).balanceOf(ALICE), 1328_449676114497362517);

        // Vault funds untouched
        assertEq(
            tychoRouter.balanceOf(ALICE, uint256(uint160(WETH_ADDR))),
            existingRouterBalance
        );

        // Router tokens untouched
        assertEq(
            IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), existingRouterBalance
        );
    }

    function testSplitIllegalSplitAmounts() public {
        // A maliciously encoded split swap attempts to take more than the input amount
        // from the user's vault - REVERT

        // The flow is:
        //            ┌─ (60% split) ───┐
        //            │                 │
        // USDC ──────┤                 ├────> WBTC
        //            │                 │
        //            └─ (60% split) ───┘
        bytes[] memory swaps = new bytes[](2);

        // WETH -> WBTC (60%)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );

        // WETH -> WBTC (60% again - illegal, total 120%)
        swaps[1] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
            address(usv2Executor),
            encodeUniswapV2Swap(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );

        uint256 amountIn = 1 ether;
        uint256 existingVaultBalance = 3 ether;
        deal(WETH_ADDR, ALICE, amountIn + existingVaultBalance);

        vm.startPrank(ALICE);

        // Deposit into vault
        IERC20(WETH_ADDR).approve(tychoRouterAddr, existingVaultBalance);
        tychoRouter.deposit(WETH_ADDR, existingVaultBalance);

        // Should revert with arithmetic underflow when trying to take 120% of the
        // input amount (0.6 + 0.6 = 1.2 ether, but only 1 ether available)
        vm.expectRevert(stdError.arithmeticError);
        tychoRouter.splitSwapUsingVault(
            amountIn,
            WETH_ADDR,
            WBTC_ADDR,
            1, // expected amount out
            1, // min amount out
            4,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();
    }

    function testSplitOutputCyclicSwapInternalMethod() public {
        // This test has start and end tokens that are the same
        // The flow is:
        //                        ┌─── (USV3, 60% split) ───┐
        //                        │                         │
        // USDC ──(USV2) ── WETH──|                         ├─> USDC
        //                        │                         │
        //                        └─── (USV3, 40% split) ───┘

        uint256 amountIn = 100 * 10 ** 6;
        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory usdcWethV2Data =
            encodeUniswapV2Swap(USDC_WETH_USV2, USDC_ADDR, WETH_ADDR);

        bytes memory usdcWethV3Pool1OneZeroData =
            encodeUniswapV3Swap(WETH_ADDR, USDC_ADDR, USDC_WETH_USV3, false);

        bytes memory usdcWethV3Pool2OneZeroData =
            encodeUniswapV3Swap(WETH_ADDR, USDC_ADDR, USDC_WETH_USV3_2, false);

        bytes[] memory swaps = new bytes[](3);
        // USDC -> WETH
        swaps[0] = encodeSplitSwap(
            uint8(0), uint8(1), uint24(0), address(usv2Executor), usdcWethV2Data
        );
        // WETH -> USDC
        swaps[1] = encodeSplitSwap(
            uint8(1),
            uint8(0),
            (0xffffff * 60) / 100,
            address(usv3Executor),
            usdcWethV3Pool1OneZeroData
        );

        // WETH -> USDC
        swaps[2] = encodeSplitSwap(
            uint8(1),
            uint8(0),
            uint24(0),
            address(usv3Executor),
            usdcWethV3Pool2OneZeroData
        );

        // Set transient storage to allow transferFrom from ALICE
        tychoRouter.tstoreExposed(USDC_ADDR, amountIn, false, false);
        tychoRouter.exposedSplitSwap(
            amountIn, 2, pleEncode(swaps), tychoRouterAddr, true
        );
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 99444510);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
        vm.stopPrank();
    }

    // Base Network Tests
    // Make sure to set the RPC_URL to base network
    function testSplitSwapInternalMethodBase() public {
        vm.skip(true);
        vm.rollFork(26857267);
        uint256 amountIn = 10 * 10 ** 6;
        deal(BASE_USDC, tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(USDC_MAG7_POOL, BASE_USDC, BASE_MAG7);

        bytes memory swap = encodeSplitSwap(
            uint8(0), uint8(1), uint24(0), address(usv2Executor), protocolData
        );
        bytes[] memory swaps = new bytes[](1);
        swaps[0] = swap;

        tychoRouter.exposedSplitSwap(
            amountIn, 2, pleEncode(swaps), tychoRouterAddr, false
        );
        assertGt(IERC20(BASE_MAG7).balanceOf(tychoRouterAddr), 1379830606);
    }

    function testSplitSwapIntegration() public {
        // Performs a split swap from WETH to USDC though WBTC and DAI using USV2 pools
        //
        //         ┌──(USV2)──> WBTC ───(USV2)──> USDC
        //   WETH ─┤
        //         └──(USV2)──> DAI  ───(USV2)──> USDC
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_split_swap_strategy_encoder");
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGe(balanceAfter - balanceBefore, 26173932);

        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSplitInputCyclicSwapIntegration() public {
        deal(USDC_ADDR, ALICE, 100 * 10 ** 6);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_split_input_cyclic_swap");
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 99654537);

        vm.stopPrank();
    }

    function testSplitOutputCyclicSwapIntegration() public {
        deal(USDC_ADDR, ALICE, 100 * 10 ** 6);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_split_output_cyclic_swap");
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 99444510);

        vm.stopPrank();
    }

    function testHackedPoolTokenInjectionBlocked() public {
        // A split swap where one leg routes through a pool that pool sends a
        // malicious callback requesting a transfer of PEPE (a different token) from
        // the router to the attacker. The Dispatcher ignores the pool-reported tokenIn
        // and uses the tokenIn stored in transient storage from getTransferData (DAI),
        // so PEPE is never transferred.
        //
        //  WETH ──(USV2)──> DAI ─┬──(USV2, 60%)──────────> USDC
        //                        └──(HACKED POOL, 40%)─────> (nothing)
        //                             └─> callback injection blocked

        HackedCallbackDataPool hackedPool = new HackedCallbackDataPool();

        // Router has some PEPE thanks to someone's Vault balance.
        uint256 stolenPepe = 1000 ether;
        deal(PEPE_ADDR, tychoRouterAddr, stolenPepe);

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = new bytes[](3);

        // Swap 1: WETH -> DAI, full amount
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24(0),
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        // Swap 2: DAI -> USDC, 60% — leaves 40% for the hacked pool leg so that
        // the split input amount is non-zero
        swaps[1] = encodeSplitSwap(
            uint8(1),
            uint8(2),
            (0xffffff * 60) / 100, // 60%
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR)
        );

        // Swap 3: DAI -> USDC, 40%
        // Routes through the hacked pool via the real UniswapV3Executor. The pool's
        // callback requests a PEPE transfer instead of DAI. The Dispatcher uses
        // the tokenIn stored in transient storage (DAI) and ignores the injected PEPE.
        bytes memory v3Data = abi.encodePacked(
            DAI_ADDR, // tokenIn
            USDC_ADDR, // tokenOut
            uint24(3000), // fee
            address(hackedPool), // target
            true //  zeroForOne
        );
        swaps[2] = encodeSplitSwap(
            uint8(1), uint8(2), uint24(0), address(usv3Executor), v3Data
        );

        tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1, // expected amount out
            1, // extremely low min amount to make swap pass
            3, // nTokens
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        // PEPE was not stolen — token injection blocked.
        assertEq(IERC20(PEPE_ADDR).balanceOf(tychoRouterAddr), stolenPepe);
        vm.stopPrank();
    }

    function testSplitSwapMultipleOutputToRouter() public {
        // A split swap where both legs use a Curve executor (outputToRouter = true).
        // The Dispatcher independently measures each leg's output via balance-diff,
        // forwards to the receiver, and the split-swap accumulates the totals
        // correctly.
        //
        //          ┌──(Curve tripool, 60%)──┐
        //   DAI ───┤                        ├──> USDC
        //          └──(Curve tripool, 40%)──┘
        //
        uint256 amountIn = 1000 ether;
        deal(DAI_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amountIn);

        // Curve tripool: DAI(0) -> USDC(1), stable pool
        bytes memory curveData = abi.encodePacked(
            DAI_ADDR,
            USDC_ADDR,
            TRIPOOL,
            uint8(1), // stable pool type
            uint8(0), // i = DAI index
            uint8(1) //  j = USDC index
        );

        bytes[] memory swaps = new bytes[](2);

        // DAI -> USDC (60%)
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100,
            address(curveExecutor),
            curveData
        );

        // DAI -> USDC (remaining 40%)
        swaps[1] = encodeSplitSwap(
            uint8(0), uint8(1), uint24(0), address(curveExecutor), curveData
        );

        uint256 amountOut = tychoRouter.splitSwap(
            amountIn,
            DAI_ADDR,
            USDC_ADDR,
            1, // expected amount out
            1, // min amount out
            2, // nTokens
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        uint256 usdcBalance = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertEq(amountOut, usdcBalance);
        assertGt(usdcBalance, 0);
    }
}
