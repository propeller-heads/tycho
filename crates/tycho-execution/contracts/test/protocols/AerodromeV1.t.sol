pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {TestUtils} from "../TestUtils.sol";
import {
    AerodromeV1Executor,
    AerodromeV1Executor__InvalidDataLength
} from "@src/executors/AerodromeV1Executor.sol";
import {IAerodromeV1Pool} from "@interfaces/IAerodromeV1Pool.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

abstract contract AerodromeV1TestBase {
    address internal constant AERODROME_V1_VOLATILE_POOL =
        0x723AEf6543aecE026a15662Be4D3fb3424D502A9;
    address internal constant AERODROME_V1_STABLE_POOL =
        0x0B25c51637c43decd6CC1C1e3da4518D54ddb528;
    address internal constant AERODROME_V1_TBTC =
        0x236aa50979D5f3De3Bd1Eeb40E81137F22ab794b;
    address internal constant AERODROME_V1_USDBC =
        0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA;
    address internal constant AERODROME_V1_DOLA =
        0x4621b7A9c75199271F773Ebd9A499dbd165c3191;
}

contract AerodromeV1ExecutorExposed is AerodromeV1Executor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            address target,
            address tokenIn,
            address tokenOut,
            bool zeroForOne
        )
    {
        return _decodeData(data);
    }
}

