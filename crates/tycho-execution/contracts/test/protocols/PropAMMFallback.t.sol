pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    PropAMMFallbackExecutor,
    PropAMMFallbackExecutor__InvalidDataLength
} from "../../src/executors/PropAMMFallbackExecutor.sol";
import {IPropAMMRouter} from "@interfaces/IPropAMMRouter.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {TransferManager} from "../../src/TransferManager.sol";

/// @notice The `exactInputSingle` entry point of the Uniswap V3 router the PropAMMRouter retries
/// on. Used only to price the retry against a direct call, in `testFallbackPathGas`.
interface ISwapRouter02 {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut);
}

contract PropAMMFallbackExecutorExposed is PropAMMFallbackExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address venue, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract PropAMMFallbackExecutorTest is TestUtils, Constants {
    /// USDC out of the Uniswap V3 retry for 1 WETH at the fork block below.
    uint256 constant FALLBACK_AMOUNT_OUT = 1872190012;

    PropAMMFallbackExecutorExposed executor;

    /// Block at which FermiSwap's oracle lane is stale, so the venue reverts and the
    /// PropAMMRouter's Uniswap V3 retry is the path under test. This is the ordinary case for any
    /// block Titan did not build: it is what makes integrator simulations of pAMM routes fail.
    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 25682938);
        executor = new PropAMMFallbackExecutorExposed();
    }

    /// The router is hardcoded, so a deployment can never point somewhere else.
    function testRouterAddress() public view {
        assertEq(address(executor.PROPAMM_ROUTER()), PROPAMM_ROUTER);
    }

    function testDecodeParams() public view {
        (address venue, address tokenIn, address tokenOut) = executor.decodeParams(
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, USDC_ADDR)
        );

        assertEq(venue, FERMI_PROPAMM_VENUE);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(PropAMMFallbackExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, USDC_ADDR)
        );

        // The PropAMMRouter pulls tokenIn with transferFrom, unlike the push-payment
        // PropAMMExecutor.
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, PROPAMM_ROUTER);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    /// The venue is inactive at this block, so a direct call would revert.
    function testVenueQuoteAtForkBlock() public {
        vm.expectRevert();
        IPropAMM(FERMI_PROPAMM_VENUE).quote(WETH_ADDR, USDC_ADDR, 1 ether);
    }

    /// The whole point: the leg still delivers tokenOut, at the Uniswap V3 price.
    function testSwapWithStaleVenue() public {
        uint256 amountIn = 1 ether;

        deal(WETH_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(WETH_ADDR).approve(PROPAMM_ROUTER, amountIn);

        uint256 usdcBefore = IERC20(USDC_ADDR).balanceOf(BOB);
        executor.swap(
            amountIn,
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, USDC_ADDR),
            BOB
        );
        uint256 usdcDelta = IERC20(USDC_ADDR).balanceOf(BOB) - usdcBefore;

        // Uniswap V3 WETH/USDC at the router's resolvedFee tier.
        assertEq(usdcDelta, FALLBACK_AMOUNT_OUT);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(executor)), 0);
    }

    /// The fallback pool is chosen by the router, not by us. A pair with no pool at
    /// `resolvedFee` has no fallback, so Fynd must not route through here for it.
    function testResolvedFee() public view {
        assertEq(
            IPropAMMRouter(PROPAMM_ROUTER).resolvedFee(WETH_ADDR, USDC_ADDR),
            500
        );
    }

    /// The router reports which venue filled. With the pAMM stale it is the Uniswap V3 router,
    /// which is what makes the leg deliver tokenOut at all.
    function testFallbackFillsOnUniswapV3() public {
        (uint256 amountOut, address executedVenue) =
            _swapViaRouter(FERMI_PROPAMM_VENUE);

        assertEq(executedVenue, PROPAMM_FALLBACK_SWAP_ROUTER);
        assertEq(amountOut, FALLBACK_AMOUNT_OUT);
    }

    /// Kipseli is the second whitelisted venue and the one that produced the higher measured
    /// router overhead. Same stale-venue behavior as FermiSwap.
    function testKipseliVenueFallsBack() public {
        (uint256 amountOut, address executedVenue) =
            _swapViaRouter(KIPSELI_PROPAMM_VENUE);

        assertEq(executedVenue, PROPAMM_FALLBACK_SWAP_ROUTER);
        assertEq(amountOut, FALLBACK_AMOUNT_OUT);
    }

    /// A venue outside the router's whitelist reverts instead of falling back. This is what keeps
    /// the swap data from reaching arbitrary code, and it is why only whitelisted venues may carry
    /// the `propammfallback:` prefix.
    function testUnknownVenueReverts() public {
        _fundAndApprove(PROPAMM_ROUTER);

        vm.prank(ALICE);
        vm.expectRevert();
        IPropAMMRouter(PROPAMM_ROUTER)
            .swapViaVenueV1(
                makeAddr("not a whitelisted venue"),
                WETH_ADDR,
                USDC_ADDR,
                1 ether,
                0,
                ALICE,
                block.timestamp
            );
    }

    /// Source of the retry gas figure in `gas_estimator.rs`. Read together with
    /// `testDirectUniswapV3Gas`: the retry costs about 143k more than the same Uniswap V3 swap
    /// called directly, because it pays for the failed venue call and the router's own wrapper.
    function testFallbackPathGas() public {
        _fundAndApprove(PROPAMM_ROUTER);

        vm.prank(ALICE);
        uint256 gasBefore = gasleft();
        IPropAMMRouter(PROPAMM_ROUTER)
            .swapViaVenueV1(
                FERMI_PROPAMM_VENUE,
                WETH_ADDR,
                USDC_ADDR,
                1 ether,
                0,
                ALICE,
                block.timestamp
            );
        uint256 gasUsed = gasBefore - gasleft();

        // 249,895 at this block.
        assertGt(gasUsed, 200_000);
        assertLt(gasUsed, 300_000);
    }

    /// The same Uniswap V3 swap the retry performs, called directly, from the same cold state.
    function testDirectUniswapV3Gas() public {
        _fundAndApprove(PROPAMM_FALLBACK_SWAP_ROUTER);

        vm.prank(ALICE);
        uint256 gasBefore = gasleft();
        ISwapRouter02(PROPAMM_FALLBACK_SWAP_ROUTER)
            .exactInputSingle(
                ISwapRouter02.ExactInputSingleParams({
                    tokenIn: WETH_ADDR,
                    tokenOut: USDC_ADDR,
                    fee: 500,
                    recipient: ALICE,
                    amountIn: 1 ether,
                    amountOutMinimum: 0,
                    sqrtPriceLimitX96: 0
                })
            );
        uint256 gasUsed = gasBefore - gasleft();

        // 106,784 at this block.
        assertGt(gasUsed, 80_000);
        assertLt(gasUsed, 130_000);
    }

    function _fundAndApprove(address spender) internal {
        deal(WETH_ADDR, ALICE, 1 ether);
        vm.prank(ALICE);
        IERC20(WETH_ADDR).approve(spender, 1 ether);
    }

    function _swapViaRouter(address venue)
        internal
        returns (uint256 amountOut, address executedVenue)
    {
        uint256 amountIn = 1 ether;
        _fundAndApprove(PROPAMM_ROUTER);
        vm.prank(ALICE);
        (amountOut, executedVenue) = IPropAMMRouter(PROPAMM_ROUTER)
            .swapViaVenueV1(
                venue, WETH_ADDR, USDC_ADDR, amountIn, 0, ALICE, block.timestamp
            );
    }
}

