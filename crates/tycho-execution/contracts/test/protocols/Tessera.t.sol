// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;
import "../TychoRouterTestSetup.sol";
import {
    TesseraExecutor,
    TesseraExecutor__InvalidDataLength,
    TesseraExecutor__ZeroTesseraSwapAddress
} from "../../src/executors/TesseraExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

address constant TESSERA_SWAP = 0x55555522005BcAE1c2424D474BfD5ed477749E3e;
uint256 constant TESSERA_FORK_BLOCK = 50_548_423;

interface ITesseraQuote {
    function tesseraSwapViewAmounts(address, address, int256)
        external
        view
        returns (uint256, uint256);
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

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), TESSERA_FORK_BLOCK);
        executor = new TesseraExecutorExposed(TESSERA_SWAP);
    }

    function testConstructorConfig() public view {
        assertEq(address(executor.tesseraSwap()), TESSERA_SWAP);
    }

    function testConstructorRevertsOnZeroAddress() public {
        vm.expectRevert(TesseraExecutor__ZeroTesseraSwapAddress.selector);
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
        uint256 amountIn = 0.01 ether;
        deal(BASE_WETH, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(BASE_WETH).approve(TESSERA_SWAP, amountIn);
        // TesseraSwap quotes for its caller; quote as the executor.
        vm.prank(address(executor));
        (, uint256 quote) = ITesseraQuote(TESSERA_SWAP)
            .tesseraSwapViewAmounts(BASE_WETH, BASE_USDC, int256(amountIn));

        uint256 usdcBefore = IERC20(BASE_USDC).balanceOf(BOB);
        executor.swap(amountIn, _params(), BOB);

        assertGt(quote, 0);
        assertEq(IERC20(BASE_USDC).balanceOf(BOB) - usdcBefore, quote);
        assertEq(IERC20(BASE_WETH).balanceOf(address(executor)), 0);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_tessera_usdc_weth");

        (address tokenIn, address tokenOut) =
            executor.decodeParams(protocolData);

        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, BASE_WETH);
    }

    function _params() internal pure returns (bytes memory) {
        return abi.encodePacked(BASE_WETH, BASE_USDC);
    }
}

contract TesseraRouterTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return TESSERA_FORK_BLOCK;
    }

    function testRouterSettlesViewQuoteWithEmptyTag() public {
        uint256 amount = 0.01 ether;
        deal(BASE_WETH, ALICE, amount);
        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, amount);
        // The venue sees the router as its caller; use identical quote context.
        vm.stopPrank();
        vm.prank(tychoRouterAddr);
        (, uint256 quote) = ITesseraQuote(TESSERA_SWAP)
            .tesseraSwapViewAmounts(BASE_WETH, BASE_USDC, int256(amount));
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_tessera_weth_usdc");
        assertEq(protocolData, abi.encodePacked(BASE_WETH, BASE_USDC));
        vm.expectCall(
            TESSERA_SWAP,
            abi.encodeWithSignature(
                "tesseraSwapWithAllowances(address,address,int256,uint256,address,bytes)",
                BASE_WETH,
                BASE_USDC,
                int256(amount),
                uint256(0),
                ALICE,
                bytes("")
            )
        );
        uint256 beforeBalance = IERC20(BASE_USDC).balanceOf(ALICE);
        vm.prank(ALICE);
        uint256 received = tychoRouter.singleSwap(
            amount,
            BASE_WETH,
            BASE_USDC,
            quote,
            quote,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(tesseraExecutor), protocolData)
        );
        assertEq(received, quote);
        assertEq(IERC20(BASE_USDC).balanceOf(ALICE) - beforeBalance, quote);
        emit log_named_address(
            "Tessera executor test address", address(tesseraExecutor)
        );
    }

    function testSingleSwapIntegration() public {
        _checkEncodedRouterSwap(
            BASE_WETH,
            BASE_USDC,
            0.01 ether,
            "test_single_encoding_strategy_tessera_weth_usdc"
        );
    }

    function testSingleSwapIntegrationUsdcToWeth() public {
        _checkEncodedRouterSwap(
            BASE_USDC,
            BASE_WETH,
            100e6,
            "test_single_encoding_strategy_tessera_usdc_weth"
        );
    }

    function _checkEncodedRouterSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        string memory fixture
    ) internal {
        vm.prank(tychoRouterAddr);
        (, uint256 quote) = ITesseraQuote(TESSERA_SWAP)
            .tesseraSwapViewAmounts(tokenIn, tokenOut, int256(amountIn));
        assertGt(quote, 0);
        // Exercise the encoder-produced router call, including the executor's fee-tag choice.
        vm.expectCall(
            TESSERA_SWAP,
            abi.encodeWithSignature(
                "tesseraSwapWithAllowances(address,address,int256,uint256,address,bytes)",
                tokenIn,
                tokenOut,
                int256(amountIn),
                uint256(0),
                ALICE,
                bytes("")
            )
        );
        deal(tokenIn, ALICE, amountIn);
        uint256 outputBefore = IERC20(tokenOut).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(fixture);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(tokenOut).balanceOf(ALICE) - outputBefore, quote);
        assertEq(IERC20(tokenIn).balanceOf(ALICE), 0);
        assertEq(IERC20(tokenIn).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), 0);
    }
}
