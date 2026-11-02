pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {Vm} from "../../lib/forge-std/src/Vm.sol";

interface INamiPool {
    function plugin() external view returns (address);

    function globalState()
        external
        view
        returns (
            uint160 price,
            int24 tick,
            uint16 lastFee,
            uint8 pluginConfig,
            uint16 communityFee,
            bool unlocked
        );
}

/// Swaps on Nami's live Base WETH/USDC pool (an Algebra Integral pool with the
/// Nami plugin) through TychoRouterV3 with the Uniswap V3 executor: Algebra's
/// `swap` shares Uniswap V3's selector and the router answers
/// `algebraSwapCallback` through its fallback.
contract TychoRouterForNamiV4Test is TychoRouterTestSetup {
    address constant NAMI_WETH_USDC_POOL =
        0x0E8AF367DbA0c9CB4674D38539eF3465A25a7237;
    /// `beforeSwap` flag of the pool's plugin config.
    uint8 constant BEFORE_SWAP_FLAG = 1;
    /// `DYNAMIC_FEE` flag of the pool's plugin config.
    uint8 constant DYNAMIC_FEE_FLAG = 128;
    bytes32 constant SWAP_FEE_TOPIC =
        keccak256("SwapFee(address,uint24,uint24)");

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        // After the last swap Nami made on the pool (block 52562858).
        return 52562900;
    }

    function testPoolUsesThePluginFee() public view {
        INamiPool pool = INamiPool(NAMI_WETH_USDC_POOL);
        (,,, uint8 pluginConfig,,) = pool.globalState();
        assertTrue(pool.plugin() != address(0));
        assertEq(pluginConfig & BEFORE_SWAP_FLAG, BEFORE_SWAP_FLAG);
        assertEq(pluginConfig & DYNAMIC_FEE_FLAG, DYNAMIC_FEE_FLAG);
    }

    function testSingleSwapWethForUsdc() public {
        _swapAndCheck(BASE_WETH, BASE_USDC, 0.001 ether);
    }

    function testSingleSwapUsdcForWeth() public {
        _swapAndCheck(BASE_USDC, BASE_WETH, 2e6);
    }

    function _swapAndCheck(address tokenIn, address tokenOut, uint256 amountIn)
        internal
    {
        uint256 spotAmountOut = _spotAmountOut(tokenIn, tokenOut, amountIn);
        deal(tokenIn, ALICE, amountIn);
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(ALICE);

        // The layout `NamiV4SwapEncoder` produces: the Uniswap V3 executor's
        // with a zero fee field, which the executor does not read.
        bytes memory protocolData = abi.encodePacked(
            tokenIn,
            tokenOut,
            uint24(0),
            NAMI_WETH_USDC_POOL,
            tokenIn < tokenOut
        );
        bytes memory swap =
            encodeSingleSwap(address(usv3Executor), protocolData);

        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        vm.recordLogs();
        uint256 gasBefore = gasleft();
        // The spot amount bounds the output (fee and price impact only lower
        // it), so no positive slippage is captured.
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            spotAmountOut,
            (spotAmountOut * 95) / 100,
            ALICE,
            noClientFee(),
            swap
        );
        uint256 gasUsed = gasBefore - gasleft();
        vm.stopPrank();
        emit log_named_uint("nami_v4 singleSwap gas", gasUsed);

        assertEq(IERC20(tokenOut).balanceOf(ALICE) - balanceBefore, amountOut);
        assertLe(amountOut, spotAmountOut);
        assertGe(amountOut, (spotAmountOut * 95) / 100);
        assertEq(IERC20(tokenIn).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), 0);
        assertGt(_pluginOverrideFee(), 0, "the plugin must set the fee");
    }

    /// `amountIn` converted at the pool's current price, before fee and price
    /// impact.
    function _spotAmountOut(address tokenIn, address tokenOut, uint256 amountIn)
        internal
        view
        returns (uint256)
    {
        (uint160 sqrtPrice,,,,,) = INamiPool(NAMI_WETH_USDC_POOL).globalState();
        uint256 priceX192 = uint256(sqrtPrice) * uint256(sqrtPrice);
        return tokenIn < tokenOut
            ? (amountIn * priceX192) >> 192
            : (amountIn << 192) / priceX192;
    }

    /// The `overrideFee` of the pool's `SwapFee` event in the recorded logs.
    function _pluginOverrideFee() internal returns (uint256) {
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (
                logs[i].emitter == NAMI_WETH_USDC_POOL
                    && logs[i].topics[0] == SWAP_FEE_TOPIC
            ) {
                (uint24 overrideFee,) =
                    abi.decode(logs[i].data, (uint24, uint24));
                return overrideFee;
            }
        }
        revert("pool emitted no SwapFee");
    }
}
