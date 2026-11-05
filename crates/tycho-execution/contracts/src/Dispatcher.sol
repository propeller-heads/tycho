// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {TransientSlot} from "@openzeppelin/contracts/utils/TransientSlot.sol";
import {LowLevelCall} from "@openzeppelin/contracts/utils/LowLevelCall.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "./TransferManager.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
error Dispatcher__UnapprovedExecutor(address executor);
error Dispatcher__ExecutorIsTimelocked(address executor);
error Dispatcher__NonContractExecutor();
error Dispatcher__UnsupportedSingleHopCycle(address token);
error Dispatcher__ExecutorAlreadyExists(address executor);
error Dispatcher__SwapReverted(address executor);
error Dispatcher__CallbackReverted(address executor);

// Shared with TychoSubsidizingExecutor, which points the router's fallback at
// its inner executor.
TransientSlot.AddressSlot constant CURRENTLY_SWAPPING_EXECUTOR_SLOT =
    TransientSlot.AddressSlot
        .wrap(keccak256("Dispatcher#CURRENTLY_SWAPPING_EXECUTOR_SLOT"));

/**
 * @title Dispatcher - Dispatch execution to external contracts
 * @dev Provides the ability to delegate execution of swaps to external
 *  contracts. This allows dynamically adding new supported protocols
 *  without needing to upgrade any contracts. External contracts will
 *  be called using delegatecall so they can share state with the main
 *  contract if needed.
 *
 *  Note: Executor contracts need to implement the IExecutor interface unless
 *  an alternate selector is specified.
 */
