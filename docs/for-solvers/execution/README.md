---
description: Execute swaps through any protocol.
---

# Execution

<figure><img src="../../.gitbook/assets/image (8).png" alt=""><figcaption></figcaption></figure>

Tycho Execution provides tools for **encoding and executing swaps** against Tycho Router and protocol executors. It is divided into two main components:

* **Encoding**: A Rust crate that encodes swaps and generates calldata for execution.
* **Executing**: Solidity contracts for executing trades on-chain.

The source code for **Tycho Execution** lives at <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-execution" target="_blank" rel="noopener noreferrer">`crates/tycho-execution`</a> inside the <a href="https://github.com/propeller-heads/tycho-indexer" target="_blank" rel="noopener noreferrer">Tycho monorepo</a>. For a practical example of its usage, please refer to our [Quickstart](../../).

## Token transfers

You can transfer tokens in one of three ways with Tycho Execution:

* Permit2
* Standard ERC20 Approvals
* Using Vault funds

See how to change between these options when encoding [here](encoding/#usertransfertype).

### Permit2

Tycho Execution supports **Permit2** for token approvals. Before executing a swap via our router, you must approve the **Permit2 contract** for the specified token and amount. This ensures the router has the necessary permissions to execute trades on your behalf.

Permit2 handling is **not** part of the encoding step. You are responsible for creating and signing the permit yourself. The `Permit2` utility struct is publicly exported from the encoding crate, so you can use it to build the `PermitSingle` and obtain the data needed for signing.

For more details on Permit2 and how to use it, see the <a href="https://docs.uniswap.org/contracts/permit2/overview" target="_blank" rel="noopener noreferrer">**Permit2 official documentation**</a>.

### **Standard ERC20 Approvals**

Tycho also supports traditional ERC20 approvals. In this model, you explicitly call `approve` on the token contract to grant the router permission to transfer tokens on your behalf. This is widely supported and may be preferred in environments where Permit2 is not yet available.

### Using the Vault

The TychoRouter includes a built-in vault (<a href="https://eips.ethereum.org/EIPS/eip-6909" target="_blank" rel="noopener noreferrer">ERC6909</a>) that lets you deposit, hold, and withdraw tokens directly in the router contract. The vault tracks per-user balances, so your tokens are only accessible by you.

The router draws from your deposited balance instead of performing a `transferFrom` on your wallet. This saves gas (no approval or external transfer needed) and lets you use fees, proceeds from previous trades, or pre-positioned liquidity directly.

Fees earned through the fee-taking system are automatically credited to the fee receiver's vault balance, making them immediately available for future swaps or withdrawals.

More on the Vault [here](vault.md).

## Security and Audits

The Tycho Router V2 and V3 have been audited by <a href="https://snd.github.io/" target="_blank" rel="noopener noreferrer">Maximilian Krüger</a>. Past audits are <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-execution/docs/audits" target="_blank" rel="noopener noreferrer">here</a>.

### Security Checklist

Follow this checklist when using TychoRouter. It covers essential security requirements but is not exhaustive.

* **Always set `minAmountOut`** on TychoRouter's `swap...` functions to the minimum acceptable output amount.
  * Example: if you expect 1000 USDC and accept 5% slippage, set `minAmountOut` to `950 * 10**6`.
  * Setting `minAmountOut` to `1` means you may receive just `1` due to faulty swap sequences, slippage or an attack.
* **Verify the price data used for `minAmountOut`** against at least one other independent price source. Incorrect price data may set `minAmountOut` too low, resulting in significant losses.
* **Never approve infinite allowances**, including those for Permit2.
* **Set Permit2 allowance and deadline as low as is practical.**

If you discover potential security issues or have suggestions for improvements, please reach out through our official channels.
