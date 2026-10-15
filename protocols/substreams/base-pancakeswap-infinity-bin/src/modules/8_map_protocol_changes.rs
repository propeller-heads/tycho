use crate::{
    modules::store_bin_reserves::bin_reserves_key,
    parameters,
    pb::pancakeswap::infinity::bin::{
        events::{pool_event, PoolEvent},
        BinDelta, BinDeltas, Events, Transaction as BinTransaction,
    },
};
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw},
};
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
/// BinDelta, and a `bins/{bin_id}` deletion per left bin that was published.
///
/// A BinDelta carries absolute before and after values, so no accumulator store as in CL. The
/// reserves store answers only for a left bin, which the swap did not write: whether it held
/// reserves, and so was published, when the swap moved off it.
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
    bin_reserves_store: StoreGetRaw,
) -> Result<BlockChanges, substreams::errors::Error> {
    let changes = collect_transaction_changes(
        created_pools,
        events,
        balances_map_deltas,
        balances_store_deltas,
        bin_deltas,
        &bin_reserves_store,
    );
    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

#[allow(clippy::too_many_arguments)]
pub fn collect_transaction_changes<S: StoreGet<Vec<u8>>>(
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    bin_deltas: BinDeltas,
    bin_reserves_store: &S,
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

    for (tx, pool_id, attribute) in bin_attributes(bin_deltas, bin_reserves_store) {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));
        builder.add_entity_change(&EntityChanges {
            component_id: pool_id.to_hex(),
            attributes: vec![attribute],
        });
    }

    transaction_changes
        .drain()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
        .collect()
}

/// `bins/{id}` attributes in log order: one per visible delta, and a deletion per left bin that
/// still held reserves, so was published while active.
///
/// Log order because the builder keeps the last write per attribute within a transaction.
fn bin_attributes<S: StoreGet<Vec<u8>>>(
    bin_deltas: BinDeltas,
    bin_reserves_store: &S,
) -> Vec<(BinTransaction, PoolAddress, Attribute)> {
    let mut attributes: Vec<(u64, BinTransaction, PoolAddress, Attribute)> = Vec::new();
    for delta in bin_deltas.deltas {
        let Some(change) = bin_change_type(&delta) else {
            continue;
        };
        let attribute = Attribute {
            name: format!("bins/{}", delta.bin_id),
            // Raw packed word, so indexing and simulation cannot drift.
            value: delta.new_packed,
            change: change.into(),
        };
        attributes.push((delta.ordinal, delta.transaction.unwrap(), delta.pool_id, attribute));
    }
    for left in bin_deltas.left_bins {
        // Deleting an attribute that was never published fails the indexer's storage write, so
        // only delete what the store says held reserves.
        let held_reserves = bin_reserves_store
            .get_at(left.ordinal, bin_reserves_key(&left.pool_id, left.bin_id))
            .is_some_and(|packed| !is_zero(&packed));
        if !held_reserves {
            continue;
        }
        let attribute = Attribute {
            name: format!("bins/{}", left.bin_id),
            value: vec![],
            change: ChangeType::Deletion.into(),
        };
        attributes.push((left.ordinal, left.transaction.unwrap(), left.pool_id, attribute));
    }
    attributes.sort_by_key(|(ordinal, ..)| *ordinal);
    attributes
        .into_iter()
        .map(|(_, tx, pool_id, attribute)| (tx, pool_id, attribute))
        .collect()
}

fn is_zero(value: &[u8]) -> bool {
    value.iter().all(|byte| *byte == 0)
}

