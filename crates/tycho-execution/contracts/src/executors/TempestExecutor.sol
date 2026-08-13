// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface ITempest {
    function swapWithAllowances(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient,
        uint256 deadline
    ) external returns (uint256 amountOut);
}

error TempestExecutor__ZeroRouterAddress();
error TempestExecutor__InvalidDataLength();

/// @title TempestExecutor
/// @notice Executes swaps against Tempest, Flowdesk's propAMM.
/// @dev Tempest gates every settlement entrypoint on `allowedTaker[msg.sender]`.
/// Because executors run under `delegatecall`, that check sees the TychoRouter
/// address — so TychoRouter must be on Tempest's taker allowlist for swaps to
/// settle.
contract TempestExecutor is IExecutor {
    ITempest public immutable tempest;

    constructor(address tempest_) {
        if (tempest_ == address(0)) {
            revert TempestExecutor__ZeroRouterAddress();
        }
        tempest = ITempest(tempest_);
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

        // minAmountOut is zero and the deadline unbounded: the Dispatcher
        // derives the real output from its own balance diff and TychoRouter
        // enforces the user's minAmountOut, so duplicating either check here
        // would only add a second, weaker bound.
        // slither-disable-next-line unused-return
        tempest.swapWithAllowances(
            tokenIn, tokenOut, amountIn, 0, receiver, type(uint256).max
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
        // Tempest pulls `tokenIn` from the caller inside `swapWithAllowances`
        // and pays `tokenOut` straight to `recipient`.
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = address(tempest);
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut)
    {
        if (data.length != 40) {
            revert TempestExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
    }
}
