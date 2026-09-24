use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChanges, TickDeltas,
};
use itertools::Itertools;
use std::{collections::HashMap, str::FromStr, vec};
use substreams::{pb::substreams::StoreDeltas, scalar::BigInt};
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
    ticks_gross_store_deltas: StoreDeltas,
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

    // Insert ticks net-liquidity changes
    ticks_store_deltas
        .deltas
        .into_iter()
        .zip(ticks_gross_store_deltas.deltas)
        .zip(ticks_map_deltas.deltas)
        .for_each(|((store_delta, gross_store_delta), tick_delta)| {
            let new_value_bigint =
                BigInt::from_str(&String::from_utf8(store_delta.new_value).unwrap()).unwrap();

            let attribute_name = format!("ticks/{}/net-liquidity", tick_delta.tick_index);
            let attribute = Attribute {
                name: attribute_name,
                value: new_value_bigint.to_signed_bytes_be(),
                change: tick_change_type_from_gross(
                    &gross_store_delta.old_value,
                    &gross_store_delta.new_value,
                )
                .into(),
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

fn tick_change_type_from_gross(old_value: &[u8], new_value: &[u8]) -> ChangeType {
    let old_is_zero = old_value.is_empty() || gross_value_is_zero(old_value);
    let new_is_zero = gross_value_is_zero(new_value);

    if new_is_zero {
        ChangeType::Deletion
    } else if old_is_zero {
        ChangeType::Creation
    } else {
        ChangeType::Update
    }
}

fn gross_value_is_zero(value: &[u8]) -> bool {
    BigInt::from_str(std::str::from_utf8(value).unwrap())
        .unwrap()
        .is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gross_liquidity_classifies_tick_lifecycle() {
        assert_eq!(tick_change_type_from_gross(b"", b"100"), ChangeType::Creation);
        assert_eq!(tick_change_type_from_gross(b"100", b"200"), ChangeType::Update);
        assert_eq!(tick_change_type_from_gross(b"100", b"0"), ChangeType::Deletion);
    }

    #[test]
    fn adjacent_positions_keep_a_zero_net_tick_as_an_update() {
        let (net_liquidity, gross_liquidity) = [(100, 100), (-100, 100)]
            .into_iter()
            .fold((0, 0), |(net, gross), (net_delta, gross_delta)| {
                (net + net_delta, gross + gross_delta)
            });

        assert_eq!(net_liquidity, 0);
        assert_eq!(gross_liquidity, 200);
        assert_eq!(tick_change_type_from_gross(b"100", b"200"), ChangeType::Update);
    }

    #[test]
    fn burning_both_adjacent_positions_deletes_at_zero_gross_liquidity() {
        let (net_liquidity, gross_liquidity) = [(-100, -100), (100, -100)]
            .into_iter()
            .fold((0, 200), |(net, gross), (net_delta, gross_delta)| {
                (net + net_delta, gross + gross_delta)
            });

        assert_eq!(net_liquidity, 0);
        assert_eq!(gross_liquidity, 0);
        assert_eq!(tick_change_type_from_gross(b"200", b"100"), ChangeType::Update);
        assert_eq!(tick_change_type_from_gross(b"100", b"0"), ChangeType::Deletion);
    }
}
