---
description: Overview of Tycho, its components and how to get started.
---

# About

<figure><img src="../.gitbook/assets/Tycho-header (1).png" alt=""><figcaption></figcaption></figure>

## What is Tycho?

Tycho is an open-source interface to on-chain liquidity. Tycho

* **Indexes** DEX protocol state for you with low latency,
* **Simulates** swaps extremely fast with one interface for all DEXs, and
* **Executes** swaps on-chain

## Ending the integration nightmare

<figure><img src="../.gitbook/assets/image (5).png" alt=""><figcaption></figcaption></figure>

Integrations are the largest point of friction for both DEXs and Solvers:

* **Solvers** can't scale integrations. So, Solvers spend much of their time on integrations, and new solvers can't catch up and compete.
* **DEXs** need to convince solvers to integrate them to get orderflow and win LPs. But Solvers prioritize DEXs with liquidity. This makes it hard for new DEXs to get the flow their design deserves.

In the end, every solver separately integrates every DEX — leading to massive wasted effort from which no one benefits.

Tycho fixes this:

* DEXs can integrate themselves and don't need to wait for solvers, and
* Solvers can use new DEXs without any additional effort.

Tycho lowers barriers to entry so that both innovative DEXs and Solvers have a chance to win flow.

## Get started

### Solvers — Access more liquidity

Tycho makes it easy to simulate and execute over on-chain liquidity sources — without needing to understand protocol internals, run nodes, or do RPC calls.

To set up, go to the [Tycho Indexer quickstart](for-solvers/indexer/) and start your liquidity stream.

### DEXs — Get more flow

To integrate your DEX, submit a PR to Tycho Protocol Integrations on <a href="https://github.com/propeller-heads/tycho-protocol-sdk" target="_blank" rel="noopener noreferrer">GitHub</a>.

To get started, check the [Protocol SDK](for-dexs/protocol-integration/) docs.

Or <a href="https://t.me/jaintanay_" target="_blank" rel="noopener noreferrer">contact our team</a> so we can help you integrate.

## Components of Tycho

<figure><img src="../.gitbook/assets/tycho.png" alt="" width="563"><figcaption></figcaption></figure>

Tycho has three components for solvers:

* **Tycho Indexer**: Infrastructure to parse, store and stream protocol state deltas. It also comes with clients in Python and Rust and a hosted webstream if you don't want to run your version of the Indexer. → [Indexer docs](for-solvers/indexer/).
* **Tycho Protocol Simulation:** A simulation library with a unified interface to query swaps and prices. Optimized for speed, running on compiled contracts in REVM with in-memory storage. → [Protocol Simulation docs](for-solvers/simulation.md).
* **Tycho Execution:** Audited and gas-efficient router and DEX executor contracts for safe, simple, and competitive execution of swaps.

And one integration SDK for DEXs:

* **Tycho Protocol Integration:** An SDK for any DEX (or Stable Coin, LRT, etc.) to integrate their liquidity and receive flow from solvers.

## FAQ

<details>

<summary><strong>How does Tycho compare to just parsing logs myself?</strong></summary>

While you can parse logs directly, Tycho provides parsed and structured data, offers a unified interface across protocols, manages reorgs automatically, can handle protocols that don't emit logs, and saves you the infrastructure cost of running a node.

</details>

<details>

<summary><strong>Does this add gas to my swaps?</strong></summary>

No it does not. Tycho contracts make it easy to simulate a DEX correctly for your swaps. But you can still execute the swaps directly with the DEX — as gas efficient as possible.

</details>

<details>

<summary><strong>How do you handle reorgs?</strong></summary>

Reorgs are handled automatically through our delta system. The client maintains the correct state without you having to worry about block reorganizations.

</details>

<details>

<summary><strong>How does latency compare to other solutions?</strong></summary>

Tycho processes updates in under 100ms (plus network latency). While an additional hop compared to running your own nodes, geographically distributed nodes race to provide data, which can be faster than relying on a single node.

</details>

<details>

<summary><strong>Can I still use my own UniV2/V3 implementations?</strong></summary>

Yes! Many teams use Tycho VM for newer/complex protocols while keeping their analytical implementations for simpler pools.

</details>

<details>

<summary><strong>What about UniV4 hooks?</strong></summary>

We aim to support as many hook variants as possible through our VM implementation.

</details>

<details>

<summary><strong>What's the difference between Native and VM implementations?</strong></summary>

Native implementations provide protocol-specific state variables directly, letting you implement your own math and optimizations. VM implementations provide a unified interface through REVM, making integration easier but with less low-level control. Choose based on your needs: Native for maximum control, VM for easier integration.

</details>

<details>

<summary><strong>How reliable is the state data?</strong></summary>

The system handles reorgs automatically, keeps track of TVL changes, and maintains consistency across state updates. The data is synced against on-chain state and continuously validated.

</details>
