// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IAerodromeV1Pool} from "@interfaces/IAerodromeV1Pool.sol";

/// @notice An AERO basic (stable or volatile) pool. Its factory may add a MEV tax, which depends
/// on the transaction's priority fee, on top of the base fee that `getAmountOut` prices.
interface IAeroBasicPool is IAerodromeV1Pool {
    /// @notice The output for `amountIn`, after the base fee and any MEV tax that the current
    /// transaction pays.
    function getAmountOutWithTotalFee(uint256 amountIn, address tokenIn)
        external
        view
        returns (uint256);
}
