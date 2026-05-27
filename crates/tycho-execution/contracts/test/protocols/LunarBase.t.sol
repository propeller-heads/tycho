pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    ILunarBasePool,
    LunarBaseExecutor,
    LunarBaseExecutor__InvalidDataLength,
    LunarBaseExecutor__MsgValueMismatch
} from "@src/executors/LunarBaseExecutor.sol";

contract LunarBaseExecutorExposed is LunarBaseExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 amountOutMinimum
        )
    {
        return _decodeData(data);
    }
}

interface ILunarBaseQuoter {
    function quoteExactIn(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut);
}

contract LunarBaseExecutorTest is Constants, TestUtils {
    address internal constant LUNARBASE_POOL =
        0x0000eFC4ec03a7c47D3a38A9Be7Ff1d52dD01b99;
    address internal constant NATIVE_TOKEN =
        0x0000000000000000000000000000000000000000;

    LunarBaseExecutorExposed lunarBaseExecutor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), 46498514);
        lunarBaseExecutor = new LunarBaseExecutorExposed();
    }

    function testDecodeParams() public view {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN, BASE_USDC);

        (
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 amountOutMinimum
        ) = lunarBaseExecutor.decodeParams(params);

        assertEq(pool, LUNARBASE_POOL);
        assertEq(tokenIn, NATIVE_TOKEN);
        assertEq(tokenOut, BASE_USDC);
        assertEq(amountOutMinimum, 0);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN);

        vm.expectRevert(LunarBaseExecutor__InvalidDataLength.selector);
        lunarBaseExecutor.decodeParams(invalidParams);
    }

    function testGetTransferDataNativeInput() public {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN, BASE_USDC);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lunarBaseExecutor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
        assertEq(tokenIn, NATIVE_TOKEN);
        assertEq(tokenOut, BASE_USDC);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataErc20Input() public {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, NATIVE_TOKEN);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lunarBaseExecutor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, LUNARBASE_POOL);
        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, NATIVE_TOKEN);
        assertEq(outputToRouter, false);
    }

    function testFundsExpectedAddressNativeInput() public view {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN, BASE_USDC);

        assertEq(
            lunarBaseExecutor.fundsExpectedAddress(params),
            address(lunarBaseExecutor)
        );
    }

    function testFundsExpectedAddressErc20Input() public view {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, NATIVE_TOKEN);

        assertEq(lunarBaseExecutor.fundsExpectedAddress(params), address(this));
    }

    function testSwapNativeInputRevertsOnMsgValueMismatch() public {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN, BASE_USDC);

        vm.expectRevert(LunarBaseExecutor__MsgValueMismatch.selector);
        lunarBaseExecutor.swap{value: 0.005 ether}(0.01 ether, params, BOB);
    }

    function testSwapNativeInput() public {
        uint256 amountIn = 0.01 ether;
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN, BASE_USDC);
        uint256 expectedAmountOut = ILunarBaseQuoter(LUNARBASE_POOL)
            .quoteExactIn(NATIVE_TOKEN, BASE_USDC, amountIn);

        assertGt(expectedAmountOut, 0);
        vm.deal(address(this), amountIn);

        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);
        lunarBaseExecutor.swap{value: amountIn}(amountIn, params, BOB);
        uint256 balanceAfter = IERC20(BASE_USDC).balanceOf(BOB);

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
    }

    function testSwapErc20Input() public {
        uint256 amountIn = 20e6;
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, NATIVE_TOKEN);
        uint256 expectedAmountOut = ILunarBaseQuoter(LUNARBASE_POOL)
            .quoteExactIn(BASE_USDC, NATIVE_TOKEN, amountIn);

        assertGt(expectedAmountOut, 0);
        deal(BASE_USDC, address(lunarBaseExecutor), amountIn);
        vm.prank(address(lunarBaseExecutor));
        IERC20(BASE_USDC).approve(LUNARBASE_POOL, amountIn);

        uint256 balanceBefore = BOB.balance;
        lunarBaseExecutor.swap(amountIn, params, BOB);
        uint256 balanceAfter = BOB.balance;

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(BASE_USDC).balanceOf(address(lunarBaseExecutor)), 0);
    }
}
