// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ISubsidyConfig} from "@interfaces/ISubsidyConfig.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {
    MessageHashUtils
} from "@openzeppelin/contracts/utils/cryptography/MessageHashUtils.sol";
import {StorageSlot} from "@openzeppelin/contracts/utils/StorageSlot.sol";
import {TransferManager} from "../TransferManager.sol";
import {TychoRouterV3} from "../TychoRouterV3.sol";

error TychoSubsidizingExecutor__InvalidDataLength();
error TychoSubsidizingExecutor__UnapprovedInnerExecutor(address executor);
error TychoSubsidizingExecutor__UnsupportedInnerExecutor();
error TychoSubsidizingExecutor__SubsidyDisabled();
error TychoSubsidizingExecutor__ExpiredSignature(
    uint256 deadline, uint256 blockTimestamp
);
error TychoSubsidizingExecutor__InvalidSignature();
error TychoSubsidizingExecutor__SubsidyTooHigh(uint256 subsidy, uint256 cap);
error TychoSubsidizingExecutor__InsufficientVaultBalance(
    uint256 subsidy, uint256 balance
);

/**
 * @title TychoSubsidizingExecutor
 * @notice Adds a signed subsidy from the subsidy wallet's vault balance to a
 *         hop's input, then runs the hop through an inner executor the
 *         router approved. Only inner executors the router pays before the
 *         swap, or that pull from the router, are supported.
 */
