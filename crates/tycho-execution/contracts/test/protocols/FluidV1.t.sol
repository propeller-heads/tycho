pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {FluidV1Executor, IFluidV1Dex} from "@src/executors/FluidV1Executor.sol";
import {Constants} from "../Constants.sol";
import "forge-std/Test.sol";

contract FluidV1ExecutorExposed is FluidV1Executor {
    constructor(address _liquidity) FluidV1Executor(_liquidity) {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (IFluidV1Dex dex, bool zero2one, bool isNative)
    {
        return _decodeData(data);
    }

    function setCurrentDex(IFluidV1Dex dex) public {
        _setCurrentDex(dex);
    }

    function getCurrentDex() public view returns (address) {
        return _getCurrentDex();
    }

    // The Fluid protocol calls dexCallback(address token, uint256 amount) during swapInWithCallback.
    // The first argument is the token that must be transferred to liquidity.
    function dexCallback(address tokenIn, uint256) public {
        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType == TransferManager.TransferType.Transfer) {
            uint256 amount = abi.decode(msg.data[36:68], (uint256));
            IERC20(tokenIn).transfer(receiver, amount);
        }
        handleCallback(msg.data);
    }
}

contract FluidV1ExecutorTest is Test, Constants {
    FluidV1ExecutorExposed executor;

    function setUp() public {
        uint256 forkBlock = 23748828;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        executor = new FluidV1ExecutorExposed(FLUIDV1_LIQUIDITY);
    }

    function testDecodeData() public view {
        address dex = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        address inputToken = 0x9D39A5DE30e57443BfF2A8307A4256c8797A3497; // sUSDe
        address outputToken = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
        bytes memory params =
            abi.encodePacked(dex, true, inputToken, outputToken, false);
        IFluidV1Dex dexVal;
        bool zero2oneVal;
        bool isNative;

        (dexVal, zero2oneVal, isNative) = executor.decodeData(params);

        assertEq(address(dexVal), dex);
        assert(zero2oneVal);
    }

    function testGetTransferData() public {
        address dex = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        address inputToken = 0x9D39A5DE30e57443BfF2A8307A4256c8797A3497; // sUSDe
        address outputToken = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
        bool zero2one = true;
        bool isNative = false;
        bytes memory params =
            abi.encodePacked(dex, zero2one, inputToken, outputToken, isNative);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(params);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tokenIn, inputToken);
        assertEq(tokenOut, 0xdAC17F958D2ee523a2206206994597C13D831ec7);
        assertEq(outputToRouter, false);
    }

    function testGetCallbackTransferData() public {
        uint256 amountOwed = 1000000000000000000;
        bytes memory data =
            abi.encodeWithSelector(hex"12345678", DAI_ADDR, amountOwed);
        address dexAddress = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        executor.setCurrentDex(IFluidV1Dex(dexAddress));

        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData(data, DAI_ADDR, address(this));

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, FLUIDV1_LIQUIDITY);
    }

    function testGetCallbackTransferDataETH() public {
        uint256 amountOwed = 1000000000000000000;
        bytes memory data =
            abi.encodeWithSelector(hex"12345678", ETH_ADDR, amountOwed);
        address dexAddress = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        executor.setCurrentDex(IFluidV1Dex(dexAddress));

        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData(data, ETH_ADDR, address(this));

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, FLUIDV1_LIQUIDITY);
    }

    function testSwapParamsRoundtrip() public {
        address dexAddress = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        IFluidV1Dex dex = IFluidV1Dex(dexAddress);

        executor.setCurrentDex(dex);
        address dexVal = executor.getCurrentDex();

        assertEq(dexVal, dexAddress);
    }

    function testVerifyCallbackOk() public {
        address dexAddress = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        executor.setCurrentDex(IFluidV1Dex(dexAddress));
        bytes memory param = abi.encodePacked(bytes4(0x9410ae88));

        vm.prank(dexAddress);
        executor.verifyCallback(param);
    }

    function testVerifyCallbackBadSender() public {
        address dexAddress = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        executor.setCurrentDex(IFluidV1Dex(dexAddress));
        bytes memory param = abi.encodePacked(bytes4(0x9410ae88));

        vm.expectRevert();
        executor.verifyCallback(param);
    }

    function testVerifyCallbackBadSelector() public {
        address dexAddress = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        executor.setCurrentDex(IFluidV1Dex(dexAddress));
        bytes memory param = abi.encodePacked(bytes4(0x00000000));

        vm.prank(dexAddress);
        vm.expectRevert();
        executor.verifyCallback(param);
    }

    function testSwap() public {
        address dex = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
        IERC20 sUSDe = IERC20(0x9D39A5DE30e57443BfF2A8307A4256c8797A3497);
        IERC20 USDT = IERC20(0xdAC17F958D2ee523a2206206994597C13D831ec7);
        uint256 amountIn = 10e18;
        bytes memory params =
            abi.encodePacked(dex, true, address(sUSDe), address(USDT), false);
        deal(address(sUSDe), address(executor), amountIn);
        uint256 balanceBefore = USDT.balanceOf(BOB);

        executor.swap(amountIn, params, BOB);

        uint256 balanceAfter = USDT.balanceOf(BOB);
        assertGt(balanceAfter - balanceBefore, 0);
    }

    function testSellNative() public {
        address dex = 0xDD72157A021804141817d46D9852A97addfB9F59;
        IERC20 ezETH = IERC20(0xbf5495Efe5DB9ce00f80364C8B423567e58d2110);
        uint256 amountIn = 10e18;
        bytes memory params =
            abi.encodePacked(dex, false, ETH_ADDR, address(ezETH), true);
        deal(address(executor), amountIn);
        uint256 balanceBefore = ezETH.balanceOf(BOB);

        executor.swap(amountIn, params, BOB);

        uint256 balanceAfter = ezETH.balanceOf(BOB);
        assertGt(balanceAfter - balanceBefore, 0);
    }

    function testBuyNative() public {
        address dex = 0xDD72157A021804141817d46D9852A97addfB9F59;
        IERC20 ezETH = IERC20(0xbf5495Efe5DB9ce00f80364C8B423567e58d2110);
        uint256 amountIn = 10e18;
        bytes memory params =
            abi.encodePacked(dex, true, address(ezETH), ETH_ADDR, false);
        deal(address(ezETH), address(executor), amountIn);
        uint256 balanceBefore = BOB.balance;

        executor.swap(amountIn, params, BOB);

        uint256 balanceAfter = BOB.balance;
        assertGt(balanceAfter - balanceBefore, 0);
    }
}

