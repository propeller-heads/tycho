// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ISubsidyPayer, Subsidy} from "@interfaces/ISubsidyPayer.sol";
import {CURRENTLY_SWAPPING_EXECUTOR_SLOT, Dispatcher} from "../Dispatcher.sol";
import {TransferManager} from "../TransferManager.sol";

error TychoSubsidizingExecutor__InvalidDataLength();
error TychoSubsidizingExecutor__NonContractPayer();
error TychoSubsidizingExecutor__UnapprovedInnerExecutor(address executor);

/**
 * @title TychoSubsidizingExecutor
 * @notice Runs a hop through an inner executor the router approved, then asks
 *         the subsidy payer to pay a signed subsidy in the hop's output token
 *         to where the Dispatcher measures the hop's output.
 * @dev Swap data:
 *      [boundTo: 20][amount: 32][minAmountIn: 32][nonce: 32][deadline: 6]
 *      [signature: 65][innerExecutor: 20][innerData: ...]
 */
contract TychoSubsidizingExecutor is IExecutor {
    ISubsidyPayer public immutable payer;
    address private immutable _self;

    /// @notice Emitted from the router's address, since the router
    ///         delegatecalls swap.
    event SubsidyPayerReverted(uint256 indexed nonce, bytes reason);

    constructor(ISubsidyPayer payer_) {
        if (address(payer_).code.length == 0) {
            revert TychoSubsidizingExecutor__NonContractPayer();
        }
        payer = payer_;
        _self = address(this);
    }

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (IExecutor inner, bytes calldata innerData) = _decodeInner(data);
        _checkInnerExecutorApproval(address(inner), address(this));
        // slither-disable-next-line unused-return
        (,,, address tokenOut, bool outputToRouter) =
            inner.getTransferData(innerData);

        // slither-disable-next-line assembly
        assembly {
            tstore(CURRENTLY_SWAPPING_EXECUTOR_SLOT, inner)
        }

        // The router approved the inner executor, and the Dispatcher sets
        // receiver.
        // slither-disable-next-line controlled-delegatecall,low-level-calls,missing-zero-check
        (bool success, bytes memory result) = address(inner)
            .delegatecall(
                abi.encodeCall(IExecutor.swap, (amountIn, innerData, receiver))
            );
        if (!success) {
            // slither-disable-next-line assembly
            assembly {
                revert(add(result, 0x20), mload(result))
            }
        }

        // The payer calls out to `subsidyReceiver`; nothing may reach the inner
        // executor through the router's fallback from there.
        // slither-disable-next-line assembly
        assembly {
            tstore(CURRENTLY_SWAPPING_EXECUTOR_SLOT, 0)
        }

        // Must equal the Dispatcher's measureAt in _callSwapOnExecutor, so the
        // Dispatcher counts the subsidy as hop output.
        address subsidyReceiver = outputToRouter ? address(this) : receiver;
        _addSubsidy(data, tokenOut, amountIn, subsidyReceiver);
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
        (IExecutor inner, bytes calldata innerData) = _decodeInner(data);
        _checkInnerExecutorApproval(address(inner), msg.sender);
        (transferType, receiver, tokenIn, tokenOut, outputToRouter) =
            inner.getTransferData(innerData);
        // The inner executor names its caller, this contract, to mean the
        // router.
        if (receiver == address(this)) receiver = msg.sender;
    }

    function fundsExpectedAddress(bytes calldata data)
        external
        view
        returns (address receiver)
    {
        (IExecutor inner, bytes calldata innerData) = _decodeInner(data);
        _checkInnerExecutorApproval(address(inner), msg.sender);
        receiver = inner.fundsExpectedAddress(innerData);
        // The inner executor names its caller, this contract, to mean the
        // router.
        if (receiver == address(this)) receiver = msg.sender;
    }

    /// @dev Asks the payer to pay the subsidy encoded in `data` to `receiver`.
    ///      A payer revert emits SubsidyPayerReverted instead of reverting the
    ///      swap; the payer emits SubsidyPaid and SubsidySkipped.
    function _addSubsidy(
        bytes calldata data,
        address token,
        uint256 amountIn,
        address receiver
    ) internal {
        Subsidy memory subsidy = Subsidy({
            boundTo: address(bytes20(data[0:20])),
            token: token,
            amount: uint256(bytes32(data[20:52])),
            minAmountIn: uint256(bytes32(data[52:84])),
            innerSwapHash: keccak256(data[187:]),
            nonce: uint256(bytes32(data[84:116])),
            deadline: uint48(bytes6(data[116:122]))
        });
        // msg.sender is the router's caller: the router delegatecalls swap.
        // The payer only matches it against the signed boundTo; the subsidy
        // goes to receiver.
        // slither-disable-next-line unused-return
        try payer.pay(subsidy, amountIn, msg.sender, receiver, data[122:187]) {}
        catch (bytes memory reason) {
            // Reentrancy cannot happen: every router entry point is
            // nonReentrant.
            // slither-disable-next-line reentrancy-events
            emit SubsidyPayerReverted(subsidy.nonce, reason);
        }
    }

    function _decodeInner(bytes calldata data)
        internal
        pure
        returns (IExecutor inner, bytes calldata innerData)
    {
        if (data.length < 207) {
            revert TychoSubsidizingExecutor__InvalidDataLength();
        }
        inner = IExecutor(address(bytes20(data[187:207])));
        innerData = data[207:];
    }

    /// @dev Reverts unless `router` approved `inner`, its timelock passed, and
    ///      `inner` is not this contract.
    function _checkInnerExecutorApproval(address inner, address router)
        internal
        view
    {
        uint256 activation =
            Dispatcher(router).executorsActivationTimestamp(inner);
        // slither-disable-next-line incorrect-equality
        if (activation == 0 || inner == _self) {
            revert TychoSubsidizingExecutor__UnapprovedInnerExecutor(inner);
        }
        // slither-disable-next-line timestamp
        if (block.timestamp < activation) {
            revert TychoSubsidizingExecutor__UnapprovedInnerExecutor(inner);
        }
    }
}
