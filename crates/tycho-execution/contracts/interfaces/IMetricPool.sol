// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @notice MetricOmmPool swap interface; see
/// https://docs.metric.xyz/RSm94m71kqtGICv4iKRj
/// -> Developers -> Smart Contracts Reference -> Swapping directly via pool.
/// @dev The pool pulls its input through `metricOmmSwapCallback(int256,int256,bytes)` on the
/// caller.
interface IMetricPool {
    function swap(
        address recipient,
        bool zeroForOne,
        int128 amountSpecified,
        uint128 priceLimitX64,
        bytes calldata callbackData,
        bytes calldata extensionData
    ) external returns (int128 amount0Delta, int128 amount1Delta);
}
