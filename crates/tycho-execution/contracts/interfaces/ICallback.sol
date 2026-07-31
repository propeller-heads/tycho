// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../src/TransferManager.sol";

interface ICallback {
    /**
     * @notice Handles callback data from a protocol or contract interaction.
     * @dev This method processes callback data and returns a result. Implementations
     * should handle the specific callback logic required by the protocol.
     *
     * @param data The encoded callback data to be processed.
     * @return result The encoded result of the callback processing.
     */
    function handleCallback(
        bytes calldata data
    ) external returns (bytes memory result);

    /**
     * @notice Gets transfer data for callback-based token transfers.
     * @dev Used by the Dispatcher during protocol callbacks to determine
     * if and how to transfer tokens. Some protocols require all token transfers
     * to happen within the callback context rather than before swap execution.
     *
     * The Dispatcher reads the input token from its own transient storage (set
     * during getTransferData) and passes it here, preventing protocols from
     * injecting a false token via crafted callback data.
     *
     * @param data The encoded callback data.
     * @param tokenIn The address of the input token to transfer.
     * @param caller The address that triggered the callback (msg.sender
     *     in the fallback).
     * @return transferType The transfer type for this executor (None,
     *     ProtocolWillDebit, or Transfer).
     * @return receiver The address that should receive the pre swap
     *     tokens (usually a pool or the TychoRouter - depending on the
     *     protocol)
     */
    function getCallbackTransferData(
        bytes calldata data,
        address tokenIn,
        address caller
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver);
}
