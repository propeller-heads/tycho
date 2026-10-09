use crate::{
    parameters,
    pb::pancakeswap::infinity::bin::{
        events::{pool_event, PoolEvent},
        BinDelta, BinDeltas, Events,
    },
};
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{pb::substreams::StoreDeltas, scalar::BigInt};
use substreams_ethereum::pb::eth::v2 as eth;
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    models::{
        Attribute, BlockBalanceDeltas, BlockChanges, BlockEntityChanges, ChangeType, EntityChanges,
        Transaction, TransactionChanges, TransactionChangesBuilder,
    },
};

type PoolAddress = Vec<u8>;

// `Transaction` is tycho's, not this package's proto one. The `From<&bin::Transaction>` impls in
// modules/mod.rs are what make `.into()` below compile.
/// Assembles `BlockChanges`: created components, entity attributes, balance changes.
///
/// Emits `active_id` per Swap, `protocol_fees/*` per ProtocolFeeUpdated, `bins/{bin_id}` per
/// BinDelta.
///
/// No store join, unlike CL: tick liquidity accumulates so only the store knows the running
/// total, but a BinDelta carries absolute before and after values. Hence no bin accumulator store
/// in the manifest.
///
/// `storage_changes` stays empty: Bin simulation is native, so no VM needs the raw slots.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::Block,
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    bin_deltas: BinDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let changes = collect_transaction_changes(
        created_pools,
        events,
        balances_map_deltas,
        balances_store_deltas,
        bin_deltas,
    );
    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

#[allow(clippy::too_many_arguments)]
pub fn collect_transaction_changes(
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    bin_deltas: BinDeltas,
) -> Vec<TransactionChanges> {
    // Merged per transaction, sorted at the end.
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

    // Absolute balances from this block's store deltas, merged onto the per-tx builders.
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

    // One `bins/{id}` per visible delta. No store lookup: the delta has both sides.
    for delta in bin_deltas.deltas {
        let Some(change) = bin_change_type(&delta) else {
            continue;
        };
        let attribute = Attribute {
            name: format!("bins/{}", delta.bin_id),
            // Raw packed word, so indexing and simulation cannot drift.
            value: delta.new_packed.clone(),
            change: change.into(),
        };
        let tx = delta.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

        builder.add_entity_change(&EntityChanges {
            component_id: delta.pool_id.to_hex(),
            attributes: vec![attribute],
        });
    }

    transaction_changes
        .drain()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
        .collect()
}

/// Change type for a `bins/` attribute, `None` when the bin is invisible on both sides.
///
/// Visible means in the tree with reserves. A swap can drain a tree bin and a burn can drop a
/// bin that keeps dust; both must leave the snapshot, so visibility picks the change.
fn bin_change_type(delta: &BinDelta) -> Option<ChangeType> {
    let is_zero = |value: &[u8]| value.iter().all(|byte| *byte == 0);
    let before = delta.was_in_tree && !is_zero(&delta.old_packed);
    let after = delta.in_tree && !is_zero(&delta.new_packed);
    match (before, after) {
        (false, true) => Some(ChangeType::Creation),
        (true, true) => Some(ChangeType::Update),
        (true, false) => Some(ChangeType::Deletion),
        (false, false) => None,
    }
}

fn event_to_attributes_updates(event: PoolEvent) -> Vec<(Transaction, PoolAddress, Attribute)> {
    let updates: Vec<(&str, BigInt)> = match event.r#type.as_ref().unwrap() {
        pool_event::Type::Swap(swap) => vec![("active_id", BigInt::from(swap.active_id))],
        pool_event::Type::ProtocolFeeUpdated(updated) => {
            let (zero2one, one2zero) = parameters::split_protocol_fee(updated.protocol_fee);
            vec![
                ("protocol_fees/zero2one", BigInt::from(zero2one)),
                ("protocol_fees/one2zero", BigInt::from(one2zero)),
            ]
        }
        pool_event::Type::Initialize(..) |
        pool_event::Type::Mint(..) |
        pool_event::Type::Burn(..) |
        pool_event::Type::Donate(..) => return vec![],
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
    use rstest::rstest;

    fn word(value: u8) -> Vec<u8> {
        vec![value; 32]
    }

    fn delta(was_in_tree: bool, old: Vec<u8>, in_tree: bool, new: Vec<u8>) -> BinDelta {
        BinDelta {
            pool_id: vec![0xab; 32],
            currency0: vec![],
            currency1: vec![],
            bin_id: 7,
            old_packed: old,
            new_packed: new,
            ordinal: 1,
            transaction: None,
            was_in_tree,
            in_tree,
        }
    }

    /// An empty slice is how a bin first written this block reports its old side.
    #[rstest]
    #[case::funded_from_zero(true, word(0), true, word(1), Some(ChangeType::Creation))]
    #[case::funded_from_empty(true, vec![], true, word(1), Some(ChangeType::Creation))]
    #[case::emptied_to_zero(true, word(1), true, word(0), Some(ChangeType::Deletion))]
    #[case::emptied_to_empty(true, word(1), true, vec![], Some(ChangeType::Deletion))]
    #[case::moved(true, word(1), true, word(2), Some(ChangeType::Update))]
    #[case::never_funded(true, word(0), true, word(0), None)]
    #[case::burnt_to_dust_leaves_the_tree(
        true,
        word(10),
        false,
        word(1),
        Some(ChangeType::Deletion)
    )]
    #[case::dust_bin_minted_back_into_the_tree(
        false,
        word(1),
        true,
        word(20),
        Some(ChangeType::Creation)
    )]
    #[case::swap_through_a_dust_active_bin(false, word(1), false, word(2), None)]
    fn test_bin_change_type(
        #[case] was_in_tree: bool,
        #[case] old: Vec<u8>,
        #[case] in_tree: bool,
        #[case] new: Vec<u8>,
        #[case] expected: Option<ChangeType>,
    ) {
        assert_eq!(bin_change_type(&delta(was_in_tree, old, in_tree, new)), expected);
    }
}
