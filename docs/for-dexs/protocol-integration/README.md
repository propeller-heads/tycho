# Protocol Integration

The <a href="https://github.com/propeller-heads/tycho-indexer" target="_blank" rel="noopener noreferrer">Tycho monorepo</a> contains everything you need to integrate liquidity layer protocols (DEXs, Staking, Lending, etc.) into Tycho.

## Integration Process

To integrate with Tycho, you need three components:

1. **Indexing**: You must provide the protocol state/data needed for simulation and execution.
2. **Simulation:** You have to implement the protocol's logic for simulations.
3. **Execution**: You have to define how to encode and execute swaps against your protocol.

We provide a comprehensive [testing](3.-testing.md) suite to ensure you can integrate indexing, simulation, and execution correctly. A passing test suite is essential for an integration to be considered complete.

### Indexing

You will need a <a href="https://substreams.streamingfast.io/" target="_blank" rel="noopener noreferrer">substreams</a> package that emits a specified set of messages. If your protocol already has a <a href="https://github.com/messari/substreams" target="_blank" rel="noopener noreferrer">substreams package</a>, you can adjust it to emit the required messages.

It's important to note that simulation happens entirely off-chain. This means everything you need during simulation must be explicitly indexed.

### Simulation

Tycho offers two integration modes:

* **VM Integration:** You need to implement an adapter interface in a language that compiles to VM bytecode. This SDK provides a Solidity interface ([**read more here**](simulation/ethereum-solidity.md)**).** Simulations run in an empty VM loaded only with the indexed contracts, storage and token balances.&#x20;
* **Native Rust Integration:** You need to implement a Rust trait that defines the protocol logic. You must index values used in this logic as state attributes.

### Execution

To enable swap execution, implement:

1. **SwapEncoder**: This is a Rust struct that formats input/output tokens, pool addresses, and other parameters correctly for the `Executor` contract.
2. **Executor**: This is a Solidity contract that handles the execution of swaps over your protocol's liquidity pools.&#x20;

## Integration Criteria

Tycho supports many protocol designs. However, certain architectures present indexing challenges.

Before you integrate, consider these unsupported designs:

* Protocols where any operation that Tycho should support requires off-chain data, such as signed prices.
