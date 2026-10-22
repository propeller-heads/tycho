// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error BiconomyExecutor__InvalidDataLength();
error BiconomyExecutor__NativeTokenNotSupported();

/// @title BiconomyExecutor
/// @notice Swaps on the Biconomy PropAMM venue. The venue is push-payment
/// (`IPropAMM`): `amountIn` of `tokenIn` is transferred to the venue before
/// `swap` is called in the same transaction, and the venue delivers `tokenOut`
/// to the receiver. The venue only handles ERC20 tokens.
contract BiconomyExecutor is IExecutor {
    uint256 internal constant DATA_LENGTH = 60;

    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address receiver)
    {
        (receiver,,) = _decodeData(data);
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address venue, address tokenIn, address tokenOut) = _decodeData(data);
        // slither-disable-next-line unused-return
        IPropAMM(venue)
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
        returns (address venue, address tokenIn, address tokenOut)
    {
        if (data.length != DATA_LENGTH) {
            revert BiconomyExecutor__InvalidDataLength();
        }
        venue = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        if (tokenIn == ETH_ADDRESS || tokenOut == ETH_ADDRESS) {
            revert BiconomyExecutor__NativeTokenNotSupported();
        }
    }
}
