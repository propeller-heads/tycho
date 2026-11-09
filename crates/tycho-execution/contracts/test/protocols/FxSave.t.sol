pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/FxSaveExecutor.sol";
import {Constants} from "../Constants.sol";

contract FxSaveExecutorExposed is FxSaveExecutor {
    constructor(address fxBasePool_, address fxSave_)
        FxSaveExecutor(fxBasePool_, fxSave_)
    {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address tokenIn)
    {
        return _decodeData(data);
    }
}

contract FxSaveExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    // Block 26,353,817: tycho-simulation's FxSaveState quotes these deposits
    // to the same wei (state.rs tests).
    uint256 constant FORK_BLOCK = 26353817;
    uint256 constant USDC_DEPOSIT_SHARES = 885837212934908530177;
    uint256 constant FXUSD_DEPOSIT_SHARES = 885996151784577165475;

    FxSaveExecutorExposed fxSaveExposed;
    IERC20 fxSave = IERC20(FXSAVE_ADDR);
    IERC20 fxSp = IERC20(FX_BASE_POOL);

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        fxSaveExposed = new FxSaveExecutorExposed(FX_BASE_POOL, FXSAVE_ADDR);
    }

    function _deposit(address tokenIn, uint256 amountIn) internal {
        deal(tokenIn, address(fxSaveExposed), amountIn);
        // The router approves fxSP for ProtocolWillDebit before the swap.
        vm.prank(address(fxSaveExposed));
        IERC20(tokenIn).forceApprove(FX_BASE_POOL, amountIn);
        fxSaveExposed.swap(amountIn, abi.encodePacked(tokenIn), BOB);
    }

    function testDecodeParams() public view {
        assertEq(
            fxSaveExposed.decodeParams(abi.encodePacked(USDC_ADDR)), USDC_ADDR
        );
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(FxSaveExecutor__InvalidDataLength.selector);
        fxSaveExposed.decodeParams(abi.encodePacked(USDC_ADDR, uint8(0)));
    }

    function testDecodeRustCalldata() public view {
        bytes memory usdcData = loadCallDataFromFile("fxsave_usdc_deposit");
        bytes memory fxUsdData = loadCallDataFromFile("fxsave_fxusd_deposit");

        assertEq(fxSaveExposed.decodeParams(usdcData), USDC_ADDR);
        assertEq(fxSaveExposed.decodeParams(fxUsdData), FXUSD_ADDR);
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(FxSaveExecutor__ZeroAddress.selector);
        new FxSaveExecutorExposed(address(0), FXSAVE_ADDR);
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = fxSaveExposed.getTransferData(abi.encodePacked(USDC_ADDR));

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, FX_BASE_POOL);
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, FXSAVE_ADDR);
        assertEq(outputToRouter, false);
    }

    function testFundsExpectedAddress() public {
        vm.prank(ALICE);
        assertEq(
            fxSaveExposed.fundsExpectedAddress(abi.encodePacked(USDC_ADDR)),
            ALICE
        );
    }

    function testDepositUsdc() public {
        _deposit(USDC_ADDR, 1000e6);

        assertEq(fxSave.balanceOf(BOB), USDC_DEPOSIT_SHARES);
        assertEq(fxSp.balanceOf(address(fxSaveExposed)), 0);
        assertEq(
            fxSp.allowance(address(fxSaveExposed), FXSAVE_ADDR),
            0,
            "fxSAVE allowance left over"
        );
    }

    function testDepositFxUsd() public {
        _deposit(FXUSD_ADDR, 1000e18);

        assertEq(fxSave.balanceOf(BOB), FXUSD_DEPOSIT_SHARES);
        assertEq(fxSp.balanceOf(address(fxSaveExposed)), 0);
    }

    function testHeldFxSpStaysWithTheRouter() public {
        // The router's vault may hold fxSP for its users; a deposit must not
        // sweep it into fxSAVE.
        uint256 held = 5e18;
        deal(FX_BASE_POOL, address(fxSaveExposed), held);

        _deposit(USDC_ADDR, 1000e6);

        assertEq(fxSave.balanceOf(BOB), USDC_DEPOSIT_SHARES);
        assertEq(fxSp.balanceOf(address(fxSaveExposed)), held);
    }

    function testOtherTokenReverts() public {
        uint256 amountIn = 1000e18;
        deal(DAI_ADDR, address(fxSaveExposed), amountIn);
        vm.prank(address(fxSaveExposed));
        IERC20(DAI_ADDR).forceApprove(FX_BASE_POOL, amountIn);

        vm.expectRevert();
        fxSaveExposed.swap(amountIn, abi.encodePacked(DAI_ADDR), BOB);
    }
}

contract TychoRouterForFxSaveTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 26353817;
    }

    function testSingleFxSaveIntegration() public {
        IERC20 fxSave = IERC20(FXSAVE_ADDR);
        deal(USDC_ADDR, ALICE, 1000e6);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_fxsave");
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(fxSave.balanceOf(ALICE), 885837212934908530177);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(FX_BASE_POOL).balanceOf(tychoRouterAddr), 0);
        assertEq(fxSave.balanceOf(tychoRouterAddr), 0);
    }
}
