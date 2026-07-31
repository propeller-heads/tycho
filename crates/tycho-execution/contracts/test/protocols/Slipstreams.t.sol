pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "@permit2/src/interfaces/IAllowanceTransfer.sol";
import "@src/executors/SlipstreamsExecutor.sol";
import {Constants} from "../Constants.sol";
import {Permit2TestHelper} from "../Permit2TestHelper.sol";
import {Test} from "../../lib/forge-std/src/Test.sol";

contract SlipstreamsExecutorExposed is SlipstreamsExecutor {
    constructor() SlipstreamsExecutor() {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (address target, bool zeroForOne)
    {
        return _decodeData(data);
    }

    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata /* data */
    ) external {
        // tokenIn is determined by which delta is positive (the token being sold).
        // Callback data no longer encodes tokenIn; the Dispatcher passes it directly.
        address tokenIn = amount0Delta > 0
            ? IUniswapV3Pool(msg.sender).token0()
            : IUniswapV3Pool(msg.sender).token1();

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        assert(transferType == TransferManager.TransferType.Transfer);

        uint256 amount =
            amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        IERC20(tokenIn).transfer(receiver, amount);
        handleCallback(msg.data);
    }
}

contract SlipstreamsExecutorTest is Test, TestUtils, Constants {
    using SafeERC20 for IERC20;

    SlipstreamsExecutorExposed slipstreamsExposed;
    IERC20 DAI = IERC20(DAI_ADDR);

    function setUp() public {
        uint256 forkBlock = 38086214;
        vm.createSelectFork(vm.rpcUrl("base"), forkBlock);

        slipstreamsExposed = new SlipstreamsExecutorExposed();
    }

    function testDecodeParams() public view {
        int24 expectedTickSpacing = 100;
        bytes memory data = abi.encodePacked(
            BASE_WETH, BASE_USDC, expectedTickSpacing, address(3), false
        );

        (address target, bool zeroForOne) = slipstreamsExposed.decodeData(data);

        assertEq(target, address(3));
        assertEq(zeroForOne, false);
    }

    function testGetTransferData() public {
        bytes memory params = "";

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = slipstreamsExposed.getTransferData(params);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tokenIn, address(0));
        assertEq(tokenOut, address(0));
        assertEq(outputToRouter, false);
    }

    function testGetCallbackTransferData() public {
        uint24 poolTickSpacing = 100;
        uint256 amountOwed = 1000000000000000000;

        bytes memory protocolData =
            abi.encodePacked(BASE_WETH, BASE_USDC, poolTickSpacing);
        uint256 dataOffset = 3; // some offset
        uint256 dataLength = protocolData.length;

        bytes memory callbackData = abi.encodePacked(
            bytes4(0xfa461e33),
            int256(amountOwed), // amount0Delta
            int256(0), // amount1Delta
            dataOffset,
            dataLength,
            protocolData
        );
        (TransferManager.TransferType transferType, address receiver) = slipstreamsExposed.getCallbackTransferData(
            callbackData, BASE_WETH, address(this)
        );

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(this));
    }

    function testSwap() public {
        uint256 amountIn = 10 ** 18;
        deal(BASE_WETH, address(slipstreamsExposed), amountIn);

        bool zeroForOne = true;

        bytes memory data = abi.encodePacked(
            BASE_WETH,
            BASE_USDC,
            IUniswapV3Pool(SLIPSTREAMS_WETH_USDC_POOL).tickSpacing(),
            SLIPSTREAMS_WETH_USDC_POOL,
            zeroForOne
        );

        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(address(this));
        slipstreamsExposed.swap(amountIn, data, address(this));
        uint256 amountOut =
            IERC20(BASE_USDC).balanceOf(address(this)) - balanceBefore;

        assertEq(IERC20(BASE_WETH).balanceOf(address(slipstreamsExposed)), 0);
        assertGt(amountOut, 0);
    }

    function testSwapNewFactory() public {
        uint256 amountIn = 10 ** 18;
        deal(BASE_WETH, address(slipstreamsExposed), amountIn);

        bool zeroForOne = false;

        bytes memory data = abi.encodePacked(
            BASE_WETH,
            BASE_BMI,
            IUniswapV3Pool(SLIPSTREAMS_WETH_BMI_POOL).tickSpacing(),
            SLIPSTREAMS_WETH_BMI_POOL,
            zeroForOne
        );

        uint256 balanceBefore = IERC20(BASE_BMI).balanceOf(address(this));
        slipstreamsExposed.swap(amountIn, data, address(this));
        uint256 amountOut =
            IERC20(BASE_BMI).balanceOf(address(this)) - balanceBefore;

        assertEq(IERC20(BASE_WETH).balanceOf(address(slipstreamsExposed)), 0);
        assertGt(amountOut, 0);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(BASE_WETH, address(2), address(3));

        vm.expectRevert(SlipstreamsExecutor__InvalidDataLength.selector);
        slipstreamsExposed.decodeData(invalidParams);
    }

    function testSlipstreamsCallback() public {
        uint24 poolTickSpacing = 100;
        uint256 amountOwed = 1000000000000000000;
        deal(BASE_WETH, address(slipstreamsExposed), amountOwed);
        uint256 initialPoolReserve =
            IERC20(BASE_WETH).balanceOf(SLIPSTREAMS_WETH_USDC_POOL);

        bytes memory protocolData =
            abi.encodePacked(BASE_WETH, BASE_USDC, poolTickSpacing);
        uint256 dataOffset = 3; // some offset
        uint256 dataLength = protocolData.length;

        bytes memory callbackData = abi.encodePacked(
            bytes4(0xfa461e33),
            int256(amountOwed), // amount0Delta
            int256(0), // amount1Delta
            dataOffset,
            dataLength,
            protocolData
        );
        // transfer funds into the pool - this is taken cared of by the Dispatcher now
        vm.prank(address(slipstreamsExposed));
        IERC20(BASE_WETH).transfer(SLIPSTREAMS_WETH_USDC_POOL, amountOwed);
        vm.startPrank(SLIPSTREAMS_WETH_USDC_POOL);
        slipstreamsExposed.handleCallback(callbackData);
        vm.stopPrank();

        uint256 finalPoolReserve =
            IERC20(BASE_WETH).balanceOf(SLIPSTREAMS_WETH_USDC_POOL);
        assertEq(finalPoolReserve - initialPoolReserve, amountOwed);
    }
}

contract TychoRouterForSlipstreamsTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 37987780;
    }

    function testSingleSlipstreamsIntegration() public {
        deal(BASE_WETH, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_slipstreams");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(BASE_USDC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
    }

    function testSequentialSlipstreamsIntegration() public {
        deal(BASE_WETH, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_slipstreams"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(BASE_cbBTC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertGt(balanceAfter, balanceBefore);
    }
}
