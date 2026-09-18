// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @notice An Aerodrome V1 (Solidly-style) pool. `getAmountOut` prices the trade, fee and
/// stable curve included; `swap` takes the explicit output amounts like Uniswap V2.
interface IAerodromeV1Pool {
    function getAmountOut(uint256 amountIn, address tokenIn)
        external
        view
        returns (uint256);
    function swap(
        uint256 amount0Out,
        uint256 amount1Out,
        address to,
        bytes calldata data
    ) external;
}
