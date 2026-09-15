pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {
    LidoV4Executor,
    LidoV4Executor__InvalidDataLength,
    LidoV4Executor__InvalidDirection,
    IStETH,
    IWstETH,
    LidoV4Direction
} from "../../src/executors/LidoV4Executor.sol";
import {TestUtils} from "../TestUtils.sol";

/// Only the rate view the tests need; the executor itself never reads it.
interface IWstETHRate {
    function getWstETHByStETH(uint256 stETHAmount)
        external
        view
        returns (uint256);
}

contract LidoV4ExecutorExposed is LidoV4Executor {
    constructor(address stEthAddress, address wstEthAddress)
        LidoV4Executor(stEthAddress, wstEthAddress)
    {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (LidoV4Direction direction)
    {
        return _decodeData(data);
    }
}

contract LidoV4ExecutorTest is TestUtils, Constants {
    LidoV4ExecutorExposed lidoV4Executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 25603404);
        lidoV4Executor = new LidoV4ExecutorExposed(STETH_ADDR, WSTETH_ADDR);
    }

    function _mintStEthToExecutor(uint256 depositAmount)
        internal
        returns (uint256 minted)
    {
        bytes memory submitData =
            abi.encodePacked(uint8(LidoV4Direction.EthToStEth));

        vm.deal(address(this), depositAmount);
        uint256 balanceBefore =
            IERC20(STETH_ADDR).balanceOf(address(lidoV4Executor));

        lidoV4Executor.swap{value: depositAmount}(
            depositAmount, submitData, address(lidoV4Executor)
        );

        uint256 balanceAfter =
            IERC20(STETH_ADDR).balanceOf(address(lidoV4Executor));
        minted = balanceAfter - balanceBefore;
    }

    function testDecodeParamsSubmit() public view {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.EthToStEth));
        LidoV4Direction direction = lidoV4Executor.decodeParams(params);

        assertEq(uint8(direction), uint8(LidoV4Direction.EthToStEth));
    }

    function testDecodeParamsWrap() public view {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.StEthToWstEth));
        LidoV4Direction direction = lidoV4Executor.decodeParams(params);

        assertEq(uint8(direction), uint8(LidoV4Direction.StEthToWstEth));
    }

    function testDecodeParamsUnwrap() public view {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.WstEthToStEth));
        LidoV4Direction direction = lidoV4Executor.decodeParams(params);

        assertEq(uint8(direction), uint8(LidoV4Direction.WstEthToStEth));
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(uint8(LidoV4Direction.EthToStEth), uint8(1));

        vm.expectRevert(LidoV4Executor__InvalidDataLength.selector);
        lidoV4Executor.decodeParams(invalidParams);
    }

    function testDecodeParamsInvalidDirection() public {
        // One past the last variant, EthToWstEth.
        bytes memory invalidParams = abi.encodePacked(uint8(4));

        vm.expectRevert(LidoV4Executor__InvalidDirection.selector);
        lidoV4Executor.decodeParams(invalidParams);
    }

    function testGetTransferDataSubmit() public {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.EthToStEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV4Executor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, ETH_ADDRESS);
        assertEq(tokenOut, STETH_ADDR);
        assertEq(outputToRouter, true);
    }

    function testGetTransferDataWrap() public {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.StEthToWstEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV4Executor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, WSTETH_ADDR);
        assertEq(tokenIn, STETH_ADDR);
        assertEq(tokenOut, WSTETH_ADDR);
        assertEq(outputToRouter, true);
    }

    function testGetTransferDataUnwrap() public {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.WstEthToStEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV4Executor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, WSTETH_ADDR);
        assertEq(tokenOut, STETH_ADDR);
        assertEq(outputToRouter, true);
    }

    function testGetTransferDataSubmitAndWrap() public {
        bytes memory params =
            abi.encodePacked(uint8(LidoV4Direction.EthToWstEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV4Executor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, ETH_ADDRESS);
        assertEq(tokenOut, WSTETH_ADDR);
        assertEq(outputToRouter, true);
    }

    function testSwapSubmitAndWrap() public {
        uint256 amountIn = 1 ether;
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV4Direction.EthToWstEth));

        vm.deal(address(this), amountIn);
        uint256 balanceBefore =
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV4Executor));

        lidoV4Executor.swap{value: amountIn}(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV4Executor));
        assertGt(balanceAfter, balanceBefore);
    }

    /// The shortcut has to mint what `wrap` would, so a router is never worse off taking it.
    function testSwapSubmitAndWrapMatchesSubmitThenWrap() public {
        uint256 amountIn = 1 ether;

        uint256 expected = IWstETHRate(WSTETH_ADDR).getWstETHByStETH(amountIn);

        vm.deal(address(this), amountIn);
        lidoV4Executor.swap{value: amountIn}(
            amountIn, abi.encodePacked(uint8(LidoV4Direction.EthToWstEth)), BOB
        );

        assertApproxEqAbs(
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV4Executor)), expected, 2
        );
    }

    function testSwapSubmit() public {
        uint256 amountIn = 1 ether;
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV4Direction.EthToStEth));

        vm.deal(address(this), amountIn);
        uint256 balanceBefore =
            IERC20(STETH_ADDR).balanceOf(address(lidoV4Executor));

        lidoV4Executor.swap{value: amountIn}(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(STETH_ADDR).balanceOf(address(lidoV4Executor));
        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapWrap() public {
        uint256 amountIn = _mintStEthToExecutor(1 ether);
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV4Direction.StEthToWstEth));

        vm.prank(address(lidoV4Executor));
        IERC20(STETH_ADDR).approve(WSTETH_ADDR, amountIn);

        uint256 balanceBefore =
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV4Executor));

        lidoV4Executor.swap(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV4Executor));
        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapUnwrap() public {
        uint256 amountIn = 1 ether;
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV4Direction.WstEthToStEth));

        deal(WSTETH_ADDR, address(lidoV4Executor), amountIn);

        uint256 balanceBefore =
            IERC20(STETH_ADDR).balanceOf(address(lidoV4Executor));

        lidoV4Executor.swap(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(STETH_ADDR).balanceOf(address(lidoV4Executor));
        assertGt(balanceAfter, balanceBefore);
    }
}

