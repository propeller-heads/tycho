use crate::pb::pancakeswap::v3::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChanges, TickDeltas,
};
use itertools::Itertools;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    vec,
};
use substreams::{pb::substreams::StoreDeltas, scalar::BigInt};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use super::store_deltas::{
    index_store_deltas, liquidity_store_key, store_bigint, take_store_delta, tick_change_type,
    tick_store_key,
};

type PoolAddress = Vec<u8>;

#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::Block,
    created_pools: BlockChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
    pool_liquidity_changes: LiquidityChanges,
    pool_liquidity_store_deltas: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let changes = collect_transaction_changes(
        created_pools,
        events,
        balances_map_deltas,
        balances_store_deltas,
        ticks_map_deltas,
        ticks_store_deltas,
        pool_liquidity_changes,
        pool_liquidity_store_deltas,
    );
    Ok(BlockChanges { block: Some((&block).into()), changes })
}

#[allow(clippy::too_many_arguments)]
fn collect_transaction_changes(
    created_pools: BlockChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
    pool_liquidity_changes: LiquidityChanges,
    pool_liquidity_store_deltas: StoreDeltas,
) -> Vec<TransactionChanges> {
    // We merge contract changes by transaction (identified by transaction index) making it easy to
    //  sort them at the very end.
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

    // Insert ticks net-liquidity changes
    ticks_to_attribute_updates(ticks_map_deltas, ticks_store_deltas)
        .into_iter()
        .for_each(|(tx, pool_address, attribute)| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));

            builder.add_entity_change(&EntityChanges {
                component_id: pool_address.to_hex(),
                attributes: vec![attribute],
            });
        });

    // Insert liquidity changes
    liquidity_to_attribute_updates(pool_liquidity_changes, pool_liquidity_store_deltas)
        .into_iter()
        .for_each(|(tx, pool_address, attribute)| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.add_entity_change(&EntityChanges {
                component_id: pool_address.to_hex(),
                attributes: vec![attribute],
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
        .into_iter()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
        .collect()
}

fn event_to_attributes_updates(event: PoolEvent) -> Vec<(Transaction, PoolAddress, Attribute)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initalize) => {
            let (zero_to_one, one_to_zero) = fee_to_default_protocol_fees(event.fee);
            vec![
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(&event.pool_address).unwrap(),
                    Attribute {
                        name: "sqrt_price_x96".to_string(),
                        value: BigInt::from_str(&initalize.sqrt_price)
                            .unwrap()
                            .to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(&event.pool_address).unwrap(),
                    Attribute {
                        name: "tick".to_string(),
                        value: BigInt::from(initalize.tick).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(&event.pool_address).unwrap(),
                    Attribute {
                        name: "protocol_fees/zero2one".to_string(),
                        value: BigInt::from(zero_to_one).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event.transaction.unwrap().into(),
                    hex::decode(event.pool_address).unwrap(),
                    Attribute {
                        name: "protocol_fees/one2zero".to_string(),
                        value: BigInt::from(one_to_zero).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
            ]
        }
        pool_event::Type::Swap(swap) => vec![
            (
                event
                    .transaction
                    .as_ref()
                    .unwrap()
                    .into(),
                hex::decode(&event.pool_address).unwrap(),
                Attribute {
                    name: "sqrt_price_x96".to_string(),
                    value: BigInt::from_str(&swap.sqrt_price)
                        .unwrap()
                        .to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
            (
                event.transaction.unwrap().into(),
                hex::decode(event.pool_address).unwrap(),
                Attribute {
                    name: "tick".to_string(),
                    value: BigInt::from(swap.tick).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
        ],
        pool_event::Type::SetFeeProtocol(sfp) => vec![
            (
                event
                    .transaction
                    .as_ref()
                    .unwrap()
                    .into(),
                hex::decode(&event.pool_address).unwrap(),
                Attribute {
                    name: "protocol_fees/zero2one".to_string(),
                    value: BigInt::from(sfp.fee_protocol_0_new).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
            (
                event.transaction.unwrap().into(),
                hex::decode(event.pool_address).unwrap(),
                Attribute {
                    name: "protocol_fees/one2zero".to_string(),
                    value: BigInt::from(sfp.fee_protocol_1_new).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
        ],
        _ => vec![],
    }
}

// Map the pool fee to the default protocol fees.
// For the reference implementation see https://github.com/pancakeswap/pancake-v3-contracts/blob/5cc479f0c5a98966c74d94700057b8c3ca629afd/projects/v3-core/contracts/PancakeV3Pool.sol#L298-L306
fn fee_to_default_protocol_fees(fee: u64) -> (u64, u64) {
    match fee {
        100 => (3300, 3300),
        500 => (3400, 3400),
        2500 => (3200, 3200),
        10000 => (3200, 3200),
        _ => panic!("Unexpected fee value"),
    }
}

struct TickUpdate {
    transaction: Transaction,
    old_value: Vec<u8>,
    new_value: BigInt,
}

/// Joins tick writes by `(store key, ordinal)` and emits one net change per transaction and tick.
/// The first old value determines existence before the transaction; the last new value determines
/// its final state. This preserves Creation across later updates and cancels creation then
/// deletion.
fn ticks_to_attribute_updates(
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
) -> Vec<(Transaction, PoolAddress, Attribute)> {
    let mut store_deltas = index_store_deltas(ticks_store_deltas);
    let mut ticks = ticks_map_deltas.deltas;
    ticks.sort_unstable_by_key(|delta| delta.ordinal);
    let mut updates: BTreeMap<(u64, PoolAddress, i32), TickUpdate> = BTreeMap::new();

    for tick in ticks {
        let delta = take_store_delta(
            &mut store_deltas,
            tick_store_key(&tick.pool_address, tick.tick_index),
            tick.ordinal,
        );
        let new_value = store_bigint(&delta.new_value);
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
            |((_, pool_address, tick), TickUpdate { transaction: tx, old_value, new_value })| {
                let change = tick_change_type(&old_value, &new_value)?;
                Some((
                    tx,
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

fn liquidity_to_attribute_updates(
    changes: LiquidityChanges,
    store_deltas: StoreDeltas,
) -> Vec<(Transaction, PoolAddress, Attribute)> {
    let mut indexed = index_store_deltas(store_deltas);
    let mut changes = changes.changes;
    changes.sort_unstable_by_key(|change| change.ordinal);
    changes
        .into_iter()
        .map(|change| {
            let delta = take_store_delta(
                &mut indexed,
                liquidity_store_key(&change.pool_address),
                change.ordinal,
            );
            // The set_sum store prefixes each decimal bigint with its update policy.
            let value = std::str::from_utf8(&delta.new_value)
                .unwrap()
                .split_once(':')
                .unwrap()
                .1;
            (
                change.transaction.unwrap().into(),
                change.pool_address,
                Attribute {
                    name: "liquidity".to_string(),
                    value: BigInt::from_str(value)
                        .unwrap()
                        .to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::StoreDelta;

    use super::*;
    use crate::pb::pancakeswap::v3::{
        LiquidityChange, TickDelta, TickDeltas, Transaction as PbTransaction,
    };

    const POOL: [u8; 20] = [0xaa; 20];

    fn tick_delta(tick: i32, ordinal: u64, tx_index: u64) -> TickDelta {
        TickDelta {
            pool_address: POOL.to_vec(),
            tick_index: tick,
            liquidity_net_delta: vec![],
            ordinal,
            transaction: Some(PbTransaction {
                hash: vec![0x11; 32],
                from: vec![0x22; 20],
                to: vec![0x33; 20],
                index: tx_index,
            }),
        }
    }

    fn store_delta(tick: i32, ordinal: u64, old: &str, new: &str) -> StoreDelta {
        StoreDelta {
            operation: 0,
            ordinal,
            key: tick_store_key(&POOL, tick),
            old_value: old.as_bytes().to_vec(),
            new_value: new.as_bytes().to_vec(),
        }
    }

    #[test]
    fn store_deltas_join_by_key_not_position() {
        // One Mint, so both ticks carry ordinal 5. The store wrote the upper tick first, which
        // the unstable sort in store_ticks_liquidity can produce for a tied ordinal.
        let map_deltas = TickDeltas { deltas: vec![tick_delta(-100, 5, 1), tick_delta(100, 5, 1)] };
        let store_deltas = StoreDeltas {
            deltas: vec![store_delta(100, 5, "", "75"), store_delta(-100, 5, "100", "150")],
        };

        let updates = ticks_to_attribute_updates(map_deltas, store_deltas);

        let lower = &updates[0].2;
        assert_eq!(lower.name, "ticks/-100/net-liquidity");
        assert_eq!(lower.value, BigInt::from(150).to_signed_bytes_be());
        assert_eq!(lower.change, i32::from(ChangeType::Update));

        let upper = &updates[1].2;
        assert_eq!(upper.name, "ticks/100/net-liquidity");
        assert_eq!(upper.value, BigInt::from(75).to_signed_bytes_be());
        assert_eq!(upper.change, i32::from(ChangeType::Creation));
    }

    #[test]
    fn same_tick_written_in_two_transactions() {
        // One tick touched twice in a block: created in tx 1, then back to zero in tx 2, which
        // must classify as a deletion. Dropping the ordinal from the key would collapse the two
        // store deltas into one.
        let map_deltas =
            TickDeltas { deltas: vec![tick_delta(-100, 5, 1), tick_delta(-100, 9, 2)] };
        let store_deltas = StoreDeltas {
            deltas: vec![store_delta(-100, 5, "", "40"), store_delta(-100, 9, "40", "0")],
        };

        let updates = ticks_to_attribute_updates(map_deltas, store_deltas);

        assert_eq!(updates[0].0.index, 1);
        assert_eq!(updates[0].2.value, BigInt::from(40).to_signed_bytes_be());
        assert_eq!(updates[0].2.change, i32::from(ChangeType::Creation));

        assert_eq!(updates[1].0.index, 2);
        assert_eq!(updates[1].2.value, BigInt::from(0).to_signed_bytes_be());
        assert_eq!(updates[1].2.change, i32::from(ChangeType::Deletion));
    }

    #[test]
    #[should_panic(expected = "no store delta for key")]
    fn tick_delta_without_store_delta_panics() {
        let map_deltas = TickDeltas { deltas: vec![tick_delta(-100, 5, 1)] };
        let store_deltas = StoreDeltas { deltas: vec![store_delta(-100, 7, "", "40")] };

        ticks_to_attribute_updates(map_deltas, store_deltas);
    }
    fn collect_ticks(ticks: TickDeltas, deltas: StoreDeltas) -> Vec<TransactionChanges> {
        collect_transaction_changes(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            ticks,
            deltas,
            Default::default(),
            Default::default(),
        )
    }

    #[test]
    fn same_transaction_tick_changes_use_initial_and_final_state() {
        let cases = [
            ("", "40", "60", Some(ChangeType::Creation)),
            ("0", "40", "-20", Some(ChangeType::Creation)),
            ("", "40", "0", None),
            ("", "0", "0", None),
            ("100", "0", "60", Some(ChangeType::Update)),
            ("100", "60", "0", Some(ChangeType::Deletion)),
        ];
        for (old, middle, final_value, expected) in cases {
            // Both input lists are reversed: the first old and last new values are temporal.
            let changes = collect_ticks(
                TickDeltas { deltas: vec![tick_delta(-100, 9, 1), tick_delta(-100, 5, 1)] },
                StoreDeltas {
                    deltas: vec![
                        store_delta(-100, 9, middle, final_value),
                        store_delta(-100, 5, old, middle),
                    ],
                },
            );
            if let Some(expected) = expected {
                assert_eq!(changes.len(), 1);
                let attr = &changes[0].entity_changes[0].attributes[0];
                assert_eq!(attr.name, "ticks/-100/net-liquidity");
                assert_eq!(
                    attr.value,
                    BigInt::from_str(final_value)
                        .unwrap()
                        .to_signed_bytes_be()
                );
                assert_eq!(attr.change, i32::from(expected), "{old} -> {middle} -> {final_value}");
            } else {
                assert!(changes.is_empty(), "{old} -> {middle} -> {final_value}");
            }
        }
    }

    #[test]
    fn creation_deletion_recreation_in_one_transaction_remains_creation() {
        let changes = collect_ticks(
            TickDeltas {
                deltas: vec![
                    tick_delta(-100, 5, 1),
                    tick_delta(-100, 9, 1),
                    tick_delta(-100, 12, 1),
                ],
            },
            StoreDeltas {
                deltas: vec![
                    store_delta(-100, 5, "", "40"),
                    store_delta(-100, 9, "40", "0"),
                    store_delta(-100, 12, "0", "60"),
                ],
            },
        );
        let attr = &changes[0].entity_changes[0].attributes[0];
        assert_eq!(attr.value, BigInt::from(60).to_signed_bytes_be());
        assert_eq!(attr.change, i32::from(ChangeType::Creation));
    }

    #[test]
    fn identical_tick_indices_in_different_pools_do_not_merge() {
        let pool_b = vec![0xbb; 20];
        let mut tick_b = tick_delta(-100, 9, 1);
        tick_b.pool_address = pool_b.clone();
        let mut store_b = store_delta(-100, 9, "100", "150");
        store_b.key = tick_store_key(&pool_b, -100);
        let changes = collect_ticks(
            TickDeltas { deltas: vec![tick_delta(-100, 5, 1), tick_b] },
            StoreDeltas { deltas: vec![store_b, store_delta(-100, 5, "", "40")] },
        );
        let attrs: HashMap<_, _> = changes[0]
            .entity_changes
            .iter()
            .map(|ec| (ec.component_id.clone(), &ec.attributes[0]))
            .collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[&POOL.to_vec().to_hex()].change, i32::from(ChangeType::Creation));
        assert_eq!(attrs[&pool_b.to_hex()].change, i32::from(ChangeType::Update));
    }

    fn liquidity_change(pool: &[u8], ordinal: u64, tx_index: u64) -> LiquidityChange {
        LiquidityChange {
            pool_address: pool.to_vec(),
            ordinal,
            transaction: tick_delta(0, ordinal, tx_index).transaction,
            ..Default::default()
        }
    }

    fn liquidity_store_delta(pool: &[u8], ordinal: u64, new: &str) -> StoreDelta {
        StoreDelta {
            key: liquidity_store_key(pool),
            ordinal,
            new_value: new.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn liquidity_store_deltas_join_by_key_and_ordinal() {
        let pool_b = [0xbb; 20];
        let updates = liquidity_to_attribute_updates(
            LiquidityChanges {
                changes: vec![
                    liquidity_change(&POOL, 3, 1),
                    liquidity_change(&pool_b, 4, 1),
                    liquidity_change(&POOL, 9, 2),
                ],
            },
            StoreDeltas {
                deltas: vec![
                    liquidity_store_delta(&POOL, 9, "sum:30"),
                    liquidity_store_delta(&pool_b, 4, "set:20"),
                    liquidity_store_delta(&POOL, 3, "set:10"),
                ],
            },
        );
        assert_eq!(updates.len(), 3);
        for (update, (pool, tx, value)) in updates.iter().zip([
            (POOL.as_slice(), 1, 10),
            (pool_b.as_slice(), 1, 20),
            (POOL.as_slice(), 2, 30),
        ]) {
            assert_eq!(update.0.index, tx);
            assert_eq!(update.1, pool);
            assert_eq!(update.2.name, "liquidity");
            assert_eq!(update.2.value, BigInt::from(value).to_signed_bytes_be());
        }
    }

    #[test]
    #[should_panic(expected = "no store delta for key")]
    fn liquidity_change_without_store_delta_panics() {
        liquidity_to_attribute_updates(
            LiquidityChanges { changes: vec![liquidity_change(&POOL, 3, 1)] },
            StoreDeltas::default(),
        );
    }
}
