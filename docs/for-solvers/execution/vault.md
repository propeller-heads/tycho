# Vault

The TychoRouter V3 includes an integrated <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/src/Vault.sol" target="_blank" rel="noopener noreferrer">vault</a> built on the <a href="https://eips.ethereum.org/EIPS/eip-6909" target="_blank" rel="noopener noreferrer">ERC6909</a> multi-token standard. This replaces the "direct transfer" pattern from V2, where tokens sent to the router risked being lost.

## How It Works

The vault uses dual storage:

* Transient storage tracks balance changes ("deltas") during a swap. Credits are recorded when tokens arrive at the router; debits when they leave. This is cheap (\~100 gas per operation) and automatically clears at the end of each transaction.
* Persistent storage (ERC6909 balances) holds final user balances across transactions. These are updated only after a swap is fully validated.

At the end of every swap, `_finalizeBalances` validates the transient state before committing:

* For wallet-funded swaps: all deltas must net to zero.
* For vault-funded swaps: at most one negative delta is allowed (the input token), which is burned from the user's vault balance.

This catches encoding errors and balance mismatches before any persistent state changes.

## Depositing and Withdrawing

```solidity
// Deposit ERC-20 tokens 
router.deposit(tokenAddress, amount);

// Deposit native ETH (use ETH_ADDRESS, not address(0))
router.deposit{value: amount}(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE, amount);

// Withdraw 
router.withdraw(tokenAddress, amount);
```

Tokens in the vault can be used for swaps by setting `user_transfer_type: UseVaultsFunds` in the [Solution](encoding/#solution-struct). They can be withdrawn at any time.

## Crediting Output to the Vault

By default, output tokens are sent to the receiver address after a swap. If you set the receiver to the TychoRouter address, the output tokens are credited to the caller's vault balance instead of transferred out.

This works with all swap types — single, sequential, and split — and with both wallet-funded and vault-funded swaps.

This enables vault rebalancing: converting one token to another without tokens leaving the contract. For example, a solver holding WETH in the vault can convert it to USDC in a single transaction, with both the debit and credit happening within the vault.

It also supports cyclical arbitrage, where you route through multiple pools and end up with more of the starting token, all settled within the vault.

## Why a Vault?

The vault serves three purposes:

1. Gas savings. Keep tokens in the contract to avoid repeated approvals and transfer costs across swaps.
2. In-contract rebalancing. Convert between tokens without ERC-20 transfers or approvals — input and output settle entirely within the router.
3. Fee accounting. Fees are credited with a storage write rather than an ERC-20 transfer, saving gas at fee-taking time.

## Security Guarantees

* Vault balances are scoped per user. Only the owner can use their funds (via `msg.sender` or signature checks). This is enforced at the contract level and cannot be overridden through encoding.
* Output tokens credited to the vault always go to `msg.sender`, not to the receiver parameter. This prevents a malicious encoder from redirecting output into another user's vault.
* Tokens sent to the router without calling `deposit()` are considered lost. The vault does not credit balances from raw transfers.