contract TychoRouterForLidoV4Test is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 25603404;
    }

    function _mintStEthTo(address recipient, uint256 depositAmount)
        internal
        returns (uint256 minted)
    {
        uint256 balanceBefore = IERC20(STETH_ADDR).balanceOf(recipient);

        vm.deal(recipient, depositAmount);
        vm.prank(recipient);
        IStETH(STETH_ADDR).submit{value: depositAmount}(address(0));

        uint256 balanceAfter = IERC20(STETH_ADDR).balanceOf(recipient);
        minted = balanceAfter - balanceBefore;
    }

    function testSingleLidoV4SubmitIntegration() public {
        IERC20 stEth = IERC20(STETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_lido_v4_submit"
        );

        vm.deal(ALICE, amountIn);
        vm.startPrank(ALICE);

        uint256 balanceBefore = stEth.balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call{value: amountIn}(callData);
        uint256 balanceAfter = stEth.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(balanceAfter, balanceBefore);
        assertLe(stEth.balanceOf(tychoRouterAddr), 1);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSingleLidoV4WrapIntegration() public {
        IERC20 wstEth = IERC20(WSTETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_lido_v4_wrap");

        _mintStEthTo(ALICE, 2 ether);
        vm.startPrank(ALICE);
        IERC20(STETH_ADDR).approve(tychoRouterAddr, amountIn);

        uint256 balanceBefore = wstEth.balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 balanceAfter = wstEth.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(balanceAfter, balanceBefore);
        assertLe(IERC20(STETH_ADDR).balanceOf(tychoRouterAddr), 1);
        assertEq(wstEth.balanceOf(tychoRouterAddr), 0);
    }

    function testSingleLidoV4UnwrapIntegration() public {
        IERC20 stEth = IERC20(STETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_lido_v4_unwrap"
        );

        deal(WSTETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WSTETH_ADDR).approve(tychoRouterAddr, amountIn);

        uint256 balanceBefore = stEth.balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 balanceAfter = stEth.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(balanceAfter, balanceBefore);
        assertEq(IERC20(WSTETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertLe(stEth.balanceOf(tychoRouterAddr), 1);
    }

    function testSingleLidoV4SubmitAndWrapIntegration() public {
        IERC20 wstEth = IERC20(WSTETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_lido_v4_submit_and_wrap"
        );

        vm.deal(ALICE, amountIn);
        vm.startPrank(ALICE);

        uint256 balanceBefore = wstEth.balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call{value: amountIn}(callData);
        uint256 balanceAfter = wstEth.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(balanceAfter, balanceBefore);
        assertEq(wstEth.balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
        vm.stopPrank();
    }

    function testSequentialLidoV4SubmitThenWrapIntegration() public {
        IERC20 wstEth = IERC20(WSTETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_lido_v4_submit_then_wrap"
        );

        vm.deal(ALICE, amountIn);
        vm.startPrank(ALICE);

        uint256 balanceBefore = wstEth.balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call{value: amountIn}(callData);
        uint256 balanceAfter = wstEth.balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(balanceAfter, balanceBefore);
        assertLe(IERC20(STETH_ADDR).balanceOf(tychoRouterAddr), 1);
        assertEq(wstEth.balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }
}