/// @notice The claim this executor exists for, run through the whole router: a stale pAMM venue
/// still delivers tokenOut. Every leg here takes the Uniswap V3 retry, so these tests exercise the
/// TychoRouter machinery the executor is new to - the `ProtocolWillDebit` approval of a
/// third-party router, the balance diff measured at the leg's receiver, the unconsumed-approval
/// revoke, and `minAmountOut` binding on the retry price.
contract PropAMMFallbackRouterTest is TychoRouterTestSetup {
    /// USDC out of the Uniswap V3 retry for 1 WETH at the fork block.
    uint256 constant FALLBACK_AMOUNT_OUT = 1872190012;
    /// DAI out of `FALLBACK_AMOUNT_OUT` USDC on the Uniswap V2 DAI/USDC pool.
    uint256 constant SEQUENTIAL_AMOUNT_OUT = 1857347552744449019860;
    /// USDC out of a 60/40 split, two retries on the same Uniswap V3 pool.
    uint256 constant SPLIT_AMOUNT_OUT = 1872190012;

    /// Block at which FermiSwap's oracle lane is stale, so the venue reverts.
    function getForkBlock() public pure override returns (uint256) {
        return 25682938;
    }

    function testSingleSwap() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            FALLBACK_AMOUNT_OUT,
            1800e6,
            ALICE,
            noClientFee(),
            _fallbackSwap(USDC_ADDR)
        );
        vm.stopPrank();

        assertEq(amountOut, FALLBACK_AMOUNT_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), FALLBACK_AMOUNT_OUT);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);
        _assertRouterHolds(USDC_ADDR, 0);
    }

    /// `minAmountOut` is the only price check on this leg: the executor passes `amountOutMin = 0`
    /// to the PropAMMRouter, so a caller pricing the pAMM quote instead of the retry reverts.
    function testSingleSwapMinAmountOutBinds() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NegativeSlippage.selector,
                FALLBACK_AMOUNT_OUT,
                2000e6
            )
        );
        tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            2000e6,
            2000e6,
            ALICE,
            noClientFee(),
            _fallbackSwap(USDC_ADDR)
        );
        vm.stopPrank();
    }

    /// With fees active the leg's output lands at the router first, then the fee path pays out.
    function testSingleSwapWithRouterFee() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(routerFeeReceiver);
        feeCalculator.setRouterFeeOnOutput(1_000_000); // 1%
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            FALLBACK_AMOUNT_OUT,
            1800e6,
            ALICE,
            noClientFee(),
            _fallbackSwap(USDC_ADDR)
        );
        vm.stopPrank();

        uint256 fee = FALLBACK_AMOUNT_OUT / 100;
        assertEq(amountOut, FALLBACK_AMOUNT_OUT - fee);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), amountOut);
        assertEq(
            tychoRouter.balanceOf(
                routerFeeReceiver, uint256(uint160(USDC_ADDR))
            ),
            fee
        );
        // The fee stays in the router as the vault balance's backing.
        _assertRouterHolds(USDC_ADDR, fee);
    }

    /// The retry sends tokenOut to the next hop's pool, not to the router, so the second leg is
    /// funded by the third-party router.
    function testSequentialSwap() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(propAMMFallbackExecutor),
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, USDC_ADDR)
        );
        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_USDC_POOL, USDC_ADDR, DAI_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            SEQUENTIAL_AMOUNT_OUT,
            1800e18,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, SEQUENTIAL_AMOUNT_OUT);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), SEQUENTIAL_AMOUNT_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        _assertRouterHolds(DAI_ADDR, 0);
    }

    /// Two legs on the same executor in one transaction: the second approval must survive the
    /// first leg's `_revokeUnconsumedApproval`.
    function testSplitSwap() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
            address(propAMMFallbackExecutor),
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, USDC_ADDR)
        );
        swaps[1] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24(0),
            address(propAMMFallbackExecutor),
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, USDC_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.splitSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            SPLIT_AMOUNT_OUT,
            1800e6,
            2,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, SPLIT_AMOUNT_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), SPLIT_AMOUNT_OUT);
        _assertRouterHolds(USDC_ADDR, 0);
    }

    function _fallbackSwap(address tokenOut)
        internal
        view
        returns (bytes memory)
    {
        return encodeSingleSwap(
            address(propAMMFallbackExecutor),
            abi.encodePacked(FERMI_PROPAMM_VENUE, WETH_ADDR, tokenOut)
        );
    }

    /// The router keeps only `expectedTokenOut` and leaves no allowance behind for the
    /// third-party router.
    function _assertRouterHolds(address tokenOut, uint256 expectedTokenOut)
        internal
        view
    {
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(WETH_ADDR).allowance(tychoRouterAddr, PROPAMM_ROUTER), 0
        );
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), expectedTokenOut);
    }
}
