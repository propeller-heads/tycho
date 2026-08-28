// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface ITesseraSwap {
    function tesseraSwapWithAllowances(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256 amountCheck,
        address recipient,
        bytes calldata swapData
    ) external;
}

error TesseraExecutor__ZeroEntrypointAddress();
error TesseraExecutor__InvalidDataLength();
error TesseraExecutor__AmountExceedsInt256();

contract TesseraExecutor is IExecutor {
    ITesseraSwap public immutable tesseraSwap;

    constructor(address tesseraSwap_) {
        if (tesseraSwap_ == address(0)) {
            revert TesseraExecutor__ZeroEntrypointAddress();
        }
        tesseraSwap = ITesseraSwap(tesseraSwap_);
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // The router enforces the user's minAmountOut and the Dispatcher measures
    // the output via balance diff, so the protocol-level amountCheck is 0
    // (exact-input: the venue requires `amountOut >= amountCheck`).
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn, address tokenOut) = _decodeData(data);

        if (amountIn > uint256(type(int256).max)) {
            revert TesseraExecutor__AmountExceedsInt256();
        }
        // A positive amountSpecified is an exact-input swap. Bounded by the
        // guard above.
        // forge-lint: disable-next-line(unsafe-typecast)
        tesseraSwap.tesseraSwapWithAllowances(
            tokenIn, tokenOut, int256(amountIn), 0, receiver, ""
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
        (tokenIn, tokenOut) = _decodeData(data);
        // TesseraSwap pulls tokenIn via `transferFrom(msg.sender, treasury)`,
        // so the router must approve it as the spender.
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = address(tesseraSwap);
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut)
    {
        if (data.length != 40) {
            revert TesseraExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
    }
}
