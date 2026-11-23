pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
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

    bytes32 constant FILL_TX =
        0x16927066210ca504b19c8de4d97586594d27ae66e72228b629366b2fd604d436;
    // The USDT amount that fill swapped for USDC (6 decimals).
    uint256 constant FILL_AMOUNT_IN = 168143666903;

    function setUp() public {
        // The state just before a real fill of the router, in block 26443801. FermiSwap only
        // quotes in the block its oracle was refreshed in, so a fork at a block boundary reverts
        // every swap; replaying the block up to this transaction keeps the refresh.
        vm.createSelectFork(vm.rpcUrl("mainnet"), FILL_TX);
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

    function testSwapUsdtToUsdc() public {
        uint256 amountIn = FILL_AMOUNT_IN;

        deal(USDT_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        // USDT's approve returns no value, which the IERC20 interface cannot decode.
        SafeERC20.forceApprove(IERC20(USDT_ADDR), FERMI_SWAPPER, amountIn);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        uint256 usdtBalanceBefore =
            IERC20(USDT_ADDR).balanceOf(address(executor));
        executor.swap(amountIn, abi.encodePacked(USDT_ADDR, USDC_ADDR), BOB);
        uint256 usdcDelta = IERC20(USDC_ADDR).balanceOf(BOB) - usdcBalanceBefore;
        uint256 usdtDelta =
            usdtBalanceBefore - IERC20(USDT_ADDR).balanceOf(address(executor));

        assertGt(usdcDelta, 0);
        assertEq(usdtDelta, amountIn);
        assertEq(IERC20(USDT_ADDR).balanceOf(address(executor)), 0);
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
    // The state just before the real fill that FermiSwapExecutorTest replays, in block 26443801.
    bytes32 constant FILL_TX =
        0x16927066210ca504b19c8de4d97586594d27ae66e72228b629366b2fd604d436;
    // The USDT amount that fill swapped for USDC (6 decimals).
    uint256 constant FILL_AMOUNT_IN = 168143666903;

    function getForkBlock() public pure override returns (uint256) {
        return 26443801;
    }

    function getForkTx() public pure override returns (bytes32) {
        return FILL_TX;
    }

    function testSingleSwap() public {
        uint256 amountIn = FILL_AMOUNT_IN;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_fermiswap_usdt_usdc"
        );

        deal(USDT_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        // USDT's approve returns no value, which the IERC20 interface cannot decode.
        SafeERC20.forceApprove(
            IERC20(USDT_ADDR), tychoRouterAddr, type(uint256).max
        );

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        uint256 usdtBalanceBefore = IERC20(USDT_ADDR).balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 usdcDelta =
            IERC20(USDC_ADDR).balanceOf(ALICE) - usdcBalanceBefore;
        uint256 usdtDelta =
            usdtBalanceBefore - IERC20(USDT_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertGt(usdcDelta, 0);
        assertEq(usdtDelta, amountIn);
        assertEq(IERC20(USDT_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
