// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @title IPropAMMRouter
/// @notice The subset of the PropAMMRouter used by `PropAMMFallbackExecutor`.
/// @dev The router serving Titan's pAMM ecosystem, written by LambdaClass. Full interface:
/// https://github.com/lambdaclass/propamm-router-contracts
/// Mainnet: `0x4DdF368080CD7946db5b459aD591c350158175e1`.
interface IPropAMMRouter {
    /// @notice Swaps `amountIn` through `venue`, retrying on a single-hop Uniswap V3 pool if the
    /// venue reverts.
    /// @dev The router pulls `amountIn` with `transferFrom`, so the caller must have approved it.
    /// A `venue` that is not whitelisted reverts `UnknownVenue`.
    /// @param venue The pAMM to try first.
    /// @param tokenIn The token being sold.
    /// @param tokenOut The token being bought.
    /// @param amountIn The exact amount of `tokenIn` to sell.
    /// @param amountOutMin The minimum acceptable amount of `tokenOut`.
    /// @param recipient The address that receives `tokenOut`.
    /// @param deadline Unix timestamp after which the swap is no longer valid.
    /// @return amountOut The amount of `tokenOut` delivered to `recipient`.
    /// @return executedVenue The venue that filled, or the Uniswap V3 fallback router address.
    function swapViaVenueV1(
        address venue,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 amountOutMin,
        address recipient,
        uint256 deadline
    ) external payable returns (uint256 amountOut, address executedVenue);

    /// @notice The Uniswap V3 fee tier the retry uses for a pair.
    /// @dev Per-pair override if set, else the global `fallbackFee`. Read off-chain to price the
    /// retry and check a pool exists at that tier.
    function resolvedFee(address tokenIn, address tokenOut)
        external
        view
        returns (uint24 fee);
}
