// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IMetricPool} from "../executors/MetricExecutor.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {
    TychoFallbackRouter,
    TychoFallbackRouter__SimulatedAmountOut
} from "./TychoFallbackRouter.sol";

error MetricFallbackRouter__InvalidDataLength(uint256 length);

/// @title MetricFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is a MetricOmm pool, quoted by simulation.
/// Metric's v1 `MetricOmmSwapQuoter` reverts inside a transaction: its oracle returns a price
/// only to a pool in a swap, or to an `eth_call` from `address(0)`.
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

    /// @notice Quotes `pool` and `fallbackSwap` and runs whichever quotes more; a pool that fails
    /// falls through to `fallbackSwap`.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
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

    /// @dev Runs the pool and reads the output off `simulatePrimary`'s revert. A pool that
    /// reverts quotes zero.
    function _quotePrimary(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData
    ) internal override returns (uint256 amountOut) {
        if (swap_.amountIn > _INT128_MAX) {
            return 0;
        }
        try this.simulatePrimary(swap_, pool, metricData) {
            return 0;
        } catch (bytes memory revertData) {
            return _amountOutFromRevert(
                revertData, TychoFallbackRouter__SimulatedAmountOut.selector
            );
        }
    }

    /// @notice Runs the pool, then reverts `TychoFallbackRouter__SimulatedAmountOut` with the
    /// amount it delivered so the swap rolls back. External only so `_quotePrimary` can
    /// try/catch it.
    function simulatePrimary(
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

    function _swapPrimary(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData
    ) internal override {
        bool zeroForOne = uint8(metricData[0]) > 0;

        _setCallbackContext(pool, swap_.tokenIn, swap_.amountIn);
        // slither-disable-next-line unused-return
        IMetricPool(pool)
            .swap(
                swap_.receiver,
                zeroForOne,
                // `_quotePrimary` quotes zero above int128's maximum, so no swap runs.
                // forge-lint: disable-next-line(unsafe-typecast)
                int128(uint128(swap_.amountIn)),
                _priceLimit(zeroForOne),
                "",
                ""
            );
        _clearCallbackContext();
    }

    /// @dev No limit in either direction, as `MetricExecutor` swaps.
    function _priceLimit(bool zeroForOne) internal pure returns (uint128) {
        return zeroForOne ? 0 : type(uint128).max;
    }
}
