use anyhow::{anyhow, Result};
use substreams_ethereum::pb::eth::v2::{StorageChange, TransactionTrace};
use tycho_substreams::models::{Attribute, ChangeType};

pub fn attribute_with_bytes(name: &str, value: &[u8], change: ChangeType) -> Attribute {
    Attribute { name: name.to_string(), value: value.to_vec(), change: change.into() }
}

pub fn bytes_from_hex(value: &str) -> Result<Vec<u8>> {
    let value = value
        .strip_prefix("0x")
        .unwrap_or(value);
    hex::decode(value).map_err(|e| anyhow!("Failed to decode hex value: {e}"))
}

/// The storage writes in `tx` that `keep` selects, from calls the chain kept, in execution
/// order.
///
/// The trace lists each call's writes together, so a parent's writes made after a child call
/// come before the child's. The store engine replays writes in the order they are set, so a
/// later write to the same key has to be set later: sorting by ordinal is what makes that hold.
/// Only the kept writes are sorted; a transaction can carry thousands and the callers want a
/// handful.
pub fn ordered_storage_changes(
    tx: &TransactionTrace,
    keep: impl Fn(&StorageChange) -> bool,
) -> Vec<&StorageChange> {
    let mut changes: Vec<_> = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter(|change| keep(change))
        .collect();
    changes.sort_unstable_by_key(|change| change.ordinal);
    changes
}

#[cfg(test)]
mod tests {
    use super::bytes_from_hex;

    #[test]
    fn decode_prefixed_hex() {
        assert_eq!(bytes_from_hex("0xaabbcc").unwrap(), vec![0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn decode_unprefixed_hex() {
        assert_eq!(bytes_from_hex("aabbcc").unwrap(), vec![0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn reject_non_hex() {
        assert!(bytes_from_hex("0xzz").is_err());
    }
}
