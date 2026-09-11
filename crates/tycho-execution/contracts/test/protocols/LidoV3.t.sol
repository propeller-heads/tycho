pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {
    LidoV3Executor,
    LidoV3Executor__InvalidDataLength,
    LidoV3Executor__InvalidDirection,
    IStETH,
    IWstETH,
    LidoV3Direction
} from "../../src/executors/LidoV3Executor.sol";
import {TestUtils} from "../TestUtils.sol";

contract LidoV3ExecutorExposed is LidoV3Executor {
    constructor(address stEthAddress, address wstEthAddress)
        LidoV3Executor(stEthAddress, wstEthAddress)
    {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (LidoV3Direction direction)
    {
        return _decodeData(data);
    }
}

contract LidoV3ExecutorTest is TestUtils, Constants {
    LidoV3ExecutorExposed lidoV3Executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 24480104);
        lidoV3Executor = new LidoV3ExecutorExposed(STETH_ADDR, WSTETH_ADDR);
    }

    function _mintStEthToExecutor(uint256 depositAmount)
        internal
        returns (uint256 minted)
    {
        bytes memory submitData =
            abi.encodePacked(uint8(LidoV3Direction.EthToStEth));

        vm.deal(address(this), depositAmount);
        uint256 balanceBefore =
            IERC20(STETH_ADDR).balanceOf(address(lidoV3Executor));

        lidoV3Executor.swap{value: depositAmount}(
            depositAmount, submitData, address(lidoV3Executor)
        );

        uint256 balanceAfter =
            IERC20(STETH_ADDR).balanceOf(address(lidoV3Executor));
        minted = balanceAfter - balanceBefore;
    }

    function testDecodeParamsSubmit() public view {
        bytes memory params =
            abi.encodePacked(uint8(LidoV3Direction.EthToStEth));
        LidoV3Direction direction = lidoV3Executor.decodeParams(params);

        assertEq(uint8(direction), uint8(LidoV3Direction.EthToStEth));
    }

    function testDecodeParamsWrap() public view {
        bytes memory params =
            abi.encodePacked(uint8(LidoV3Direction.StEthToWstEth));
        LidoV3Direction direction = lidoV3Executor.decodeParams(params);

        assertEq(uint8(direction), uint8(LidoV3Direction.StEthToWstEth));
    }

    function testDecodeParamsUnwrap() public view {
        bytes memory params =
            abi.encodePacked(uint8(LidoV3Direction.WstEthToStEth));
        LidoV3Direction direction = lidoV3Executor.decodeParams(params);

        assertEq(uint8(direction), uint8(LidoV3Direction.WstEthToStEth));
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(uint8(LidoV3Direction.EthToStEth), uint8(1));

        vm.expectRevert(LidoV3Executor__InvalidDataLength.selector);
        lidoV3Executor.decodeParams(invalidParams);
    }

    function testDecodeParamsInvalidDirection() public {
        bytes memory invalidParams = abi.encodePacked(uint8(3));

        vm.expectRevert(LidoV3Executor__InvalidDirection.selector);
        lidoV3Executor.decodeParams(invalidParams);
    }

    function testGetTransferDataSubmit() public {
        bytes memory params =
            abi.encodePacked(uint8(LidoV3Direction.EthToStEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV3Executor.getTransferData(params);

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
            abi.encodePacked(uint8(LidoV3Direction.StEthToWstEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV3Executor.getTransferData(params);

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
            abi.encodePacked(uint8(LidoV3Direction.WstEthToStEth));

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lidoV3Executor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, WSTETH_ADDR);
        assertEq(tokenOut, STETH_ADDR);
        assertEq(outputToRouter, true);
    }

    function testSwapSubmit() public {
        uint256 amountIn = 1 ether;
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV3Direction.EthToStEth));

        vm.deal(address(this), amountIn);
        uint256 balanceBefore =
            IERC20(STETH_ADDR).balanceOf(address(lidoV3Executor));

        lidoV3Executor.swap{value: amountIn}(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(STETH_ADDR).balanceOf(address(lidoV3Executor));
        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapWrap() public {
        uint256 amountIn = _mintStEthToExecutor(1 ether);
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV3Direction.StEthToWstEth));

        vm.prank(address(lidoV3Executor));
        IERC20(STETH_ADDR).approve(WSTETH_ADDR, amountIn);

        uint256 balanceBefore =
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV3Executor));

        lidoV3Executor.swap(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(WSTETH_ADDR).balanceOf(address(lidoV3Executor));
        assertGt(balanceAfter, balanceBefore);
    }

    function testSwapUnwrap() public {
        uint256 amountIn = 1 ether;
        bytes memory protocolData =
            abi.encodePacked(uint8(LidoV3Direction.WstEthToStEth));

        deal(WSTETH_ADDR, address(lidoV3Executor), amountIn);

        uint256 balanceBefore =
            IERC20(STETH_ADDR).balanceOf(address(lidoV3Executor));

        lidoV3Executor.swap(amountIn, protocolData, BOB);

        uint256 balanceAfter =
            IERC20(STETH_ADDR).balanceOf(address(lidoV3Executor));
        assertGt(balanceAfter, balanceBefore);
    }
}

contract TychoRouterForLidoV3Test is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 24480104;
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

    function testSingleLidoV3SubmitIntegration() public {
        IERC20 stEth = IERC20(STETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_lido_v3_submit"
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

    function testSingleLidoV3WrapIntegration() public {
        IERC20 wstEth = IERC20(WSTETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_lido_v3_wrap");

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

    function testSingleLidoV3UnwrapIntegration() public {
        IERC20 stEth = IERC20(STETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_lido_v3_unwrap"
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

    function testSequentialLidoV3SubmitThenWrapIntegration() public {
        IERC20 wstEth = IERC20(WSTETH_ADDR);
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_lido_v3_submit_then_wrap"
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