contract TychoRouterForFluidV1Test is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 23768324;
    }

    function testSingleSwap() public {
        IERC20 sUSDe = IERC20(0x9D39A5DE30e57443BfF2A8307A4256c8797A3497);
        IERC20 USDT = IERC20(0xdAC17F958D2ee523a2206206994597C13D831ec7);
        uint256 amountIn = 10e18;
        deal(address(sUSDe), ALICE, amountIn);
        uint256 balanceBefore = USDT.balanceOf(ALICE);
        vm.startPrank(ALICE);
        sUSDe.approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_fluid_v1");

        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = USDT.balanceOf(ALICE);
        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1201540);
        assertEq(sUSDe.balanceOf(tychoRouterAddr), 0);
        assertEq(USDT.balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialSwap() public {
        IERC20 sUSDe = IERC20(0x9D39A5DE30e57443BfF2A8307A4256c8797A3497);
        IERC20 USDC = IERC20(0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48);
        uint256 amountIn = 10e18;
        deal(address(sUSDe), ALICE, amountIn);
        uint256 balanceBefore = USDC.balanceOf(ALICE);
        vm.startPrank(ALICE);
        sUSDe.approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_sequential_encoding_strategy_fluid_v1");

        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = USDC.balanceOf(ALICE);
        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 1201417);
        assertEq(sUSDe.balanceOf(tychoRouterAddr), 0);
        assertEq(USDC.balanceOf(tychoRouterAddr), 0);
    }

    function testSingleSwapNativeSell() public {
        address fluidDex = 0xDD72157A021804141817d46D9852A97addfB9F59;
        IERC20 ezETH = IERC20(0xbf5495Efe5DB9ce00f80364C8B423567e58d2110);
        uint256 amountIn = 1 ether;

        deal(ALICE, amountIn);
        uint256 balanceBefore = ezETH.balanceOf(ALICE);

        bytes memory protocolData = abi.encodePacked(
            fluidDex,
            false, // zero2one
            ETH_ADDR, // tokenIn = native ETH
            address(ezETH), // tokenOut
            true // isNativeSell
        );
        bytes memory swap =
            encodeSingleSwap(address(fluidV1Executor), protocolData);

        vm.prank(ALICE);
        uint256 amountOut = tychoRouter.singleSwap{value: amountIn}(
            amountIn, ETH_ADDR, address(ezETH), 1, ALICE, noClientFee(), swap
        );

        assertGt(amountOut, 0);
        assertEq(ezETH.balanceOf(ALICE) - balanceBefore, amountOut);
        assertEq(tychoRouterAddr.balance, 0);
    }
}