contract TychoSubsidizingExecutor is IExecutor {
    using SafeERC20 for IERC20;

    uint256 private constant _MAX_BPS = 100_000_000;
    uint256 private constant _INNER_EXECUTOR_OFFSET = 119;
    uint256 private constant _INNER_DATA_OFFSET = 139;

    // Storage slot of Vault._vaultBalances in TychoRouterV3.
    uint256 private constant _VAULT_BALANCES_SLOT = 5;

    bytes32 public constant SUBSIDY_TYPEHASH = keccak256(
        "Subsidy(address executor,address tokenIn,uint256 subsidy,"
        "uint256 nonce,uint256 deadline)"
    );
    bytes32 private constant _DOMAIN_TYPEHASH = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,"
        "address verifyingContract)"
    );
    bytes32 private constant _NAME_HASH = keccak256("TychoSubsidizingExecutor");
    bytes32 private constant _VERSION_HASH = keccak256("1");

    // keccak256("TychoSubsidizingExecutor#NONCE_BITMAP")
    bytes32 private constant _NONCE_BITMAP_NAMESPACE =
        0x3f8f641040d0aac2474600c59035205d76766029f298338eda05cab4bc051e88;

    address private immutable _self;

    event Subsidized(address indexed tokenIn, uint256 subsidy, uint256 nonce);

    constructor() {
        _self = address(this);
    }

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (IExecutor inner, bytes calldata innerData) =
            _inner(data, address(this));
        // slither-disable-next-line unused-return
        (
            TransferManager.TransferType transferType,
            address transferReceiver,
            address tokenIn,,
        ) = inner.getTransferData(innerData);

        uint256 subsidy = _useSubsidy(data, tokenIn, amountIn);
        if (subsidy > 0) {
            if (transferType == TransferManager.TransferType.Transfer) {
                IERC20(tokenIn).safeTransfer(transferReceiver, subsidy);
            } else {
                IERC20(tokenIn)
                    .forceApprove(transferReceiver, amountIn + subsidy);
            }
        }

        // The router approved the inner executor, and the Dispatcher sets
        // receiver.
        // slither-disable-next-line controlled-delegatecall,low-level-calls,missing-zero-check
        (bool success, bytes memory result) = address(inner)
            .delegatecall(
                abi.encodeCall(
                    IExecutor.swap, (amountIn + subsidy, innerData, receiver)
                )
            );
        if (!success) {
            // slither-disable-next-line assembly
            assembly {
                revert(add(result, 0x20), mload(result))
            }
        }
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
        (IExecutor inner, bytes calldata innerData) = _inner(data, msg.sender);
        (transferType, receiver, tokenIn, tokenOut, outputToRouter) =
            inner.getTransferData(innerData);
        if (
            transferType != TransferManager.TransferType.Transfer
                && transferType
                    != TransferManager.TransferType.ProtocolWillDebit
        ) {
            revert TychoSubsidizingExecutor__UnsupportedInnerExecutor();
        }
        receiver = _routerIfSelf(receiver);
    }

    function fundsExpectedAddress(bytes calldata data)
        external
        view
        returns (address receiver)
    {
        (IExecutor inner, bytes calldata innerData) = _inner(data, msg.sender);
        receiver = _routerIfSelf(inner.fundsExpectedAddress(innerData));
    }

    /// @dev Returns the subsidy to add, or zero when the nonce was already
    ///      used, so a copied signature cannot make the original swap revert.
    function _useSubsidy(bytes calldata data, address tokenIn, uint256 amountIn)
        internal
        returns (uint256 subsidy)
    {
        subsidy = uint128(bytes16(data[0:16]));
        uint256 nonce = uint256(bytes32(data[16:48]));
        (address subsidyWallet, uint32 subsidyBps) =
            _verifySignature(data, tokenIn, subsidy, nonce);

        // Caps the subsidy so a copier must trade as much as the signer
        // priced.
        uint256 cap = amountIn * subsidyBps / _MAX_BPS;
        if (subsidy > cap) {
            revert TychoSubsidizingExecutor__SubsidyTooHigh(subsidy, cap);
        }
        if (!_tryUseNonce(nonce)) return 0;

        _debitVault(subsidyWallet, tokenIn, subsidy);
        emit Subsidized(tokenIn, subsidy, nonce);
    }

    /// @dev Lowers `wallet`'s vault balance in the router's storage. The
    ///      tokens backing it stay in the router for the hop to use.
    function _debitVault(address wallet, address token, uint256 amount)
        internal
    {
        bytes32 slot = keccak256(
            abi.encode(
                uint256(uint160(token)),
                keccak256(abi.encode(wallet, _VAULT_BALANCES_SLOT))
            )
        );
        StorageSlot.Uint256Slot storage balance =
            StorageSlot.getUint256Slot(slot);
        if (balance.value < amount) {
            revert TychoSubsidizingExecutor__InsufficientVaultBalance(
                amount, balance.value
            );
        }
        balance.value -= amount;
    }

    function _verifySignature(
        bytes calldata data,
        address tokenIn,
        uint256 subsidy,
        uint256 nonce
    ) internal view returns (address subsidyWallet, uint32 subsidyBps) {
        uint256 deadline = uint48(bytes6(data[48:54]));
        // forge-lint: disable-start(block-timestamp)
        // slither-disable-next-line timestamp
        if (block.timestamp > deadline) {
            revert TychoSubsidizingExecutor__ExpiredSignature(
                deadline, block.timestamp
            );
        }
        // forge-lint: disable-end(block-timestamp)

        address subsidySigner;
        (subsidyWallet, subsidySigner, subsidyBps) = ISubsidyConfig(
                TychoRouterV3(payable(address(this))).getFeeCalculator()
            ).getSubsidyConfig();
        if (subsidySigner == address(0) || subsidyWallet == address(0)) {
            revert TychoSubsidizingExecutor__SubsidyDisabled();
        }

        bytes32 structHash = keccak256(
            abi.encode(
                SUBSIDY_TYPEHASH, _self, tokenIn, subsidy, nonce, deadline
            )
        );
        bytes32 signingHash =
            MessageHashUtils.toTypedDataHash(_domainSeparator(), structHash);
        // slither-disable-next-line unused-return
        (address recovered, ECDSA.RecoverError err,) = ECDSA.tryRecoverCalldata(
            signingHash, data[54:_INNER_EXECUTOR_OFFSET]
        );
        if (err != ECDSA.RecoverError.NoError || recovered != subsidySigner) {
            revert TychoSubsidizingExecutor__InvalidSignature();
        }
    }

    /// @dev Returns false when the nonce was already used.
    function _tryUseNonce(uint256 nonce) internal returns (bool) {
        bytes32 slot =
            keccak256(abi.encode(nonce >> 8, _NONCE_BITMAP_NAMESPACE));
        // The low 8 bits of the nonce pick the bit within the word.
        // forge-lint: disable-next-line(incorrect-shift)
        uint256 bit = 1 << (nonce & 0xff);
        StorageSlot.Uint256Slot storage word = StorageSlot.getUint256Slot(slot);
        uint256 bitmap = word.value;
        if (bitmap & bit != 0) return false;
        word.value = bitmap | bit;
        return true;
    }

    /// @dev The router is the verifying contract.
    function _domainSeparator() internal view returns (bytes32) {
        return keccak256(
            abi.encode(
                _DOMAIN_TYPEHASH,
                _NAME_HASH,
                _VERSION_HASH,
                block.chainid,
                address(this)
            )
        );
    }

    /// @dev The inner executor names its caller, this contract, to mean the
    ///      router.
    function _routerIfSelf(address receiver) internal view returns (address) {
        return receiver == address(this) ? msg.sender : receiver;
    }

    /// @dev Reverts unless `router` approved the inner executor and its
    ///      timelock passed.
    function _inner(bytes calldata data, address router)
        internal
        view
        returns (IExecutor inner, bytes calldata innerData)
    {
        if (data.length < _INNER_DATA_OFFSET) {
            revert TychoSubsidizingExecutor__InvalidDataLength();
        }
        address innerAddress =
            address(bytes20(data[_INNER_EXECUTOR_OFFSET:_INNER_DATA_OFFSET]));
        uint256 activation = TychoRouterV3(payable(router))
            .executorsActivationTimestamp(innerAddress);
        // slither-disable-next-line incorrect-equality
        if (activation == 0 || innerAddress == _self) {
            revert TychoSubsidizingExecutor__UnapprovedInnerExecutor(innerAddress);
        }
        // forge-lint: disable-start(block-timestamp)
        // slither-disable-next-line timestamp
        if (block.timestamp < activation) {
            revert TychoSubsidizingExecutor__UnapprovedInnerExecutor(innerAddress);
        }
        // forge-lint: disable-end(block-timestamp)
        inner = IExecutor(innerAddress);
        innerData = data[_INNER_DATA_OFFSET:];
    }
}
