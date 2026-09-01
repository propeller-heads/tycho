pragma solidity ^0.8.26;

import {
    SelfAccountingFallbackExecutor,
    SelfAccountingFallbackExecutor__InsufficientRouterBalance
} from "@src/executors/SelfAccountingFallbackExecutor.sol";
import "./TychoRouterTestSetup.sol";

/// @dev Stands in for the primary Uniswap V3-style pool to force the
/// executor onto its fallback leg deterministically.
contract RevertingV3Pool {
    error RevertingV3Pool__AlwaysReverts();

    function swap(address, bool, int256, uint160, bytes calldata)
        external
        pure
        returns (int256, int256)
    {
        revert RevertingV3Pool__AlwaysReverts();
    }
}

/// @notice Fork tests for the EXPERIMENTAL SelfAccountingFallbackExecutor.
/// The executor bypasses the Dispatcher's transfer machinery on purpose;
/// these tests verify that its replicated delta accounting keeps
/// _finalizeBalances passing (the router-level call succeeding proves it).
contract SelfAccountingFallbackExecutorTest is TychoRouterTestSetup {
    SelfAccountingFallbackExecutor saExecutor;
    RevertingV3Pool revertingPool;

    // DAI/USDC 0.01% Uniswap V3 pool (token0 = DAI, token1 = USDC)
    address constant DAI_USDC_USV3 = 0x5777d92f208679DB4b9778590Fa3CAB3aC9e2168;

    function setUp() public override {
        super.setUp();
        saExecutor = new SelfAccountingFallbackExecutor();
        revertingPool = new RevertingV3Pool();
        _approveNewExecutor(address(saExecutor));
    }

    function _approveNewExecutor(address executor) internal {
        uint256 forkBlockTime = vm.getBlockTimestamp();
        vm.warp(forkBlockTime - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        address[] memory targets = new address[](1);
        targets[0] = executor;
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(targets);
        vm.warp(forkBlockTime);
    }

    function encodeSelfAccountingSwap(address primaryPool)
        internal
        view
        returns (bytes memory)
    {
        // DAI -> USDC: primary is a V3-style pool (zeroForOne = true since
        // DAI < USDC), fallback is the DAI/USDC Uniswap V2 pair.
        return abi.encodePacked(
            DAI_ADDR,
            USDC_ADDR,
            primaryPool,
            true,
            0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5 // DAI_USDC_POOL (V2)
        );
    }

    function _sequentialSwaps(address primaryPool)
        internal
        view
        returns (bytes memory)
    {
        // WETH --(USV2)--> DAI --(SelfAccountingFallback)--> USDC
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        swaps[1] = encodeSequentialSwap(
            address(saExecutor), encodeSelfAccountingSwap(primaryPool)
        );
        return pleEncode(swaps);
    }

    function testSequentialPrimarySucceeds() public {
        // Middle-hop input is router-held DAI; the primary V3 leg executes.
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            _sequentialSwaps(DAI_USDC_USV3)
        );
        vm.stopPrank();

        // _finalizeBalances passed (the call did not revert) and the output
        // landed at the receiver, measured by the Dispatcher's balance diff.
        assertEq(amountOut, 2018765737);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 2018765737);
        // No tokens dangling at the router.
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialPrimaryRevertsFallbackExecutes() public {
        // The primary pool always reverts; the executor runs the V2 fallback
        // leg. The output must match the pure USV2 route WETH->DAI->USDC
        // (see TychoRouterSequentialSwap.testSequentialSwapTransferFrom).
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            WETH_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            _sequentialSwaps(address(revertingPool))
        );
        vm.stopPrank();

        assertEq(amountOut, 2005810530);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 2005810530);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testVaultFundedPrimarySucceeds() public {
        // Vault-funded single swap: input is router-held (vault deposit).
        // _finalizeBalances must burn exactly the input delta from ALICE's
        // vault balance.
        uint256 amountIn = 2000 ether;
        deal(DAI_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amountIn);
        tychoRouter.deposit(DAI_ADDR, amountIn);

        uint256 amountOut = tychoRouter.singleSwapUsingVault(
            amountIn,
            DAI_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            encodeSingleSwap(
                address(saExecutor), encodeSelfAccountingSwap(DAI_USDC_USV3)
            )
        );
        vm.stopPrank();

        assertEq(amountOut, 1999948780);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 1999948780);
        // Vault DAI burned in _finalizeBalances, nothing dangling.
        assertEq(tychoRouter.balanceOf(ALICE, uint256(uint160(DAI_ADDR))), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testVaultFundedPrimaryRevertsFallbackExecutes() public {
        uint256 amountIn = 2000 ether;
        deal(DAI_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amountIn);
        tychoRouter.deposit(DAI_ADDR, amountIn);

        uint256 amountOut = tychoRouter.singleSwapUsingVault(
            amountIn,
            DAI_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            encodeSingleSwap(
                address(saExecutor),
                encodeSelfAccountingSwap(address(revertingPool))
            )
        );
        vm.stopPrank();

        assertEq(amountOut, 1987164840);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 1987164840);
        assertEq(tychoRouter.balanceOf(ALICE, uint256(uint160(DAI_ADDR))), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testFirstSwapWithUserFundsUnsupported() public {
        // With TransferType.None the Dispatcher never pulls user funds, so a
        // first swap funded from a wallet finds no router-held input and
        // reverts with the executor's documented error.
        uint256 amountIn = 2000 ether;
        deal(DAI_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amountIn);

        vm.expectRevert(
            abi.encodeWithSelector(
                SelfAccountingFallbackExecutor__InsufficientRouterBalance.selector,
                0,
                amountIn
            )
        );
        tychoRouter.singleSwap(
            amountIn,
            DAI_ADDR,
            USDC_ADDR,
            2000_000000, // expected amount out
            2000_000000 * 9800 / 10000, // min amount out
            ALICE,
            noClientFee(),
            encodeSingleSwap(
                address(saExecutor), encodeSelfAccountingSwap(DAI_USDC_USV3)
            )
        );
        vm.stopPrank();
    }
}
