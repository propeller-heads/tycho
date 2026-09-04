use std::{collections::HashMap, str::FromStr};

use itertools::Itertools;
use substreams::{key, pb::substreams::StoreDeltas, scalar::BigInt};
use substreams_ethereum::pb::eth;
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    models::{
        Attribute, BlockBalanceDeltas, BlockChanges, ChangeType, EntityChanges,
        TransactionChangesBuilder,
    },
};

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::pool_log::Event, ActiveLiquidityChanges,
    ActiveRateChanges, BlockTransactionEvents, RateDeltas, TickDeltas,
};

/// Aggregates protocol components and balance changes by transaction.
///
/// This is the main method that will aggregate all changes as well as extract all
/// relevant contract storage deltas.
#[substreams::handlers::map]
fn map_protocol_changes(
    block: eth::v2::Block,
    new_components: BlockChanges,
    block_tx_events: BlockTransactionEvents,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
    rate_map_deltas: RateDeltas,
    rate_delta_store_deltas: StoreDeltas,
    active_liquidity_changes: ActiveLiquidityChanges,
    liquidity_store_deltas: StoreDeltas,
    active_rate_changes: ActiveRateChanges,
    active_rate_store_deltas: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // New components
    new_components
        .changes
        .iter()
        .for_each(|tx_changes| {
            let tx = tx_changes.tx.as_ref().unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            tx_changes
                .component_changes
                .iter()
                .for_each(|component| {
                    builder.add_protocol_component(component);
                });

            tx_changes
                .entity_changes
                .iter()
                .for_each(|entity_change| {
                    builder.add_entity_change(entity_change);
                });

            tx_changes
                .balance_changes
                .iter()
                .for_each(|balance_change| {
                    builder.add_balance_change(balance_change);
                });
        });

    // Component balances
    aggregate_balances_changes(balances_store_deltas, balances_map_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));

            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map.values().for_each(|bc| {
                        builder.add_balance_change(bc);
                    })
                });
        });

    // Tick liquidities
    ticks_store_deltas
        .deltas
        .into_iter()
        .zip(ticks_map_deltas.deltas)
        .for_each(|(store_delta, tick_delta)| {
            let (old_value, new_value) = (
                BigInt::from_store_bytes(&store_delta.old_value),
                BigInt::from_store_bytes(&store_delta.new_value),
            );

            let tx = tick_delta.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            builder.add_entity_change(&EntityChanges {
                component_id: tick_delta.pool_id.to_hex(),
                attributes: vec![Attribute {
                    name: format!("tick/{}", tick_delta.tick_index),
                    value: new_value.to_signed_bytes_be(),
                    change: change_type_from_delta(&old_value, &new_value).into(),
                }],
            });
        });

    // Rate deltas for TWAMM and BoostedFees
    rate_delta_store_deltas
        .deltas
        .into_iter()
        .zip(rate_map_deltas.deltas)
        .for_each(|(store_delta, rate_delta)| {
            let tx = rate_delta.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            let (old_value, new_value) = (
                BigInt::from_store_bytes(&store_delta.old_value),
                BigInt::from_store_bytes(&store_delta.new_value),
            );

            let token = if rate_delta.is_token1 { "token1" } else { "token0" };

            builder.add_entity_change(&EntityChanges {
                component_id: rate_delta.pool_id.to_hex(),
                attributes: vec![Attribute {
                    name: format!("rate_delta/{}/{}", token, rate_delta.time),
                    value: new_value.to_signed_bytes_be(),
                    change: change_type_from_delta(&old_value, &new_value).into(),
                }],
            });
        });

    // Pool liquidities
    liquidity_store_deltas
        .deltas
        .into_iter()
        .zip(active_liquidity_changes.changes)
        .for_each(|(store_delta, change)| {
            let tx = change.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            builder.add_entity_change(&EntityChanges {
                component_id: change.pool_id.to_hex(),
                attributes: vec![Attribute {
                    name: "liquidity".to_string(),
                    value: bigint_from_set_sum_store_delta_value(store_delta.new_value)
                        .to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                }],
            });
        });

    // Active rates for TWAMM and BoostedFees
    active_rate_store_deltas
        .deltas
        .chunks(2)
        .zip(active_rate_changes.changes)
        .for_each(|(store_deltas, change)| {
            let tx = change.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            let (rate_token0, rate_token1) = (
                bigint_from_set_sum_store_delta_value(store_deltas[0].new_value.clone()),
                bigint_from_set_sum_store_delta_value(store_deltas[1].new_value.clone()),
            );

            builder.add_entity_change(&EntityChanges {
                component_id: change.pool_id.to_hex(),
                attributes: vec![
                    Attribute {
                        name: "rate_token0".to_string(),
                        value: rate_token0.to_bytes_be().1,
                        change: ChangeType::Update.into(),
                    },
                    Attribute {
                        name: "rate_token1".to_string(),
                        value: rate_token1.to_bytes_be().1,
                        change: ChangeType::Update.into(),
                    },
                ],
            });
        });

    // Remaining event changes not subject to special treatment
    block_tx_events
        .block_transaction_events
        .into_iter()
        .flat_map(|tx_events| {
            let tx = tx_events.transaction.unwrap();

            tx_events
                .pool_logs
                .into_iter()
                .flat_map(move |log| {
                    let tx = tx.clone();

                    maybe_attribute_updates(log.event.unwrap(), block_tx_events.timestamp).map(
                        |attrs| {
                            (
                                tx,
                                EntityChanges {
                                    component_id: log.pool_id.to_hex(),
                                    attributes: attrs,
                                },
                            )
                        },
                    )
                })
        })
        .for_each(|(tx, entity_changes)| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));
            builder.add_entity_change(&entity_changes);
        });

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        ..Default::default()
    })
}

fn maybe_attribute_updates(ev: Event, timestamp: u64) -> Option<Vec<Attribute>> {
    match ev {
        Event::Swapped(ev) => Some(vec![
            Attribute {
                name: "tick".into(),
                value: ev.tick_after.to_be_bytes().to_vec(),
                change: ChangeType::Update.into(),
            },
            Attribute {
                name: "sqrt_ratio".into(),
                value: ev.sqrt_ratio_after,
                change: ChangeType::Update.into(),
            },
        ]),
        Event::VirtualExecution(_) => Some(vec![Attribute {
            name: "last_time".to_string(),
            value: timestamp.to_be_bytes().to_vec(),
            change: ChangeType::Update.into(),
        }]),
        // The component creation already carries the snapshot's tick, sqrt_ratio and last_time.
        Event::PoolSnapshot(_) => None,
        _ => None,
    }
}

fn change_type_from_delta(old_value: &BigInt, new_value: &BigInt) -> ChangeType {
    if old_value.is_zero() {
        ChangeType::Creation
    } else if new_value.is_zero() {
        ChangeType::Deletion
    } else {
        ChangeType::Update
    }
}

fn bigint_from_set_sum_store_delta_value(value: Vec<u8>) -> BigInt {
    BigInt::from_str(key::segment_at(&String::from_utf8(value).unwrap(), 1)).unwrap()
}
