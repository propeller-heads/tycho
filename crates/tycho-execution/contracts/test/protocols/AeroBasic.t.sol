pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {TestUtils} from "../TestUtils.sol";
import {AeroBasicExecutor} from "@src/executors/AeroBasicExecutor.sol";
import {AerodromeV1Executor} from "@src/executors/AerodromeV1Executor.sol";
import {IAeroBasicPool} from "@interfaces/IAeroBasicPool.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

abstract contract AeroBasicTestBase {
    /// Base block with AERO basic pools live; the aero_basic simulation fixture uses it too.
    uint256 internal constant AERO_BASIC_FORK_BLOCK = 53096561;
    address internal constant AERO_WETH_USDC_VOLATILE_POOL =
        0x800EB395B577B234C5065c8E0cCb0F3A1401B2c2;
    address internal constant AERO_DOLA_USDC_STABLE_POOL =
        0x9bfc7e23feEFEe8cf08bA544E7cD5C28C79f1679;
    address internal constant AERO_WETH =
        0x4200000000000000000000000000000000000006;
    address internal constant AERO_USDC =
        0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address internal constant AERO_DOLA =
        0x4621b7A9c75199271F773Ebd9A499dbd165c3191;
}

contract AeroBasicExecutorTest is Constants, TestUtils, AeroBasicTestBase {
    AeroBasicExecutor aeroBasicExecutor;
    IAeroBasicPool pool = IAeroBasicPool(AERO_WETH_USDC_VOLATILE_POOL);

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), AERO_BASIC_FORK_BLOCK);
        aeroBasicExecutor = new AeroBasicExecutor();
    }

    function _wethToUsdc() internal pure returns (bytes memory) {
        return abi.encodePacked(
            AERO_WETH_USDC_VOLATILE_POOL, AERO_WETH, AERO_USDC, true
        );
    }

    function _sendToPool(address executor, address token, uint256 amount)
        internal
    {
        deal(token, executor, amount);
        vm.prank(executor);
        assertTrue(IERC20(token).transfer(AERO_WETH_USDC_VOLATILE_POOL, amount));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = aeroBasicExecutor.getTransferData(_wethToUsdc());

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, AERO_WETH_USDC_VOLATILE_POOL);
        assertEq(tokenIn, AERO_WETH);
        assertEq(tokenOut, AERO_USDC);
        assertEq(outputToRouter, false);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            aeroBasicExecutor.fundsExpectedAddress(_wethToUsdc()),
            AERO_WETH_USDC_VOLATILE_POOL
        );
    }

    /// At an ordinary priority fee the factory charges no MEV tax, so the output is the
    /// base-fee `getAmountOut` that the aero_basic simulation quotes.
    function testSwapZeroForOne() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = pool.getAmountOut(amountIn, AERO_WETH);
        _sendToPool(address(aeroBasicExecutor), AERO_WETH, amountIn);

        uint256 balanceBefore = IERC20(AERO_USDC).balanceOf(BOB);
        aeroBasicExecutor.swap(amountIn, _wethToUsdc(), BOB);

        assertEq(
            IERC20(AERO_USDC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
        assertEq(IERC20(AERO_WETH).balanceOf(address(aeroBasicExecutor)), 0);
    }

    function testSwapOneForZero() public {
        uint256 amountIn = 50e6;
        uint256 expectedAmountOut = pool.getAmountOut(amountIn, AERO_USDC);
        bytes memory protocolData = abi.encodePacked(
            AERO_WETH_USDC_VOLATILE_POOL, AERO_USDC, AERO_WETH, false
        );
        _sendToPool(address(aeroBasicExecutor), AERO_USDC, amountIn);

        uint256 balanceBefore = IERC20(AERO_WETH).balanceOf(BOB);
        aeroBasicExecutor.swap(amountIn, protocolData, BOB);

        assertEq(
            IERC20(AERO_WETH).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
    }

    /// A priority fee above the volatile MEV tax module's threshold (1 gwei on Base) makes the
    /// factory add a MEV tax. The AERO executor prices it in; pricing with `getAmountOut`, as the
    /// Aerodrome V1 executor does, asks the pool for too much and reverts.
    function testSwapPaysTheMevTaxAtAHighPriorityFee() public {
        vm.fee(0.01 gwei);
        vm.txGasPrice(0.01 gwei + 2 gwei);
        uint256 amountIn = 0.01 ether;
        uint256 withoutTax = pool.getAmountOut(amountIn, AERO_WETH);
        uint256 expectedAmountOut =
            pool.getAmountOutWithTotalFee(amountIn, AERO_WETH);
        assertLt(expectedAmountOut, withoutTax);

        AerodromeV1Executor aerodromeV1Executor = new AerodromeV1Executor();
        _sendToPool(address(aerodromeV1Executor), AERO_WETH, amountIn);
        vm.expectRevert();
        aerodromeV1Executor.swap(amountIn, _wethToUsdc(), BOB);

        _sendToPool(address(aeroBasicExecutor), AERO_WETH, amountIn);
        uint256 balanceBefore = IERC20(AERO_USDC).balanceOf(BOB);
        aeroBasicExecutor.swap(amountIn, _wethToUsdc(), BOB);

        assertEq(
            IERC20(AERO_USDC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
    }

    function testSwapIntegration() public {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_aero_basic");
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = pool.getAmountOut(amountIn, AERO_WETH);
        _sendToPool(address(aeroBasicExecutor), AERO_WETH, amountIn);

        uint256 balanceBefore = IERC20(AERO_USDC).balanceOf(BOB);
        aeroBasicExecutor.swap(amountIn, protocolData, BOB);

        assertEq(
            IERC20(AERO_USDC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
    }
}

contract TychoRouterForAeroBasicTest is
    TychoRouterTestSetup,
    AeroBasicTestBase
{
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return AERO_BASIC_FORK_BLOCK;
    }

    function testSingleAeroBasicIntegration() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = IAeroBasicPool(AERO_WETH_USDC_VOLATILE_POOL)
            .getAmountOut(amountIn, AERO_WETH);
        deal(AERO_WETH, ALICE, amountIn);
        uint256 balanceBefore = IERC20(AERO_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(AERO_WETH).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_aero_basic");
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(AERO_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(AERO_USDC).balanceOf(ALICE) - balanceBefore,
            expectedAmountOut
        );
    }

    /// DOLA -> USDC on a stable pool with a 30 bps custom fee, then USDC -> WETH on a volatile
    /// pool at the 30 bps default.
    function testSequentialAeroBasicIntegration() public {
        uint256 amountIn = 100 ether;
        uint256 intermediateAmountOut = IAeroBasicPool(
                AERO_DOLA_USDC_STABLE_POOL
            ).getAmountOut(amountIn, AERO_DOLA);
        uint256 expectedAmountOut = IAeroBasicPool(AERO_WETH_USDC_VOLATILE_POOL)
            .getAmountOut(intermediateAmountOut, AERO_USDC);
        deal(AERO_DOLA, ALICE, amountIn);
        uint256 balanceBefore = IERC20(AERO_WETH).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(AERO_DOLA).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_aero_basic"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(AERO_DOLA).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(AERO_WETH).balanceOf(ALICE) - balanceBefore,
            expectedAmountOut
        );
    }
}
