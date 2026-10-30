use crate::{
    modules::map_pool_created::decode_address,
    parameters,
    pb::pancakeswap::infinity::bin::{
        events::{pool_event, PoolEvent},
        Events, TreeDelta, TreeDeltas,
    },
    storage::{in_window, storage_writes_by_tx, tx_index, with_floors, WritesBySlot},
};
use std::collections::HashMap;
use substreams_ethereum::pb::eth::v2::{self as eth};

/// Bin tree words rewritten by mints and burns, read out of BinPoolManager storage diffs.
///
/// The swap loop walks the tree, not `reserveOfBin`: a burn down to `MINIMUM_SHARE` drops a bin
/// from the tree while leaving dust in its reserves, and a swap jumps straight to the next tree
/// bin however far away. `map_bin_changes` needs the tree for both, so each `level1` and `level2`
/// word a `Mint` or `Burn` wrote is emitted as is. Swaps never write the tree.
#[substreams::handlers::map]
pub fn map_tree_changes(
    params: String,
    block: eth::Block,
    events: Events,
) -> Result<TreeDeltas, substreams::errors::Error> {
    let pool_manager = decode_address("pool_manager", &params)?;
    let writes_by_tx = storage_writes_by_tx(&block, &pool_manager, &events.pool_events);

    let no_writes = HashMap::new();
    let mut deltas: Vec<TreeDelta> = Vec::new();
    for (event, floor) in with_floors(&events.pool_events) {
        let writes = writes_by_tx
            .get(&tx_index(event))
            .unwrap_or(&no_writes);
        deltas.extend(event_to_tree_deltas(event, writes, floor));
    }

    // Events arrive in ordinal order and each event's words are sorted, so the deltas are too.
    Ok(TreeDeltas { deltas })
}

