// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;
import "../TychoRouterTestSetup.sol";
import {
    TesseraExecutor,
    TesseraExecutor__AmountTooLarge,
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

    function testSwapRevertsWhenAmountTooLarge() public {
        vm.expectRevert(TesseraExecutor__AmountTooLarge.selector);
        executor.swap(uint256(type(int256).max) + 1, _params(), BOB);
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

    function testSingleSwapWethToUsdc() public {
        _checkRouterSwap(BASE_WETH, BASE_USDC, 0.01 ether);
    }

    function testSingleSwapUsdcToWeth() public {
        _checkRouterSwap(BASE_USDC, BASE_WETH, 100e6);
    }

    function _checkRouterSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal {
        // TesseraSwap quotes for its caller, which in a swap is the router.
        vm.prank(tychoRouterAddr);
        (, uint256 quote) = ITesseraQuote(TESSERA_SWAP)
            .tesseraSwapViewAmounts(tokenIn, tokenOut, int256(amountIn));
        assertGt(quote, 0);
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
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        uint256 received = tychoRouter.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            quote,
            quote,
            ALICE,
            noClientFee(),
            encodeSingleSwap(
                address(tesseraExecutor), abi.encodePacked(tokenIn, tokenOut)
            )
        );
        vm.stopPrank();

        assertEq(received, quote);
        assertEq(IERC20(tokenOut).balanceOf(ALICE) - outputBefore, quote);
        assertEq(IERC20(tokenIn).balanceOf(ALICE), 0);
        assertEq(IERC20(tokenIn).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), 0);
    }
}
