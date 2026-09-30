// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IMetricPool} from "../executors/MetricExecutor.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

/// @notice Metric's periphery lens; see
/// https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/smart-contracts-reference/get-quote
interface IMetricOmmSwapQuoter {
    function quoteLiveExactInSingle(
        address pool,
        bool zeroForOne,
        uint128 amountIn,
        uint128 priceLimitX64
    ) external returns (uint256 amountIn_, uint256 amountOut);
}

error MetricFallbackRouter__AddressZero();
error MetricFallbackRouter__AmountInTooLarge(uint256 amountIn);
error MetricFallbackRouter__InvalidDataLength(uint256 length);

/// @title MetricFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is a MetricOmm pool, quoted through Metric's
/// `MetricOmmSwapQuoter`.
contract MetricFallbackRouter is TychoFallbackRouter {
    uint256 private constant _INT128_MAX = uint256(uint128(type(int128).max));

    IMetricOmmSwapQuoter public immutable metricQuoter;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        IMetricOmmSwapQuoter metricQuoter_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (address(metricQuoter_) == address(0)) {
            revert MetricFallbackRouter__AddressZero();
        }
        metricQuoter = metricQuoter_;
    }

    /// @notice Quotes `pool` and `fallbackSwap` and runs whichever quotes more.
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

    function _quotePrimary(
        Swap calldata swap_,
        address pool,
        bytes calldata metricData
    ) internal override returns (uint256 amountOut) {
        if (swap_.amountIn > _INT128_MAX) {
            return 0;
        }
        bool zeroForOne = uint8(metricData[0]) > 0;
        // slither-disable-next-line unused-return
        try metricQuoter.quoteLiveExactInSingle(
            pool,
            zeroForOne,
            // Checked against int128's maximum above.
            // forge-lint: disable-next-line(unsafe-typecast)
            uint128(swap_.amountIn),
            _priceLimit(zeroForOne)
        ) returns (
            uint256, uint256 quotedAmountOut
        ) {
            return quotedAmountOut;
        } catch {
            return 0;
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
