pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/EkuboV3Executor.sol";
import {ILocker} from "@ekubo-v3/interfaces/IFlashAccountant.sol";
import {Constants} from "../Constants.sol";
import {SafeTransferLib} from "@solady/utils/SafeTransferLib.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

// Handles callbacks directly and receives the native token directly
contract EkuboV3ExecutorStandalone is EkuboV3Executor, ILocker {
    constructor() EkuboV3Executor() {}

    function locked_6416899205(
        uint256 /* id */
    )
        external
    {
        // swapData layout in msg.data: selector(4) | id(32) | amountIn(16) | receiver(20) | tokenIn(20) | ...
        // tokenIn starts at byte 72 (4 + 32 + 16 + 20 = 72)
        // Swap data already uses ETH_ADDRESS for native ETH;
        // getTransferData returns it as-is.
        address tokenIn = address(bytes20(msg.data[72:92]));

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (tokenIn == ETH_ADDRESS) {
            assert(
                transferType
                    == TransferManager.TransferType.TransferNativeInExecutor
            );
        } else {
            assert(transferType == TransferManager.TransferType.Transfer);
        }
        uint256 amount = uint128(bytes16(msg.data[36:52]));

        if (tokenIn != ETH_ADDRESS) {
            IERC20(tokenIn).transfer(receiver, amount);
        }
        bytes memory res = handleCallback(msg.data);
        assembly ("memory-safe") {
            return(add(res, 32), mload(res))
        }
    }

    // To receive withdrawals from Core
    receive() external payable {}
}

contract EkuboV3ExecutorTest is Constants, TestUtils {
    EkuboV3ExecutorStandalone immutable executor =
        new EkuboV3ExecutorStandalone();

    IERC20 USDC = IERC20(USDC_ADDR);
    IERC20 USDT = IERC20(USDT_ADDR);

    bytes32 constant ORACLE_CONFIG =
        0x517E506700271AEa091b02f42756F5E174Af5230000000000000000000000000;

    constructor() {
        vm.makePersistent(address(executor));
    }

    modifier setUpFork(uint256 blockNumber) {
        vm.createSelectFork(vm.rpcUrl("mainnet"), blockNumber);
        _;
    }

    function testSingleSwapEth() public setUpFork(24218590) {
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

    function testSingleSwapERC20() public setUpFork(24218590) {
        uint256 amountIn = 1_000_000_000;

        deal(USDC_ADDR, address(executor), amountIn);

        uint256 usdcBalanceBeforeCore = USDC.balanceOf(CORE_ADDRESS);
        uint256 usdcBalanceBeforeExecutor = USDC.balanceOf(address(executor));

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        bytes memory data = abi.encodePacked(
            USDC_ADDR, // tokenIn
            ETH_ADDRESS, // tokenOut
            ORACLE_CONFIG // config
        );

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut = address(executor).balance - ethBalanceBeforeExecutor;

        assertEq(USDC.balanceOf(CORE_ADDRESS), usdcBalanceBeforeCore + amountIn);
        assertEq(
            USDC.balanceOf(address(executor)),
            usdcBalanceBeforeExecutor - amountIn
        );

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore - amountOut);
    }

    function testMevCapture() public setUpFork(24198199) {
        uint256 amountIn = 1_000;

        deal(USDC_ADDR, address(executor), amountIn);

        uint256 usdcBalanceBeforeCore = USDC.balanceOf(CORE_ADDRESS);
        uint256 usdcBalanceBeforeExecutor = USDC.balanceOf(address(executor));

        uint256 usdtBalanceBeforeCore = USDT.balanceOf(CORE_ADDRESS);
        uint256 usdtBalanceBeforeExecutor = USDT.balanceOf(address(executor));

        bytes memory data = abi.encodePacked(
            USDC_ADDR, // tokenIn
            USDT_ADDR, // tokenOut
            bytes32(
                0x5555ff9ff2757500bf4ee020dcfd0210cffa41be000053e2d6238da480000032
            ) // config (0.0005% fee and 0.005% tick spacing, mev capture ext)
        );

        executor.swap(amountIn, data, address(executor));

        uint256 amountOut =
            USDT.balanceOf(address(executor)) - usdtBalanceBeforeExecutor;

        assertEq(USDC.balanceOf(CORE_ADDRESS), usdcBalanceBeforeCore + amountIn);
        assertEq(
            USDC.balanceOf(address(executor)),
            usdcBalanceBeforeExecutor - amountIn
        );

        assertEq(
            USDT.balanceOf(CORE_ADDRESS), usdtBalanceBeforeCore - amountOut
        );
    }

    // Data is generated by test case in swap_encoder::tests::ekubo_v3::test_encode_swap_multi
    function testMultiHopSwapIntegration() public setUpFork(24218590) {
        uint256 amountIn = 1 ether;
        deal(address(executor), amountIn);

        uint256 ethBalanceBeforeCore = CORE_ADDRESS.balance;
        uint256 ethBalanceBeforeExecutor = address(executor).balance;

        uint256 usdtBalanceBeforeCore = USDT.balanceOf(CORE_ADDRESS);
        uint256 usdtBalanceBeforeAlice = USDT.balanceOf(ALICE);

        executor.swap(
            amountIn,
            loadCallDataFromFile("test_ekubo_v3_encode_swap_multi"),
            ALICE
        );

        uint256 amountOut = USDT.balanceOf(ALICE) - usdtBalanceBeforeAlice;

        assertEq(CORE_ADDRESS.balance, ethBalanceBeforeCore + amountIn);
        assertEq(address(executor).balance, ethBalanceBeforeExecutor - amountIn);

        assertEq(
            USDT.balanceOf(CORE_ADDRESS), usdtBalanceBeforeCore - amountOut
        );
    }
}

contract TychoRouterForEkuboV3Test is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 24218590;
    }

    function setUp() public virtual override {
        super.setUp();

        // Remove delegations
        vm.etch(ALICE, "");
    }

    function testSingleEkuboV3Integration() public {
        deal(ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        (bool success,) = tychoRouterAddr.call{value: 1 ether}(
            loadCallDataFromFile("test_single_encoding_strategy_ekubo_v3")
        );

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGe(balanceAfter - balanceBefore, 26173932);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testTwoEkuboV3Integration() public {
        // Test multi-hop Ekubo V3 swaps (grouped swap)
        //
        // USDT ──(EKUBO V3)──> USDC ──(EKUBO V3)──> ETH
        //
        deal(USDT_ADDR, ALICE, 10_000_000_000);
        uint256 balanceBefore = ALICE.balance;

        vm.startPrank(ALICE);
        SafeTransferLib.safeApprove(
            USDT_ADDR, tychoRouterAddr, type(uint256).max
        );

        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_single_ekubo_v3_grouped_swap")
        );
        assertTrue(success, "call failed");

        assertEq(ALICE.balance - balanceBefore, 2500939754680596105);
        assertEq(IERC20(USDT_ADDR).balanceOf(ALICE), 0);
    }
}
