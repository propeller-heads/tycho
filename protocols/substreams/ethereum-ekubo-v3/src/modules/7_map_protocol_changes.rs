use std::{collections::HashMap, str::FromStr};

use itertools::Itertools;
use substreams::{
    key,
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetProto},
};
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
    ActiveRateChanges, BlockTransactionEvents, PoolDetails, RateDeltas, TickDeltas,
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
    pool_details_store: StoreGetProto<PoolDetails>,
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

    // Tick liquidities: `store_tick_deltas` emits a (net, gross) pair per tick delta
    ticks_store_deltas
        .deltas
        .chunks(2)
        .zip(ticks_map_deltas.deltas)
        .for_each(|(store_deltas, tick_delta)| {
            let [net, gross] = store_deltas else {
                panic!("tick store deltas should come in (net, gross) pairs");
            };
            let value = |bytes: &[u8]| BigInt::from_store_bytes(bytes);
            let new_value = value(&net.new_value);

            let change = tick_change_type(
                pool_details_store
                    .get_last(tick_delta.pool_id.to_hex())
                    .is_some_and(|details| details.is_mev_capture_v21),
                (&value(&net.old_value), &new_value),
                (&value(&gross.old_value), &value(&gross.new_value)),
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
                    change: change.into(),
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
        Event::SwapFeeUpdated(ev) => Some(vec![Attribute {
            name: "swap_fee".to_string(),
            value: ev.swap_fee,
            change: ChangeType::Update.into(),
        }]),
        Event::ExtensionStateUpdated(ev) => Some(vec![Attribute {
            name: "anchor_state".to_string(),
            value: ev.state,
            change: ChangeType::Update.into(),
        }]),
        _ => None,
    }
}

/// The change type of a `tick/{index}` attribute from its (old, new) net
/// liquidity delta and gross liquidity. MEVCapture v2.1 cuts its Core calls at
/// initialized ticks, so its pools keep a tick while any position references
/// it (non-zero gross liquidity), even when its net delta is zero. Other pools
/// keep a tick while its net delta is non-zero.
fn tick_change_type(
    is_mev_capture_v21: bool,
    net: (&BigInt, &BigInt),
    gross: (&BigInt, &BigInt),
) -> ChangeType {
    let (old_value, new_value) = if is_mev_capture_v21 { gross } else { net };
    change_type_from_delta(old_value, new_value)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn change(is_mev_capture_v21: bool, net: (i64, i64), gross: (i64, i64)) -> ChangeType {
        tick_change_type(
            is_mev_capture_v21,
            (&BigInt::from(net.0), &BigInt::from(net.1)),
            (&BigInt::from(gross.0), &BigInt::from(gross.1)),
        )
    }

    #[test]
    fn test_mev_capture_v21_ticks_follow_gross_liquidity() {
        // A tick whose net delta reaches zero stays while positions reference it.
        assert_eq!(change(true, (5, 0), (5, 10)), ChangeType::Update);
        assert_eq!(change(true, (-5, 0), (15, 10)), ChangeType::Update);
        // It is created and deleted with its gross liquidity, even at net zero.
        assert_eq!(change(true, (0, 0), (0, 5)), ChangeType::Creation);
        assert_eq!(change(true, (0, 0), (5, 0)), ChangeType::Deletion);
        assert_eq!(change(true, (5, 0), (5, 0)), ChangeType::Deletion);
    }

    #[test]
    fn test_other_ticks_follow_net_liquidity() {
        assert_eq!(change(false, (0, 5), (0, 5)), ChangeType::Creation);
        assert_eq!(change(false, (5, 10), (5, 10)), ChangeType::Update);
        assert_eq!(change(false, (5, 0), (5, 10)), ChangeType::Deletion);
    }

    #[test]
    fn test_zero_net_value_is_one_zero_byte() {
        assert_eq!(BigInt::zero().to_signed_bytes_be(), vec![0]);
    }
}
