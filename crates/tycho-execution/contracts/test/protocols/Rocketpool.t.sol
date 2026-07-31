pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {CommonBase} from "../../lib/forge-std/src/Base.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {
    RocketpoolExecutor,
    RocketpoolExecutor__InvalidDataLength,
    IRocketTokenRETH
} from "../../src/executors/RocketpoolExecutor.sol";
import {StdAssertions} from "../../lib/forge-std/src/StdAssertions.sol";
import {StdChains} from "../../lib/forge-std/src/StdChains.sol";
import {StdCheats, StdCheatsSafe} from "../../lib/forge-std/src/StdCheats.sol";
import {StdUtils} from "../../lib/forge-std/src/StdUtils.sol";
import {TestUtils} from "../TestUtils.sol";

contract RocketpoolExecutorExposed is RocketpoolExecutor {
    constructor(address _rocketDepositPool)
        RocketpoolExecutor(_rocketDepositPool)
    {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (bool isDeposit)
    {
        return _decodeData(data);
    }
}

contract RocketpoolExecutorTest is TestUtils, Constants {
    RocketpoolExecutorExposed rocketpoolExecutor;

    modifier setUpFork(uint256 blockNumber) {
        vm.createSelectFork(vm.rpcUrl("mainnet"), blockNumber);
        rocketpoolExecutor = new RocketpoolExecutorExposed(ROCKET_DEPOSIT_POOL);
        _;
    }

    function setUp() public setUpFork(24480104) {}

    function testDecodeParams() public view {
        bytes memory params = abi.encodePacked(
            uint8(1) // isDeposit = true
        );

        bool isDeposit = rocketpoolExecutor.decodeParams(params);

        assertTrue(isDeposit);
    }

    function testDecodeParamsBurn() public view {
        bytes memory params = abi.encodePacked(
            uint8(0) // isDeposit = false (burn)
        );

        bool isDeposit = rocketpoolExecutor.decodeParams(params);

        assertFalse(isDeposit);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams = abi.encodePacked(BOB);

        vm.expectRevert(RocketpoolExecutor__InvalidDataLength.selector);
        rocketpoolExecutor.decodeParams(invalidParams);
    }

    function testGetTransferData() public {
        bytes memory params = abi.encodePacked(
            uint8(1) // isDeposit = true
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = rocketpoolExecutor.getTransferData(params);

        // receiver is msg.sender (this test contract) since getTransferData
        // is called via staticcall in production (msg.sender = TychoRouter)
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, ETH_ADDR);
        assertEq(tokenOut, RETH_ADDR);
        assertEq(outputToRouter, true);
    }

    function testGetTransferDataBurn() public {
        bytes memory params = abi.encodePacked(
            uint8(0) // isDeposit = false (burn)
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = rocketpoolExecutor.getTransferData(params);

        // receiver is msg.sender (this test contract) since getTransferData
        // is called via staticcall in production (msg.sender = TychoRouter)
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, RETH_ADDR);
        assertEq(tokenOut, ETH_ADDR);
        assertEq(outputToRouter, true);
    }

    /// Test deposit
    /// Tx 0xe0f1db165b621cb1e50b629af9d47e064be464fbcc7f2bcba3df1d27dbb916be at block 24480105
    /// User deposited 85 ETH and received 73382345660413064855 rETH (0.05% fee)
    function testSwapDeposit() public setUpFork(24480104) {
        // ETH -> rETH
        IRocketTokenRETH RETH = IRocketTokenRETH(RETH_ADDR);
        uint256 amountIn = 85 ether;
        bytes memory protocolData = abi.encodePacked(
            uint8(1) // isDeposit = true
        );

        vm.deal(address(rocketpoolExecutor), amountIn);

        uint256 rethBalanceBefore = RETH.balanceOf(address(rocketpoolExecutor));
        rocketpoolExecutor.swap(amountIn, protocolData, BOB);
        uint256 rethBalanceAfter = RETH.balanceOf(address(rocketpoolExecutor));

        assertEq(
            rethBalanceAfter - rethBalanceBefore, 73_382_345_660_413_064_855
        );
    }

    /// Test burn
    /// Block 24481338: user burned 2515686112138065226 rETH and received 2912504376202664754 ETH
    function testSwapBurn() public setUpFork(24481337) {
        uint256 amountIn = 2_515_686_112_138_065_226;
        bytes memory protocolData = abi.encodePacked(
            uint8(0) // isDeposit = false (burn)
        );

        deal(RETH_ADDR, address(rocketpoolExecutor), amountIn);

        uint256 ethBalanceBefore = address(rocketpoolExecutor).balance;
        rocketpoolExecutor.swap(amountIn, protocolData, BOB);
        uint256 ethBalanceAfter = address(rocketpoolExecutor).balance;

        assertEq(ethBalanceAfter - ethBalanceBefore, 2_912_504_376_202_664_754);
    }

    function testDecodeDepositIntegration() public view {
        // Generated by the SwapEncoder - test_encode_rocketpool
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_rocketpool_deposit");

        bool isDeposit = rocketpoolExecutor.decodeParams(protocolData);

        assertTrue(isDeposit);
    }

    function testDecodeBurnIntegration() public view {
        // Generated by the SwapEncoder - test_encode_rocketpool
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_rocketpool_burn");

        bool isDeposit = rocketpoolExecutor.decodeParams(protocolData);

        assertFalse(isDeposit);
    }

    /// Integration test for deposit encoding (uses calldata generated by Rust SwapEncoder)
    /// Tx 0xe0f1db165b621cb1e50b629af9d47e064be464fbcc7f2bcba3df1d27dbb916be at block 24480105
    /// User deposited 85 ETH and received 73382345660413064855 rETH
    function testSwapDepositIntegration() public setUpFork(24480104) {
        // Generated by the SwapEncoder - test_encode_rocketpool_deposit
        IRocketTokenRETH RETH = IRocketTokenRETH(RETH_ADDR);
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_rocketpool_deposit");

        uint256 amountIn = 85 ether;

        // Fund the executor with ETH
        vm.deal(address(rocketpoolExecutor), amountIn);

        uint256 rethBalanceBefore = RETH.balanceOf(address(rocketpoolExecutor));
        rocketpoolExecutor.swap(amountIn, protocolData, BOB);
        uint256 rethBalanceAfter = RETH.balanceOf(address(rocketpoolExecutor));

        assertEq(
            rethBalanceAfter - rethBalanceBefore, 73_382_345_660_413_064_855
        );
    }

    /// Integration test for burn encoding (uses calldata generated by Rust SwapEncoder)
    function testSwapBurnIntegration() public setUpFork(24481337) {
        // Generated by the SwapEncoder - test_encode_rocketpool_burn
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_rocketpool_burn");

        uint256 amountIn = 1 ether;

        // Fund the executor with rETH
        deal(RETH_ADDR, address(rocketpoolExecutor), amountIn);

        uint256 ethBalanceBefore = address(rocketpoolExecutor).balance;
        rocketpoolExecutor.swap(amountIn, protocolData, BOB);
        uint256 ethBalanceAfter = address(rocketpoolExecutor).balance;

        // Check balances
        assertEq(ethBalanceAfter - ethBalanceBefore, 1157737589816937166);
    }
}

// Rocketpool deposit and burn tests are separated into different contracts because they
// require different fork blocks to test against real historical transactions.

/// @notice Tests Rocketpool deposit (ETH -> rETH) via TychoRouter
/// Tx 0xe0f1db165b621cb1e50b629af9d47e064be464fbcc7f2bcba3df1d27dbb916be at block 24480105contract
contract RocketpoolDepositTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 24480104;
    }

    function testSingleSwap() public {
        IRocketTokenRETH RETH = IRocketTokenRETH(RETH_ADDR);

        uint256 amountIn = 85 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_rocketpool_deposit"
        );

        // Fund ALICE with ETH to send with the call
        vm.deal(ALICE, amountIn);

        vm.startPrank(ALICE);

        uint256 rethBalanceBefore = RETH.balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call{value: amountIn}(callData);
        uint256 rethBalanceAfter = RETH.balanceOf(ALICE);

        // Check balances
        assertTrue(success, "Call Failed");
        assertEq(
            rethBalanceAfter - rethBalanceBefore, 73_382_345_660_413_064_855
        );
        assertEq(RETH.balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }
}