contract Dispatcher is TransferManager {
    // One directive per slot type: Slither 0.11.6 can fail to analyse a
    // contract that uses `using TransientSlot for *`.
    using TransientSlot for TransientSlot.AddressSlot;
    using TransientSlot for TransientSlot.BooleanSlot;
    using TransientSlot for TransientSlot.Uint256Slot;
    using SafeERC20 for IERC20;

    mapping(address => uint256) public executorsActivationTimestamp;

    TransientSlot.BooleanSlot private constant _IS_SPLIT_SWAP_SLOT =
        TransientSlot.BooleanSlot
            .wrap(keccak256("Dispatcher#IS_SPLIT_SWAP_SLOT"));
    TransientSlot.BooleanSlot private constant _IS_FIRST_SWAP_SLOT =
        TransientSlot.BooleanSlot
            .wrap(keccak256("Dispatcher#IS_FIRST_SWAP_SLOT"));
    TransientSlot.Uint256Slot private constant _SWAP_INPUT_AMOUNT_SLOT =
        TransientSlot.Uint256Slot
            .wrap(keccak256("Dispatcher#SWAP_INPUT_AMOUNT_SLOT"));
    TransientSlot.AddressSlot private constant _SWAP_INPUT_TOKEN_SLOT =
        TransientSlot.AddressSlot
            .wrap(keccak256("Dispatcher#SWAP_INPUT_TOKEN_SLOT"));

    uint256 public constant DELAY_EXECUTOR_ACTIVATION = 1 days;

    event ExecutorSet(address indexed executor, uint256 timelockExpiresAt);
    event ExecutorRemoved(address indexed executor);

    constructor(address permit2_) TransferManager(permit2_) {}

    /**
     * @dev Adds an approved executor contract address if it is a contract.
     * @param target address of the executor contract
     */
    function _setExecutor(address target) internal {
        if (target.code.length == 0) {
            // forge-lint: disable-next-line(require-revert-in-loop) setExecutors applies a batch completely or not at all.
            revert Dispatcher__NonContractExecutor();
        }

        if (executorsActivationTimestamp[target] != 0) {
            // forge-lint: disable-next-line(require-revert-in-loop) setExecutors applies a batch completely or not at all.
            revert Dispatcher__ExecutorAlreadyExists(target);
        }

        uint256 timelockExpiresAt = block.timestamp + DELAY_EXECUTOR_ACTIVATION;
        executorsActivationTimestamp[target] = timelockExpiresAt;
        emit ExecutorSet(target, timelockExpiresAt);
    }

    /**
     * @dev Removes an approved executor contract address
     * @param target address of the executor contract
     */
    function _removeExecutor(address target) internal {
        delete executorsActivationTimestamp[target];
        emit ExecutorRemoved(target);
    }

    /**
     * @dev Calls an executor, assumes swap.protocolData contains
     *  protocol-specific data required by the executor.
     */
    // slither-disable-next-line controlled-delegatecall
    function _callSwapOnExecutor(
        address executor,
        uint256 amount,
        bytes calldata data,
        bool isFirstSwap,
        bool isSplitSwap,
        address receiver
    ) internal returns (uint256 amountOut) {
        _validateExecutor(executor);

        CURRENTLY_SWAPPING_EXECUTOR_SLOT.tstore(executor);
        _IS_FIRST_SWAP_SLOT.tstore(isFirstSwap);
        _IS_SPLIT_SWAP_SLOT.tstore(isSplitSwap);
        _SWAP_INPUT_AMOUNT_SLOT.tstore(amount);

        // forge-lint: disable-next-item(calls-loop) Each hop calls its own executor; a failing hop fails the route.
        (
            TransferManager.TransferType transferType,
            address transferReceiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = IExecutor(executor).getTransferData(data);

        if (tokenIn == tokenOut) {
            // Single-hop cycles (such as those made possible via UniswapV4 flash
            // accounting) are not supported.
            // forge-lint: disable-next-line(require-revert-in-loop) A route runs completely or not at all.
            revert Dispatcher__UnsupportedSingleHopCycle(tokenIn);
        }

        // Store tokenIn so it can be passed to getCallbackTransferData if a
        // callback occurs during the swap.
        _SWAP_INPUT_TOKEN_SLOT.tstore(tokenIn);

        // Measure output before _transfer so cyclic handling is uniform
        // across callback and direct-transfer types.
        address measureAt = outputToRouter ? address(this) : receiver;
        uint256 balanceBeforeSwap = _balanceOf(tokenOut, measureAt);

        amount = _transfer(
            transferReceiver,
            transferType,
            tokenIn,
            amount,
            isFirstSwap,
            isSplitSwap,
            false
        );

        // forge-lint: disable-next-item(delegatecall-loop, low-level-calls, calls-loop) Each hop delegatecalls its own executor and bubbles its revert data.
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(
                IExecutor.swap.selector, amount, data, receiver
            )
        );

        // Clear transient storage in case no callback was performed
        CURRENTLY_SWAPPING_EXECUTOR_SLOT.tstore(address(0));
        _IS_FIRST_SWAP_SLOT.tstore(false);
        _IS_SPLIT_SWAP_SLOT.tstore(false);
        _SWAP_INPUT_AMOUNT_SLOT.tstore(0);
        _SWAP_INPUT_TOKEN_SLOT.tstore(address(0));

        // Revoke any lingering allowance the protocol didn't consume.
        if (transferType == TransferType.ProtocolWillDebit) {
            _revokeUnconsumedApproval(tokenIn, transferReceiver);
        }

        if (!success) {
            if (result.length == 0) {
                // forge-lint: disable-next-line(require-revert-in-loop) A route runs completely or not at all.
                revert Dispatcher__SwapReverted(executor);
            }
            // Bubble the executor's revert payload byte-for-byte so callers
            // see the original Error(string)/Panic/custom-error.
            // forge-lint: disable-next-line(inline-assembly) LowLevelCall.bubbleRevert(result) exceeds the stack limit here, and a later call overwrites the return data.
            assembly {
                // forge-lint: disable-next-line(require-revert-in-loop) A route runs completely or not at all.
                revert(add(result, 0x20), mload(result))
            }
        }

        uint256 balanceAfterSwap = _balanceOf(tokenOut, measureAt);
        amountOut = balanceAfterSwap - balanceBeforeSwap;

        // Forward if output landed at router but needs to go elsewhere
        if (outputToRouter && receiver != address(this)) {
            amountOut = _transferOut(tokenOut, receiver, amountOut);
        }

        // Delta accounting if tokens stay at router
        if (receiver == address(this)) {
            _updateDeltaAccounting(tokenOut, int256(amountOut));
        }
    }

    function _callHandleCallbackOnExecutor(bytes calldata data, address caller)
        internal
        returns (bytes memory)
    {
        address executor;
        bool isFirstSwap;
        bool isSplitSwap;
        uint256 amount;
        address tokenIn;
        executor = CURRENTLY_SWAPPING_EXECUTOR_SLOT.tload();
        isFirstSwap = _IS_FIRST_SWAP_SLOT.tload();
        isSplitSwap = _IS_SPLIT_SWAP_SLOT.tload();
        amount = _SWAP_INPUT_AMOUNT_SLOT.tload();
        tokenIn = _SWAP_INPUT_TOKEN_SLOT.tload();

        _validateExecutor(executor);

        (TransferManager.TransferType transferType, address receiver) =
            ICallback(executor).getCallbackTransferData(data, tokenIn, caller);

        _transfer(
            receiver,
            transferType,
            tokenIn,
            amount,
            isFirstSwap,
            isSplitSwap,
            true
        );

        // forge-lint: disable-next-item(low-level-calls)
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(ICallback.handleCallback.selector, data)
        );

        if (!success) {
            if (result.length == 0) {
                revert Dispatcher__CallbackReverted(executor);
            }
            // Bubble the executor's revert payload byte-for-byte so callers
            // see the original Error(string)/Panic/custom-error
            LowLevelCall.bubbleRevert(result);
        }

        // Revoke any lingering allowance the protocol didn't consume.
        if (transferType == TransferType.ProtocolWillDebit) {
            _revokeUnconsumedApproval(tokenIn, receiver);
        }

        // to prevent multiple callbacks
        CURRENTLY_SWAPPING_EXECUTOR_SLOT.tstore(address(0));
        _IS_FIRST_SWAP_SLOT.tstore(false);
        _IS_SPLIT_SWAP_SLOT.tstore(false);

        // The final callback result should not be ABI encoded. That is why we are decoding here.
        // ABI encoding is very gas expensive and we want to avoid it if possible.
        // The result from `handleCallback` is always ABI encoded.
        bytes memory decodedResult = abi.decode(result, (bytes));
        return decodedResult;
    }

    function _callFundsExpectedAddress(address executor, bytes calldata data)
        internal
        view
        returns (address receiver)
    {
        _validateExecutor(executor);

        // forge-lint: disable-next-line(calls-loop) Each hop calls its own executor; a failing hop fails the route.
        receiver = IExecutor(executor).fundsExpectedAddress(data);
    }

    function _validateExecutor(address executor) private view {
        uint256 activationTimestamp = executorsActivationTimestamp[executor];

        if (activationTimestamp == 0) {
            // forge-lint: disable-next-line(require-revert-in-loop) A hop through an unapproved executor fails the route.
            revert Dispatcher__UnapprovedExecutor(executor);
        }
        // forge-lint: disable-next-line(block-timestamp) A one-day timelock is unaffected by seconds of block-time drift.
        if (block.timestamp < activationTimestamp) {
            // forge-lint: disable-next-line(require-revert-in-loop) A hop through an unapproved executor fails the route.
            revert Dispatcher__ExecutorIsTimelocked(executor);
        }
    }
}
