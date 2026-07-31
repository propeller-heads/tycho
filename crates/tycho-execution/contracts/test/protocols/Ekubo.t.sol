pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/EkuboExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {Constants} from "../Constants.sol";
import {ICore} from "@ekubo/interfaces/ICore.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {console} from "forge-std/Test.sol";

contract EkuboExecutorTest is Constants, TestUtils {
    address constant EXECUTOR_ADDRESS =
        0xcA4F73Fe97D0B987a0D12B39BBD562c779BAb6f6; // Same address as in swap_encoder.rs tests
    EkuboExecutor executor;

    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 USDT = IERC20(USDT_ADDR);

    // payCallback(uint256,address) selector
    bytes4 constant PAY_CALLBACK_SELECTOR = 0x599d0714;
    // locked(uint256) selector
    bytes4 constant LOCKED_SELECTOR = 0xb45a3c0e;

    address constant CORE_ADDRESS = 0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444;
    address constant MEV_RESIST_ADDRESS =
        0x553a2EFc570c9e104942cEC6aC1c18118e54C091;

    bytes32 constant ORACLE_CONFIG =
        0x51d02a5948496a67827242eabc5725531342527c000000000000000000000000;

    // 0.01% fee and 0.02% tick spacing
    bytes32 constant MEV_RESIST_POOL_CONFIG =
        0x553a2EFc570c9e104942cEC6aC1c18118e54C09100068db8bac710cb000000c8;

    modifier setUpFork(uint256 blockNumber) {
        vm.createSelectFork(vm.rpcUrl("mainnet"), blockNumber);

        deployCodeTo(
            "executors/EkuboExecutor.sol",
            abi.encode(CORE_ADDRESS, MEV_RESIST_ADDRESS),
            EXECUTOR_ADDRESS
        );
        executor = EkuboExecutor(payable(EXECUTOR_ADDRESS));
        _;
    }

    function testSingleSwapEth() public setUpFork(22722989) {
        uint256 amountIn = 1 ether;

        deal(address(executor), amountIn);

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        uint256 usdcBalanceBeforeCore = USDC.balanceOf(CORE_ADDRESS);
        uint256 usdcBalanceBeforeExecutor = USDC.balanceOf(address(executor));

        bytes memory data = abi.encodePacked(
            ETH_ADDRESS, // tokenIn
            USDC_ADDR, // tokenOut
            ORACLE_CONFIG // poolConfig
        );

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut =
            USDC.balanceOf(address(executor)) - usdcBalanceBeforeExecutor;

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore + amountIn);
        assertEq(address(executor).balance, ethBalanceBeforeExecutor - amountIn);

        assertEq(
            USDC.balanceOf(CORE_ADDRESS), usdcBalanceBeforeCore - amountOut
        );
    }

    // Expects input that encodes the same test case as swap_encoder::tests::ekubo::test_encode_swap_multi
    function multiHopSwap(bytes memory data) internal {
        uint256 amountIn = 1 ether;

        deal(address(executor), amountIn);

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        uint256 usdtBalanceBeforeCore = USDT.balanceOf(CORE_ADDRESS);
        uint256 usdtBalanceBeforeExecutor = USDT.balanceOf(address(executor));

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut =
            USDT.balanceOf(address(executor)) - usdtBalanceBeforeExecutor;

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore + amountIn);
        assertEq(address(executor).balance, ethBalanceBeforeExecutor - amountIn);

        assertEq(
            USDT.balanceOf(CORE_ADDRESS), usdtBalanceBeforeCore - amountOut
        );
    }

    function testGetCallbackTransferDataPayCallback() internal {
        bytes memory data = abi.encodePacked(PAY_CALLBACK_SELECTOR);

        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData(data, USDC_ADDR, address(this));

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, CORE_ADDRESS);
    }

    function testGetCallbackTransferDataLockedERC20() internal {
        bytes memory data = abi.encodePacked(LOCKED_SELECTOR);

        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData(data, USDC_ADDR, address(this));

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
    }

    function testGetCallbackTransferDataLockedNative() internal {
        bytes memory data = abi.encodePacked(LOCKED_SELECTOR);

        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData(data, ETH_ADDRESS, address(this));

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
    }

    // Same test case as in swap_encoder::tests::ekubo::test_encode_swap_multi
    function testMultiHopSwap() public setUpFork(22082754) {
        bytes memory data = abi.encodePacked(
            ETH_ADDRESS, // tokenIn
            USDC_ADDR, // tokenOut of 1st swap
            ORACLE_CONFIG, // config of 1st swap
            USDT_ADDR, // tokenOut of 2nd swap
            bytes32(
                0x00000000000000000000000000000000000000000001a36e2eb1c43200000032
            ) // config of 2nd swap (0.0025% fee & 0.005% base pool)
        );
        multiHopSwap(data);
    }

    // Data is generated by test case in swap_encoder::tests::ekubo::test_encode_swap_multi
    function testMultiHopSwapIntegration() public setUpFork(22082754) {
        multiHopSwap(loadCallDataFromFile("test_ekubo_encode_swap_multi"));
    }
}

contract TychoRouterForEkuboTest is TychoRouterTestSetup {
    function getForkBlock() public view virtual override returns (uint256) {
        return 23518049;
    }

    function testSingleEkuboIntegration() public {
        // ETH -> USDC
        vm.stopPrank();

        deal(ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_ekubo");
        (bool success,) = tychoRouterAddr.call{value: 1 ether}(callData);

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGe(balanceAfter - balanceBefore, 26173932);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSingleEkuboIntegrationERC20() public {
        // USDC -> ETH
        vm.stopPrank();

        deal(USDC_ADDR, ALICE, 1_000_000_000);
        uint256 balanceBefore = ALICE.balance;

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_ekubo_erc20");
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = ALICE.balance;

        assertTrue(success, "Call Failed");
        assertGe(balanceAfter - balanceBefore, 214000721769214133);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testTwoEkuboIntegration() public {
        // Test multi-hop Ekubo swaps (grouped swap)
        //
        // USDE ──(EKUBO)──> USDC ──(EKUBO)──> USDT
        //
        deal(USDE_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDT_ADDR).balanceOf(ALICE);

        // Approve
        vm.startPrank(ALICE);
        IERC20(USDE_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_ekubo_grouped_swap");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDT_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, 999804);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}

contract TychoRouterForEkuboMevTest is TychoRouterTestSetup {
    function getForkBlock() public view virtual override returns (uint256) {
        return 22722989;
    }

    function testSingleEkuboIntegrationMevResist() public {
        // USDC -> ETH
        vm.stopPrank();

        deal(USDC_ADDR, ALICE, 1_000_000_000);
        uint256 balanceBefore = ALICE.balance;

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_ekubo_mev_resist"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = ALICE.balance;

        assertTrue(success, "Call Failed");
        assertGe(balanceAfter - balanceBefore, 26173932);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
