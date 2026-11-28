// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IDollarStore {
    function swapExactInput(
        address offerAsset,
        address wantAsset,
        uint256 amount,
        uint256 minAmountOut,
        address recipient,
        uint256 deadline
    ) external returns (uint256 amountIn, uint256 amountOut);
}

error DollarStoreExecutor__ZeroStoreAddress();
error DollarStoreExecutor__InvalidDataLength();

/// Executes swaps on DollarStore through `swapExactInput`, which fills fully or
/// reverts and never queues. The store (a constructor immutable) pulls the
/// input with `transferFrom`, rejects any token it has not listed
/// (SwapRouteLib.validateRoute), and pays the output to `receiver`.
///
/// The store swaps 1:1 in normalized 6-decimal units and takes whole units
/// only (NormalizationLib.toUnits): for an 18-decimal input, up to 1e12 - 1 wei
/// of `amountIn` is not pulled and stays at the router.
contract DollarStoreExecutor is IExecutor {
    IDollarStore public immutable store;

    constructor(address store_) {
        if (store_ == address(0)) {
            revert DollarStoreExecutor__ZeroStoreAddress();
        }
        store = IDollarStore(store_);
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
    // the output by balance diff, so the protocol-level minAmountOut is 0. The
    // swap fills every whole unit of amountIn or reverts (InsufficientLiquidity),
    // so no input is lost to a partial fill. The store requires
    // block.timestamp <= deadline.
    // slither-disable-next-line locked-ether The router delegatecalls swap, so any ETH it receives is held by the router.
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn, address tokenOut) = _decodeData(data);
        // slither-disable-next-line unused-return The Dispatcher derives the output from balance diffs, not from return values.
        store.swapExactInput(
            tokenIn, tokenOut, amountIn, 0, receiver, block.timestamp
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
        receiver = address(store);
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut)
    {
        if (data.length != 40) {
            revert DollarStoreExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
    }
}
