use crate::pb::ramses::v3::{events::pool_event::Typ, Events, LiquidityChanges, TickDeltas};
use itertools::Itertools;
use std::{collections::HashMap, str::FromStr, vec};
use substreams::{pb::substreams::StoreDeltas, scalar::BigInt};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

#[substreams::handlers::map]
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
) -> BlockChanges {
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // Add created pools to the tx changes map
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
    ticks_store_deltas
        .deltas
        .into_iter()
        .zip(ticks_map_deltas.deltas)
        .for_each(|(store_delta, tick_delta)| {
            let new_value_bigint =
                BigInt::from_str(&String::from_utf8(store_delta.new_value).unwrap()).unwrap();

            // If old value is empty or the int value is 0, it's considered as a creation.
            let is_creation = store_delta.old_value.is_empty() ||
                BigInt::from_str(&String::from_utf8(store_delta.old_value).unwrap())
                    .unwrap()
                    .is_zero();
            let attribute_name = format!("ticks/{}", tick_delta.tick_index);
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
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));

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
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));

            builder.add_entity_change(&EntityChanges {
                component_id: change.pool_address.to_hex(),
                attributes: vec![Attribute {
                    name: "liquidity".to_string(),
                    value: new_value_bigint.to_bytes_be().1,
                    change: ChangeType::Update.into(),
                }],
            });
        });

    // Insert other attributes
    for event in events.pool_events {
        let attributes = event_to_attributes_updates(event.typ.unwrap());
        if attributes.is_empty() {
            continue;
        }
        let tx = event.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        builder.add_entity_change(&EntityChanges {
            component_id: event.pool_address.to_hex(),
            attributes,
        });
    }

    BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        ..Default::default()
    }
}

fn event_to_attributes_updates(ty: Typ) -> Vec<Attribute> {
    match ty {
        Typ::Initialize(init) => vec![
            Attribute {
                name: "sqrt_price_x96".to_string(),
                value: init.sqrt_price,
                change: ChangeType::Update.into(),
            },
            Attribute {
                name: "tick".to_string(),
                value: BigInt::from(init.tick).to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            },
        ],
        Typ::Swap(swap) => vec![
            Attribute {
                name: "sqrt_price_x96".to_string(),
                value: swap.sqrt_price,
                change: ChangeType::Update.into(),
            },
            Attribute {
                name: "tick".to_string(),
                value: BigInt::from(swap.tick).to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            },
        ],
        Typ::FeeAdjustment(fee_adjustment) => vec![Attribute {
            name: "fee".to_string(),
            value: fee_adjustment.new_fee,
            change: ChangeType::Update.into(),
        }],
        _ => vec![],
    }
}
