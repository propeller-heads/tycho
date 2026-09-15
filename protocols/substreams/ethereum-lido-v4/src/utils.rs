use anyhow::{anyhow, Result};
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
