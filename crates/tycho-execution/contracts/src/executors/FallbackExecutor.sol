// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ITychoFallbackRouter} from "@interfaces/ITychoFallbackRouter.sol";
import {TransferManager} from "../TransferManager.sol";

error FallbackExecutor__AddressZero();
error FallbackExecutor__InvalidDataLength();

/// @title FallbackExecutor
/// @notice Runs one leg through `TychoFallbackRouter`.
/// @dev `TransferType.Transfer` sends `amountIn` to the fallback router, which then owns the
/// tokens and pays each venue itself. The router address is immutable, so no swap data can select
/// a call target outside it.
///
/// Every venue gets `minAmountOut = 0`, since a binding value would revert the trades the fallback
/// exists to rescue. The caller's leg-level `minAmountOut` must clear the fallback venue.
contract FallbackExecutor is IExecutor {
    ITychoFallbackRouter public immutable fallbackRouter;

    constructor(address fallbackRouter_) {
        if (fallbackRouter_ == address(0)) {
            revert FallbackExecutor__AddressZero();
        }
        fallbackRouter = ITychoFallbackRouter(fallbackRouter_);
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

        // slither-disable-next-line unused-return
        fallbackRouter.swap(
            tokenIn, tokenOut, amountIn, receiver, pamm, fallbackSwap
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

    /// @dev Data layout: `[tokenIn: 20][tokenOut: 20][pamm: 20][fallback: rest]`, where the
    /// fallback is `[venue: uint8][venue data]`. The pAMM is a bare address, so no length prefix
    /// is needed to find where the fallback starts.
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
            revert FallbackExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        pamm = address(bytes20(data[40:60]));
        fallbackSwap = data[60:];
    }
}