/// Change type for a `bins/` attribute, `None` when the bin is invisible on both sides.
///
/// Visible means holding reserves while in the tree or active: a swap starts in the active bin
/// whatever the tree says, and steps on only through the tree. A swap can drain a bin, a burn can
/// drop a bin that keeps dust and a swap can move off an active dust bin; each must leave the
/// snapshot, so visibility picks the change.
fn bin_change_type(delta: &BinDelta) -> Option<ChangeType> {
    let before = (delta.was_in_tree || delta.was_active) && !is_zero(&delta.old_packed);
    let after = (delta.in_tree || delta.is_active) && !is_zero(&delta.new_packed);
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
    use crate::{
        modules::test_stores::FakeRawStore,
        pb::pancakeswap::infinity::bin::{LeftBin, Transaction},
    };
    use rstest::rstest;

    fn word(value: u8) -> Vec<u8> {
        vec![value; 32]
    }

    /// One side of a delta: (in tree, active, packed reserves).
    type Side = (bool, bool, Vec<u8>);

    fn delta(bin_id: u32, ordinal: u64, before: Side, after: Side) -> BinDelta {
        BinDelta {
            pool_id: vec![0xab; 32],
            currency0: vec![],
            currency1: vec![],
            bin_id,
            old_packed: before.2,
            new_packed: after.2,
            ordinal,
            transaction: Some(Transaction::default()),
            was_in_tree: before.0,
            in_tree: after.0,
            was_active: before.1,
            is_active: after.1,
        }
    }

    fn left(bin_id: u32, ordinal: u64) -> LeftBin {
        LeftBin {
            pool_id: vec![0xab; 32],
            bin_id,
            ordinal,
            transaction: Some(Transaction::default()),
        }
    }

    /// An empty value reads as zero: words are big-endian, so missing leading bytes are zeros.
    #[rstest]
    #[case::funded_from_zero((true, false, word(0)), (true, false, word(1)), Some(ChangeType::Creation))]
    #[case::funded_from_empty((true, false, vec![]), (true, false, word(1)), Some(ChangeType::Creation))]
    #[case::emptied_to_zero((true, false, word(1)), (true, false, word(0)), Some(ChangeType::Deletion))]
    #[case::emptied_to_empty((true, false, word(1)), (true, false, vec![]), Some(ChangeType::Deletion))]
    #[case::moved((true, false, word(1)), (true, false, word(2)), Some(ChangeType::Update))]
    #[case::never_funded((true, false, word(0)), (true, false, word(0)), None)]
    #[case::burnt_to_dust_leaves_the_tree(
        (true, false, word(10)),
        (false, false, word(1)),
        Some(ChangeType::Deletion)
    )]
    #[case::active_bin_burnt_to_dust_stays(
        (true, true, word(10)),
        (false, true, word(1)),
        Some(ChangeType::Update)
    )]
    #[case::dust_bin_minted_back_into_the_tree(
        (false, false, word(1)),
        (true, false, word(20)),
        Some(ChangeType::Creation)
    )]
    #[case::donate_to_an_active_dust_bin(
        (false, true, word(1)),
        (false, true, word(5)),
        Some(ChangeType::Update)
    )]
    #[case::swap_stays_in_an_active_dust_bin(
        (false, true, word(1)),
        (false, true, word(2)),
        Some(ChangeType::Update)
    )]
    #[case::swap_moves_off_an_active_dust_bin(
        (false, true, word(1)),
        (false, false, word(2)),
        Some(ChangeType::Deletion)
    )]
    #[case::swap_through_a_dust_bin_it_did_not_start_in(
        (false, false, word(1)),
        (false, false, word(2)),
        None
    )]
    fn test_bin_change_type(
        #[case] before: Side,
        #[case] after: Side,
        #[case] expected: Option<ChangeType>,
    ) {
        assert_eq!(bin_change_type(&delta(7, 1, before, after)), expected);
    }

    fn reserves_store(writes: Vec<(u64, u32, Vec<u8>)>) -> FakeRawStore {
        FakeRawStore(
            writes
                .into_iter()
                .map(|(ordinal, bin_id, packed)| {
                    (ordinal, bin_reserves_key(&[0xab; 32], bin_id), packed)
                })
                .collect(),
        )
    }

    fn names_and_changes(bin_deltas: BinDeltas, store: &FakeRawStore) -> Vec<(String, ChangeType)> {
        bin_attributes(bin_deltas, store)
            .into_iter()
            .map(|(_, _, attribute)| (attribute.name.clone(), attribute.change()))
            .collect()
    }

    /// A left bin is deleted only if it held reserves, since only then was it published.
    /// Deleting an attribute that was never published fails the indexer's storage write.
    #[rstest]
    #[case::held_dust(vec![(3, 100, word(1))], vec![("bins/100".to_string(), ChangeType::Deletion)])]
    #[case::emptied_before(vec![(3, 100, word(1)), (5, 100, word(0))], vec![])]
    #[case::never_written(vec![], vec![])]
    fn left_bin_is_deleted_only_if_it_held_reserves(
        #[case] writes: Vec<(u64, u32, Vec<u8>)>,
        #[case] expected: Vec<(String, ChangeType)>,
    ) {
        let bin_deltas = BinDeltas { deltas: vec![], left_bins: vec![left(100, 10)] };

        assert_eq!(names_and_changes(bin_deltas, &reserves_store(writes)), expected);
    }

    /// The builder keeps the last write per attribute, so a left bin's deletion must sort among
    /// the deltas by log, not after them.
    #[test]
    fn bin_attributes_are_in_log_order() {
        let bin_deltas = BinDeltas {
            deltas: vec![delta(100, 20, (false, false, word(1)), (true, false, word(9)))],
            left_bins: vec![left(100, 10)],
        };
        let store = reserves_store(vec![(3, 100, word(1))]);

        assert_eq!(
            names_and_changes(bin_deltas, &store),
            vec![
                ("bins/100".to_string(), ChangeType::Deletion),
                ("bins/100".to_string(), ChangeType::Creation),
            ]
        );
    }
}
