// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

error FallbackExecutor__AddressZero();
error FallbackExecutor__InvalidDataLength(uint256 length);

/// @title FallbackExecutor
/// @notice Runs one swap through a `TychoFallbackRouter`.
/// @dev `TransferType.Transfer` sends `amountIn` to the fallback router, which then owns the
/// tokens and pays each protocol itself. The router address is immutable, so the executor only
/// ever calls this one contract. Every address inside the swap data is called by the router, not
/// by the executor.
///
/// Every protocol gets `minAmountOut = 0`, since a binding value would revert the trades the
/// fallback exists to rescue. The TychoRouter's route-level `minAmountOut` must clear the price
/// the fallback fills at.
abstract contract FallbackExecutor is IExecutor {
    address public immutable fallbackRouter;

    constructor(address fallbackRouter_) {
        if (fallbackRouter_ == address(0)) {
            revert FallbackExecutor__AddressZero();
        }
        fallbackRouter = fallbackRouter_;
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return fallbackRouter;
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        _validateDataLength(data);
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        transferType = TransferManager.TransferType.Transfer;
        receiver = fallbackRouter;
        outputToRouter = false;
    }

    /// @notice Reverts `FallbackExecutor__InvalidDataLength` on data it cannot decode.
    function _validateDataLength(bytes calldata data) internal pure virtual;
}
