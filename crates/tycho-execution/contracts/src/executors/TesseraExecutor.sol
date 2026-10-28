// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
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

error TesseraExecutor__ZeroTesseraSwapAddress();
error TesseraExecutor__InvalidDataLength();
error TesseraExecutor__AmountTooLarge();

contract TesseraExecutor is IExecutor {
    ITesseraSwap public immutable tesseraSwap;

    constructor(address tesseraSwap_) {
        if (tesseraSwap_ == address(0)) {
            revert TesseraExecutor__ZeroTesseraSwapAddress();
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

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn, address tokenOut) = _decodeData(data);
        // A negative amountSpecified would make TesseraSwap swap exact-out.
        // forge-lint: disable-next-line(unsafe-typecast)
        if (amountIn > uint256(type(int256).max)) {
            revert TesseraExecutor__AmountTooLarge();
        }
        // amountIn is checked above against int256.max.
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 amountSpecified = int256(amountIn);
        // Empty swapData selects TesseraSwap's default fee tag.
        tesseraSwap.tesseraSwapWithAllowances(
            tokenIn, tokenOut, amountSpecified, 0, receiver, ""
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
