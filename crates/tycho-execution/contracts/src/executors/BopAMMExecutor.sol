// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IBopAmmV2 {
    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        uint256 expiry,
        address recipient
    ) external payable;
}

error BopAMMExecutor__ZeroSettlementAddress();
error BopAMMExecutor__InvalidDataLength();

contract BopAMMExecutor is IExecutor {
    IBopAmmV2 public immutable settlement;

    constructor(address settlement_) {
        if (settlement_ == address(0)) {
            revert BopAMMExecutor__ZeroSettlementAddress();
        }
        settlement = IBopAmmV2(settlement_);
    }

    function fundsExpectedAddress(bytes calldata /* data */ )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // The router enforces the user's minAmountOut and the Dispatcher measures
    // the output via balance diff, so the protocol-level minAmountOut is 0.
    // Settlement requires expiry >= block.timestamp; the swap is only valid in
    // the block whose timestamp matches the book's committed quote anyway.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn, address tokenOut) = _decodeData(data);

        settlement.swap(
            tokenIn, tokenOut, amountIn, 0, block.timestamp, receiver
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
        receiver = address(settlement);
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut)
    {
        if (data.length != 40) {
            revert BopAMMExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
    }
}