/// Tree words one event rewrote: `level2` per touched segment, `level1` per touched group of 256
/// segments. A word with no write in the window `(floor, log_ordinal)` is unchanged, so nothing is
/// emitted for it.
fn event_to_tree_deltas(event: &PoolEvent, writes: &WritesBySlot, floor: u64) -> Vec<TreeDelta> {
    let bin_ids: &[u32] = match event.r#type.as_ref().unwrap() {
        pool_event::Type::Mint(mint) => &mint.ids,
        pool_event::Type::Burn(burn) => &burn.ids,
        pool_event::Type::Initialize(..) |
        pool_event::Type::Swap(..) |
        pool_event::Type::Donate(..) |
        pool_event::Type::ProtocolFeeUpdated(..) => return Vec::new(),
    };

    let pool_id = &event.pool_id;
    let base = parameters::pool_base_slot(pool_id);

    let mut words: Vec<(u32, u32)> = bin_ids
        .iter()
        .flat_map(|id| [(1, id >> 16), (2, id >> 8)])
        .collect();
    words.sort_unstable();
    words.dedup();

    words
        .into_iter()
        .filter_map(|(level, key)| {
            let slot = if level == 1 {
                parameters::tree_level1_slot(&base, key)
            } else {
                parameters::tree_level2_slot(&base, key)
            };
            writes
                .get(slot.as_slice())
                .and_then(|slot_writes| {
                    slot_writes
                        .iter()
                        .filter(|change| in_window(change.ordinal, floor, event.log_ordinal))
                        .max_by_key(|change| change.ordinal)
                })
                .map(|change| TreeDelta {
                    pool_id: pool_id.clone(),
                    key,
                    bitmap: change.new_value.clone(),
                    ordinal: event.log_ordinal,
                    level,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::pancakeswap::infinity::bin::{events::pool_event::Type, Transaction};
    use substreams_ethereum::pb::eth::v2::StorageChange;

    const POOL_MANAGER: [u8; 20] = [0xc6; 20];
    const POOL_ID: &str = "abababababababababababababababababababababababababababababababab";

    fn pool_id_bytes() -> [u8; 32] {
        hex::decode(POOL_ID)
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn leaf_write(segment: u32, new: Vec<u8>, ordinal: u64) -> StorageChange {
        let base = parameters::pool_base_slot(&pool_id_bytes());
        tree_write(parameters::tree_level2_slot(&base, segment), new, ordinal)
    }

    fn middle_write(key: u32, new: Vec<u8>, ordinal: u64) -> StorageChange {
        let base = parameters::pool_base_slot(&pool_id_bytes());
        tree_write(parameters::tree_level1_slot(&base, key), new, ordinal)
    }

    fn tree_write(slot: [u8; 32], new: Vec<u8>, ordinal: u64) -> StorageChange {
        StorageChange {
            address: POOL_MANAGER.to_vec(),
            key: slot.to_vec(),
            old_value: vec![0u8; 32],
            new_value: new,
            ordinal,
        }
    }

    fn index(changes: &[StorageChange]) -> WritesBySlot<'_> {
        let mut map = WritesBySlot::new();
        for change in changes {
            map.entry(change.key.clone())
                .or_default()
                .push(change);
        }
        map
    }

    fn event(kind: Type, ordinal: u64) -> PoolEvent {
        PoolEvent {
            log_ordinal: ordinal,
            pool_id: pool_id_bytes().to_vec(),
            currency0: Vec::new(),
            currency1: Vec::new(),
            transaction: Some(Transaction { hash: vec![1], from: vec![], to: vec![], index: 0 }),
            r#type: Some(kind),
        }
    }

    fn mint(ids: Vec<u32>) -> Type {
        Type::Mint(pool_event::Mint { ids })
    }

    fn word(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    /// Three bins in two segments, both leaves written: one delta per segment, not per bin.
    #[test]
    fn mint_emits_one_delta_per_written_segment() {
        let changes = vec![leaf_write(0, word(0x01), 1), leaf_write(1, word(0x02), 2)];

        let deltas = event_to_tree_deltas(&event(mint(vec![5, 6, 256]), 9), &index(&changes), 0);

        assert_eq!(
            deltas
                .iter()
                .map(|d| (d.key, d.bitmap[0]))
                .collect::<Vec<_>>(),
            vec![(0, 0x01), (1, 0x02)]
        );
        assert!(deltas.iter().all(|d| d.ordinal == 9));
    }

    /// A bin already in the tree writes no leaf, so its segment is silent.
    #[test]
    fn unchanged_segment_is_skipped() {
        let changes = vec![leaf_write(1, word(0x02), 2)];

        let deltas = event_to_tree_deltas(&event(mint(vec![5, 256]), 9), &index(&changes), 0);

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].key, 1);
    }

    /// A segment's first bin also sets its bit one level up; both words are emitted, `level1`
    /// keyed by `bin_id >> 16`.
    #[test]
    fn first_bin_in_a_segment_emits_its_level1_word() {
        let id = (3 << 16) | (7 << 8) | 5;
        let changes = vec![leaf_write(id >> 8, word(0x01), 1), middle_write(3, word(0x80), 2)];

        let deltas = event_to_tree_deltas(&event(mint(vec![id]), 9), &index(&changes), 0);

        assert_eq!(
            deltas
                .iter()
                .map(|d| (d.level, d.key, d.bitmap[0]))
                .collect::<Vec<_>>(),
            vec![(1, 3, 0x80), (2, id >> 8, 0x01)]
        );
    }

    /// Writes at or below the floor belong to the previous event; writes at or after the log
    /// ordinal belong to a later one. Only the latest in between counts.
    #[test]
    fn only_writes_inside_the_window_count() {
        let changes = vec![
            leaf_write(0, word(0xaa), 3),
            leaf_write(0, word(0xbb), 5),
            leaf_write(0, word(0xcc), 7),
            leaf_write(0, word(0xdd), 9),
        ];

        let deltas = event_to_tree_deltas(&event(mint(vec![1]), 9), &index(&changes), 3);

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].bitmap, word(0xcc));
    }
}
