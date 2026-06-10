use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    prelude::BigInt,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::{abi::pool::events::Sync, store_key::StoreKey, traits::PoolAddresser};
use tycho_substreams::prelude::*;

// Auxiliary struct to serve as a key for the HashMaps.
#[derive(Clone, Hash, Eq, PartialEq)]
struct ComponentKey<T> {
    component_id: String,
    name: T,
}

impl<T> ComponentKey<T> {
    fn new(component_id: String, name: T) -> Self {
        ComponentKey { component_id, name }
    }
}

#[derive(Clone)]
struct PartialChanges {
    transaction: Transaction,
    entity_changes: HashMap<ComponentKey<String>, Attribute>,
    balance_changes: HashMap<ComponentKey<Vec<u8>>, BalanceChange>,
}

impl PartialChanges {
    // Consolidate the entity changes into a vector of EntityChanges. Initially, the entity changes
    // are in a map to prevent duplicates. For each transaction, we need to have only one final
    // state change, per state. Example:
    // If we have two sync events for the same pool (in the same tx), we need to have only one final
    // state change for the reserves. This will be the last sync event, as it is the final state
    // of the pool after the transaction.
    fn consolidate_entity_changes(self) -> Vec<EntityChanges> {
        self.entity_changes
            .into_iter()
            .map(|(key, attribute)| (key.component_id, attribute))
            .into_group_map()
            .into_iter()
            .map(|(component_id, attributes)| EntityChanges { component_id, attributes })
            .collect()
    }
}

#[substreams::handlers::map]
pub fn map_pool_events(
    block: eth::Block,
    block_entity_changes: BlockChanges,
    pools_store: StoreGetProto<ProtocolComponent>,
) -> Result<BlockChanges, substreams::errors::Error> {
    // Sync event is sufficient for our use-case. Since it's emitted on every reserve-altering
    // function call, we can use it as the only event to update the reserves of a pool.
    let mut block_entity_changes = block_entity_changes;
    let mut tx_changes: HashMap<Vec<u8>, PartialChanges> = HashMap::new();

    handle_sync(&block, &mut tx_changes, &pools_store);
    merge_block(&mut tx_changes, &mut block_entity_changes);

    Ok(block_entity_changes)
}

