use crate::{
    keys::bin_reserves_key,
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

type PoolId = Vec<u8>;

// `Transaction` is tycho's, not this package's proto one. The `From<&bin::Transaction>` impls in
// modules/mod.rs are what make `.into()` below compile.
/// Assembles `BlockChanges`: created components, entity attributes, balance changes.
///
/// Emits `active_id` per Swap, `protocol_fees/*` per ProtocolFeeUpdated, `bins/{bin_id}/reserve_x`
/// and `bins/{bin_id}/reserve_y` per BinDelta, `active_in_tree` per BinDelta on the active bin, and
/// deletions of both reserve attributes per left bin that was published.
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
        .for_each(|(tx, pool_id, attr)| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.add_entity_change(&EntityChanges {
                component_id: pool_id.to_hex(),
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

/// `bins/{id}/reserve_x` and `bins/{id}/reserve_y` in log order: a pair per visible delta, and a
/// pair of deletions per left bin that still held reserves, so was published while active. Plus
/// `active_in_tree` per delta that writes the active bin: the simulation keeps only visible bins,
/// so this is the one bit that tells it whether a swap moving off the active bin leaves it
/// reachable.
///
/// Log order because the builder keeps the last write per attribute within a transaction.
fn bin_attributes<S: StoreGet<Vec<u8>>>(
    bin_deltas: BinDeltas,
    bin_reserves_store: &S,
) -> Vec<(BinTransaction, PoolId, Attribute)> {
    let mut attributes: Vec<(u64, BinTransaction, PoolId, Attribute)> = Vec::new();
    for delta in bin_deltas.deltas {
        let change = bin_change_type(&delta);
        let tx = delta.transaction.unwrap();
        if delta.is_active {
            let attribute = Attribute {
                name: "active_in_tree".to_string(),
                value: vec![delta.in_tree as u8],
                change: ChangeType::Update.into(),
            };
            attributes.push((delta.ordinal, tx.clone(), delta.pool_id.clone(), attribute));
        }
        let Some(change) = change else {
            continue;
        };
        let packed = parameters::reserve_word(&delta.new_packed)
            .expect("map_bin_changes widens every reserve word to 32 bytes");
        let (x, y) = parameters::unpack_reserves(&packed);
        for (name, value) in reserve_attributes(delta.bin_id, x, y) {
            let attribute = Attribute { name, value, change: change.into() };
            attributes.push((delta.ordinal, tx.clone(), delta.pool_id.clone(), attribute));
        }
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
        let tx = left.transaction.unwrap();
        for (name, _) in reserve_attributes(left.bin_id, 0, 0) {
            let attribute = Attribute { name, value: vec![], change: ChangeType::Deletion.into() };
            attributes.push((left.ordinal, tx.clone(), left.pool_id.clone(), attribute));
        }
    }
    attributes.sort_by_key(|(ordinal, ..)| *ordinal);
    attributes
        .into_iter()
        .map(|(_, tx, pool_id, attribute)| (tx, pool_id, attribute))
        .collect()
}

/// The two reserve attributes of a bin, each a 16-byte big-endian `u128`. Unpacked here so no
/// reader has to know which half of `reserveOfBin` is which.
fn reserve_attributes(bin_id: u32, x: u128, y: u128) -> [(String, Vec<u8>); 2] {
    [
        (format!("bins/{bin_id}/reserve_x"), x.to_be_bytes().to_vec()),
        (format!("bins/{bin_id}/reserve_y"), y.to_be_bytes().to_vec()),
    ]
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

fn event_to_attributes_updates(event: PoolEvent) -> Vec<(Transaction, PoolId, Attribute)> {
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
    updates
        .into_iter()
        .map(|(name, value)| {
            let attribute = Attribute {
                name: name.to_string(),
                value: value.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            };
            (tx.clone(), event.pool_id.clone(), attribute)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        modules::{
            test_fixtures::*,
            test_stores::{FakeRawStore, FakeStore},
        },
        pb::pancakeswap::infinity::bin::{events::pool_event, LeftBin, Transaction},
    };
    use rstest::rstest;

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
    fn visibility_picks_the_change_type(
        #[case] before: Side,
        #[case] after: Side,
        #[case] expected: Option<ChangeType>,
    ) {
        assert_eq!(bin_change_type(&delta(7, 1, before, after)), expected);
    }

    fn names_and_values(event_kind: pool_event::Type) -> Vec<(String, Vec<u8>)> {
        event_to_attributes_updates(event(event_kind, 3))
            .into_iter()
            .map(|(_, pool_id, attribute)| {
                assert_eq!(pool_id, POOL_ID.to_vec());
                (attribute.name, attribute.value)
            })
            .collect()
    }

    /// The packed uint24 splits into the two 12-bit halves, low bits first, each under its own
    /// name; a swap emits the post-swap bin as `active_id`.
    #[test]
    fn fee_update_emits_both_halves_and_swap_the_active_id() {
        assert_eq!(
            names_and_values(pool_event::Type::ProtocolFeeUpdated(
                pool_event::ProtocolFeeUpdated { protocol_fee: 0x00_1005 }
            )),
            vec![
                ("protocol_fees/zero2one".to_string(), vec![0x05]),
                ("protocol_fees/one2zero".to_string(), vec![0x01]),
            ]
        );
        assert_eq!(
            names_and_values(swap(8_388_608)),
            vec![("active_id".to_string(), vec![0x00, 0x80, 0x00, 0x00])]
        );
        assert!(names_and_values(mint(vec![1])).is_empty(), "bin events carry no pool attribute");
    }

    fn reserves_store(writes: Vec<(u64, u32, Vec<u8>)>) -> FakeRawStore {
        FakeStore(
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

    fn reserves(bin_id: u32, change: ChangeType) -> Vec<(String, ChangeType)> {
        vec![
            (format!("bins/{bin_id}/reserve_x"), change),
            (format!("bins/{bin_id}/reserve_y"), change),
        ]
    }

    /// The word is unpacked once, here: y is the first 16 bytes on chain, x the last, and each
    /// attribute carries its half as a plain 16-byte big-endian number.
    #[test]
    fn reserve_attributes_carry_each_half_unpacked() {
        let mut packed = [0u8; 32];
        packed[15] = 7;
        packed[31] = 3;
        let bin_deltas = BinDeltas {
            deltas: vec![delta(100, 1, (true, false, word(0)), (true, false, packed.to_vec()))],
            left_bins: vec![],
        };

        let values: Vec<(String, Vec<u8>)> = bin_attributes(bin_deltas, &reserves_store(vec![]))
            .into_iter()
            .map(|(_, _, attribute)| (attribute.name, attribute.value))
            .collect();

        let mut x = vec![0u8; 16];
        x[15] = 3;
        let mut y = vec![0u8; 16];
        y[15] = 7;
        assert_eq!(
            values,
            vec![("bins/100/reserve_x".to_string(), x), ("bins/100/reserve_y".to_string(), y)]
        );
    }

    /// A left bin is deleted only if it held reserves, since only then was it published.
    /// Deleting an attribute that was never published fails the indexer's storage write.
    #[rstest]
    #[case::held_dust(vec![(3, 100, word(1))], reserves(100, ChangeType::Deletion))]
    #[case::emptied_before(vec![(3, 100, word(1)), (5, 100, word(0))], vec![])]
    #[case::never_written(vec![], vec![])]
    fn left_bin_is_deleted_only_if_it_held_reserves(
        #[case] writes: Vec<(u64, u32, Vec<u8>)>,
        #[case] expected: Vec<(String, ChangeType)>,
    ) {
        let bin_deltas = BinDeltas { deltas: vec![], left_bins: vec![left(100, 10)] };

        assert_eq!(names_and_changes(bin_deltas, &reserves_store(writes)), expected);
    }

    /// Only a delta on the active bin says whether that bin is in the tree; the flag follows the
    /// delta's `in_tree`, whatever happened to the reserves.
    #[rstest]
    #[case::active_dust_bin_after_a_burn((true, true, word(10)), (false, true, word(1)), Some(0))]
    #[case::swap_lands_in_a_tree_bin((true, false, word(10)), (true, true, word(9)), Some(1))]
    #[case::active_bin_emptied((true, true, word(10)), (false, true, word(0)), Some(0))]
    #[case::bin_that_is_not_active((true, false, word(10)), (true, false, word(9)), None)]
    fn active_in_tree_follows_the_active_delta(
        #[case] before: Side,
        #[case] after: Side,
        #[case] expected: Option<u8>,
    ) {
        let bin_deltas = BinDeltas { deltas: vec![delta(7, 1, before, after)], left_bins: vec![] };

        let flag = bin_attributes(bin_deltas, &reserves_store(vec![]))
            .into_iter()
            .map(|(_, _, attribute)| attribute)
            .find(|attribute| attribute.name == "active_in_tree")
            .map(|attribute| attribute.value[0]);

        assert_eq!(flag, expected);
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
            [reserves(100, ChangeType::Deletion), reserves(100, ChangeType::Creation)].concat()
        );
    }
}
