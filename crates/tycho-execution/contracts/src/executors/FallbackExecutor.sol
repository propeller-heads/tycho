// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {TransferManager} from "../TransferManager.sol";

error FallbackExecutor__AddressZero();
error FallbackExecutor__InvalidDataLength(uint256 length);

/// @title FallbackExecutor
/// @notice Runs one swap through `TychoFallbackRouter`.
/// @dev `TransferType.Transfer` sends `amountIn` to the fallback router, which then owns the
/// tokens and pays each protocol itself. The router address is immutable, so the executor only
/// ever calls this one contract. Every address inside the swap data is called by the router, not
/// by the executor.
///
/// Every protocol gets `minAmountOut = 0`, since a binding value would revert the trades the
/// fallback exists to rescue. The TychoRouter's route-level `minAmountOut` must clear the price
/// the fallback fills at.
contract FallbackExecutor is IExecutor {
    TychoFallbackRouter public immutable fallbackRouter;

    constructor(address fallbackRouter_) {
        if (fallbackRouter_ == address(0)) {
            revert FallbackExecutor__AddressZero();
        }
        fallbackRouter = TychoFallbackRouter(fallbackRouter_);
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return address(fallbackRouter);
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (
            address tokenIn,
            address tokenOut,
            address pamm,
            bytes calldata fallbackSwap
        ) = _decodeData(data);

        fallbackRouter.swap(
            TychoFallbackRouter.Swap({
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                amountIn: amountIn,
                receiver: receiver
            }),
            pamm,
            fallbackSwap
        );
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
        (tokenIn, tokenOut,,) = _decodeData(data);
        transferType = TransferManager.TransferType.Transfer;
        receiver = address(fallbackRouter);
        outputToRouter = false;
    }

    /// @dev The pAMM is a bare address, so no length prefix is needed to find where the fallback
    /// starts.
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            address pamm,
            bytes calldata fallbackSwap
        )
    {
        if (data.length <= 60) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        pamm = address(bytes20(data[40:60]));
        fallbackSwap = data[60:];
    }
}
