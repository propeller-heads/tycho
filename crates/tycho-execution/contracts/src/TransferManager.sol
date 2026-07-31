// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IAllowanceTransfer
} from "@permit2/src/interfaces/IAllowanceTransfer.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {Vault} from "./Vault.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";

error TransferManager__AddressZero();
error TransferManager__NotAContract(address addr);
error TransferManager__ExceededTransferFromAllowance(
    uint256 allowedAmount, uint256 amountAttempted
);
error TransferManager__DifferentTokenIn(
    address tokenIn, address tokenInStorage
);
error TransferManager__UnknownTransferType();

/**
 * @title TransferManager
 * @dev Orchestrates all token transfers during swap execution. Inherits from Vault
 * (ERC6909) and sits between TychoRouter and Dispatcher in the inheritance chain.
 *
 * Responsibilities:
 * - Routes each transfer through one of 6 scenarios based on: TransferType returned
 *   by the executor, whether it is the first swap, whether it is a split swap,
 *   whether the call is inside a callback, and whether vault funds are in use.
 * - Caps `transferFrom` to the declared input amount (stored in transient storage at
 *   swap start). This prevents maliciously encoded split swaps from withdrawing more
 *   than the user authorised. Reverts if the cap is exceeded or a different token is
 *   attempted.
 * - Supports both standard ERC20 `transferFrom` and Permit2's `transferFrom`.
 * - Handles native ETH accounting for executors that forward ETH as `msg.value`
 *   directly to the protocol (e.g. Fluid, Rocketpool, Lido).
 * - When vault-funded, skips `transferFrom` entirely and relies on vault delta
 *   accounting (`_updateDeltaAccounting`) to settle balances at the end of the swap.
 *
 * Per-swap state is kept in transient storage (EIP-1153): token in, allowed amount,
 * Permit2 flag, original sender, and vault-in-use flag. These are written once at
 * swap entry (`_tstoreTransferFromInfo`) and consumed throughout the swap.
 */
