// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {IPartyPool} from "./IPartyPool.sol";

interface IPartyInfo {
    /// @notice returns true iff the pool is not killed and has been initialized
    /// with liquidity.
    function working(IPartyPool pool) external view returns (bool);

    /// @notice Per-asset swap fees in ppm. The effective pair fee for a swap
    /// i->j is fees()[i] + fees()[j]. Moved here from IPartyPool so the pool's
    /// deployed bytecode stays within EIP-170.
    function fees(IPartyPool pool) external view returns (uint256[] memory);

    /// @notice Infinitesimal marginal price for a swap input->output as
    /// Q128.128, denomination-adjusted to external token units. Represents the
    /// cost in input token units to acquire one unit of the output token
    /// (i.e. input-per-output). Fee-free and infinitesimal. On a balanced pool
    /// with equal denominators this returns exactly 1 << 128.
    /// @param inputTokenIndex index of the token being sold
    /// @param outputTokenIndex index of the token being bought
    /// @return price Q128.128 input-per-output external price
    function price(
        IPartyPool pool,
        uint256 inputTokenIndex,
        uint256 outputTokenIndex
    ) external view returns (uint256);

    /// @notice Quote an exact-input swap. The fee is deducted from the output,
    /// not added to the input.
    /// @param pool pool being quoted
    /// @param inputTokenIndex index of token being sold
    /// @param outputTokenIndex index of token being bought
    /// @param maxAmountIn exact input to transfer
    /// @return amountIn exact input transferred (== maxAmountIn)
    /// @return amountOut net output the user receives (gross output minus
    /// outFee) @return outFee fee deducted from the gross output
    function swapAmounts(
        IPartyPool pool,
        uint256 inputTokenIndex,
        uint256 outputTokenIndex,
        uint256 maxAmountIn
    )
        external
        view
        returns (uint256 amountIn, uint256 amountOut, uint256 outFee);

    /// @notice Closed-form exact-output swap quote. Given the desired NET
    /// output (what the caller receives after fee), returns the required input.
    /// Reverts
    /// if amountOut is infeasible.
    /// @param pool pool being quoted
    /// @param inputTokenIndex index of token being sold
    /// @param outputTokenIndex index of token being bought
    /// @param amountOut desired NET output in token units (after fee deduction)
    /// @return amountIn total input required (no fee on input; fee is on
    /// output) @return outFee fee deducted from the gross output
    function swapAmountsForExactOutput(
        IPartyPool pool,
        uint256 inputTokenIndex,
        uint256 outputTokenIndex,
        uint256 amountOut
    ) external view returns (uint256 amountIn, uint256 outFee);
}
