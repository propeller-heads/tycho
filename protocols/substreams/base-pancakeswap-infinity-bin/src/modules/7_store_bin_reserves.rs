use substreams::store::{StoreNew, StoreSet, StoreSetRaw};

use crate::pb::pancakeswap::infinity::bin::BinDeltas;

/// Tracks each bin's packed reserves (`bin:{pool}:{bin_id}`) from `map_bin_changes`.
///
/// Writes at the delta's LOG ordinal, so `8_map_protocol_changes` can ask what a bin held as of a
/// swap that left it without writing it. A bin never written is absent: it never held reserves.
#[substreams::handlers::store]
pub fn store_bin_reserves(deltas: BinDeltas, store: StoreSetRaw) {
    for delta in deltas.deltas.into_iter() {
        store.set(delta.ordinal, bin_reserves_key(&delta.pool_id, delta.bin_id), &delta.new_packed);
    }
}

/// `bin:{pool}:{bin_id}`, the pool id as lowercase hex without `0x`.
pub fn bin_reserves_key(pool_id: &[u8], bin_id: u32) -> String {
    format!("bin:{}:{bin_id}", hex::encode(pool_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_pool_hex_and_bin_id() {
        assert_eq!(
            bin_reserves_key(&[0xab; 32], 8_388_608),
            format!("bin:{}:8388608", "ab".repeat(32))
        );
    }
}
