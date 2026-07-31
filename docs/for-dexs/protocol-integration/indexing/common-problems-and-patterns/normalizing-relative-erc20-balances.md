# Normalizing relative ERC20 Balances

Tracking balances is complex if only relative values are available. If the protocol provides absolute balances (e.g., through logs), you can skip this section and simply emit the absolute balances.

To derive absolute balances from relative values, you’ll need to aggregate by component and token, ensuring that balance changes are tracked at the transaction level within each block.&#x20;

### Implementation Steps:

#### 1. Index relative balance changes

To accurately process each block and report balance changes, implement a handler that returns the `BlockBalanceDeltas` struct. Each `BalanceDelta` for a component-token pair must be assigned a strictly increasing ordinal to preserve transaction-level integrity. Incorrect ordinal sequencing can lead to inaccurate balance aggregation.

Example interface for a handler that uses an integer, loaded from a store to indicate if a specific address is a component:

```rust
#[substreams::handlers::map]
pub fn map_relative_balances(
    block: eth::v2::Block,
    components_store: StoreGetInt64,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    todo!()
}
```

Use the `tycho_substream::balances::extract_balance_deltas_from_tx` function from our Substreams SDK to extract `BalanceDelta` data from ERC20 Transfer events for a given transaction, as in the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/substreams/ethereum-curve/src/modules.rs" target="_blank" rel="noopener noreferrer">Curve implementation</a>.

#### 2. Aggregate balances with an additive store

To efficiently convert `BlockBalanceDeltas` messages into absolute values while preserving transaction granularity, use the `StoreAddBigInt` type with a store module. The `tycho_substream::balances::store_balance_changes` helper function simplifies this task.

Typical usage of this function:

```rust
#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}
```

#### 3. Combine absolute values with component and address

Finally, associate absolute balances with their corresponding transaction, component, and token. Use the `tycho_substream::balances::aggregate_balances_changes` helper function for the final aggregation step. This function outputs `BalanceChange` structs for each transaction, which can then be integrated into `map_protocol_changes` to retrieve absolute balance changes per transaction.

Example usage:

```rust
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::v2::Block,
    grouped_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetInt64,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let mut transaction_contract_changes: HashMap<_, TransactionChanges> = HashMap::new();

    aggregate_balances_changes(balance_store, deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            transaction_contract_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChanges::new(&tx))
                .balance_changes
                .extend(balances.into_values());
        });
}
```

Each step ensures accurate tracking of balance changes, making it possible to reflect absolute values for components and tokens reliably.
