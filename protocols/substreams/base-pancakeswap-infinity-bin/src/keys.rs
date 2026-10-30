//! Store keys, built here by writer and reader alike so the two cannot drift. A key that drifts
//! reads as an absent value, which the readers treat as "no pool", "empty tree" or "never held
//! reserves", and nothing errors.
//!
//! Pool ids are lowercase hex without `0x`.

/// `pool:{pool}`: the `Pool` in `store_pools` and the active bin in `store_active_id`.
pub(crate) fn pool_key(pool_id: &[u8]) -> String {
    format!("pool:{}", hex::encode(pool_id))
}

/// `tree{level}:{pool}:{key}`: a bin tree word in `store_bin_trees`. `level` is 1 or 2, `key`
/// is `bin_id >> 16` for level 1 and `bin_id >> 8` for level 2.
pub(crate) fn tree_key(level: u32, pool_id: &[u8], key: u32) -> String {
    format!("tree{level}:{}:{key}", hex::encode(pool_id))
}

/// `bin:{pool}:{bin_id}`: a bin's packed reserves in `store_bin_reserves`.
pub(crate) fn bin_reserves_key(pool_id: &[u8], bin_id: u32) -> String {
    format!("bin:{}:{bin_id}", hex::encode(pool_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: [u8; 2] = [0xab, 0xcd];

    #[test]
    fn keys_are_prefix_hex_and_index() {
        assert_eq!(pool_key(&POOL), "pool:abcd");
        assert_eq!(tree_key(2, &POOL, 32_767), "tree2:abcd:32767");
        assert_eq!(bin_reserves_key(&POOL, 8_388_608), "bin:abcd:8388608");
    }
}
