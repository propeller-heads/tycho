// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @notice Fluid's own `FluidDexSwapResult(uint256)`, declared here so the selector matches: a
/// dex asked to pay `0xdEaD` reverts with it before moving any token.
error FluidDexSwapResult(uint256 amountOut);

/// @notice A Fluid V1 dex. `swap0to1_` is the dex's own token order, not the address sort order.
interface IFluidV1Dex {
    function swapInWithCallback(
        bool swap0to1_,
        uint256 amountIn_,
        uint256 amountOutMin_,
        address to_
    ) external payable returns (uint256 amountOut_);

    function swapIn(
        bool swap0to1_,
        uint256 amountIn_,
        uint256 amountOutMin_,
        address to_
    ) external payable returns (uint256 amountOut_);
}
