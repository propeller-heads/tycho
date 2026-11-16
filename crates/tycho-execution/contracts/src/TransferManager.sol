// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {TransientSlot} from "@openzeppelin/contracts/utils/TransientSlot.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IAllowanceTransfer
} from "@permit2/src/interfaces/IAllowanceTransfer.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";

error TransferManager__NotAContract(address addr);
error TransferManager__ExceededTransferFromAllowance(
    uint256 allowedAmount, uint256 amountAttempted
);
error TransferManager__DifferentTokenIn(
    address tokenIn, address tokenInStorage
);
error TransferManager__UnknownTransferType();
error TransferManager__Permit2AmountTooLarge(uint256 amount);

/**
 * @title TransferManager
 * @dev Makes every token transfer of a swap. Sits between
 * TychoRouterV3 and Dispatcher in the inheritance chain.
 *
 * Responsibilities:
 * - Routes each transfer through one of 5 scenarios based on: TransferType returned
 *   by the executor, whether it is the first swap, whether it is a split swap,
 *   and whether the call is inside a callback.
 * - Caps `transferFrom` to the declared input amount (stored in transient storage at
 *   swap start). This prevents maliciously encoded split swaps from withdrawing more
 *   than the user authorised. Reverts if the cap is exceeded or a different token is
 *   attempted.
 * - Supports both standard ERC20 `transferFrom` and Permit2's `transferFrom`.
 * - Makes no transfer for executors that forward ETH as `msg.value` directly to
 *   the protocol (e.g. Fluid, Rocketpool, Lido); they spend the router's ETH.
 *
 * Per-swap state is kept in transient storage (EIP-1153): token in, allowed amount,
 * Permit2 flag and original sender. These are written once at
 * swap entry (`_tstoreTransferFromInfo`) and consumed throughout the swap.
 */
contract TransferManager {
    // One directive per slot type: Slither 0.11.6 can fail to analyse a
    // contract that uses `using TransientSlot for *`.
    using TransientSlot for TransientSlot.AddressSlot;
    using TransientSlot for TransientSlot.BooleanSlot;
    using TransientSlot for TransientSlot.Uint256Slot;
    using SafeERC20 for IERC20;

    IAllowanceTransfer public immutable permit2;
    TransientSlot.AddressSlot private constant _TOKEN_IN_SLOT =
        TransientSlot.AddressSlot
            .wrap(keccak256("TransferManager#TOKEN_IN_SLOT"));
    TransientSlot.Uint256Slot private constant _AMOUNT_ALLOWED_SLOT =
        TransientSlot.Uint256Slot
            .wrap(keccak256("TransferManager#AMOUNT_ALLOWED_SLOT"));
    TransientSlot.BooleanSlot private constant _IS_PERMIT2_SLOT =
        TransientSlot.BooleanSlot
            .wrap(keccak256("TransferManager#IS_PERMIT2_SLOT"));
    TransientSlot.AddressSlot private constant _SENDER_SLOT =
        TransientSlot.AddressSlot
        .wrap(keccak256("TransferManager#SENDER_SLOT"));

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
     * contract's storage. This is done as the first step in the swap process in TychoRouterV3.
     */
    function _tstoreTransferFromInfo(
        address tokenIn,
        uint256 amountIn,
        bool isPermit2
    ) internal {
        _TOKEN_IN_SLOT.tstore(tokenIn);
        _AMOUNT_ALLOWED_SLOT.tstore(amountIn);
        _IS_PERMIT2_SLOT.tstore(isPermit2);
        _SENDER_SLOT.tstore(msg.sender);
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
     *
     * Handles 5 transfer scenarios (in order of execution):
     * 1. None, or native ETH sent by the executor - the router transfers nothing
     * 2. TransferFrom user wallet to router, then approve protocol to debit
     * 3. Protocol debits from router (with approval if needed). No transferFrom needed
     * 4. TransferFrom user wallet directly to protocol
     * 5. Transfer the funds the previous swap left in the router to the protocol
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
        // Scenario 1: The router transfers nothing. The protocol is paid
        // inside a callback (e.g. UniswapV3), or the executor sends native ETH
        // as msg.value when it calls the protocol (e.g. Fluid, Lido).
        if (
            transferType == TransferType.None
                || transferType == TransferType.TransferNativeInExecutor
        ) {
            return amount;
        }

        // Scenario 2 & 3: Protocol will debit tokens from router
        if (transferType == TransferType.ProtocolWillDebit) {
            // Scenario 2: the first swap pulls the user's funds into the router.
            // Scenario 3: a later swap uses the funds the previous swap left.
            if (isFirstSwap) {
                amount = _transferFromUser(tokenIn, address(this), amount);
            }
            _approveIfNeeded(tokenIn, receiver, amount);
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

            if (isFirstSwap) {
                // Scenario 4: First swap with user funds - transfer directly to pool
                amount = _transferFromUser(tokenIn, receiver, amount);
            } else {
                // Scenario 5: Transfer the funds the previous swap left in
                // the router to the pool.
                amount = _transferOut(tokenIn, receiver, amount);
            }
            return amount;
        }

        // forge-lint: disable-next-line(require-revert-in-loop) Unreachable for the four transfer types; rejects a type added without a branch above.
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
    function _revokeUnconsumedApproval(address token, address spender)
        internal
    {
        // forge-lint: disable-next-line(calls-loop) Each hop revokes its own spender's approval; a failing token fails the route.
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
        tokenInStorage = _TOKEN_IN_SLOT.tload();
        amountAllowed = _AMOUNT_ALLOWED_SLOT.tload();
        sender = _SENDER_SLOT.tload();
        isPermit2 = _IS_PERMIT2_SLOT.tload();

        if (amount > amountAllowed) {
            // forge-lint: disable-next-item(require-revert-in-loop) Pulling more than the swap's input amount fails the route.
            revert TransferManager__ExceededTransferFromAllowance(
                amountAllowed, amount
            );
        }
        if (token != tokenInStorage) {
            // forge-lint: disable-next-line(require-revert-in-loop) Pulling a token other than the swap's input fails the route.
            revert TransferManager__DifferentTokenIn(token, tokenInStorage);
        }

        // Update remaining allowance
        amountAllowed -= amount;
        _AMOUNT_ALLOWED_SLOT.tstore(amountAllowed);

        uint256 balanceBefore = _balanceOf(token, receiver);
        // Perform the actual transfer
        if (isPermit2) {
            // Permit2.permit is already called from the TychoRouterV3
            // Permit2 amounts are uint160: a larger amount reverts instead of truncating.
            if (amount > type(uint160).max) {
                // forge-lint: disable-next-line(require-revert-in-loop) An amount Permit2 cannot carry fails the route.
                revert TransferManager__Permit2AmountTooLarge(amount);
            }
            // forge-lint: disable-next-line(calls-loop, unsafe-typecast) Each split from the input token pulls its share from the user; a failing pull fails the route. The check above keeps the amount within uint160.
            permit2.transferFrom(sender, receiver, uint160(amount), token);
        } else {
            // forge-lint: disable-next-line(arbitrary-send-erc20) sender is the router's msg.sender, stored by the entry point.
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
        // forge-lint: disable-next-item(calls-loop) Each hop reads its own token's balances; a failing token fails the route.
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
            // forge-lint: disable-next-line(arbitrary-send-eth) Sends a swap's output to the receiver its caller chose.
            Address.sendValue(payable(to), amount);
        } else {
            IERC20(token).safeTransfer(to, amount);
        }
        uint256 balanceAfter = _balanceOf(token, to);
        return balanceAfter - balanceBefore;
    }
}
