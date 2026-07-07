pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    BopAMMExecutor,
    BopAMMExecutor__InvalidDataLength,
    BopAMMExecutor__ZeroSettlementAddress
} from "../../src/executors/BopAMMExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";

contract BopAMMExecutorExposed is BopAMMExecutor {
    constructor(address settlement_) BopAMMExecutor(settlement_) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract BopAMMExecutorTest is TestUtils, Constants {
    BopAMMExecutorExposed executor;

    // A block containing a quote commit for the WETH/USDC book. BopAMM swaps
    // only succeed when block.timestamp equals the book's committed update
    // timestamp.
    uint256 constant FORK_BLOCK = 25266710;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        executor = new BopAMMExecutorExposed(BOPAMM_SETTLEMENT);

        // The quote caps at the committed lane size, but settlement transfers
        // come out of the maker's wallet; fund it and make its allowances
        // deterministic.
        deal(USDC_ADDR, BOPAMM_MAKER, 10_000_000e6);
        vm.prank(BOPAMM_MAKER);
        IERC20(USDC_ADDR).approve(BOPAMM_SETTLEMENT, type(uint256).max);
    }

    function testConstructorConfig() public view {
        assertEq(address(executor.settlement()), BOPAMM_SETTLEMENT);
    }

    function testConstructorRevertsOnZeroAddress() public {
        vm.expectRevert(BopAMMExecutor__ZeroSettlementAddress.selector);
        new BopAMMExecutorExposed(address(0));
    }

    function testDecodeParams() public view {
        (address tokenIn, address tokenOut) = executor.decodeParams(_params());

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(BopAMMExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(WETH_ADDR));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(_params());

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, BOPAMM_SETTLEMENT);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(executor.fundsExpectedAddress(_params()), address(this));
    }

    function testSwapWethToUsdc() public {
        uint256 amountIn = 0.1 ether;

        deal(WETH_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(WETH_ADDR).approve(BOPAMM_SETTLEMENT, amountIn);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        uint256 wethBalanceBefore =
            IERC20(WETH_ADDR).balanceOf(address(executor));
        executor.swap(amountIn, _params(), BOB);
        uint256 usdcDelta = IERC20(USDC_ADDR).balanceOf(BOB) - usdcBalanceBefore;
        uint256 wethDelta =
            wethBalanceBefore - IERC20(WETH_ADDR).balanceOf(address(executor));

        assertGt(usdcDelta, 0);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(executor)), 0);
    }

    function testSwapRevertsWhenStale() public {
        uint256 amountIn = 0.1 ether;

        deal(WETH_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(WETH_ADDR).approve(BOPAMM_SETTLEMENT, amountIn);

        // Any timestamp other than the committed one trips the registry's
        // exact-timestamp gate.
        vm.warp(block.timestamp + 1);
        vm.expectRevert();
        executor.swap(amountIn, _params(), BOB);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_bopamm_weth_usdc");

        (address tokenIn, address tokenOut) =
            executor.decodeParams(protocolData);

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function _params() internal view returns (bytes memory) {
        return abi.encodePacked(WETH_ADDR, USDC_ADDR);
    }
}

contract BopAMMRouterTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        // A block containing a quote commit for the WETH/USDC book (the
        // registry's exact-timestamp gate passes at this block).
        return 25266710;
    }

    function testSingleSwap() public {
        uint256 amountIn = 0.1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_bopamm_weth_usdc"
        );

        deal(USDC_ADDR, BOPAMM_MAKER, 10_000_000e6);
        vm.prank(BOPAMM_MAKER);
        IERC20(USDC_ADDR).approve(BOPAMM_SETTLEMENT, type(uint256).max);

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        uint256 wethBalanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 usdcDelta =
            IERC20(USDC_ADDR).balanceOf(ALICE) - usdcBalanceBefore;
        uint256 wethDelta =
            wethBalanceBefore - IERC20(WETH_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(usdcDelta, 160e6);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
