use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChanges, TickDeltas,
};
use itertools::Itertools;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    vec,
};
use substreams::{
    pb::substreams::{StoreDelta, StoreDeltas},
    scalar::BigInt,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

type PoolAddress = Vec<u8>;

/// Core logic for collecting transaction changes from various inputs.
/// Returns the sorted transaction changes ready to be used in BlockChanges.
#[allow(clippy::too_many_arguments)]
pub fn collect_transaction_changes(
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
    pool_liquidity_changes: LiquidityChanges,
    pool_liquidity_store_deltas: StoreDeltas,
) -> Vec<TransactionChanges> {
    // We merge contract changes by transaction (identified by transaction index) making it easy to
    // sort them at the very end.
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // Add created pools to the tx_changes_map
    for change in created_pools.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        change
            .component_changes
            .iter()
            .for_each(|c| {
                builder.add_protocol_component(c);
            });
        change
            .entity_changes
            .iter()
            .for_each(|ec| {
                builder.add_entity_change(ec);
            });
        change
            .balance_changes
            .iter()
            .for_each(|bc| {
                builder.add_balance_change(bc);
            });
    }

    // Balance changes are gathered by the `StoreDelta` based on `PoolBalanceChanged` creating
    //  `BlockBalanceDeltas`. We essentially just process the changes that occurred to the `store`
    // this  block. Then, these balance changes are merged onto the existing map of tx contract
    // changes,  inserting a new one if it doesn't exist.
    aggregate_balances_changes(balances_store_deltas, balances_map_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    // Aggregate before the SDK builder so later writes cannot replace the transaction's
    // net change type (Creation -> Update, or Deletion -> Creation).
    for (tx, pool_address, attribute) in
        ticks_to_attribute_updates(ticks_map_deltas, ticks_store_deltas)
    {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        builder.add_entity_change(&EntityChanges {
            component_id: pool_address.to_hex(),
            attributes: vec![attribute],
        });
    }

    // Insert liquidity changes
    pool_liquidity_store_deltas
        .deltas
        .into_iter()
        .zip(pool_liquidity_changes.changes)
        .for_each(|(store_delta, change)| {
            let new_value_bigint = BigInt::from_str(
                String::from_utf8(store_delta.new_value)
                    .unwrap()
                    .split(':')
                    .nth(1)
                    .unwrap(),
            )
            .unwrap();
            let tx = change.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            builder.add_entity_change(&EntityChanges {
                component_id: change.pool_address.to_hex(),
                attributes: vec![Attribute {
                    name: "liquidity".to_string(),
                    value: new_value_bigint.to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                }],
            });
        });

    // Insert others changes
    events
        .pool_events
        .into_iter()
        .flat_map(event_to_attributes_updates)
        .for_each(|(tx, pool_address, attr)| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.add_entity_change(&EntityChanges {
                component_id: pool_address.to_hex(),
                attributes: vec![attr],
            });
        });

    transaction_changes
        .drain()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
        .collect()
}

struct TickUpdate {
    transaction: Transaction,
    old_value: Vec<u8>,
    new_value: BigInt,
}

