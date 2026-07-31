// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {IFeeCalculator} from "@interfaces/IFeeCalculator.sol";
import {FeeRecipient} from "../lib/FeeStructs.sol";
import {TransferManager} from "./TransferManager.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
error Dispatcher__UnapprovedExecutor(address executor);
error Dispatcher__ExecutorIsTimelocked(address executor);
error Dispatcher__NonContractExecutor();
error Dispatcher__InvalidDataLength();
error Dispatcher__AddressZero();
error Dispatcher__UnsupportedSingleHopCycle(address token);
error Dispatcher__ExecutorAlreadyExists(address executor);
error Dispatcher__SwapReverted(address executor);
error Dispatcher__CallbackReverted(address executor);

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
    using SafeERC20 for IERC20;

    mapping(address => uint256) public executorsActivationTimestamp;

    // keccak256("Dispatcher#CURRENTLY_SWAPPING_EXECUTOR_SLOT")
    uint256 private constant _CURRENTLY_SWAPPING_EXECUTOR_SLOT =
        0x098a7a3b47801589e8cdf9ec791b93ad44273246946c32ef1fc4dbe45390c80e;
    // keccak256("Dispatcher#IS_SPLIT_SWAP_SLOT")
    uint256 private constant _IS_SPLIT_SWAP_SLOT =
        0x7b3c4e5f6a8d9e0f1c2b3a4d5e6f7c8d9e0f1c2b3a4d5e6f7c8d9e0f1c2b3a4d;
    // keccak256("Dispatcher#IS_FIRST_SWAP_SLOT")
    uint256 private constant _IS_FIRST_SWAP_SLOT =
        0x8c47a7e3f4c2e1b5a6d9f0e8c7b3a2d1e4f5c6b7a8d9e0f1c2b3a4d5e6f7c8d9;
    // keccak256("Dispatcher#SWAP_INPUT_AMOUNT_SLOT")
    uint256 private constant _SWAP_INPUT_AMOUNT_SLOT =
        0xce9e2e8e50d57f2d688020ea7ab16e2039bcf4dc7175eba827e178586597bb39;
    // keccak256("Dispatcher#SWAP_INPUT_TOKEN_SLOT")
    uint256 private constant _SWAP_INPUT_TOKEN_SLOT =
        0x0c22c14aba48b0e26e3b58475c66f358c352532122e537c32e8184c0159e6e10;

    uint256 public constant DELAY_EXECUTOR_ACTIVATION = 3 days;

    event ExecutorSet(address indexed executor, uint256 timelockExpiresAt);
    event ExecutorRemoved(address indexed executor);

    constructor(address permit2_) TransferManager(permit2_) {}

    /**
     * @dev Adds an approved executor contract address if it is a contract.
     * @param target address of the executor contract
     */
    function _setExecutor(address target) internal {
        if (target.code.length == 0) {
            revert Dispatcher__NonContractExecutor();
        }

        // slither-disable-next-line timestamp
        if (executorsActivationTimestamp[target] != 0) {
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
    // slither-disable-next-line delegatecall-loop,assembly,controlled-delegatecall,cyclomatic-complexity
    function _callSwapOnExecutor(
        address executor,
        uint256 amount,
        bytes calldata data,
        bool isFirstSwap,
        bool isSplitSwap,
        address receiver
    ) internal returns (uint256 amountOut) {
        _validateExecutor(executor);

        assembly {
            tstore(_CURRENTLY_SWAPPING_EXECUTOR_SLOT, executor)
            tstore(_IS_FIRST_SWAP_SLOT, isFirstSwap)
            tstore(_IS_SPLIT_SWAP_SLOT, isSplitSwap)
            tstore(_SWAP_INPUT_AMOUNT_SLOT, amount)
        }

        // slither-disable-next-line calls-loop
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
            revert Dispatcher__UnsupportedSingleHopCycle(tokenIn);
        }

        // Store tokenIn so it can be passed to getCallbackTransferData if a
        // callback occurs during the swap.
        assembly {
            tstore(_SWAP_INPUT_TOKEN_SLOT, tokenIn)
        }

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

        // slither-disable-next-line controlled-delegatecall,low-level-calls,calls-loop,reentrancy-balance
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(
                IExecutor.swap.selector, amount, data, receiver
            )
        );

        // Clear transient storage in case no callback was performed
        assembly {
            tstore(_CURRENTLY_SWAPPING_EXECUTOR_SLOT, 0)
            tstore(_IS_FIRST_SWAP_SLOT, 0)
            tstore(_IS_SPLIT_SWAP_SLOT, 0)
            tstore(_SWAP_INPUT_AMOUNT_SLOT, 0)
            tstore(_SWAP_INPUT_TOKEN_SLOT, 0)
        }

        // Revoke any lingering allowance the protocol didn't consume.
        if (transferType == TransferType.ProtocolWillDebit) {
            _revokeUnconsumedApproval(tokenIn, transferReceiver);
        }

        if (!success) {
            // slither-disable-next-line incorrect-equality
            if (result.length == 0) {
                revert Dispatcher__SwapReverted(executor);
            }
            // Bubble the executor's revert payload byte-for-byte so callers
            // see the original Error(string)/Panic/custom-error
            assembly {
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
            // slither-disable-next-line calls-loop
            _updateDeltaAccounting(tokenOut, int256(amountOut));
        }
    }

    // slither-disable-next-line assembly
    function _callHandleCallbackOnExecutor(bytes calldata data, address caller)
        internal
        returns (bytes memory)
    {
        address executor;
        bool isFirstSwap;
        bool isSplitSwap;
        uint256 amount;
        address tokenIn;
        assembly {
            executor := tload(_CURRENTLY_SWAPPING_EXECUTOR_SLOT)
            isFirstSwap := tload(_IS_FIRST_SWAP_SLOT)
            isSplitSwap := tload(_IS_SPLIT_SWAP_SLOT)
            amount := tload(_SWAP_INPUT_AMOUNT_SLOT)
            tokenIn := tload(_SWAP_INPUT_TOKEN_SLOT)
        }

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

        // slither-disable-next-line controlled-delegatecall,low-level-calls
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(ICallback.handleCallback.selector, data)
        );

        if (!success) {
            // slither-disable-next-line incorrect-equality
            if (result.length == 0) {
                revert Dispatcher__CallbackReverted(executor);
            }
            // Bubble the executor's revert payload byte-for-byte so callers
            // see the original Error(string)/Panic/custom-error
            assembly {
                revert(add(result, 0x20), mload(result))
            }
        }

        // Revoke any lingering allowance the protocol didn't consume.
        if (transferType == TransferType.ProtocolWillDebit) {
            _revokeUnconsumedApproval(tokenIn, receiver);
        }

        // to prevent multiple callbacks
        assembly {
            tstore(_CURRENTLY_SWAPPING_EXECUTOR_SLOT, 0)
            tstore(_IS_FIRST_SWAP_SLOT, 0)
            tstore(_IS_SPLIT_SWAP_SLOT, 0)
        }

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

        // slither-disable-next-line calls-loop
        receiver = IExecutor(executor).fundsExpectedAddress(data);
    }

    function _validateExecutor(address executor) private view {
        uint256 activationTimestamp = executorsActivationTimestamp[executor];

        // slither-disable-next-line incorrect-equality
        if (activationTimestamp == 0) {
            revert Dispatcher__UnapprovedExecutor(executor);
        }
        // slither-disable-next-line timestamp
        if (block.timestamp < activationTimestamp) {
            revert Dispatcher__ExecutorIsTimelocked(executor);
        }
    }

    function _callGetEffectiveRouterFeeOnOutput(
        address feeCalculator,
        address client
    ) internal view returns (uint16 routerFeeOnOutputBps) {
        // slither-disable-next-line calls-loop
        routerFeeOnOutputBps =
            IFeeCalculator(feeCalculator).getEffectiveRouterFeeOnOutput(client);
    }

    function _callCalculateFee(
        address feeCalculator,
        uint256 amountIn,
        uint16 clientFeeBps,
        address client
    )
        internal
        view
        returns (uint256 amountOut, FeeRecipient[] memory feeRecipients)
    {
        // slither-disable-next-line calls-loop
        (amountOut, feeRecipients) = IFeeCalculator(feeCalculator)
            .calculateFee(amountIn, client, clientFeeBps);
    }
}
