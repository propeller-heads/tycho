pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    FermiSwapExecutor,
    FermiSwapExecutor__AmountTooLarge,
    FermiSwapExecutor__InvalidDataLength
} from "../../src/executors/FermiSwapExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";

contract FermiSwapExecutorExposed is FermiSwapExecutor {
    constructor(address fermiSwapper_) FermiSwapExecutor(fermiSwapper_) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract FermiSwapExecutorTest is TestUtils, Constants {
    FermiSwapExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 25143884);
        executor = new FermiSwapExecutorExposed(FERMI_SWAPPER);
    }

    function testConstructorConfig() public view {
        assertEq(address(executor.fermiSwapper()), FERMI_SWAPPER);
    }

    function testDecodeParams() public view {
        (address tokenIn, address tokenOut) = executor.decodeParams(_params());

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(FermiSwapExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(WETH_ADDR));
    }

    function testGetTransferData() public {
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
        assertEq(receiver, FERMI_SWAPPER);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    function testSwapWethToUsdc() public {
        uint256 amountIn = 1 ether;

        deal(WETH_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(WETH_ADDR).approve(FERMI_SWAPPER, amountIn);

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

    function testSwapRevertsWhenAmountTooLarge() public {
        vm.expectRevert(FermiSwapExecutor__AmountTooLarge.selector);
        executor.swap(uint256(type(int256).max) + 1, _params(), BOB);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_fermiswap_weth_usdc");

        (address tokenIn, address tokenOut) =
            executor.decodeParams(protocolData);

        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function _params() internal view returns (bytes memory) {
        return abi.encodePacked(WETH_ADDR, USDC_ADDR);
    }
}

contract FermiSwapRouterTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 25143884;
    }

    function testSingleSwap() public {
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_fermiswap_weth_usdc"
        );

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
        assertGt(usdcDelta, 0);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
