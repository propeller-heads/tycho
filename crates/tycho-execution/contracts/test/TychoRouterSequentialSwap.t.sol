pragma solidity ^0.8.26;

import {
    TychoRouter,
    ClientFeeParams,
    TychoRouter__NegativeSlippage
} from "@src/TychoRouter.sol";
import "./TychoRouterTestSetup.sol";

contract TychoRouterSequentialSwapTest is TychoRouterTestSetup {
    function _getSequentialSwaps() internal view returns (bytes[] memory) {
        // Trade 1 WETH for USDC through DAI with 2 swaps on Uniswap V2
        // 1 WETH   ->   DAI   ->   USDC
        //       (univ2)     (univ2)

        bytes[] memory swaps = new bytes[](2);
        // WETH -> DAI
        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        // DAI -> USDC
        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR)
        );
        return swaps;
    }

    function testSequentialSwapPermit2() public {
        // Trade 1 WETH for USDC through DAI - see _getSequentialSwaps for more info
        // Make sure DAI vault funds are unaffected
        uint256 amountIn = 1 ether;
        uint256 existingDAIVaultBalance = 10_000 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        deal(DAI_ADDR, ALICE, existingDAIVaultBalance);

        vm.startPrank(ALICE);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, tychoRouterAddr, amountIn);

        IERC20(DAI_ADDR).approve(tychoRouterAddr, existingDAIVaultBalance);
        tychoRouter.deposit(DAI_ADDR, existingDAIVaultBalance);

        bytes[] memory swaps = _getSequentialSwaps();
        tychoRouter.sequentialSwapPermit2(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1000_000000, // min amount,
            ALICE,
            noClientFee(),
            permitSingle,
            signature,
            pleEncode(swaps)
        );

        uint256 usdcBalance = IERC20(USDC_ADDR).balanceOf(ALICE);
        assertEq(usdcBalance, 2005810530);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);

        // Vault balances untouched
        assertEq(
            IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), existingDAIVaultBalance
        );
        assertEq(
            tychoRouter.balanceOf(ALICE, uint256(uint160(DAI_ADDR))),
            existingDAIVaultBalance
        );
    }

    function testSequentialSwapTransferFrom() public {
        // Trade 1 WETH for USDC through DAI - see _getSequentialSwaps for more info
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = _getSequentialSwaps();
        tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1000_000000, // min amount
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );

        uint256 usdcBalance = IERC20(USDC_ADDR).balanceOf(ALICE);
        assertEq(usdcBalance, 2005810530);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialSwapIntoVault() public {
        // Trade 1 WETH for USDC through DAI, crediting output into vault
        // WETH -> DAI -> USDC (same route as _getSequentialSwaps)
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = _getSequentialSwaps();
        tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            1000_000000, // min amount
            tychoRouterAddr, // output goes to vault
            noClientFee(),
            pleEncode(swaps)
        );

        uint256 vaultUsdc =
            tychoRouter.balanceOf(ALICE, uint256(uint160(USDC_ADDR)));
        assertEq(vaultUsdc, 2005810530);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        vm.stopPrank();
    }

    function testSequentialSwapUndefinedMinAmount() public {
        // Trade 1 WETH for USDC through DAI - see _getSequentialSwaps for more info
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes[] memory swaps = _getSequentialSwaps();
        vm.expectRevert(TychoRouter__UndefinedMinAmountOut.selector);
        tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            0, // min amount
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
    }

    function testSequentialSwapZeroInput() public {
        bytes[] memory swaps = _getSequentialSwaps();

        vm.expectRevert(TychoRouter__ZeroInput.selector);
        tychoRouter.sequentialSwap(
            0, WETH_ADDR, USDC_ADDR, 1, ALICE, noClientFee(), pleEncode(swaps)
        );
    }

    function testSequentialSwapInsufficientApproval() public {
        // Trade 1 WETH for USDC through DAI - see _getSequentialSwaps for more info
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn - 1);

        bytes[] memory swaps = _getSequentialSwaps();
        vm.expectRevert();
        tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            0, // min amount
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
    }

    function testSequentialSwapNegativeSlippageFailure() public {
        // Trade 1 WETH for USDC through DAI - see _getSequentialSwaps for more info

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, tychoRouterAddr, amountIn);

        bytes[] memory swaps = _getSequentialSwaps();

        uint256 minAmountOut = 3000 * 1e18;

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NegativeSlippage.selector,
                2005810530, // actual amountOut
                minAmountOut
            )
        );
        tychoRouter.sequentialSwapPermit2(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            minAmountOut,
            ALICE,
            noClientFee(),
            permitSingle,
            signature,
            pleEncode(swaps)
        );
        vm.stopPrank();
    }

    function testCyclicSequentialSwap() public {
        // This test has start and end tokens that are the same
        // The flow is:
        // USDC --(USV3)--> WETH --(USV3)--> USDC
        uint256 amountIn = 100 * 10 ** 6;
        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory usdcWethV3Pool1ZeroOneData =
            encodeUniswapV3Swap(USDC_ADDR, WETH_ADDR, USDC_WETH_USV3, true);

        bytes memory usdcWethV3Pool2OneZeroData =
            encodeUniswapV3Swap(WETH_ADDR, USDC_ADDR, USDC_WETH_USV3_2, false);

        bytes[] memory swaps = new bytes[](2);
        // USDC -> WETH
        swaps[0] = encodeSequentialSwap(
            address(usv3Executor), usdcWethV3Pool1ZeroOneData
        );
        // WETH -> USDC
        swaps[1] = encodeSequentialSwap(
            address(usv3Executor), usdcWethV3Pool2OneZeroData
        );

        // Set transient storage to allow transferFrom from ALICE
        tychoRouter.tstoreExposed(USDC_ADDR, amountIn, false, false);
        tychoRouter.exposedSequentialSwap(
            amountIn, pleEncode(swaps), tychoRouterAddr
        );
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 99792554);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
        vm.stopPrank();
    }

    function testSequentialSwapIntegrationPermit2() public {
        // Performs a split swap from WETH to USDC though WBTC and DAI using USV2 pools
        //
        //   WETH ──(USV2)──> WBTC ───(USV2)──> USDC
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_sequential_swap_strategy_encoder");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1951856272);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialSwapIntegration() public {
        // Performs a split swap from WETH to USDC though WBTC and DAI using USV2 pools
        //
        //   WETH ──(USV2)──> WBTC ───(USV2)──> USDC
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_swap_strategy_encoder_transfer_from"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1951856272);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialCyclicSwapIntegration() public {
        // USDC -> WETH -> USDC  using two pools
        deal(USDC_ADDR, ALICE, 100 * 10 ** 6);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(PERMIT2_ADDRESS, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_sequential_strategy_cyclic_swap");
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 99792554);

        vm.stopPrank();
    }

    function testUSV3USV2Integration() public {
        // Performs a sequential swap from WETH to USDC though WBTC and DAI using USV3 and USV2 pools
        //
        //   WETH ──(USV3)──> WBTC ───(USV2)──> USDC
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_uniswap_v3_uniswap_v2");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1952973189);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testUSV3USV3Integration() public {
        // Performs a sequential swap from WETH to USDC though WBTC using USV3 pools
        //
        //   WETH ──(USV3)──> WBTC ───(USV3)──> USDC
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_uniswap_v3_uniswap_v3");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 2015740345);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testUSV3CurveIntegration() public {
        // Performs a sequential swap from WETH to USDT though WBTC using USV3 and Curve pools
        //
        //   WETH ──(USV3)──> WBTC ───(USV3)──> USDT
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDT_ADDR).balanceOf(ALICE);

        // Approve permit2
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile("test_uniswap_v3_curve");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDT_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 2018869128);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testCurveUSV2SequentialSwap() public {
        // ETH → stETH (Curve stETH pool) → WETH (UniV2 stETH/WETH)
        uint256 amountIn = 1 ether;
        deal(ALICE, amountIn);

        vm.startPrank(ALICE);

        bytes memory curveStEthData = abi.encodePacked(
            ETH_ADDR,
            STETH_ADDR,
            STETH_POOL,
            uint8(1), // poolType = stable
            uint8(0), // i = 0 (ETH)
            uint8(1) // j = 1 (stETH)
        );

        address stethWethV2Pool = 0x4028DAAC072e492d34a3Afdbef0ba7e35D8b55C4;
        bytes memory usv2StethWethData =
            encodeUniswapV2Swap(stethWethV2Pool, STETH_ADDR, WETH_ADDR);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(address(curveExecutor), curveStEthData);
        swaps[1] =
            encodeSequentialSwap(address(usv2Executor), usv2StethWethData);

        uint256 amountOut = tychoRouter.sequentialSwap{value: amountIn}(
            amountIn,
            ETH_ADDR, // tokenIn = native ETH
            WETH_ADDR,
            1, // min amount out
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        // curve pool transfers 999958043830457008 stETH to router
        // router actually got 999958043830457007 stETH
        // univ2 pool only gets 999958043830457006 stETH
        assertEq(amountOut, 993908205983850532);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(ALICE.balance, 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);

        vm.stopPrank();
    }

    function testBalancerV2USV2Integration() public {
        // Performs a sequential swap from WETH to USDC though WBTC using Balancer v2 and USV2 pools
        //
        //   WETH ──(balancer)──> WBTC ───(USV2)──> USDC
        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDT_ADDR).balanceOf(ALICE);

        // Approve
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_balancer_v2_uniswap_v2");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1949668893);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
