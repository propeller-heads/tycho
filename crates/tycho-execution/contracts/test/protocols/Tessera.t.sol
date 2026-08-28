pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    TesseraExecutor,
    TesseraExecutor__InvalidDataLength,
    TesseraExecutor__ZeroEntrypointAddress,
    ITesseraSwap
} from "../../src/executors/TesseraExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";

interface ITesseraSwapView {
    function tesseraSwapViewAmounts(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified
    ) external view returns (uint256 amountIn, uint256 amountOut);
}

contract TesseraExecutorExposed is TesseraExecutor {
    constructor(address tesseraSwap_) TesseraExecutor(tesseraSwap_) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract TesseraExecutorTest is TestUtils, Constants {
    TesseraExecutorExposed executor;

    // A recent Base block with 8 live books. Pinned: the venue reprices every
    // block and its freshness gate zeroes quotes when the fork's block env
    // drifts from the posted state.
    uint256 constant FORK_BLOCK = 50548423;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        executor = new TesseraExecutorExposed(TESSERA_SWAP);
    }

    function testConstructorConfig() public view {
        assertEq(address(executor.tesseraSwap()), TESSERA_SWAP);
    }

    function testConstructorRevertsOnZeroAddress() public {
        vm.expectRevert(TesseraExecutor__ZeroEntrypointAddress.selector);
        new TesseraExecutorExposed(address(0));
    }

    function testDecodeParams() public view {
        (address tokenIn, address tokenOut) = executor.decodeParams(_params());

        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(TesseraExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(BASE_WETH));
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
        assertEq(receiver, TESSERA_SWAP);
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(executor.fundsExpectedAddress(_params()), address(this));
    }

    function testSwapWethToUsdc() public {
        uint256 amountIn = 1 ether;
        (, uint256 quoted) = ITesseraSwapView(TESSERA_SWAP)
            .tesseraSwapViewAmounts(BASE_WETH, BASE_USDC, int256(amountIn));
        assertGt(quoted, 0);

        // TesseraSwap pulls tokenIn from msg.sender; in production the router
        // grants this allowance via TransferManager (ProtocolWillDebit).
        deal(BASE_WETH, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(BASE_WETH).approve(TESSERA_SWAP, amountIn);

        uint256 usdcBalanceBefore = IERC20(BASE_USDC).balanceOf(BOB);
        uint256 wethBalanceBefore =
            IERC20(BASE_WETH).balanceOf(address(executor));
        executor.swap(amountIn, _params(), BOB);
        uint256 usdcDelta = IERC20(BASE_USDC).balanceOf(BOB) - usdcBalanceBefore;
        uint256 wethDelta =
            wethBalanceBefore - IERC20(BASE_WETH).balanceOf(address(executor));

        assertEq(usdcDelta, quoted);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(BASE_WETH).balanceOf(address(executor)), 0);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_tessera_weth_usdc");

        (address tokenIn, address tokenOut) =
            executor.decodeParams(protocolData);

        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
    }

    function _params() internal view returns (bytes memory) {
        return abi.encodePacked(BASE_WETH, BASE_USDC);
    }
}

contract TesseraRouterTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        // A recent Base block with 8 live Tessera books.
        return 50548423;
    }

    function testTesseraExecutorDeploymentAddress() public view {
        // The Rust encoding tests reference this executor address through
        // config/test_executor_addresses.json (base."vm:tessera"); it is
        // deterministic from the setUp deployment order. This assertion
        // pins it so a reordering of deployExecutors fails loudly here
        // instead of as a calldata mismatch in the integration tests.
        assertEq(
            address(tesseraExecutor), 0x886D6d1eB8D415b00052828CD6d5B321f072073d
        );
    }

    function testSingleSwapTessera() public {
        uint256 amountIn = 1 ether;
        (, uint256 quoted) = ITesseraSwapView(TESSERA_SWAP)
            .tesseraSwapViewAmounts(BASE_WETH, BASE_USDC, int256(amountIn));
        assertGt(quoted, 0);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_tessera");

        deal(BASE_WETH, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);

        uint256 usdcBalanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);
        uint256 wethBalanceBefore = IERC20(BASE_WETH).balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);
        uint256 usdcDelta =
            IERC20(BASE_USDC).balanceOf(ALICE) - usdcBalanceBefore;
        uint256 wethDelta =
            wethBalanceBefore - IERC20(BASE_WETH).balanceOf(ALICE);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(usdcDelta, quoted);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
    }
}