contract TransferManager is Vault {
    using SafeERC20 for IERC20;

    IAllowanceTransfer public immutable permit2;
    // keccak256("TransferManager#TOKEN_IN_SLOT")
    uint256 private constant _TOKEN_IN_SLOT =
        0xaf58e9f1b0923d55d0ec6a57763d43cd9bb1d6bd8bad5ce9522fbe772c6ec42b;
    // keccak256("TransferManager#AMOUNT_ALLOWED_SLOT")
    uint256 private constant _AMOUNT_ALLOWED_SLOT =
        0x28d0e2684c9341fec58f816e0c375a4e51f9f34ec034a29c97467df00497c8d9;
    // keccak256("TransferManager#IS_PERMIT2_SLOT")
    uint256 private constant _IS_PERMIT2_SLOT =
        0xd847dc13274ab371f6244234290f56fa8650ec9dceff86e20efe1ddc141bdb03;
    // keccak256("TransferManager#SENDER_SLOT")
    uint256 private constant _SENDER_SLOT =
        0x99298391997747e556e81b2d36d99151315b6c1b92e826ca8d37acad5fddaf70;

    constructor(address permit2_) {
        if (permit2_.code.length == 0) {
            revert TransferManager__NotAContract(permit2_);
        }
        permit2 = IAllowanceTransfer(permit2_);
    }

    enum TransferType {
        Transfer,
        TransferNativeInExecutor,
        ProtocolWillDebit,
        None
    }

    /**
     * @dev This function is used to store the transfer information in the
     * contract's storage. This is done as the first step in the swap process in TychoRouter.
     */
    // slither-disable-next-line assembly
    function _tstoreTransferFromInfo(
        address tokenIn,
        uint256 amountIn,
        bool isPermit2,
        bool useVault
    ) internal {
        uint256 amountAllowed = useVault ? 0 : amountIn;
        assembly {
            tstore(_TOKEN_IN_SLOT, tokenIn)
            tstore(_AMOUNT_ALLOWED_SLOT, amountAllowed)
            tstore(_IS_PERMIT2_SLOT, isPermit2)
            tstore(_SENDER_SLOT, caller())
            tstore(_USE_VAULT_SLOT, useVault)
        }
    }

    /**
     * @dev Transfers tokens based on the transfer type and swap context.
     * This function is called within the Dispatcher before calling executors or in callbacks.
     *
     * The function determines the appropriate transfer strategy based on:
     * - transferType: The base transfer requirement from the executor
     * - isFirstSwap: Whether this is the first swap or subsequent
     * - isSplitSwap: Whether this is part of a split swap strategy
     * - inCallback: Whether being called from within a callback (e.g., UniswapV3)
     * - useVault: Whether using vault funds
     *
     * Handles 6 transfer scenarios (in order of execution):
     * 1. None - no transfer needed
     * 2. Native ETH sent via executor - perform accounting but do no transfers
     * 3. TransferFrom user wallet to router, then approve protocol to debit
     * 4. Protocol debits from router/vault (with approval if needed). No transferFrom needed
     * 5. TransferFrom user wallet directly to protocol
     * 6. Transfer from router balance to protocol (either from vault or previous swap funds)
     */
    function _transfer(
        address receiver,
        TransferType transferType,
        address tokenIn,
        uint256 amount,
        bool isFirstSwap,
        bool isSplitSwap,
        bool inCallback
    ) internal returns (uint256) {
        // Scenario 1: No transfer needed. Likely called from outside the callback,
        // when funds are only transferred in callback for this protocol (e.g.
        // UniswapV3).
        if (transferType == TransferType.None) {
            return amount;
        }

        // Scenario 2: Native ETH sent via executor - accounting only
        if (transferType == TransferType.TransferNativeInExecutor) {
            // Protocols like Fluid or Lido require us to send the ETH as
            // msg.value when calling the swap function from inside the executor.
            _updateDeltaAccounting(tokenIn, -int256(amount));
            return amount;
        }

        // Determine if we need to transfer from user wallet (first swap + not using vault)
        bool useVault;
        // slither-disable-next-line assembly
        assembly {
            useVault := tload(_USE_VAULT_SLOT)
        }
        bool needsTransferFromUser = isFirstSwap && !useVault;

        // Scenario 3 & 4: Protocol will debit tokens from router
        if (transferType == TransferType.ProtocolWillDebit) {
            if (needsTransferFromUser) {
                // Scenario 3: First swap with user funds - transfer to router, then approve
                amount = _transferFromUser(tokenIn, address(this), amount);
                _approveIfNeeded(tokenIn, receiver, amount);
            } else {
                // Scenario 4: Funds already in router (from vault or previous swap)
                _updateDeltaAccounting(tokenIn, -int256(amount));
                _approveIfNeeded(tokenIn, receiver, amount);
            }
            return amount;
        }

        if (transferType == TransferType.Transfer) {
            // Scenario 1 optimization: Tokens already at pool from previous swap.
            // This optimization assumes that the previous swap sent tokens
            // directly to the current pool (e.g. UniswapV2: Pool1 -> Pool2 -> Pool3).
            // We must NOT apply this optimization when in a callback context, because
            // callback-constrained protocols (UniswapV3, BalancerV3, etc.) hold tokens in the
            // router between swaps, and do not account for transfers before the callback.
            bool canUseSequentialSwapOptimization =
                !isFirstSwap && !isSplitSwap && !inCallback;
            if (canUseSequentialSwapOptimization) {
                return amount;
            }

            if (needsTransferFromUser) {
                // Scenario 5: First swap with user funds - transfer directly to pool
                amount = _transferFromUser(tokenIn, receiver, amount);
            } else {
                // Scenario 6: Transfer from router balance to pool
                // This could mean the funds come from the user's vault (first swap with vault)
                // or funds are in the router from the previous swap.
                _updateDeltaAccounting(tokenIn, -int256(amount));
                amount = _transferOut(tokenIn, receiver, amount);
            }
            return amount;
        }

        revert TransferManager__UnknownTransferType();
    }

    /**
     * @dev Approves a receiver to spend tokens if receiver is not this contract.
     * For special cases like Rocketpool, the contract burns the user's balance
     * without physically transferring the input token, so an approval is not
     * always needed.
     */
    function _approveIfNeeded(address token, address receiver, uint256 amount)
        internal
    {
        if (receiver != address(this)) {
            IERC20(token).forceApprove(receiver, amount);
        }
    }

    /**
     * @dev Revokes a token approval if the spender didn't fully consume it.
     * In the normal case the protocol consumed the approval, so we skip the expensive
     * forceApprove.
     */
    // slither-disable-next-line calls-loop
    function _revokeUnconsumedApproval(address token, address spender)
        internal
    {
        if (IERC20(token).allowance(address(this), spender) > 0) {
            IERC20(token).forceApprove(spender, 0);
        }
    }

    /**
     * @dev Transfers tokens from user wallet using either Permit2 or regular transferFrom.
     * Validates the transfer doesn't exceed allowed amount and updates allowance tracking.
     * @return The actual amount received by the receiver, measured as the balance
     * difference before and after the transfer. This may differ from `amount` for
     * rebasing or fee-on-transfer tokens.
     */
    function _transferFromUser(address token, address receiver, uint256 amount)
        internal
        returns (uint256)
    {
        // Validate and track allowance to prevent badly encoded split swaps from
        // taking more than the input amount out of the user's wallet
        address tokenInStorage;
        uint256 amountAllowed;
        address sender;
        bool isPermit2;

        // slither-disable-next-line assembly
        assembly {
            tokenInStorage := tload(_TOKEN_IN_SLOT)
            amountAllowed := tload(_AMOUNT_ALLOWED_SLOT)
            sender := tload(_SENDER_SLOT)
            isPermit2 := tload(_IS_PERMIT2_SLOT)
        }

        if (amount > amountAllowed) {
            revert TransferManager__ExceededTransferFromAllowance(
                amountAllowed, amount
            );
        }
        if (token != tokenInStorage) {
            revert TransferManager__DifferentTokenIn(token, tokenInStorage);
        }

        // Update remaining allowance
        amountAllowed -= amount;
        assembly {
            tstore(_AMOUNT_ALLOWED_SLOT, amountAllowed)
        }

        uint256 balanceBefore = _balanceOf(token, receiver);
        // Perform the actual transfer
        if (isPermit2) {
            // Permit2.permit is already called from the TychoRouter
            // slither-disable-next-line calls-loop
            permit2.transferFrom(sender, receiver, uint160(amount), token);
        } else {
            // slither-disable-next-line arbitrary-send-erc20
            IERC20(token).safeTransferFrom(sender, receiver, amount);
        }
        uint256 balanceAfter = _balanceOf(token, receiver);
        return balanceAfter - balanceBefore;
    }

    /**
     * @dev Gets balance of a token for a given address. Supports both native ETH and ERC20 tokens.
     */
    function _balanceOf(address token, address owner)
        internal
        view
        returns (uint256)
    {
        // slither-disable-next-line calls-loop
        return
            token == ETH_ADDRESS
                ? owner.balance
                : IERC20(token).balanceOf(owner);
    }

    /**
     * @dev Transfers tokens from this contract to a recipient. Supports both
     * native ETH (via `sendValue`) and ERC20 tokens (via `safeTransfer`).
     * @return The actual amount received by `to`, measured as the balance
     * difference before and after the transfer. This may differ from `amount`
     * for rebasing or fee-on-transfer tokens.
     */
    function _transferOut(address token, address to, uint256 amount)
        internal
        returns (uint256)
    {
        uint256 balanceBefore = _balanceOf(token, to);
        if (token == ETH_ADDRESS) {
            Address.sendValue(payable(to), amount);
        } else {
            IERC20(token).safeTransfer(to, amount);
        }
        uint256 balanceAfter = _balanceOf(token, to);
        return balanceAfter - balanceBefore;
    }
}
