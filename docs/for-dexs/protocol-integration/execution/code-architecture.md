---
description: >-
  Tycho Execution offers an encoding tool (a Rust crate for generating swap
  calldata) and execution components (Solidity contracts). This is how
  everything works together.
---

# Code Architecture

The following diagram summarizes the code architecture:

<figure><img src="../../../.gitbook/assets/image (13).png" alt=""><figcaption></figcaption></figure>

### Encoding

The `TychoRouterEncoder` validates solutions and produces a list of transactions to execute against the `TychoRouterV3`.

The `TychoRouterEncoder` uses a `StrategyEncoder` that it chooses automatically based on the solution (see more about strategies [here](../../../concepts.md#strategy)).

Internally, all encoders choose the appropriate `SwapEncoder`(s) to encode the individual swaps, which depend on the protocols used in the solution.

### Execution

The `TychoRouterV3` calls one or more `Executor`s (corresponding with the output of the `SwapEncoder`s) to interact with the correct protocol and perform each swap of the solution. The `TychoRouterV3` verifies that the user receives a minimum amount of the output token.
