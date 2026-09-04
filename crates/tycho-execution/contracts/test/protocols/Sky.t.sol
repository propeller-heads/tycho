pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/SkyExecutor.sol";
import {Constants} from "../Constants.sol";

contract SkyExecutorExposed is SkyExecutor {
    constructor(address psm_, address wrapper_, address converter_)
        SkyExecutor(psm_, wrapper_, converter_)
    {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (Target target, bool gemToStable)
    {
        return _decodeData(data);
    }
}

contract SkyExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    SkyExecutorExposed skyExposed;

    function setUp() public {
        uint256 forkBlock = 25600000;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        skyExposed = new SkyExecutorExposed(
            SKY_LITE_PSM, SKY_USDS_PSM_WRAPPER, SKY_DAI_USDS_CONVERTER
        );
    }

    function _params(SkyExecutor.Target target, bool gemToStable)
        internal
        pure
        returns (bytes memory)
    {
        return
            abi.encodePacked(uint8(target), gemToStable ? uint8(1) : uint8(0));
    }

    function testDecodeParams() public view {
        (SkyExecutor.Target target, bool gemToStable) =
            skyExposed.decodeParams(_params(SkyExecutor.Target.Psm, true));
        assertEq(uint8(target), uint8(SkyExecutor.Target.Psm));
        assertEq(gemToStable, true);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams = abi.encodePacked(uint8(0));
        vm.expectRevert(SkyExecutor__InvalidDataLength.selector);
        skyExposed.decodeParams(invalidParams);
    }

    function testDecodeParamsInvalidTarget() public {
        vm.expectRevert(SkyExecutor__InvalidTarget.selector);
        skyExposed.decodeParams(abi.encodePacked(uint8(3), uint8(0)));
    }

    function testDecodeParamsInvalidDirection() public {
        vm.expectRevert(SkyExecutor__InvalidDirection.selector);
        skyExposed.decodeParams(abi.encodePacked(uint8(0), uint8(2)));
    }

    function testConstructorRejectsMismatchedVenues() public {
        // A converter in the PSM slot wires DAI/USDS where USDC/DAI belong.
        vm.expectRevert();
        new SkyExecutorExposed(
            SKY_DAI_USDS_CONVERTER, SKY_USDS_PSM_WRAPPER, SKY_DAI_USDS_CONVERTER
        );
    }

    function testGetTransferDataSellGem() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = skyExposed.getTransferData(_params(SkyExecutor.Target.Psm, true));
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, SKY_LITE_PSM);
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, DAI_ADDR);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataBuyGemWrapper() public view {
        // The wrapper's stable side is USDS.
        (, address receiver, address tokenIn, address tokenOut,) = skyExposed.getTransferData(
            _params(SkyExecutor.Target.Wrapper, false)
        );
        assertEq(receiver, SKY_USDS_PSM_WRAPPER);
        assertEq(tokenIn, USDS_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function testGetTransferDataConverter() public view {
        (, address receiver,, address tokenOutToUsds,) = skyExposed.getTransferData(
            _params(SkyExecutor.Target.Converter, false)
        );
        assertEq(receiver, SKY_DAI_USDS_CONVERTER);
        assertEq(tokenOutToUsds, USDS_ADDR);
        (,,, address tokenOutToDai,) = skyExposed.getTransferData(
            _params(SkyExecutor.Target.Converter, true)
        );
        assertEq(tokenOutToDai, DAI_ADDR);
    }

    function testSellGemPsm() public {
        uint256 amountIn = 1000e6; // 1000 USDC
        deal(USDC_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(USDC_ADDR).approve(SKY_LITE_PSM, amountIn);

        uint256 balanceBefore = IERC20(DAI_ADDR).balanceOf(BOB);
        skyExposed.swap(amountIn, _params(SkyExecutor.Target.Psm, true), BOB);
        // tin is 0 at the fork block: exact 1:1 with decimal rescaling.
        assertEq(IERC20(DAI_ADDR).balanceOf(BOB) - balanceBefore, 1000e18);
    }

    function testBuyGemPsm() public {
        uint256 amountIn = 1000e18; // 1000 DAI, grid-aligned
        deal(DAI_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(DAI_ADDR).approve(SKY_LITE_PSM, amountIn);

        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        skyExposed.swap(amountIn, _params(SkyExecutor.Target.Psm, false), BOB);
        assertEq(IERC20(USDC_ADDR).balanceOf(BOB) - balanceBefore, 1000e6);
        // tout is 0 and the input is grid-aligned: no dust remains.
        assertEq(IERC20(DAI_ADDR).balanceOf(address(skyExposed)), 0);
    }

    function testBuyGemPsmLeavesOnlyDust() public {
        // Not a multiple of 1e12: the sub-1e-6-dollar remainder cannot be
        // converted and stays with the caller.
        uint256 amountIn = 1000e18 + 123456;
        deal(DAI_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(DAI_ADDR).approve(SKY_LITE_PSM, amountIn);

        skyExposed.swap(amountIn, _params(SkyExecutor.Target.Psm, false), BOB);
        assertEq(IERC20(DAI_ADDR).balanceOf(address(skyExposed)), 123456);
    }

    function _fileTout(uint256 value) internal {
        // MCD_PAUSE_PROXY is a ward of the LitePSM, so it can set fees the
        // way governance would.
        vm.prank(0xBE8E3e3618f7474F8cB1d074A26afFef007E98FB);
        (bool ok,) = SKY_LITE_PSM.call(
            abi.encodeWithSignature(
                "file(bytes32,uint256)", bytes32("tout"), value
            )
        );
        require(ok, "file(tout) failed");
    }

    function testBuyGemPsmWithTout() public {
        // At 1% tout the cost per gem unit is 1.01 stable, so 1010 DAI buys
        // exactly 1000 USDC and the sizing spends the full input.
        _fileTout(0.01e18);
        uint256 amountIn = 1010e18;
        deal(DAI_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(DAI_ADDR).approve(SKY_LITE_PSM, amountIn);

        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        skyExposed.swap(amountIn, _params(SkyExecutor.Target.Psm, false), BOB);
        assertEq(IERC20(USDC_ADDR).balanceOf(BOB) - balanceBefore, 1000e6);
        assertEq(IERC20(DAI_ADDR).balanceOf(address(skyExposed)), 0);
    }

    function testBuyGemPsmHaltedReverts() public {
        // A HALTED tout (uint256.max) must revert on the checked addition in
        // the sizing, matching the venue's halt semantics.
        _fileTout(type(uint256).max);
        uint256 amountIn = 1000e18;
        deal(DAI_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(DAI_ADDR).approve(SKY_LITE_PSM, amountIn);

        vm.expectRevert(stdError.arithmeticError);
        skyExposed.swap(amountIn, _params(SkyExecutor.Target.Psm, false), BOB);
    }

    function testSellGemWrapper() public {
        uint256 amountIn = 1000e6;
        deal(USDC_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(USDC_ADDR).approve(SKY_USDS_PSM_WRAPPER, amountIn);

        uint256 balanceBefore = IERC20(USDS_ADDR).balanceOf(BOB);
        skyExposed.swap(
            amountIn, _params(SkyExecutor.Target.Wrapper, true), BOB
        );
        assertEq(IERC20(USDS_ADDR).balanceOf(BOB) - balanceBefore, 1000e18);
    }

    function testBuyGemWrapper() public {
        uint256 amountIn = 1000e18;
        deal(USDS_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(USDS_ADDR).approve(SKY_USDS_PSM_WRAPPER, amountIn);

        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        skyExposed.swap(
            amountIn, _params(SkyExecutor.Target.Wrapper, false), BOB
        );
        assertEq(IERC20(USDC_ADDR).balanceOf(BOB) - balanceBefore, 1000e6);
    }

    function testDaiToUsds() public {
        uint256 amountIn = 500e18;
        deal(DAI_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(DAI_ADDR).approve(SKY_DAI_USDS_CONVERTER, amountIn);

        uint256 balanceBefore = IERC20(USDS_ADDR).balanceOf(BOB);
        skyExposed.swap(
            amountIn, _params(SkyExecutor.Target.Converter, false), BOB
        );
        assertEq(IERC20(USDS_ADDR).balanceOf(BOB) - balanceBefore, amountIn);
    }

    function testUsdsToDai() public {
        uint256 amountIn = 500e18;
        deal(USDS_ADDR, address(skyExposed), amountIn);
        vm.prank(address(skyExposed));
        IERC20(USDS_ADDR).approve(SKY_DAI_USDS_CONVERTER, amountIn);

        uint256 balanceBefore = IERC20(DAI_ADDR).balanceOf(BOB);
        skyExposed.swap(
            amountIn, _params(SkyExecutor.Target.Converter, true), BOB
        );
        assertEq(IERC20(DAI_ADDR).balanceOf(BOB) - balanceBefore, amountIn);
    }

    function testDecodeRustEncodedCalldata() public view {
        // Cross-language check: decode the exact bytes the Rust encoder wrote.
        bytes memory rustData = loadCallDataFromFile("sky_psm_sell_gem");
        (SkyExecutor.Target target, bool gemToStable) =
            skyExposed.decodeParams(rustData);
        assertEq(uint8(target), uint8(SkyExecutor.Target.Psm));
        assertEq(gemToStable, true);

        rustData = loadCallDataFromFile("sky_usds_to_dai");
        (target, gemToStable) = skyExposed.decodeParams(rustData);
        assertEq(uint8(target), uint8(SkyExecutor.Target.Converter));
        assertEq(gemToStable, true);
    }
}

contract TychoRouterForSkyTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 25600000;
    }

    function testSkyExecutorAddress() public view {
        // Pins the deterministic test-deployment address used in
        // config/test_executor_addresses.json; fails loudly if the deploy
        // order in TychoRouterTestSetup changes.
        assertEq(
            address(skyExecutor), 0x886D6d1eB8D415b00052828CD6d5B321f072073d
        );
    }

    function testSingleSkyIntegration() public {
        // USDC -> (LitePSM) -> DAI through the full router path.
        deal(USDC_ADDR, ALICE, 1000e6);
        uint256 balanceBefore = IERC20(DAI_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_sky");
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE) - balanceBefore, 1000e18);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialSkyIntegration() public {
        // DAI -> (DaiUsds) -> USDS -> (UsdsPsmWrapper) -> USDC
        deal(DAI_ADDR, ALICE, 1000e18);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_sequential_encoding_strategy_sky");
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE) - balanceBefore, 1000e6);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDS_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
