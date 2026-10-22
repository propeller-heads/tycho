// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {
    BiconomyExecutor,
    BiconomyExecutor__InvalidDataLength,
    BiconomyExecutor__NativeTokenNotSupported
} from "@src/executors/BiconomyExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {MockPropAMM} from "./PropAMM.t.sol";
import {MockERC20} from "forge-std/mocks/MockERC20.sol";

contract BiconomyExecutorExposed is BiconomyExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address venue, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract BiconomyExecutorTest is TestUtils, Constants {
    address constant MOCK_VENUE = 0x2222222222222222222222222222222222222222;
    // 2000 USDC (6 decimals) per 1e18 units (1 WETH) of tokenIn.
    uint256 constant WETH_USDC_PRICE = 2000e6;

    BiconomyExecutorExposed executor;

    function setUp() public {
        executor = new BiconomyExecutorExposed();
        deployCodeTo("MockERC20.sol:MockERC20", BASE_WETH);
        MockERC20(BASE_WETH).initialize("Wrapped Ether", "WETH", 18);
        deployCodeTo("MockERC20.sol:MockERC20", BASE_USDC);
        MockERC20(BASE_USDC).initialize("USD Coin", "USDC", 6);
        deployCodeTo("PropAMM.t.sol:MockPropAMM", MOCK_VENUE);
        MockPropAMM(MOCK_VENUE).setPrice(BASE_WETH, BASE_USDC, WETH_USDC_PRICE);
    }

    function testDecodeParams() public view {
        (address venue, address tokenIn, address tokenOut) =
            executor.decodeParams(_params());

        assertEq(venue, MOCK_VENUE);
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(BiconomyExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(MOCK_VENUE, BASE_WETH));
    }

    function testDecodeParamsNativeTokenIn() public {
        vm.expectRevert(BiconomyExecutor__NativeTokenNotSupported.selector);
        executor.decodeParams(
            abi.encodePacked(MOCK_VENUE, ETH_ADDRESS, BASE_USDC)
        );
    }

    function testDecodeParamsNativeTokenOut() public {
        vm.expectRevert(BiconomyExecutor__NativeTokenNotSupported.selector);
        executor.decodeParams(
            abi.encodePacked(MOCK_VENUE, BASE_USDC, ETH_ADDRESS)
        );
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
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, MOCK_VENUE);
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(executor.fundsExpectedAddress(_params()), MOCK_VENUE);
    }

    function testSwapWethToUsdc() public {
        uint256 amountIn = 1 ether;

        // The router pushes the input to the venue before calling the
        // executor; the deal simulates that transfer and funds the output.
        deal(BASE_WETH, MOCK_VENUE, amountIn);
        deal(BASE_USDC, MOCK_VENUE, 1_000_000e6);

        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);
        executor.swap(amountIn, _params(), BOB);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB) - balanceBefore, 2000e6);
    }

    function _params() internal pure returns (bytes memory) {
        return abi.encodePacked(MOCK_VENUE, BASE_WETH, BASE_USDC);
    }
}

interface IBiconomyVenue {
    error BadRecipient();
}

/// @notice Runs the executor against the deployed Biconomy venue on Base, at a
/// block where a maker's WETH/USDC boards are live in both directions.
contract BiconomyExecutorBaseForkTest is TestUtils, Constants {
    address constant VENUE = 0x0000009E5F98cDB4dd1E1d2E15B220217c88B3b8;
    uint256 constant FORK_BLOCK = 52549180;
    address RECEIVER = makeAddr("biconomy-receiver");

    BiconomyExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        executor = new BiconomyExecutorExposed();
    }

    function testSwapWethToUsdcMatchesQuote() public {
        _assertSwapMatchesQuote(BASE_WETH, BASE_USDC, 0.0005 ether);
    }

    function testSwapUsdcToWethMatchesQuote() public {
        _assertSwapMatchesQuote(BASE_USDC, BASE_WETH, 5e6);
    }

    function testConsecutiveVenueHopReverts() public {
        // A sequential route through the venue twice would set the first
        // hop's receiver to the venue (the next hop's fundsExpectedAddress).
        // The venue rejects itself as recipient.
        uint256 amountIn = 0.001 ether;
        deal(BASE_WETH, VENUE, IERC20(BASE_WETH).balanceOf(VENUE) + amountIn);

        vm.expectRevert(IBiconomyVenue.BadRecipient.selector);
        executor.swap(
            amountIn, abi.encodePacked(VENUE, BASE_WETH, BASE_USDC), VENUE
        );
    }

    function _assertSwapMatchesQuote(
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal {
        uint256 quoted = IPropAMM(VENUE).quote(tokenIn, tokenOut, amountIn);
        assertGt(quoted, 0);

        // Push-payment: the router transfers amountIn to the venue first.
        deal(tokenIn, VENUE, IERC20(tokenIn).balanceOf(VENUE) + amountIn);

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(RECEIVER);
        executor.swap(
            amountIn, abi.encodePacked(VENUE, tokenIn, tokenOut), RECEIVER
        );

        assertEq(IERC20(tokenOut).balanceOf(RECEIVER) - balanceBefore, quoted);
    }
}

contract BiconomyRouterTest is TychoRouterTestSetup {
    // Must match the venue address in the Rust test that writes the calldata.
    address constant MOCK_VENUE = 0x2222222222222222222222222222222222222222;

    function getForkBlock() public pure override returns (uint256) {
        return 25143884;
    }

    function setUp() public override {
        super.setUp();
        deployCodeTo("PropAMM.t.sol:MockPropAMM", MOCK_VENUE);
        MockPropAMM(MOCK_VENUE).setPrice(WETH_ADDR, USDC_ADDR, 2000e6);
        deal(USDC_ADDR, MOCK_VENUE, 1_000_000e6);
    }

    function testSingleSwap() public {
        uint256 amountIn = 1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_biconomy_weth_usdc"
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
        assertEq(usdcDelta, 2000e6);
        assertEq(wethDelta, amountIn);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
