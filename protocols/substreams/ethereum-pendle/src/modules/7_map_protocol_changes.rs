use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetRaw},
};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use crate::{
    keys::{contract_id, market_by_yt_key, market_tokens_key},
    market_state::{
        last_ln_implied_rate, py_index_stored, LAST_LN_IMPLIED_RATE, PY_INDEX_STORED, TOTAL_PT,
        TOTAL_SY,
    },
};

/// Joins new components, market state and balance changes into the per-transaction output.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
    reserve_deltas: BlockBalanceDeltas,
    reserve_store: StoreDeltas,
    components_store: StoreGetRaw,
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

    // Reserves arrive as accumulated absolutes keyed by the PT and SY token addresses; the market's
    // role list is what turns those back into named attributes.
    for (_, (tx, balances)) in aggregate_balances_changes(reserve_store, reserve_deltas) {
        for (component_id, token_balances) in &balances {
            let component_id = String::from_utf8(component_id.clone())
                .expect("reserve component id is not valid utf-8");
            let Some((sy, pt)) = market_sy_pt(&components_store, &component_id) else { continue };

            let attributes = token_balances
                .values()
                .filter_map(|change| {
                    let name = match &change.token {
                        token if *token == pt => TOTAL_PT,
                        token if *token == sy => TOTAL_SY,
                        _ => return None,
                    };
                    Some(state_attribute(name, change.balance.clone()))
                })
                .collect::<Vec<_>>();
            if attributes.is_empty() {
                continue;
            }

            transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                .add_entity_change(&EntityChanges { component_id, attributes });
        }
    }

    for (tx, change) in absolute_state_changes(&block, &components_store) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
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

/// Collects the state attributes that events report in full rather than as a change.
///
/// `UpdateImpliedRate` comes from the market and `NewInterestIndex` from its yield token, which is
/// not itself a component — hence the reverse lookup. Logs are walked in block order, so a market
/// touched twice in one transaction ends on its last value.
fn absolute_state_changes(
    block: &eth::Block,
    components_store: &StoreGetRaw,
) -> Vec<(Transaction, EntityChanges)> {
    let mut changes = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let entry = if let Some(rate) = last_ln_implied_rate(log) {
                let component_id = contract_id(&log.address);
                components_store
                    .get_last(market_tokens_key(&component_id))
                    .map(|_| (component_id, LAST_LN_IMPLIED_RATE, rate))
            } else if let Some(index) = py_index_stored(log) {
                components_store
                    .get_last(market_by_yt_key(&log.address))
                    .map(|id| {
                        let component_id =
                            String::from_utf8(id).expect("market id is not valid utf-8");
                        (component_id, PY_INDEX_STORED, index)
                    })
            } else {
                continue;
            };

            let Some((component_id, name, value)) = entry else { continue };
            changes.push((
                tx.into(),
                EntityChanges {
                    component_id,
                    attributes: vec![state_attribute(name, value.to_signed_bytes_be())],
                },
            ));
        }
    }
    changes
}

/// Returns a market's SY and PT addresses, or `None` if the id names something that is not a
/// market.
fn market_sy_pt(store: &StoreGetRaw, component_id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let roles = store.get_last(market_tokens_key(component_id))?;
    let roles: Vec<Vec<u8>> = serde_sibor::from_bytes(&roles)
        .expect("deserializing market token roles from the component store");
    let [sy, pt, _yt] = roles.as_slice() else {
        panic!("market {component_id} stored {} roles, expected 3", roles.len())
    };
    Some((sy.clone(), pt.clone()))
}

fn state_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}
