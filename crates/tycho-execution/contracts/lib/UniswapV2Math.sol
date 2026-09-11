// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

error UniswapV2Math__ZeroReserves();

/// @notice Constant-product output math for Uniswap V2-style pairs.
library UniswapV2Math {
    /// @notice Computes the output a V2-style pair fills for `amountIn`
    /// given its reserves and fee.
    function getAmountOut(
        uint256 amountIn,
        uint112 reserveIn,
        uint112 reserveOut,
        uint256 feeBps
    ) internal pure returns (uint256 amount) {
        if (reserveIn == 0 || reserveOut == 0) {
            revert UniswapV2Math__ZeroReserves();
        }
        uint256 amountInWithFee = amountIn * (10000 - feeBps);
        uint256 numerator = amountInWithFee * uint256(reserveOut);
        uint256 denominator = (uint256(reserveIn) * 10000) + amountInWithFee;
        amount = numerator / denominator;
    }
}
