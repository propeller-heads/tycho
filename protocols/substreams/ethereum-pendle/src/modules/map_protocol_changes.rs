use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::pb::substreams::StoreDeltas;
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

/// Joins new components and balance changes into the per-transaction output.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    for tx_components in &new_components.tx_components {
        let tx = tx_components
            .tx
            .as_ref()
            .expect("component batch without a transaction");
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &tx_components.components {
            builder.add_protocol_component(component);
        }
    }

    for (_, (tx, balances)) in aggregate_balances_changes(balance_store, deltas) {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for token_balances in balances.values() {
            for balance_change in token_balances.values() {
                builder.add_balance_change(balance_change);
            }
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        // Raw storage is only consumed by the Dynamic Contract Indexer. Pendle is a native
        // integration: everything the simulation needs is an explicit attribute here.
        storage_changes: vec![],
    })
}
