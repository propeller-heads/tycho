// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/**
 * @title FeeStructs
 * @notice Shared fee-related data structures used across the protocol
 */

struct FeeRecipient {
    address recipient;
    uint256 feeAmount;
}

struct FeeInput {
    uint256 actualAmountOut;
    uint256 expectedAmountOut;
    uint256 amountIn;
    address tokenIn;
    address tokenOut;
    uint32 clientFeeBps;
    address client;
}
