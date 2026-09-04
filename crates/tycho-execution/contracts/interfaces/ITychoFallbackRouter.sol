// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @title ITychoFallbackRouter
/// @notice The subset of `TychoFallbackRouter` that `FallbackExecutor` calls.
interface ITychoFallbackRouter {
    /// @notice Runs `pamm` and, only if it fails, `fallbackSwap`. A failing fallback reverts the
    /// swap; there is no third attempt.
    /// @dev Push-payment: the caller MUST transfer `amountIn` of `tokenIn` here first. Native ETH
    /// is not supported. `fallbackSwap` is `[venue: uint8][venue data]`, and no venue kind is a
    /// pAMM.
    /// @return amountOut The `tokenOut` balance increase measured at `receiver`.
    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        address pamm,
        bytes calldata fallbackSwap
    ) external returns (uint256 amountOut);
}