/// @notice Tests Rocketpool burn (rETH -> ETH) via TychoRouter
/// Block 24481338: user burned 2515686112138065226
contract RocketpoolBurnTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 24481337;
    }

    function testSingleSwap() public {
        IRocketTokenRETH RETH = IRocketTokenRETH(RETH_ADDR);

        uint256 amountIn = 2_515_686_112_138_065_226 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_rocketpool_burn"
        );

        // Fund BOB with rETH to send with the call
        deal(RETH_ADDR, BOB, amountIn);

        vm.startPrank(BOB);

        RETH.approve(tychoRouterAddr, type(uint256).max);

        uint256 ethBalanceBefore = BOB.balance;
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 ethBalanceAfter = BOB.balance;

        // Check balances
        assertTrue(success, "Call Failed");
        assertEq(ethBalanceAfter - ethBalanceBefore, 2_912_504_376_202_664_754);
        assertEq(RETH.balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSingleSwapBurnNoApproval() public {
        /// Verifies that burning rETH via TychoRouter does not emit any
        /// Approval event from the RETH token, since we are interacting directly
        /// with the token contract.
        IRocketTokenRETH RETH = IRocketTokenRETH(RETH_ADDR);

        uint256 amountIn = 2_515_686_112_138_065_226 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_rocketpool_burn"
        );

        deal(RETH_ADDR, BOB, amountIn);

        vm.startPrank(BOB);
        RETH.approve(tychoRouterAddr, type(uint256).max);

        // Record all logs during the swap to check for Approval events
        vm.recordLogs();
        (bool success,) = tychoRouterAddr.call(callData);
        assertTrue(success, "Call Failed");
        vm.stopPrank();

        // Search recorded logs for any Approval event emitted by the RETH token
        // where the router is the owner (i.e. the router granting an approval).
        bytes32 approvalTopic = keccak256("Approval(address,address,uint256)");
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (
                logs[i].emitter == RETH_ADDR && logs[i].topics.length > 1
                    && logs[i].topics[0] == approvalTopic
                    && logs[i].topics[1]
                        == bytes32(uint256(uint160(tychoRouterAddr)))
            ) {
                revert("Router should not approve any spender for rETH burn");
            }
        }
    }
}
