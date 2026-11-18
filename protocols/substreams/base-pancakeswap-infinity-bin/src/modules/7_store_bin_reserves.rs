use substreams::store::{StoreNew, StoreSet, StoreSetRaw};

use crate::{keys::bin_reserves_key, pb::pancakeswap::infinity::bin::BinDeltas};

/// Tracks each bin's packed reserves from `map_bin_changes`.
///
/// Writes at the delta's LOG ordinal, so `8_map_protocol_changes` can ask what a bin held as of a
/// swap that left it without writing it. A bin never written is absent: it never held reserves.
#[substreams::handlers::store]
pub fn store_bin_reserves(deltas: BinDeltas, store: StoreSetRaw) {
    for delta in deltas.deltas.into_iter() {
        store.set(delta.ordinal, bin_reserves_key(&delta.pool_id, delta.bin_id), &delta.new_packed);
    }
}
