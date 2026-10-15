use substreams::store::{StoreNew, StoreSet, StoreSetRaw};

use crate::pb::pancakeswap::infinity::bin::{TreeDelta, TreeDeltas};

/// Tracks the bin tree's `level1` and `level2` words per pool and key (`tree{level}:{pool}:{key}`)
/// from `map_tree_changes`.
///
/// Writes at the LOG ordinal of the Mint or Burn, like `store_active_id`, so `6_map_bin_changes`
/// can read the tree as of the log just before a swap with `get_at(log_ordinal - 1)`. A word
/// never written is absent, which readers treat as all zero: nothing under it is in the tree.
#[substreams::handlers::store]
pub fn store_bin_trees(deltas: TreeDeltas, store: StoreSetRaw) {
    // For every delta: `store.set(ordinal, key, &bitmap)`. The key must match what the reader
    // builds from `event.pool_id`, which carries a `0x` prefix the delta's raw bytes do not.
    for delta in deltas.deltas.into_iter() {
        store.set(delta.ordinal, tree_key(&delta), &delta.bitmap);
    }
}

/// `tree{level}:{pool}:{key}`, the pool id as lowercase hex without `0x`.
fn tree_key(delta: &TreeDelta) -> String {
    format!("tree{}:{}:{}", delta.level, hex::encode(&delta.pool_id), delta.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the key format the reader in `6_map_bin_changes` depends on.
    #[test]
    fn key_is_level_pool_hex_and_key() {
        let delta = TreeDelta {
            pool_id: vec![0xab; 32],
            key: 32_767,
            bitmap: vec![],
            ordinal: 1,
            level: 2,
        };
        assert_eq!(tree_key(&delta), format!("tree2:{}:32767", "ab".repeat(32)));
    }
}
