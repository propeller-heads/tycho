use substreams::store::{StoreNew, StoreSet, StoreSetRaw};

use crate::{keys::tree_key, pb::pancakeswap::infinity::bin::TreeDeltas};

/// Tracks the bin tree's `level1` and `level2` words per pool and key from `map_tree_changes`.
///
/// Writes at the LOG ordinal of the Mint or Burn, like `store_active_id`, so `6_map_bin_changes`
/// can read the tree as of the log just before a swap with `get_at(log_ordinal - 1)`. A word
/// never written is absent, which readers treat as all zero: nothing under it is in the tree.
#[substreams::handlers::store]
pub fn store_bin_trees(deltas: TreeDeltas, store: StoreSetRaw) {
    for delta in deltas.deltas.into_iter() {
        store.set(delta.ordinal, tree_key(delta.level, &delta.pool_id, delta.key), &delta.bitmap);
    }
}
