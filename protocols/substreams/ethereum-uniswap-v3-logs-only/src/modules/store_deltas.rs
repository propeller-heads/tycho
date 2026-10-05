use std::{collections::HashMap, str::FromStr};

use substreams::{
    pb::substreams::{StoreDelta, StoreDeltas},
    scalar::BigInt,
};
use tycho_substreams::prelude::ChangeType;

pub(super) fn tick_store_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("{}:tick:{}", liquidity_store_key(pool_address), tick_index)
}

pub(super) fn liquidity_store_key(pool_address: &[u8]) -> String {
    format!("pool:{}", hex::encode(pool_address))
}

// Event ordinals distinguish repeated writes to the same key within a block.
pub(super) fn index_store_deltas(deltas: StoreDeltas) -> HashMap<(String, u64), StoreDelta> {
    deltas
        .deltas
        .into_iter()
        .map(|delta| ((delta.key.clone(), delta.ordinal), delta))
        .collect()
}

pub(super) fn take_store_delta(
    indexed: &mut HashMap<(String, u64), StoreDelta>,
    key: String,
    ordinal: u64,
) -> StoreDelta {
    indexed
        .remove(&(key.clone(), ordinal))
        .unwrap_or_else(|| panic!("no store delta for key {key} at ordinal {ordinal}"))
}

pub(super) fn store_bigint(value: &[u8]) -> BigInt {
    BigInt::from_str(std::str::from_utf8(value).unwrap()).unwrap()
}

// Classify the net transaction change, rather than its last individual write.
pub(super) fn tick_change_type(old_value: &[u8], new_value: &BigInt) -> Option<ChangeType> {
    let was_absent = old_value.is_empty() || store_bigint(old_value).is_zero();
    if was_absent && new_value.is_zero() {
        None
    } else if was_absent {
        Some(ChangeType::Creation)
    } else if new_value.is_zero() {
        Some(ChangeType::Deletion)
    } else {
        Some(ChangeType::Update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_keys_use_unprefixed_hex() {
        assert_eq!(tick_store_key(&[0x0a, 0x0b], -100), "pool:0a0b:tick:-100");
        assert_eq!(liquidity_store_key(&[0x0a, 0x0b]), "pool:0a0b");
    }
}