/// Emits one net change per transaction, pool and tick, using the earliest old value and latest
/// new value. A tick absent at both ends of the transaction emits no change.
fn ticks_to_attribute_updates(
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
) -> Vec<(Transaction, PoolAddress, Attribute)> {
    // Both ticks of a ModifyLiquidity event share an ordinal; their store writes may be
    // reordered by the unstable sort, so pair by key and ordinal rather than position.
    let mut store_deltas: HashMap<(String, u64), StoreDelta> = ticks_store_deltas
        .deltas
        .into_iter()
        .map(|delta| ((delta.key.clone(), delta.ordinal), delta))
        .collect();
    let mut ticks = ticks_map_deltas.deltas;
    ticks.sort_unstable_by_key(|delta| delta.ordinal);
    let mut updates: BTreeMap<(u64, PoolAddress, i32), TickUpdate> = BTreeMap::new();

    for tick in ticks {
        let store_key = format!("pool:{}:tick:{}", tick.pool_address.to_hex(), tick.tick_index);
        let delta = store_deltas
            .remove(&(store_key.clone(), tick.ordinal))
            .unwrap_or_else(|| {
                panic!("no tick store delta for key {store_key} at ordinal {}", tick.ordinal)
            });
        let new_value = BigInt::from_str(std::str::from_utf8(&delta.new_value).unwrap()).unwrap();
        let tx: Transaction = tick.transaction.unwrap().into();
        let key = (tx.index, tick.pool_address, tick.tick_index);
        match updates.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(TickUpdate { transaction: tx, old_value: delta.old_value, new_value });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().new_value = new_value;
            }
        }
    }

    updates
        .into_iter()
        .filter_map(
            |((_, pool_address, tick), TickUpdate { transaction, old_value, new_value })| {
                let was_absent = old_value.is_empty() ||
                    BigInt::from_str(std::str::from_utf8(&old_value).unwrap())
                        .unwrap()
                        .is_zero();
                let change = if was_absent && new_value.is_zero() {
                    return None;
                } else if was_absent {
                    ChangeType::Creation
                } else if new_value.is_zero() {
                    ChangeType::Deletion
                } else {
                    ChangeType::Update
                };
                Some((
                    transaction,
                    pool_address,
                    Attribute {
                        name: format!("ticks/{tick}/net-liquidity"),
                        value: new_value.to_signed_bytes_be(),
                        change: change.into(),
                    },
                ))
            },
        )
        .collect()
}

