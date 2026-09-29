// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IMetricPool} from "@interfaces/IMetricPool.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {
    TychoFallbackRouter,
    TychoFallbackRouter__SimulatedAmountOut
} from "./TychoFallbackRouter.sol";

error MetricFallbackRouter__AmountInTooLarge(uint256 amountIn);
error MetricFallbackRouter__InvalidDataLength(uint256 length);

/// @title MetricFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary venue is a MetricOmm pool.
/// @dev Metric has no quote function, so the quote runs the real swap in `simulateMetric` and
/// reads the output off its revert, as a Uniswap V4 fallback quote does. The quote costs a full
/// Metric swap on every leg.
///
/// The pool pulls its input through `metricOmmSwapCallback`, which the catch-all `fallback`
/// answers: it pays the full `amountIn` to the pool the swap called, as the Dispatcher does for
/// `MetricExecutor`.
contract MetricFallbackRouter is TychoFallbackRouter {
    uint256 private constant _INT128_MAX = uint256(uint128(type(int128).max));

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {}

    /// @notice Quotes `pool` and `fallbackSwap`, then runs the fallback if it quotes more
    /// `tokenOut`, otherwise `pool` and, only if that fails, `fallbackSwap`. A failing fallback
    /// reverts the swap; there is no third attempt.
    /// @dev Permissionless: the caller names every parameter, so a balance sitting in this
    /// contract can be taken by anyone and is considered lost. Push-payment: the caller MUST
    /// transfer `swap_.amountIn` of `swap_.tokenIn` here first. Native ETH, fee-on-transfer and
    /// rebasing tokens are not supported.
    /// @param metricData `[zeroForOne: 1]`, in the pool's own token order.
    function swap(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData,
        bytes calldata fallbackSwap
    ) external {
        if (metricData.length != 1) {
            revert MetricFallbackRouter__InvalidDataLength(metricData.length);
        }
        _swap(swap_, pool, metricData, fallbackSwap);
    }

    /// @notice Runs the Metric swap, then reverts `TychoFallbackRouter__SimulatedAmountOut` with
    /// the amount it delivered so the swap rolls back. External only so `_quotePrimary` can
    /// try/catch it.
    function simulateMetric(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData
    ) external {
        _requireSelf();
        uint256 balanceBefore = IERC20(swap_.tokenOut).balanceOf(swap_.receiver);
        _swapPrimary(swap_, pool, metricData);
        revert TychoFallbackRouter__SimulatedAmountOut(IERC20(swap_.tokenOut)
                    .balanceOf(swap_.receiver) - balanceBefore);
    }

    function _quotePrimary(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData
    ) internal override returns (uint256 amountOut) {
        try this.simulateMetric(swap_, pool, metricData) {
            return 0;
        } catch (bytes memory revertData) {
            return _amountOutFromRevert(
                revertData, TychoFallbackRouter__SimulatedAmountOut.selector
            );
        }
    }

    function _swapPrimary(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData
    ) internal override {
        if (swap_.amountIn > _INT128_MAX) {
            revert MetricFallbackRouter__AmountInTooLarge(swap_.amountIn);
        }
        bool zeroForOne = uint8(metricData[0]) > 0;

        _setCallbackContext(pool, swap_.tokenIn, swap_.amountIn);
        // slither-disable-next-line unused-return
        IMetricPool(pool)
            .swap(
                swap_.receiver,
                zeroForOne,
                // Checked against int128's maximum above.
                // forge-lint: disable-next-line(unsafe-typecast)
                int128(uint128(swap_.amountIn)),
                zeroForOne ? 0 : type(uint128).max,
                "",
                ""
            );
        _clearCallbackContext();
    }
}
