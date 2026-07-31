// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IFermiSwapper {
    function fermiSwapWithAllowances(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256 amountCheck,
        address recipient
    ) external returns (uint256 amountIn, uint256 amountOut);
}

error FermiSwapExecutor__ZeroSwapperAddress();
error FermiSwapExecutor__InvalidDataLength();
error FermiSwapExecutor__AmountTooLarge();

contract FermiSwapExecutor is IExecutor {
    IFermiSwapper public immutable fermiSwapper;

    constructor(address fermiSwapper_) {
        if (fermiSwapper_ == address(0)) {
            revert FermiSwapExecutor__ZeroSwapperAddress();
        }
        fermiSwapper = IFermiSwapper(fermiSwapper_);
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
        if (amountIn > uint256(type(int256).max)) {
            revert FermiSwapExecutor__AmountTooLarge();
        }

        // amountIn is checked above against int256.max.
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 amountSpecified = int256(amountIn);

        // slither-disable-next-line unused-return
        fermiSwapper.fermiSwapWithAllowances(
            tokenIn, tokenOut, amountSpecified, 0, receiver
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
        receiver = address(fermiSwapper);
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut)
    {
        if (data.length != 40) {
            revert FermiSwapExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
    }
}
