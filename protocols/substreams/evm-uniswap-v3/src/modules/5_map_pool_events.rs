use std::{collections::HashMap, vec};
use substreams::store::{StoreGet, StoreGetBigInt, StoreGetProto};
use substreams_ethereum::pb::eth::v2::{self as eth, TransactionTrace};

use substreams_helper::hex::Hexable;

use crate::{
    events::{get_log_changed_attributes, get_log_changed_balances},
    pb::uniswap::v3::Pool,
};

use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pool_events(
    block: eth::Block,
    created_pools: BlockChanges,
    pools_store: StoreGetProto<Pool>,
    balance_store: StoreGetBigInt,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut tx_changes_map: HashMap<Vec<u8>, TransactionChanges> = HashMap::new();

    // Add created pools to the tx_changes_map
    for change in created_pools.changes.into_iter() {
        let transaction = change.tx.as_ref().unwrap();
        tx_changes_map
            .entry(transaction.hash.clone())
            .and_modify(|c| {
                c.component_changes
                    .extend(change.component_changes.clone());
                c.entity_changes
                    .extend(change.entity_changes.clone());
            })
            .or_insert(change);
    }

    for trx in block.transactions() {
        for (log, call_view) in trx.logs_with_calls() {
            // Skip if the log is not from a known uniswapV3 pool.
            if let Some(pool) =
                pools_store.get_last(format!("{}:{}", "Pool", &log.address.to_hex()))
            {
                let changed_attributes = get_log_changed_attributes(
                    log,
                    &call_view.call.storage_changes,
                    pool.address
                        .clone()
                        .as_slice()
                        .try_into()
                        .expect("Pool address is not 20 bytes long"),
                );

                let mut balance_changes: Vec<BalanceChange> = vec![];

                if !(get_log_changed_balances(log, &pool).is_empty()) {
                    let token_0_balance = balance_store.get_last(format!(
                        "pool:{0}:token:{1}",
                        hex::encode(&pool.address),
                        hex::encode(&pool.token0)
                    ));
                    let token_1_balance = balance_store.get_last(format!(
                        "pool:{0}:token:{1}",
                        hex::encode(&pool.address),
                        hex::encode(&pool.token1)
                    ));

                    let pool_address_utf8 = pool
                        .address
                        .clone()
                        .to_hex()
                        .as_bytes()
                        .to_vec();

                    let token_0_balance_change = BalanceChange {
                        component_id: pool_address_utf8.clone(),
                        token: pool.token0.clone(),
                        balance: token_0_balance
                            .clone()
                            .expect("Couldn't get balance from store")
                            .to_bytes_be()
                            .1,
                    };
                    let token_1_balance_change = BalanceChange {
                        component_id: pool_address_utf8.clone(),
                        token: pool.token1.clone(),
                        balance: token_1_balance
                            .clone()
                            .expect("Couldn't get balance from store")
                            .to_bytes_be()
                            .1,
                    };

                    balance_changes.extend([token_0_balance_change, token_1_balance_change]);
                }

                // Create entity changes
                let entity_changes: Vec<EntityChanges> = vec![EntityChanges {
                    component_id: pool.address.clone().to_hex(),
                    attributes: changed_attributes,
                }];

                update_tx_changes_map(entity_changes, balance_changes, &mut tx_changes_map, trx);
            } else {
                continue;
            }
        }
    }

    // Make a list of all HashMap values:
    let tx_entity_changes: Vec<TransactionChanges> = tx_changes_map.into_values().collect();

    let tycho_block: Block = (&block).into();

    let block_entity_changes =
        BlockChanges { block: Some(tycho_block), changes: tx_entity_changes, ..Default::default() };

    Ok(block_entity_changes)
}

fn update_tx_changes_map(
    entity_changes: Vec<EntityChanges>,
    balance_changes: Vec<BalanceChange>,
    tx_changes_map: &mut HashMap<Vec<u8>, TransactionChanges>,
    tx_trace: &TransactionTrace,
) {
    // Get the tx hash
    let tx_hash = tx_trace.hash.clone();

    // Get the tx changes from the map
    let tx_changes = tx_changes_map.get_mut(&tx_hash);

    // Update the tx changes
    if let Some(tx_changes) = tx_changes {
        // Merge the entity changes
        tx_changes.entity_changes =
            merge_entity_changes(&tx_changes.entity_changes, &entity_changes);

        // Merge the balance changes
        tx_changes.balance_changes =
            merge_balance_changes(&tx_changes.balance_changes, &balance_changes);
    } else {
        // If the tx is not in the map, add it
        let tx_changes = TransactionChanges {
            tx: Some(tx_trace.into()),
            contract_changes: vec![],
            entity_changes,
            balance_changes,
            component_changes: vec![],
            ..Default::default()
        };
        tx_changes_map.insert(tx_hash, tx_changes);
    }
}

/// Merges new entity changes into an existing collection of entity changes and returns the merged
/// result. For each entity change, if an entity change with the same component_id exists, its
/// attributes are merged. If an attribute with the same name exists, the new attribute replaces the
/// old one.
///
/// Parameters:
/// - `existing_changes`: A reference to a vector of existing entity changes.
/// - `new_changes`: A reference to a vector of new entity changes to be merged.
///
/// Returns:
/// A new `Vec<EntityChanges>` containing the merged entity changes.
fn merge_entity_changes(
    existing_changes: &[EntityChanges],
    new_changes: &Vec<EntityChanges>,
) -> Vec<EntityChanges> {
    let mut changes_map = existing_changes
        .iter()
        .cloned()
        .map(|change| (change.component_id.clone(), change))
        .collect::<HashMap<_, _>>();

    for change in new_changes {
        match changes_map.get_mut(&change.component_id) {
            Some(existing_change) => {
                let mut attributes_map = existing_change
                    .attributes
                    .iter()
                    .cloned()
                    .map(|attr| (attr.name.clone(), attr))
                    .collect::<HashMap<_, _>>();

                for attr in &change.attributes {
                    attributes_map.insert(attr.name.clone(), attr.clone());
                }

                existing_change.attributes = attributes_map.into_values().collect();
            }
            None => {
                changes_map.insert(change.component_id.clone(), change.clone());
            }
        }
    }

    changes_map.into_values().collect()
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct BalanceChangeKey {
    token: Vec<u8>,
    component_id: Vec<u8>,
}

/// Merges two vectors of `BalanceChange` structures into a single vector. If two `BalanceChange`
/// instances have the same combination of `token` and `component_id`, the value from the
/// `new_entries` vector will replace the one from the `current` vector.
///
/// Parameters:
/// - `current`: A reference to a vector of `BalanceChange` instances representing the current
///   balance changes.
/// - `new_entries`: A reference to a vector of `BalanceChange` instances representing new balance
///   changes to be merged.
///
/// Returns:
/// A `Vec<BalanceChange>` that contains the merged balance changes.
fn merge_balance_changes(
    current: &[BalanceChange],
    new_entries: &Vec<BalanceChange>,
) -> Vec<BalanceChange> {
    let mut balances = HashMap::new();

    for balance_change in current.iter().chain(new_entries) {
        let key = BalanceChangeKey {
            token: balance_change.token.clone(),
            component_id: balance_change.component_id.clone(),
        };

        balances.insert(key, balance_change.clone());
    }

    balances.into_values().collect()
}
