// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {TransferManager} from "../TransferManager.sol";

error PropAMMExecutor__InvalidDataLength();

/// @title PropAMMExecutor
/// @notice Generic executor for any pAMM implementing the standard `IPropAMM`
/// interface, priced off-chain via the Titan price level stream.
/// @dev The pAMM address is part of the swap data instead of an immutable, so
/// a single deployment serves all venues following the standard interface and
/// new pAMMs need no new executor. `IPropAMM` uses a push-payment model: the
/// router transfers `amountIn` of `tokenIn` to the pAMM before this executor
/// runs (`TransferType.Transfer` with the pAMM as receiver), and `swap`
/// consumes that balance.
contract PropAMMExecutor is IExecutor {
    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address receiver)
    {
        (receiver,,) = _decodeData(data);
    }

    // The router enforces the user's minAmountOut and the Dispatcher measures
    // the output via balance diff, so the protocol-level minAmountOut is 0.
    // The deadline check is the router's responsibility too; block.timestamp
    // is always within any still-valid deadline.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address pamm, address tokenIn, address tokenOut) = _decodeData(data);

        // slither-disable-next-line unused-return
        IPropAMM(pamm)
            .swap(tokenIn, tokenOut, amountIn, 0, receiver, block.timestamp);
    }

    function getTransferData(bytes calldata data)
        external
        pure
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (receiver, tokenIn, tokenOut) = _decodeData(data);
        transferType = TransferManager.TransferType.Transfer;
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address pamm, address tokenIn, address tokenOut)
    {
        if (data.length != 60) {
            revert PropAMMExecutor__InvalidDataLength();
        }

        pamm = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }
}
