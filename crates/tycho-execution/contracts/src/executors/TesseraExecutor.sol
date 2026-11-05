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

    /// @notice What a swap of `amountSpecified` would take and pay, priced for the
    /// caller. Leaves out the treasury's balance, which the swap needs.
    function tesseraSwapViewAmounts(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified
    ) external view returns (uint256 amountIn, uint256 amountOut);
}

error TesseraExecutor__ZeroTesseraSwapAddress();
error TesseraExecutor__InvalidDataLength();
error TesseraExecutor__AmountTooLarge();

/// @title TesseraExecutor
/// @notice Swaps exact-in on Tessera through its `TesseraSwap` router.
/// @dev Swap data is `tokenIn ++ tokenOut`, packed, exactly 40 bytes; any
/// other length reverts with `TesseraExecutor__InvalidDataLength`, and an
/// `amountIn` above `int256.max` with `TesseraExecutor__AmountTooLarge`.
/// TesseraSwap pulls the input from its caller (the router, which approves it
/// for `amountIn`) into Tessera's treasury, and the treasury pays the output
/// straight to `receiver`. TesseraSwap prices the swap for both its caller and
/// `tx.origin`, and the pair scans the caller's code on every swap, which costs
/// about 2M gas when the caller is the router.
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
        // The pair reads swapData's first byte as a fee tag and charges that
        // tag's fee; empty swapData selects tag 0.
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
