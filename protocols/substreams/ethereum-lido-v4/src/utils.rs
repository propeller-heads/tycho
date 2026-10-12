use crate::constants::STETH_ADDRESS;
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

/// Successful stETH writes in execution order, including parent writes after child calls.
pub fn ordered_storage_changes(tx: &TransactionTrace) -> Vec<&StorageChange> {
    let mut changes: Vec<_> = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| &call.storage_changes)
        .filter(|change| change.address == STETH_ADDRESS)
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
}
