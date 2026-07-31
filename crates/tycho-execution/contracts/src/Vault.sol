// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {
    ReentrancyGuard
} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {Pausable} from "@openzeppelin/contracts/utils/Pausable.sol";
import {ERC6909} from "@openzeppelin/contracts/token/ERC6909/ERC6909.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";

error Vault__InsufficientBalance(
    address user, address token, uint256 requested, uint256 available
);
error Vault__AmountZero();
error Vault__AddressZero();
error Vault__UnexpectedNonZeroCount(uint256 nonZeroCount);
error Vault__InvalidInputDelta(address token, int256 expected, int256 actual);
error Vault__UnexpectedInputDelta(int256 inputDelta);

/**
 * @title Vault - ERC6909-compliant multi-token vault
 * @dev Implements ERC6909 for managing user token balances within the router.
 * Users can deposit tokens, use them for swaps, and withdraw them.
 */
abstract contract Vault is ERC6909, ReentrancyGuard, Pausable {
    using SafeERC20 for IERC20;

    // Vault balances - using our own mapping to avoid expensive Transfer events from ERC6909
    mapping(address => mapping(uint256 => uint256)) private _vaultBalances;

    // Transient storage slot for tracking deltas during swap sequences
    // keccak256("TychoVault#NON_ZERO_DELTA_COUNT_SLOT")
    uint256 private constant _NON_ZERO_DELTA_COUNT_SLOT =
        0xee3c9c434505299f2450d3624302a27b8a6978e973825330bc744ba925eec199;
    // keccak256("Vault#USE_VAULT_SLOT")
    uint256 internal constant _USE_VAULT_SLOT =
        0xce5ffa91873ede1b462af74ea59bab3721b06b0ff726b90311437efde2001795;

    // ============ ERC6909 Overrides and Extensions ============

    /**
     * @dev Override balanceOf to use our own mapping instead of ERC6909's
     *
     */
    function balanceOf(address owner, uint256 id)
        public
        view
        virtual
        override
        returns (uint256)
    {
        return _vaultBalances[owner][id];
    }

    /**
     * @dev Rewrite _update to use our own mapping and avoid emitting Transfer events
     */
    function _updateWithoutEvent(
        address from,
        address to,
        uint256 id,
        uint256 amount
    ) internal virtual {
        if (from != address(0)) {
            uint256 fromBalance = _vaultBalances[from][id];
            if (fromBalance < amount) {
                revert ERC6909.ERC6909InsufficientBalance(
                    from, fromBalance, amount, id
                );
            }
            unchecked {
                // Overflow not possible: amount <= fromBalance.
                _vaultBalances[from][id] = fromBalance - amount;
            }
        }
        if (to != address(0)) {
            _vaultBalances[to][id] += amount;
        }
        // Note: We intentionally do NOT emit Transfer events to save gas
    }

    /**
     * @dev Override _update to use our own mapping and emit Transfer events
     * This is called by all balance-changing operations (transfer, mint, etc.)
     */
    function _update(address from, address to, uint256 id, uint256 amount)
        internal
        virtual
        override
    {
        _updateWithoutEvent(from, to, id, amount);
        emit Transfer(msg.sender, from, to, id, amount);
    }

    /**
     * @dev Create new _mint that does not emit a Transfer event. This should be used by inner methods of the
     * TychoRouter to save gas during swapping.
     */
    function _mintWithoutEvent(address to, uint256 id, uint256 amount)
        internal
    {
        if (to == address(0)) {
            revert ERC6909InvalidReceiver(address(0));
        }
        _updateWithoutEvent(address(0), to, id, amount);
    }

    // ============ ERC6909 Vault Functions ============

    /**
     * @notice Deposit tokens into the vault for the caller
     * @param token The token address to deposit (use ETH_ADDRESS for native ETH)
     * @param amount The amount to deposit
     */
    function deposit(address token, uint256 amount)
        external
        payable
        whenNotPaused
        nonReentrant
    {
        if (token == address(0)) {
            revert Vault__AddressZero();
        }
        if (amount == 0) {
            revert Vault__AmountZero();
        }

        uint256 id = _toId(token);

        if (token == ETH_ADDRESS) {
            // Native ETH deposit
            require(msg.value == amount, "Value mismatch");
            _mint(msg.sender, id, amount);
        } else {
            require(msg.value == 0, "ETH not accepted for ERC20 deposit");
            // ERC20 deposit - transfer to this contract and measure actual received
            // amount to handle  fee-on-transfer and rebasing tokens
            uint256 balanceBefore = IERC20(token).balanceOf(address(this));
            IERC20(token).safeTransferFrom(msg.sender, address(this), amount);
            uint256 received =
                IERC20(token).balanceOf(address(this)) - balanceBefore;
            _mint(msg.sender, id, received);
        }
    }

    /**
     * @notice Withdraw tokens from the vault
     * @param token The token to withdraw
     * @param amount The amount to withdraw
     */
    function withdraw(address token, uint256 amount) external nonReentrant {
        if (token == address(0)) {
            revert Vault__AddressZero();
        }
        if (amount == 0) {
            revert Vault__AmountZero();
        }

        uint256 id = _toId(token);
        uint256 balance = balanceOf(msg.sender, id);
        if (balance < amount) {
            revert Vault__InsufficientBalance(
                msg.sender, token, amount, balance
            );
        }

        _burn(msg.sender, id, amount);

        // Transfer tokens from contract to user
        if (token == ETH_ADDRESS) {
            Address.sendValue(payable(msg.sender), amount);
        } else {
            IERC20(token).safeTransfer(msg.sender, amount);
        }
    }

    // ============ Tracking deltas methods ============

    /**
     * @dev Internal helper to get transient storage slot for a token delta
     * @notice Only needs token since transient storage is scoped to current transaction's sender
     */
    function _getDeltaSlot(address token) private pure returns (uint256 slot) {
        slot = uint256(keccak256(abi.encodePacked(token, "TychoVault#DELTA")));
        return slot;
    }

    /**
     * @dev Get the current delta from transient storage
     * @notice Only needs token since transient storage is scoped to current transaction's sender
     */
    // Assembly required for transient storage operations (tload)
    function _getDelta(address token) internal view returns (int256 delta) {
        uint256 slot = _getDeltaSlot(token);
        // slither-disable-next-line assembly
        assembly {
            delta := tload(slot)
        }
    }

    /**
     * @dev Set the delta in transient storage
     */
    // Assembly required for transient storage operations (tstore)
    function _setDelta(address token, int256 delta) internal {
        uint256 slot = _getDeltaSlot(token);
        // slither-disable-next-line assembly
        assembly {
            tstore(slot, delta)
        }
    }

    /**
     * @dev Get non zero delta count from transient storage
     */
    // Assembly required for transient storage operations (tload)
    function _getNonZeroDeltaCount() internal view returns (uint256 count) {
        // slither-disable-next-line assembly
        assembly {
            count := tload(_NON_ZERO_DELTA_COUNT_SLOT)
        }
    }

    /**
     * @dev Set non zero delta count in transient storage
     */
    // Assembly required for transient storage operations (tstore)
    function _setNonZeroDeltaCount(uint256 count) internal {
        // slither-disable-next-line assembly
        assembly {
            tstore(_NON_ZERO_DELTA_COUNT_SLOT, count)
        }
    }

    /**
     * @dev Get the USE_VAULT flag from transient storage
     */
    // Assembly required for transient storage operations (tload)
    function _getUseVault() internal view virtual returns (bool) {
        uint256 useVault;
        // slither-disable-next-line assembly
        assembly {
            useVault := tload(_USE_VAULT_SLOT)
        }
        return useVault == 1;
    }

    /**
     * @dev Update delta accounting (transient storage)
     * @notice Only needs token since transient storage is scoped to current transaction's sender
     * @param token The token to update
     * @param deltaChange The change to apply (positive to credit, negative to debit)
     */
    function _updateDeltaAccounting(address token, int256 deltaChange)
        internal
        virtual
    {
        // slither-disable-next-line incorrect-equality
        if (deltaChange == 0) return;

        int256 oldDelta = _getDelta(token);
        int256 newDelta = oldDelta + deltaChange;

        // Update non zero delta counter based on transitions
        // slither-disable-next-line incorrect-equality
        if (oldDelta != 0 && newDelta == 0) {
            // Was non zero, now zero: decrement counter
            _setNonZeroDeltaCount(_getNonZeroDeltaCount() - 1);
        } else if (oldDelta == 0 && newDelta != 0) {
            // Was zero, now non zero: increment counter
            _setNonZeroDeltaCount(_getNonZeroDeltaCount() + 1);
        }

        _setDelta(token, newDelta);
    }

    // ============ Vault accounting ============

    /**
     * @dev Internal helper to debit user's actual vault balance (persistent storage)
     * @notice This debits the persistent vault balance and emits a Transfer event
     */
    function _debitVault(address user, address token, uint256 amount)
        internal
        virtual
    {
        // slither-disable-next-line incorrect-equality
        if (amount == 0) return;

        uint256 id = _toId(token);
        uint256 balance = balanceOf(user, id);

        if (balance < amount) {
            revert Vault__InsufficientBalance(user, token, amount, balance);
        }
        _burn(user, id, amount);
    }

    /**
     * @dev Internal helper to credit user's actual vault balance (persistent storage)
     * @notice This credits the persistent vault balance and emits a Transfer event
     */
    function _creditVault(address user, address token, uint256 amount)
        internal
        virtual
    {
        // slither-disable-next-line incorrect-equality
        if (amount == 0) return;

        uint256 id = _toId(token);

        _mint(user, id, amount);
    }

    /**
     * @dev Internal helper to credit user's actual vault balance (persistent storage)
     * @notice This debits the persistent vault balance and does not emit an event.
     * Should be used for fee taking because an extra event is emitted in this case.
     *
     */
    function _creditVaultForFees(address user, address token, uint256 amount)
        internal
        virtual
    {
        // slither-disable-next-line incorrect-equality
        if (amount == 0) return;

        uint256 id = _toId(token);

        _mintWithoutEvent(user, id, amount);
    }

    /**
     * @dev Finalizes the input transient delta to persistent storage
     * @dev Verifies that only the input token has a negative delta and burns the vault balance
     * @param user The user whose deltas should be finalized
     * @param inputToken The expected input token
     * @param inputAmount The expected input amount
     */
    function _finalizeBalances(
        address user,
        address inputToken,
        uint256 inputAmount
    ) internal {
        uint256 nonZeroCount = _getNonZeroDeltaCount();
        bool useVault = _getUseVault();

        if (useVault) {
            // When vault usage is allowed, allow a single negative delta
            // Check that there is only one negative delta: the input token
            if (nonZeroCount > 1) {
                revert Vault__UnexpectedNonZeroCount(nonZeroCount);
            } else if (nonZeroCount == 1) {
                int256 inputDelta = _getDelta(inputToken);
                if (inputDelta == 0 || inputDelta != -int256(inputAmount)) {
                    revert Vault__UnexpectedInputDelta(inputDelta);
                }
                uint256 id = _toId(inputToken);
                _burn(user, id, inputAmount);
                _setDelta(inputToken, 0);
                _setNonZeroDeltaCount(0);
            }
        } else {
            // When vault usage is NOT allowed, all deltas must be zero
            if (nonZeroCount > 0) {
                revert Vault__UnexpectedNonZeroCount(nonZeroCount);
            }
        }
    }

    // ============ Utils methods ============

    /**
     * @dev Converts a token address to an ID
     */
    function _toId(address token) internal view virtual returns (uint256) {
        return uint256(uint160(token));
    }
}