/// Handle the sync events and update the reserves of the pools.
///
/// This function is called for each block, and it will handle the sync events for each transaction.
/// Ring Swap V2 pairs are UniswapV2-style and emit Sync on every reserve-altering function call,
/// so we can use it as the only event to keep track of the pool state.
///
/// This function also relies on an intermediate HashMap to store the changes for each transaction.
/// This is necessary because we need to consolidate the changes for each transaction before adding
/// them to the block_entity_changes. This HashMap prevents us from having duplicate changes for the
/// same pool and token. See the PartialChanges struct for more details.
fn handle_sync(
    block: &eth::Block,
    tx_changes: &mut HashMap<Vec<u8>, PartialChanges>,
    store: &StoreGetProto<ProtocolComponent>,
) {
    let mut on_sync = |event: Sync, _tx: &eth::TransactionTrace, _log: &eth::Log| {
        let pool_address_hex = _log.address.to_hex();

        let pool =
            store.must_get_last(StoreKey::Pool.get_unique_pool_key(pool_address_hex.as_str()));
        // Ring pairs keep reserves in FewToken units while components expose the underlying
        // ERC-20s as tokens. Normalize reserves into the exposed underlying token order and
        // decimals so UniswapV2State can simulate the route as a standard ERC-20 pool.
        let reserves = exposed_reserves(&pool, event.reserve0, event.reserve1);

        let tx_change = tx_changes
            .entry(_tx.hash.clone())
            .or_insert_with(|| PartialChanges {
                transaction: _tx.into(),
                entity_changes: HashMap::new(),
                balance_changes: HashMap::new(),
            });

        for (i, reserve) in reserves.iter().enumerate() {
            let attribute_name = format!("reserve{}", i);
            // By using a HashMap, we can overwrite the previous value of the reserve attribute if
            // it is for the same pool and the same attribute name (reserves).
            tx_change.entity_changes.insert(
                ComponentKey::new(pool_address_hex.clone(), attribute_name.clone()),
                Attribute {
                    name: attribute_name,
                    value: reserve.clone().to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            );
        }

        // Update balance changes for each token
        for (index, token) in pool.tokens.iter().enumerate() {
            let balance = &reserves[index];
            // HashMap also prevents having duplicate balance changes for the same pool and token.
            tx_change.balance_changes.insert(
                ComponentKey::new(pool_address_hex.clone(), token.clone()),
                BalanceChange {
                    token: token.clone(),
                    balance: balance.clone().to_signed_bytes_be(),
                    component_id: pool_address_hex.as_bytes().to_vec(),
                },
            );
        }
    };

    let mut eh = EventHandler::new(block);
    // Filter the sync events by the pool address, to make sure we don't process events for other
    // Protocols that use the same event signature.
    eh.filter_by_address(PoolAddresser { store });
    eh.on::<Sync, _>(&mut on_sync);
    eh.handle_events();
}

/// Convert raw pair reserves (FewToken units, FewToken order) into the reserves of the exposed
/// component (underlying ERC-20 units, sorted underlying order).
fn exposed_reserves(pool: &ProtocolComponent, reserve0: BigInt, reserve1: BigInt) -> [BigInt; 2] {
    let reserve0 = normalize_reserve(
        reserve0,
        static_attribute_byte(pool, "fw_decimals0"),
        static_attribute_byte(pool, "underlying_decimals0"),
    );
    let reserve1 = normalize_reserve(
        reserve1,
        static_attribute_byte(pool, "fw_decimals1"),
        static_attribute_byte(pool, "underlying_decimals1"),
    );

    if static_attribute_byte(pool, "reserves_inverted") == 1 {
        [reserve1, reserve0]
    } else {
        [reserve0, reserve1]
    }
}

fn normalize_reserve(reserve: BigInt, from_decimals: u8, to_decimals: u8) -> BigInt {
    match from_decimals.cmp(&to_decimals) {
        std::cmp::Ordering::Equal => reserve,
        std::cmp::Ordering::Less => {
            reserve * BigInt::from(10u64).pow(u32::from(to_decimals - from_decimals))
        }
        std::cmp::Ordering::Greater => {
            reserve / BigInt::from(10u64).pow(u32::from(from_decimals - to_decimals))
        }
    }
}

/// Read a single-byte static attribute written by map_pools_created. Every pool stored by this
/// package is guaranteed to carry the Ring attributes, so a missing one is a bug, not a data case.
fn static_attribute_byte(pool: &ProtocolComponent, name: &str) -> u8 {
    pool.static_att
        .iter()
        .find(|att| att.name == name)
        .and_then(|att| att.value.last())
        .copied()
        .unwrap_or_else(|| panic!("Ring pool {} is missing the {} static attribute", pool.id, name))
}

/// Merge the changes from the sync events with the create_pool events previously mapped on
/// block_entity_changes.
///
/// Parameters:
/// - tx_changes: HashMap with the changes for each transaction. This is the same HashMap used in
///   handle_sync
/// - block_entity_changes: The BlockChanges struct that will be updated with the changes from the
///   sync events.
///
/// This HashMap comes pre-filled with the changes for the create_pool events, mapped in
///   1_map_pool_created.
///
/// This function is called after the handle_sync function, and it is expected that
/// block_entity_changes will be complete after this function ends.
fn merge_block(
    tx_changes: &mut HashMap<Vec<u8>, PartialChanges>,
    block_entity_changes: &mut BlockChanges,
) {
    let mut tx_entity_changes_map = HashMap::new();

    // Add created pools to the tx_changes_map
    for change in block_entity_changes
        .changes
        .clone()
        .into_iter()
    {
        let transaction = change.tx.as_ref().unwrap();
        tx_entity_changes_map
            .entry(transaction.hash.clone())
            .and_modify(|c: &mut TransactionChanges| {
                c.component_changes
                    .extend(change.component_changes.clone());
                c.entity_changes
                    .extend(change.entity_changes.clone());
            })
            .or_insert(change);
    }

    // First, iterate through the previously created transactions, extracted from the
    // map_pool_created step. If there are sync events for this transaction, add them to the
    // block_entity_changes and the corresponding balance changes.
    for change in tx_entity_changes_map.values_mut() {
        let tx = change
            .clone()
            .tx
            .expect("Transaction not found")
            .clone();

        // If there are sync events for this transaction, add them to the block_entity_changes
        if let Some(partial_changes) = tx_changes.remove(&tx.hash) {
            change.entity_changes = partial_changes
                .clone()
                .consolidate_entity_changes();
            change.balance_changes = partial_changes
                .balance_changes
                .into_values()
                .collect();
        }
    }

    // If there are any transactions left in the tx_changes, it means that they are transactions
    // that changed the state of the pools, but were not included in the block_entity_changes.
    // This happens for every regular transaction that does not actually create a pool. By the
    // end of this function, we expect block_entity_changes to be up-to-date with the changes
    // for all sync and new_pools in the block.
    for partial_changes in tx_changes.values() {
        tx_entity_changes_map.insert(
            partial_changes.transaction.hash.clone(),
            TransactionChanges {
                tx: Some(partial_changes.transaction.clone()),
                contract_changes: vec![],
                entity_changes: partial_changes
                    .clone()
                    .consolidate_entity_changes(),
                balance_changes: partial_changes
                    .balance_changes
                    .clone()
                    .into_values()
                    .collect(),
                component_changes: vec![],
            },
        );
    }

    block_entity_changes.changes = tx_entity_changes_map
        .into_values()
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_pool(attributes: &[(&str, u8)]) -> ProtocolComponent {
        ProtocolComponent {
            static_att: attributes
                .iter()
                .map(|(name, value)| Attribute {
                    name: name.to_string(),
                    value: vec![*value],
                    change: ChangeType::Creation.into(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn normalize_reserve_keeps_equal_decimals() {
        assert_eq!(normalize_reserve(BigInt::from(1_000_000u64), 6, 6), BigInt::from(1_000_000u64));
    }

    #[test]
    fn normalize_reserve_scales_down_to_underlying_decimals() {
        assert_eq!(
            normalize_reserve(BigInt::from(1_000_000_000_000_000_000u64), 18, 6),
            BigInt::from(1_000_000u64)
        );
    }

    #[test]
    fn normalize_reserve_scales_up_to_underlying_decimals() {
        assert_eq!(
            normalize_reserve(BigInt::from(1_000_000u64), 6, 18),
            BigInt::from(1_000_000_000_000_000_000u64)
        );
    }

    #[test]
    fn exposed_reserves_normalizes_decimals_in_pair_order() {
        let pool = ring_pool(&[
            ("fw_decimals0", 18),
            ("fw_decimals1", 18),
            ("underlying_decimals0", 18),
            ("underlying_decimals1", 6),
            ("reserves_inverted", 0),
        ]);

        let reserves =
            exposed_reserves(&pool, BigInt::from(5u64), BigInt::from(2_000_000_000_000u64));

        assert_eq!(reserves, [BigInt::from(5u64), BigInt::from(2u64)]);
    }

    #[test]
    fn exposed_reserves_swaps_inverted_reserves() {
        let pool = ring_pool(&[
            ("fw_decimals0", 18),
            ("fw_decimals1", 6),
            ("underlying_decimals0", 18),
            ("underlying_decimals1", 6),
            ("reserves_inverted", 1),
        ]);

        let reserves = exposed_reserves(&pool, BigInt::from(7u64), BigInt::from(9u64));

        assert_eq!(reserves, [BigInt::from(9u64), BigInt::from(7u64)]);
    }
}
