# Common Patterns & Problems

Some protocol design choices follow a common pattern. Instructions on how to handle these cases are provided. Such cases include:

* [Factory contracts](./#factory-contracts)
* [Tracking contract storage](./#tracking-contract-storage) \[VM implementations]
* [Using the Dynamic Contract Indexer (DCI)](./#using-the-dynamic-contract-indexer-dci)
* [Using relative component balances](./#using-relative-balances)
* [Vaults/singleton contracts](./#vaults-singleton-contracts)
* [Persisting data between modules](./#persisting-data-between-modules)

### Factory contracts

A common protocol design is to use factories to deploy components. In this case it is recommended to detect the creation of these components and store their contract addresses (an potentially other metadata) to track them for use later in the module. See [Tracking Components.](tracking-components.md)

### Tracking contract storage

For VM implementations it is essential that the contract code and storage of all involved contracts are tracked. **If these contracts are known, static, and their creation event is observable by the substreams package** (occurs after the start block of the package), they can be indexed by the substream package with some helpful utils: see [Tracking Contract Storage](tracking-contract-storage.md).

If these contracts need to be dynamically determined or their creation event is not observable, instead see [Using the Dynamic Contract Indexer](./#using-the-dynamic-contract-indexer-dci) below:

### Using the Dynamic Contract Indexer (DCI)

For contracts that cannot be statically determined at time of integration or their creation events are not observable by the substreams package, Dynamic Contract Indexer (DCI) support is provided. Keep in mind using this feature adds indexing latency and should be avoided if possible.

The DCI allows you to specify external contract call information, which it will use to trace and identify all contract dependencies. It then automates the indexing of those identified contracts and their relevant storage slots. See[ Dynamic Contract Indexer](dynamic-contract-indexing-dci/).

### Using relative component balances

For some protocols, absolute component balances are not easily obtainable. Instead, balance deltas/changes are observed. Since absolute balances are expected by Tycho, it is recommended to use a balance store to track current balances and apply deltas as the occur. See [Normalizing relative ERC20 Balances](normalizing-relative-erc20-balances.md).

### Vaults/Singleton contracts

For protocols that store balances in an a-typical way (not on dedicated pool contracts), a special approach to balance tracking must be used. See[ Tracking Contract Balances](tracking-contract-balances.md).

When a contract change is indexed, consumers of the indexed data typically trigger recalculating prices on all pools marked as associated with that contract (the contract is listed in the `ProtocolComponent`'s contracts field). In the case where multiple components are linked to a single contract, such as a vault, this may cause excessive and unnecessary simulations on components that are unaffected by a specific change on the linked contract. In this case it is recommended to use 'manual update' triggers. See [Reserved Attributes](../reserved-attributes.md#manual_updates) for more details.

### Persisting data between modules

It is often the case where data needs to be persisted between modules in your substream package. This may be because components and their metadata (such as their tokens, or pool type) are needed when handling state changes downstream, or could be because the protocol reports relative changes instead of absolute values and the relative changes must be compounded to reach an absolute value. For this, substream <a href="https://docs.substreams.dev/reference-material/substreams-components/modules/types#store-modules" target="_blank" rel="noopener noreferrer">Stores</a> and [Custom Protobuf Models](custom-protobuf-models.md) are recommended.