contract AerodromeV1ExecutorTest is Constants, TestUtils, AerodromeV1TestBase {
    AerodromeV1ExecutorExposed aerodromeV1Exposed;
    IERC20 token0 = IERC20(AERODROME_V1_TBTC);
    IERC20 token1 = IERC20(AERODROME_V1_USDBC);

    function setUp() public {
        uint256 forkBlock = 44682102;
        vm.createSelectFork(vm.rpcUrl("base"), forkBlock);

        aerodromeV1Exposed = new AerodromeV1ExecutorExposed();
    }

    function testDecodeParams() public view {
        bytes memory params = abi.encodePacked(
            AERODROME_V1_VOLATILE_POOL,
            AERODROME_V1_TBTC,
            AERODROME_V1_USDBC,
            true
        );

        (address target, address tokenIn, address tokenOut, bool zeroForOne) =
            aerodromeV1Exposed.decodeParams(params);

        assertEq(target, AERODROME_V1_VOLATILE_POOL);
        assertEq(tokenIn, AERODROME_V1_TBTC);
        assertEq(tokenOut, AERODROME_V1_USDBC);
        assertEq(zeroForOne, true);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(AERODROME_V1_VOLATILE_POOL, AERODROME_V1_TBTC);

        vm.expectRevert(AerodromeV1Executor__InvalidDataLength.selector);
        aerodromeV1Exposed.decodeParams(invalidParams);
    }

    function testGetTransferData() public {
        bytes memory params = abi.encodePacked(
            AERODROME_V1_VOLATILE_POOL,
            AERODROME_V1_TBTC,
            AERODROME_V1_USDBC,
            true
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = aerodromeV1Exposed.getTransferData(params);

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, AERODROME_V1_VOLATILE_POOL);
        assertEq(tokenIn, AERODROME_V1_TBTC);
        assertEq(tokenOut, AERODROME_V1_USDBC);
        assertEq(outputToRouter, false);
    }

    function testFundsExpectedAddress() public view {
        bytes memory params = abi.encodePacked(
            AERODROME_V1_VOLATILE_POOL,
            AERODROME_V1_TBTC,
            AERODROME_V1_USDBC,
            true
        );
        address receiver = aerodromeV1Exposed.fundsExpectedAddress(params);
        assertEq(receiver, AERODROME_V1_VOLATILE_POOL);
    }

    function testSwapZeroForOne() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_TBTC);
        bytes memory protocolData = abi.encodePacked(
            AERODROME_V1_VOLATILE_POOL,
            AERODROME_V1_TBTC,
            AERODROME_V1_USDBC,
            true
        );

        deal(AERODROME_V1_TBTC, address(aerodromeV1Exposed), amountIn);
        vm.prank(address(aerodromeV1Exposed));
        token0.transfer(AERODROME_V1_VOLATILE_POOL, amountIn);

        uint256 balanceBefore = token1.balanceOf(BOB);
        aerodromeV1Exposed.swap(amountIn, protocolData, BOB);
        uint256 balanceAfter = token1.balanceOf(BOB);

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(token0.balanceOf(address(aerodromeV1Exposed)), 0);
    }

    function testSwapOneForZero() public {
        uint256 amountIn = 10e6;
        uint256 expectedAmountOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_USDBC);
        bytes memory protocolData = abi.encodePacked(
            AERODROME_V1_VOLATILE_POOL,
            AERODROME_V1_USDBC,
            AERODROME_V1_TBTC,
            false
        );

        deal(AERODROME_V1_USDBC, address(aerodromeV1Exposed), amountIn);
        vm.prank(address(aerodromeV1Exposed));
        token1.transfer(AERODROME_V1_VOLATILE_POOL, amountIn);

        uint256 balanceBefore = token0.balanceOf(BOB);
        aerodromeV1Exposed.swap(amountIn, protocolData, BOB);
        uint256 balanceAfter = token0.balanceOf(BOB);

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(token1.balanceOf(address(aerodromeV1Exposed)), 0);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_aerodrome_v1");

        (address target, address tokenIn, address tokenOut, bool zeroForOne) =
            aerodromeV1Exposed.decodeParams(protocolData);

        assertEq(target, AERODROME_V1_VOLATILE_POOL);
        assertEq(tokenIn, AERODROME_V1_TBTC);
        assertEq(tokenOut, AERODROME_V1_USDBC);
        assertEq(zeroForOne, true);
    }

    function testSwapIntegration() public {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_aerodrome_v1");
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_TBTC);

        deal(AERODROME_V1_TBTC, address(aerodromeV1Exposed), amountIn);
        vm.prank(address(aerodromeV1Exposed));
        token0.transfer(AERODROME_V1_VOLATILE_POOL, amountIn);

        uint256 balanceBefore = token1.balanceOf(BOB);
        aerodromeV1Exposed.swap(amountIn, protocolData, BOB);
        uint256 balanceAfter = token1.balanceOf(BOB);

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(token0.balanceOf(address(aerodromeV1Exposed)), 0);
    }

    function testDecodeParamsOneForZero() public view {
        bytes memory params = abi.encodePacked(
            AERODROME_V1_VOLATILE_POOL,
            AERODROME_V1_USDBC,
            AERODROME_V1_TBTC,
            false
        );

        (address target, address tokenIn, address tokenOut, bool zeroForOne) =
            aerodromeV1Exposed.decodeParams(params);

        assertEq(target, AERODROME_V1_VOLATILE_POOL);
        assertEq(tokenIn, AERODROME_V1_USDBC);
        assertEq(tokenOut, AERODROME_V1_TBTC);
        assertEq(zeroForOne, false);
    }
}

contract TychoRouterForAerodromeV1Test is
    TychoRouterTestSetup,
    AerodromeV1TestBase
{
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 44682102;
    }

    function testSingleAerodromeV1Integration() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_TBTC);
        deal(AERODROME_V1_TBTC, ALICE, amountIn);
        uint256 balanceBefore = IERC20(AERODROME_V1_USDBC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(AERODROME_V1_TBTC).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_aerodrome_v1");
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 balanceAfter = IERC20(AERODROME_V1_USDBC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(AERODROME_V1_TBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
    }

    function testSequentialAerodromeV1Integration() public {
        uint256 amountIn = 10 ether;
        uint256 intermediateAmountOut = IAerodromeV1Pool(
                AERODROME_V1_STABLE_POOL
            ).getAmountOut(amountIn, AERODROME_V1_DOLA);
        uint256 expectedAmountOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(intermediateAmountOut, AERODROME_V1_USDBC);
        deal(AERODROME_V1_DOLA, ALICE, amountIn);
        uint256 balanceBefore = IERC20(AERODROME_V1_TBTC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(AERODROME_V1_DOLA).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_aerodrome_v1"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 balanceAfter = IERC20(AERODROME_V1_TBTC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(AERODROME_V1_DOLA).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
    }
}
