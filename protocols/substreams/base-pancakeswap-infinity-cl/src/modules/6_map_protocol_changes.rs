use std::{collections::HashMap, str::FromStr, vec};

use itertools::Itertools;
use substreams::{
    pb::substreams::{StoreDelta, StoreDeltas},
    scalar::BigInt,
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use crate::{
    parameters::split_protocol_fee,
    pb::pancakeswap::infinity::cl::{
        events::{pool_event, PoolEvent},
        Events, LiquidityChanges, TickDeltas,
    },
};

/// Assembles `BlockChanges`: created components, entity attributes, balance changes.
///
/// Ported from `ethereum-uniswap-v4/no-hooks/src/variant_modules/2_map_protocol_changes.rs` and
/// `ethereum-uniswap-v4/shared/src/utils/protocol_changes.rs`, except that store deltas are joined
/// to map deltas by `(store key, ordinal)`, not position. `storage_changes` stays empty: the
/// simulation is native, so no VM needs the raw slots.
#[substreams::handlers::map]
#[allow(clippy::too_many_arguments)]
pub fn map_protocol_changes(
    block: eth::Block,
    created_pools: BlockEntityChanges,
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
    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

/// Store key of `store_ticks_liquidity`; writer and reader must agree.
pub fn tick_store_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("pool:{}:tick:{}", pool_address.to_hex(), tick_index)
}

/// Store key of `store_liquidity`.
pub fn liquidity_store_key(pool_address: &[u8]) -> String {
    format!("pool:{}", pool_address.to_hex())
}

/// Indexes store deltas by `(key, ordinal)` so a map delta can find its own store write. The
/// store module sorts its writes with an unstable sort on the ordinal, and both ticks of one
/// `ModifyLiquidity` share an ordinal, so pairing by position, as the v4 package does, can attach
/// one tick's value to the other tick's name.
///
/// `collect` drops a duplicate `(key, ordinal)` silently. Keys are unique because ordinals are
/// per-log.
fn index_store_deltas(deltas: StoreDeltas) -> HashMap<(String, u64), StoreDelta> {
    deltas
        .deltas
        .into_iter()
        .map(|delta| ((delta.key.clone(), delta.ordinal), delta))
        .collect()
}

/// Panics on a miss rather than skipping: a map delta with no store write means the two modules
/// disagree, and a silent skip would mis-attribute the value to another tick.
fn take_store_delta(
    indexed: &mut HashMap<(String, u64), StoreDelta>,
    key: String,
    ordinal: u64,
) -> StoreDelta {
    indexed
        .remove(&(key.clone(), ordinal))
        .unwrap_or_else(|| panic!("no store delta for key {key} at ordinal {ordinal}"))
}

/// A `StoreAddBigInt` value: the bigint as a decimal string.
fn store_bigint(value: &[u8]) -> BigInt {
    BigInt::from_str(std::str::from_utf8(value).unwrap()).unwrap()
}

/// Change type for a `ticks/{i}/net-liquidity` attribute. An empty old value is a key the store
/// never held and counts as zero. A tick back at zero is a deletion, or it stays in the snapshot
/// as an initialized tick.
fn tick_change_type(old_value: &[u8], new_value: &BigInt) -> ChangeType {
    if old_value.is_empty() || store_bigint(old_value).is_zero() {
        ChangeType::Creation
    } else if new_value.is_zero() {
        ChangeType::Deletion
    } else {
        ChangeType::Update
    }
}

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
    // Changes are merged per transaction (keyed by tx index) and sorted at the end.
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

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

    // Absolute balances come from the store deltas of this block, merged onto the per-tx builders.
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

    let mut ticks_store_deltas = index_store_deltas(ticks_store_deltas);
    ticks_map_deltas
        .deltas
        .into_iter()
        .for_each(|tick_delta| {
            let store_delta = take_store_delta(
                &mut ticks_store_deltas,
                tick_store_key(&tick_delta.pool_address, tick_delta.tick_index),
                tick_delta.ordinal,
            );
            let new_value = store_bigint(&store_delta.new_value);
            let attribute = Attribute {
                name: format!("ticks/{}/net-liquidity", tick_delta.tick_index),
                value: new_value.to_signed_bytes_be(),
                change: tick_change_type(&store_delta.old_value, &new_value).into(),
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

    // In-range liquidity. The `set_sum` store value is `<policy>:<value>`.
    let mut pool_liquidity_store_deltas = index_store_deltas(pool_liquidity_store_deltas);
    pool_liquidity_changes
        .changes
        .into_iter()
        .for_each(|change| {
            let store_delta = take_store_delta(
                &mut pool_liquidity_store_deltas,
                liquidity_store_key(&change.pool_address),
                change.ordinal,
            );
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

fn event_to_attributes_updates(event: PoolEvent) -> Vec<(Transaction, Vec<u8>, Attribute)> {
    let updates = match event.r#type.as_ref().unwrap() {
        pool_event::Type::Swap(swap) => vec![
            ("sqrt_price_x96", BigInt::from_str(&swap.sqrt_price_x96).unwrap()),
            ("tick", BigInt::from(swap.tick)),
        ],
        pool_event::Type::ProtocolFeeUpdated(updated) => {
            let (zero2one, one2zero) = split_protocol_fee(updated.protocol_fee);
            vec![
                ("protocol_fees/zero2one", BigInt::from(zero2one)),
                ("protocol_fees/one2zero", BigInt::from(one2zero)),
            ]
        }
        pool_event::Type::Initialize(_) |
        pool_event::Type::ModifyLiquidity(_) |
        pool_event::Type::Donate(_) => return vec![],
    };

    let tx: Transaction = event
        .transaction
        .as_ref()
        .unwrap()
        .into();
    let pool_address = hex::decode(event.pool_id.trim_start_matches("0x")).unwrap();
    updates
        .into_iter()
        .map(|(name, value)| {
            let attribute = Attribute {
                name: name.to_string(),
                value: value.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            };
            (tx.clone(), pool_address.clone(), attribute)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::pancakeswap::infinity::cl::{
        LiquidityChange, LiquidityChangeType, TickDelta, Transaction as ClTransaction,
    };
    use rstest::rstest;

    fn tx(index: u64) -> ClTransaction {
        ClTransaction { hash: vec![0x11; 32], from: vec![0x22; 20], to: vec![0x33; 20], index }
    }

    fn tick_delta(pool: &[u8], tick: i32, ordinal: u64) -> TickDelta {
        TickDelta {
            pool_address: pool.to_vec(),
            tick_index: tick,
            liquidity_net_delta: vec![],
            ordinal,
            transaction: Some(tx(1)),
        }
    }

    fn store_delta(key: String, ordinal: u64, old: &str, new: &str) -> StoreDelta {
        StoreDelta {
            operation: 0,
            ordinal,
            key,
            old_value: old.as_bytes().to_vec(),
            new_value: new.as_bytes().to_vec(),
        }
    }

    /// Runs the module on tick and liquidity inputs alone.
    fn collect(
        ticks_map_deltas: TickDeltas,
        ticks_store_deltas: StoreDeltas,
        pool_liquidity_changes: LiquidityChanges,
        pool_liquidity_store_deltas: StoreDeltas,
    ) -> Vec<TransactionChanges> {
        collect_transaction_changes(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            ticks_map_deltas,
            ticks_store_deltas,
            pool_liquidity_changes,
            pool_liquidity_store_deltas,
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

    fn pool_event(r#type: pool_event::Type) -> PoolEvent {
        PoolEvent {
            log_ordinal: 7,
            pool_id: format!("0x{}", "ab".repeat(32)),
            currency0: String::new(),
            currency1: String::new(),
            transaction: Some(tx(1)),
            r#type: Some(r#type),
        }
    }

    #[test]
    fn tick_store_deltas_join_by_key_not_position() {
        let pool = [0xaa_u8; 32];
        // One ModifyLiquidity: both ticks share ordinal 5. Store order is swapped relative to map
        // order, as the store module's unstable sort can produce.
        let map_deltas =
            TickDeltas { deltas: vec![tick_delta(&pool, 100, 5), tick_delta(&pool, 200, 5)] };
        let store_deltas = StoreDeltas {
            deltas: vec![
                store_delta(tick_store_key(&pool, 200), 5, "", "75"),
                store_delta(tick_store_key(&pool, 100), 5, "100", "150"),
            ],
        };

        let changes = collect(map_deltas, store_deltas, Default::default(), Default::default());
        let attrs = attributes_by_name(&changes);

        let (value, change) = &attrs["ticks/100/net-liquidity"];
        assert_eq!(*value, BigInt::from(150).to_signed_bytes_be());
        assert_eq!(*change, i32::from(ChangeType::Update));

        let (value, change) = &attrs["ticks/200/net-liquidity"];
        assert_eq!(*value, BigInt::from(75).to_signed_bytes_be());
        assert_eq!(*change, i32::from(ChangeType::Creation));
    }

    #[test]
    fn liquidity_store_deltas_join_by_key_not_position() {
        let pool_a = [0xaa_u8; 32];
        let pool_b = [0xbb_u8; 32];
        let change = |pool: &[u8], ordinal: u64| LiquidityChange {
            pool_address: pool.to_vec(),
            value: vec![],
            change_type: LiquidityChangeType::Absolute.into(),
            ordinal,
            transaction: Some(tx(1)),
        };
        let map_deltas = LiquidityChanges { changes: vec![change(&pool_a, 3), change(&pool_b, 4)] };
        // Swapped order; values are `set_sum` encoded as `<policy>:<value>`.
        let store_deltas = StoreDeltas {
            deltas: vec![
                store_delta(liquidity_store_key(&pool_b), 4, "", "set:20"),
                store_delta(liquidity_store_key(&pool_a), 3, "", "set:10"),
            ],
        };

        let changes = collect(Default::default(), Default::default(), map_deltas, store_deltas);
        let liquidity: HashMap<String, Vec<u8>> = changes
            .iter()
            .flat_map(|tx| tx.entity_changes.iter())
            .map(|ec| (ec.component_id.clone(), ec.attributes[0].value.clone()))
            .collect();

        assert_eq!(liquidity[&pool_a.to_hex()], BigInt::from(10).to_signed_bytes_be());
        assert_eq!(liquidity[&pool_b.to_hex()], BigInt::from(20).to_signed_bytes_be());
    }

    #[test]
    #[should_panic(expected = "no store delta for key")]
    fn missing_store_delta_panics() {
        let pool = [0xaa_u8; 32];
        collect(
            TickDeltas { deltas: vec![tick_delta(&pool, 100, 5)] },
            StoreDeltas { deltas: vec![] },
            Default::default(),
            Default::default(),
        );
    }

    #[rstest]
    #[case::first_write("", "150", ChangeType::Creation)]
    #[case::refilled_from_zero("0", "-150", ChangeType::Creation)]
    #[case::emptied("150", "0", ChangeType::Deletion)]
    #[case::moved("100", "150", ChangeType::Update)]
    fn test_tick_change_type(#[case] old: &str, #[case] new: &str, #[case] expected: ChangeType) {
        let new = BigInt::from_str(new).unwrap();
        assert_eq!(tick_change_type(old.as_bytes(), &new), expected);
    }

    /// Values are minimal signed big-endian bytes: 2^96 is a one followed by 12 zero bytes, and
    /// a negative tick is its two's complement.
    #[test]
    fn swap_updates_price_and_tick() {
        let updates =
            event_to_attributes_updates(pool_event(pool_event::Type::Swap(pool_event::Swap {
                sqrt_price_x96: "79228162514264337593543950336".to_string(),
                tick: -2,
                ..Default::default()
            })));

        let mut sqrt_price = vec![0u8; 13];
        sqrt_price[0] = 1;
        let update = ChangeType::Update.into();
        let attributes: Vec<_> = updates
            .iter()
            .map(|(_, _, attribute)| attribute.clone())
            .collect();
        assert_eq!(
            attributes,
            vec![
                Attribute { name: "sqrt_price_x96".into(), value: sqrt_price, change: update },
                Attribute { name: "tick".into(), value: vec![0xfe], change: update },
            ]
        );
        assert!(
            updates
                .iter()
                .all(|(tx, pool, _)| tx.index == 1 && *pool == vec![0xab; 32]),
            "updates carry the event's transaction and the raw pool id"
        );
    }

    /// Zero-for-one is the low 12 bits of the packed fee, one-for-zero the high 12.
    #[test]
    fn protocol_fee_update_is_split_per_direction() {
        let updates = event_to_attributes_updates(pool_event(
            pool_event::Type::ProtocolFeeUpdated(pool_event::ProtocolFeeUpdated {
                pool_id: String::new(),
                protocol_fee: (300 << 12) | 200,
            }),
        ));

        let values: Vec<_> = updates
            .iter()
            .map(|(_, _, attribute)| (attribute.name.as_str(), attribute.value.clone()))
            .collect();
        assert_eq!(
            values,
            vec![
                ("protocol_fees/zero2one", vec![0x00, 0xc8]),
                ("protocol_fees/one2zero", vec![0x01, 0x2c]),
            ]
        );
    }

    #[rstest]
    #[case::initialize(pool_event::Type::Initialize(Default::default()))]
    #[case::modify_liquidity(pool_event::Type::ModifyLiquidity(Default::default()))]
    #[case::donate(pool_event::Type::Donate(Default::default()))]
    fn events_handled_by_other_modules_update_no_attribute(#[case] r#type: pool_event::Type) {
        assert!(
            event_to_attributes_updates(pool_event(r#type)).is_empty(),
            "only Swap and ProtocolFeeUpdated map straight to attributes"
        );
    }
}
