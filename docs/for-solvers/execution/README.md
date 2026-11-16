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

You can transfer tokens in one of two ways with Tycho Execution:

* Permit2
* Standard ERC20 Approvals

See how to change between these options when encoding [here](encoding/#models).

### Permit2

Tycho Execution supports **Permit2** for token approvals. Before executing a swap via our router, you must approve the **Permit2 contract** for the specified token and amount. This ensures the router has the necessary permissions to execute trades on your behalf.

Permit2 handling is **not** part of the encoding step. You are responsible for creating and signing the permit yourself. The `Permit2` utility struct is publicly exported from the encoding crate, so you can use it to build the `PermitSingle` and obtain the data needed for signing.

For more details on Permit2 and how to use it, see the <a href="https://docs.uniswap.org/contracts/permit2/overview" target="_blank" rel="noopener noreferrer">**Permit2 official documentation**</a>.

### **Standard ERC20 Approvals**

Tycho also supports traditional ERC20 approvals. In this model, you explicitly call `approve` on the token contract to grant the router permission to transfer tokens on your behalf. This is widely supported and may be preferred in environments where Permit2 is not yet available.

## Security and Audits

The Tycho Router V2 and V3 have been audited by <a href="https://snd.github.io/" target="_blank" rel="noopener noreferrer">Maximilian Krüger</a>. Past audits are <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-execution/docs/audits" target="_blank" rel="noopener noreferrer">here</a>.

### Security Checklist

Follow this checklist when using TychoRouterV3. It covers essential security requirements but is not exhaustive.

* **Always set `expectedAmountOut` and `minAmountOut`** on TychoRouterV3's `swap...` functions. `expectedAmountOut` is your quoted output; `minAmountOut` is the amount below which the swap reverts.
  * Example: if you expect 1000 USDC and accept 5% slippage, set `expectedAmountOut` to `1000 * 10**6` and `minAmountOut` to `950 * 10**6`.
  * `minAmountOut` must be non-zero and no greater than `expectedAmountOut` — see [Slippage bounds](encoding/#slippage-bounds).
  * The router sets no lower bound, so a `minAmountOut` far below your quote still passes: a faulty swap sequence, slippage or an attack can then leave you with far less than you quoted.
* **Verify the price data behind `minAmountOut`** against at least one other independent price source. A `minAmountOut` derived from a bad quote can sit far below the fair output, so the guardrail fails to protect you and the losses can be large.
* **Never leave tokens in the router.** The router holds no user balances. Its only check on a swap's result is that the receiver got at least `minAmountOut` of `tokenOut`. You lose any tokens left in the router, because any later swap can spend them. This includes tokens sent to the router directly, and ETH sent as `msg.value` that your swap sequence does not spend.
* **Never approve infinite allowances**, including those for Permit2.
* **Set Permit2 allowance and deadline as low as is practical.**

If you discover potential security issues or have suggestions for improvements, please reach out through our official channels.