fn event_to_attributes_updates(event: PoolEvent) -> Vec<(Transaction, PoolAddress, Attribute)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Swap(swap) => vec![
            (
                event
                    .transaction
                    .as_ref()
                    .unwrap()
                    .into(),
                hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                Attribute {
                    name: "sqrt_price_x96".to_string(),
                    value: BigInt::from_str(&swap.sqrt_price_x96)
                        .unwrap()
                        .to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
            (
                event.transaction.unwrap().into(),
                hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                Attribute {
                    name: "tick".to_string(),
                    value: BigInt::from(swap.tick).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
        ],
        pool_event::Type::ProtocolFeeUpdated(sfp) => {
            // Mask to extract the lower 12 bits (0xFFF corresponds to 12 bits set to 1)
            let lower_12_bits = sfp.protocol_fee & 0xFFF;

            // Shift right by 12 bits and mask again to get the next 12 bits
            let upper_12_bits = (sfp.protocol_fee >> 12) & 0xFFF;

            vec![
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    Attribute {
                        name: "protocol_fees/zero2one".to_string(),
                        value: BigInt::from(lower_12_bits).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event.transaction.unwrap().into(),
                    hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    Attribute {
                        name: "protocol_fees/one2zero".to_string(),
                        value: BigInt::from(upper_12_bits).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
            ]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::StoreDelta;

    use super::*;
    use crate::pb::uniswap::v4::{TickDelta, TickDeltas, Transaction as V4Transaction};

    fn tick_delta(pool: &[u8], tick: i32, ordinal: u64, tx_index: u64) -> TickDelta {
        TickDelta {
            pool_address: pool.to_vec(),
            tick_index: tick,
            liquidity_net_delta: vec![],
            ordinal,
            transaction: Some(V4Transaction {
                hash: vec![0x11; 32],
                from: vec![0x22; 20],
                to: vec![0x33; 20],
                index: tx_index,
            }),
        }
    }

    fn store_delta(pool: &[u8], tick: i32, ordinal: u64, old: &str, new: &str) -> StoreDelta {
        StoreDelta {
            operation: 0,
            ordinal,
            key: format!("pool:{}:tick:{}", pool.to_hex(), tick),
            old_value: old.as_bytes().to_vec(),
            new_value: new.as_bytes().to_vec(),
        }
    }

    fn collect_ticks_only(
        map_deltas: TickDeltas,
        store_deltas: StoreDeltas,
    ) -> Vec<TransactionChanges> {
        collect_transaction_changes(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            map_deltas,
            store_deltas,
            Default::default(),
            Default::default(),
        )
    }

    fn attributes_by_name(changes: &[TransactionChanges]) -> HashMap<String, (Vec<u8>, i32)> {
        changes
            .iter()
            .flat_map(|tx| tx.entity_changes.iter())
            .flat_map(|ec| ec.attributes.iter())
            .map(|a| (a.name.clone(), (a.value.clone(), a.change)))
            .collect()
    }

    #[test]
    fn tick_store_deltas_join_by_key_not_position() {
        let pool = [0xaa_u8; 32];
        // One ModifyLiquidity: both ticks share ordinal 5. Store order is swapped
        // relative to map order, as the store module's unstable sort can produce.
        let map_deltas =
            TickDeltas { deltas: vec![tick_delta(&pool, 100, 5, 1), tick_delta(&pool, 200, 5, 1)] };
        let store_deltas = StoreDeltas {
            deltas: vec![
                store_delta(&pool, 200, 5, "", "75"),
                store_delta(&pool, 100, 5, "100", "150"),
            ],
        };

        let changes = collect_ticks_only(map_deltas, store_deltas);
        let attrs = attributes_by_name(&changes);

        let (value, change) = &attrs["ticks/100/net-liquidity"];
        assert_eq!(*value, BigInt::from(150).to_signed_bytes_be());
        assert_eq!(*change, i32::from(ChangeType::Update));

        let (value, change) = &attrs["ticks/200/net-liquidity"];
        assert_eq!(*value, BigInt::from(75).to_signed_bytes_be());
        assert_eq!(*change, i32::from(ChangeType::Creation));
    }

    #[test]
    fn same_tick_written_in_two_transactions() {
        let pool = [0xaa_u8; 32];
        // Two events touch the same tick in one block: creation in tx 1, then the
        // liquidity returns to zero in tx 2, which must classify as a deletion.
        let map_deltas =
            TickDeltas { deltas: vec![tick_delta(&pool, 100, 5, 1), tick_delta(&pool, 100, 9, 2)] };
        let store_deltas = StoreDeltas {
            deltas: vec![
                store_delta(&pool, 100, 5, "", "40"),
                store_delta(&pool, 100, 9, "40", "0"),
            ],
        };

        let changes = collect_ticks_only(map_deltas, store_deltas);
        let by_tx: HashMap<u64, (Vec<u8>, i32)> = changes
            .iter()
            .map(|tx_changes| {
                let attr = tx_changes.entity_changes[0].attributes[0].clone();
                (tx_changes.tx.as_ref().unwrap().index, (attr.value, attr.change))
            })
            .collect();

        assert_eq!(
            by_tx[&1],
            (BigInt::from(40).to_signed_bytes_be(), i32::from(ChangeType::Creation))
        );
        assert_eq!(
            by_tx[&2],
            (BigInt::from(0).to_signed_bytes_be(), i32::from(ChangeType::Deletion))
        );
    }

    #[rstest::rstest]
    #[case::creation_update(&["", "40", "60"], Some(ChangeType::Creation))]
    #[case::zero_update_negative(&["0", "40", "-20"], Some(ChangeType::Creation))]
    #[case::creation_delete(&["", "40", "0"], None)]
    #[case::empty_zero(&["", "0", "0"], None)]
    #[case::zero_zero(&["0", "0", "0"], None)]
    #[case::delete_recreate(&["100", "0", "60"], Some(ChangeType::Update))]
    #[case::update_delete(&["100", "60", "0"], Some(ChangeType::Deletion))]
    #[case::update_update(&["10", "40", "60"], Some(ChangeType::Update))]
    #[case::creation_delete_recreate(&["", "40", "0", "60"], Some(ChangeType::Creation))]
    #[case::creation_delete_recreate_update(&["", "40", "0", "60", "90"], Some(ChangeType::Creation))]
    #[case::delete_recreate_delete(&["100", "0", "60", "0"], Some(ChangeType::Deletion))]
    fn same_transaction_tick_changes_use_initial_and_final_state(
        #[case] values: &[&str],
        #[case] expected: Option<ChangeType>,
        #[values(false, true)] reverse_map_order: bool,
    ) {
        let pool = [0xaa; 32];
        // Also reverse map order to exercise ordinal ordering independently of the key join.
        // Inspect the final builder output, not just intermediate attributes.
        let mut map_deltas = TickDeltas {
            deltas: (0..values.len() - 1)
                .map(|i| tick_delta(&pool, -100, 5 + i as u64, 1))
                .collect(),
        };
        if reverse_map_order {
            map_deltas.deltas.reverse();
        }
        let store_deltas = StoreDeltas {
            deltas: values
                .windows(2)
                .enumerate()
                .map(|(i, pair)| store_delta(&pool, -100, 5 + i as u64, pair[0], pair[1]))
                .collect(),
        };
        let changes = collect_ticks_only(map_deltas, store_deltas);
        if let Some(expected) = expected {
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].tx.as_ref().unwrap().index, 1);
            assert_eq!(changes[0].entity_changes.len(), 1);
            assert_eq!(
                changes[0].entity_changes[0]
                    .attributes
                    .len(),
                1
            );
            let attr = &changes[0].entity_changes[0].attributes[0];
            assert_eq!(attr.name, "ticks/-100/net-liquidity");
            assert_eq!(
                attr.value,
                BigInt::from_str(values.last().unwrap())
                    .unwrap()
                    .to_signed_bytes_be()
            );
            assert_eq!(attr.change, i32::from(expected), "{values:?}");
        } else {
            assert!(changes.is_empty(), "{values:?}: {changes:?}");
        }
    }

    #[test]
    fn repeated_tick_writes_in_different_pools_and_transactions_do_not_merge() {
        let pool_a = [0xaa; 32];
        let pool_b = [0xbb; 32];
        let changes = collect_ticks_only(
            TickDeltas {
                deltas: vec![
                    tick_delta(&pool_a, -100, 12, 2),
                    tick_delta(&pool_b, -100, 9, 1),
                    tick_delta(&pool_a, -100, 7, 1),
                    tick_delta(&pool_b, -100, 6, 1),
                    tick_delta(&pool_a, -100, 5, 1),
                ],
            },
            StoreDeltas {
                deltas: vec![
                    store_delta(&pool_b, -100, 6, "100", "0"),
                    store_delta(&pool_a, -100, 5, "", "40"),
                    store_delta(&pool_a, -100, 12, "60", "0"),
                    store_delta(&pool_b, -100, 9, "0", "150"),
                    store_delta(&pool_a, -100, 7, "40", "60"),
                ],
            },
        );
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].tx.as_ref().unwrap().index, 1);
        assert_eq!(changes[1].tx.as_ref().unwrap().index, 2);
        let attrs: HashMap<_, _> = changes
            .iter()
            .flat_map(|tx| {
                tx.entity_changes.iter().map(move |ec| {
                    assert_eq!(ec.attributes.len(), 1);
                    ((tx.tx.as_ref().unwrap().index, ec.component_id.clone()), &ec.attributes[0])
                })
            })
            .collect();
        assert_eq!(attrs.len(), 3);
        for (tx, pool, value, change) in [
            (1, pool_a, 60, ChangeType::Creation),
            (1, pool_b, 150, ChangeType::Update),
            (2, pool_a, 0, ChangeType::Deletion),
        ] {
            let attr = attrs[&(tx, pool.to_hex())];
            assert_eq!(attr.value, BigInt::from(value).to_signed_bytes_be());
            assert_eq!(attr.change, i32::from(change));
        }
    }

    #[test]
    #[should_panic(expected = "no tick store delta for key")]
    fn tick_delta_without_matching_ordinal_panics() {
        let pool = [0xaa; 32];
        collect_ticks_only(
            TickDeltas { deltas: vec![tick_delta(&pool, -100, 5, 1)] },
            StoreDeltas { deltas: vec![store_delta(&pool, -100, 7, "", "40")] },
        );
    }
}
