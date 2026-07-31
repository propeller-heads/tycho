// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "../src/TransferManager.sol";

pragma abicoder v2;

interface IExecutor {
    /**
     * @notice Performs a swap on a liquidity pool.
     * @dev Executes the swap using the provided data. The Dispatcher measures output
     * via balance checks, so executors do not need to report the amount received.
     *
     * Note Part of the informal interface is that the executor supports sending the received
     *  tokens to a receiver address. If the underlying smart contract does not provide this
     *  functionality consider adding an additional transfer in the implementation.
     *
     * @param amountIn The amount of the input token to swap.
     * @param data Data that holds information necessary to perform the swap.
     * @param receiver The address where the output tokens will be sent.
     */
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable;

    /**
     * @notice Gets transfer data for pre-swap token transfers.
     * @dev Used by the Dispatcher to determine if tokens need to be transferred
     * before executing the protocol's swap code. Some protocols require tokens
     * to be transferred upfront, while others handle transfers in callbacks.
     *
     * @param data The encoded swap data.
     * @return transferType The transfer type for this executor (None, ProtocolWillDebit, Transfer or TransferNativeInExecutor).
     * @return receiver The address that should receive the pre swap tokens (usually a pool or the TychoRouter - depending on the protocol).
     * @return tokenIn The address of the input token to transfer.
     * @return tokenOut The address of the output token.
     * @return outputToRouter Whether the protocol sends output to msg.sender
     *  rather than accepting a receiver parameter.
     */
    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        );

    /**
     * @dev Returns where funds from the previous swap should be sent in a sequential swap case.
     * For example we might have a swap WETH --(1)--> USDC --(2)--> DAI.
     * Before we perform swap 1 we need to know the receiver of the token out. If the protocol of swap 2 can
     * receive funds directly (e.g., to the pool), this function returns that address. Otherwise, it returns
     * the router address (msg.sender) indicating funds should be sent to the router
     * before this swap.
     * @param data The encoded swap data.
     * @return receiver Address where to send the funds to. Returns msg.sender if funds should stay in router,
     * or the target address (e.g., pool) if direct transfer is supported.
     */
    function fundsExpectedAddress(bytes calldata data) external view returns (address receiver);
}

interface IExecutorErrors {
    error InvalidParameterLength(uint256);
    error UnknownPoolType(uint8);
}
