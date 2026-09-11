use crate::pb::pancakeswap::v3::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChanges, TickDeltas,
};
use itertools::Itertools;
use std::{collections::HashMap, str::FromStr, vec};
use substreams::{
    pb::substreams::{StoreDelta, StoreDeltas},
    scalar::BigInt,
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use super::map_store_ticks::tick_store_key;

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

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
    })
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

/// Turns every tick delta into its `ticks/<index>/net-liquidity` attribute update, carrying the
/// value and `ChangeType` from the store delta that the same tick write produced.
///
/// Store deltas are joined by `(store key, ordinal)` rather than by position. Both ticks of one
/// Mint or Burn carry that event's log ordinal, and `store_ticks_liquidity` orders its writes
/// with an unstable sort on the ordinal, so the two sequences can disagree on the order of a
/// tied pair. Pairing by position then labels one tick's attribute with the other tick's value
/// and `ChangeType`.
///
/// Panics if a tick delta has no matching store delta, or if any store delta is left unmatched.
/// Every `store.add` emits exactly one delta under the same key and ordinal, so either case means
/// the two sides no longer derive keys the same way, which corrupts every block that touches
/// liquidity.
fn ticks_to_attribute_updates(
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
) -> Vec<(Transaction, PoolAddress, Attribute)> {
    let mut store_deltas_by_key: HashMap<(String, u64), StoreDelta> = ticks_store_deltas
        .deltas
        .into_iter()
        .map(|delta| ((delta.key.clone(), delta.ordinal), delta))
        .collect();

    let updates = ticks_map_deltas
        .deltas
        .into_iter()
        .map(|tick_delta| {
            let key = (
                tick_store_key(&tick_delta.pool_address, tick_delta.tick_index),
                tick_delta.ordinal,
            );
            let store_delta = store_deltas_by_key
                .remove(&key)
                .unwrap_or_else(|| {
                    let (store_key, ordinal) = &key;
                    panic!("no net-liquidity store delta for {store_key} at ordinal {ordinal}")
                });

            let new_value =
                BigInt::from_str(&String::from_utf8(store_delta.new_value).unwrap()).unwrap();

            // An empty or zero old value means the tick did not hold liquidity before this write.
            let is_creation = store_delta.old_value.is_empty() ||
                BigInt::from_str(&String::from_utf8(store_delta.old_value).unwrap())
                    .unwrap()
                    .is_zero();

            let attribute = Attribute {
                name: format!("ticks/{}/net-liquidity", tick_delta.tick_index),
                value: new_value.to_signed_bytes_be(),
                change: if is_creation {
                    ChangeType::Creation.into()
                } else if new_value.is_zero() {
                    ChangeType::Deletion.into()
                } else {
                    ChangeType::Update.into()
                },
            };

            (tick_delta.transaction.unwrap().into(), tick_delta.pool_address, attribute)
        })
        .collect();

    assert!(
        store_deltas_by_key.is_empty(),
        "net-liquidity store deltas without a matching tick delta: {:?}",
        store_deltas_by_key
            .keys()
            .collect::<Vec<_>>()
    );

    updates
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::StoreDelta;

    use super::*;
    use crate::pb::pancakeswap::v3::{TickDelta, TickDeltas, Transaction as PbTransaction};

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
    #[should_panic(expected = "no net-liquidity store delta for")]
    fn tick_delta_without_store_delta_panics() {
        let map_deltas = TickDeltas { deltas: vec![tick_delta(-100, 5, 1)] };
        let store_deltas = StoreDeltas { deltas: vec![store_delta(-100, 7, "", "40")] };

        ticks_to_attribute_updates(map_deltas, store_deltas);
    }

    #[test]
    #[should_panic(expected = "without a matching tick delta")]
    fn store_delta_without_tick_delta_panics() {
        let map_deltas = TickDeltas { deltas: vec![tick_delta(-100, 5, 1)] };
        let store_deltas = StoreDeltas {
            deltas: vec![store_delta(-100, 5, "", "40"), store_delta(200, 5, "", "40")],
        };

        ticks_to_attribute_updates(map_deltas, store_deltas);
    }
}
