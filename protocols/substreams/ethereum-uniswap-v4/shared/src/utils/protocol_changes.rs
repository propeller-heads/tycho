use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChanges, TickDeltas,
};
use itertools::Itertools;
use std::{collections::HashMap, str::FromStr, vec};
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

    // Insert ticks net-liquidity changes. Both ticks of a ModifyLiquidity event share one log
    // ordinal and the store module's unstable sort may reorder them, so positional zip pairing
    // is unsound; join store deltas to tick deltas by (store key, ordinal) instead.
    let mut ticks_store_deltas_by_key: HashMap<(String, u64), StoreDelta> = ticks_store_deltas
        .deltas
        .into_iter()
        .map(|delta| ((delta.key.clone(), delta.ordinal), delta))
        .collect();

    ticks_map_deltas
        .deltas
        .into_iter()
        .for_each(|tick_delta| {
            let store_key = format!(
                "pool:{0}:tick:{1}",
                tick_delta.pool_address.to_hex(),
                tick_delta.tick_index
            );
            let store_delta = ticks_store_deltas_by_key
                .remove(&(store_key.clone(), tick_delta.ordinal))
                .unwrap_or_else(|| {
                    panic!(
                        "no tick store delta for key {store_key} at ordinal {}",
                        tick_delta.ordinal
                    )
                });

            let new_value_bigint =
                BigInt::from_str(&String::from_utf8(store_delta.new_value).unwrap()).unwrap();

            // If old value is empty or the int value is 0, it's considered as a creation.
            let is_creation = store_delta.old_value.is_empty() ||
                BigInt::from_str(&String::from_utf8(store_delta.old_value).unwrap())
                    .unwrap()
                    .is_zero();
            let attribute_name = format!("ticks/{}/net-liquidity", tick_delta.tick_index);
            let attribute = Attribute {
                name: attribute_name,
                value: new_value_bigint.to_signed_bytes_be(),
                change: if is_creation {
                    ChangeType::Creation.into()
                } else if new_value_bigint.is_zero() {
                    ChangeType::Deletion.into()
                } else {
                    ChangeType::Update.into()
                },
            };
            let tx = tick_delta.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            builder.add_entity_change(&EntityChanges {
                component_id: tick_delta.pool_address.to_hex(),
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

    transaction_changes
        .drain()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
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
}
