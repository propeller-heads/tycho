use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_helper::hex::Hexable;

#[derive(Debug, Deserialize)]
pub struct Params {
    pub dex_v2_address: String,
    pub liquidity_address: String,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }
}

pub fn component_id(dex_type: &BigInt, dex_id: &[u8; 32]) -> String {
    let mut bytes = Vec::with_capacity(33);
    bytes.push(dex_type.to_u64() as u8);
    bytes.extend_from_slice(dex_id);
    bytes.to_hex()
}

pub fn pool_store_key(dex_type: &BigInt, dex_id: &[u8; 32]) -> String {
    format!("Pool:{}", component_id(dex_type, dex_id))
}

#[cfg(test)]
mod tests {
    use super::{component_id, pool_store_key};
    use substreams::scalar::BigInt;

    #[test]
    fn component_id_prefixes_dex_id_with_dex_type() {
        let dex_id = [0x11u8; 32];

        let id = component_id(&BigInt::from(4), &dex_id);

        assert_eq!(id, format!("0x04{}", "11".repeat(32)));
    }

    #[test]
    fn component_id_distinguishes_same_dex_id_across_dex_types() {
        let dex_id = [0xabu8; 32];

        let d3_id = component_id(&BigInt::from(3), &dex_id);
        let d4_id = component_id(&BigInt::from(4), &dex_id);

        assert_ne!(d3_id, d4_id);
        assert!(d3_id.ends_with(&"ab".repeat(32)));
        assert!(d4_id.ends_with(&"ab".repeat(32)));
        assert_eq!(pool_store_key(&BigInt::from(3), &dex_id), format!("Pool:{d3_id}"));
    }
}
