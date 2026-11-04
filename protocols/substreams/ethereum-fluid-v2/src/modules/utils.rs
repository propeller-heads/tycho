use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_helper::hex::Hexable;

/// Fluid's native token sentinel. Tycho represents the native token as the zero address.
pub const FLUID_NATIVE_TOKEN: [u8; 20] = [0xee; 20];

/// D3 (smart collateral) and D4 (smart debt), the only dex types live on DEX v2.
pub const SUPPORTED_DEX_TYPES: [u64; 2] = [3, 4];

#[derive(Debug, Deserialize)]
pub struct Params {
    pub dex_v2_address: String,
    /// The implementation behind the DEX v2 proxy that this package and the simulation mirror.
    pub dex_v2_implementation: String,
    pub resolver_address: String,
    pub liquidity_address: String,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }

    pub fn dex_v2_address(&self) -> Result<Vec<u8>> {
        decode_address("dex_v2_address", &self.dex_v2_address)
    }

    pub fn dex_v2_implementation(&self) -> Result<Vec<u8>> {
        decode_address("dex_v2_implementation", &self.dex_v2_implementation)
    }

    pub fn resolver_address(&self) -> Result<Vec<u8>> {
        decode_address("resolver_address", &self.resolver_address)
    }

    pub fn liquidity_address(&self) -> Result<Vec<u8>> {
        decode_address("liquidity_address", &self.liquidity_address)
    }
}

fn decode_address(name: &str, value: &str) -> Result<Vec<u8>> {
    let address = hex::decode(value.trim_start_matches("0x"))
        .map_err(|e| anyhow!("Invalid {name} '{value}': {e}"))?;
    if address.len() != 20 {
        return Err(anyhow!("Invalid {name} '{value}': expected 20 bytes"));
    }
    Ok(address)
}

/// Maps a Fluid token address to the address Tycho uses for it.
pub fn to_tycho_token(token: &[u8]) -> Vec<u8> {
    if token == FLUID_NATIVE_TOKEN {
        vec![0u8; 20]
    } else {
        token.to_vec()
    }
}

/// Maps a Tycho token address to the address Fluid uses for it.
pub fn to_fluid_token(token: &[u8]) -> Vec<u8> {
    if token == [0u8; 20] {
        FLUID_NATIVE_TOKEN.to_vec()
    } else {
        token.to_vec()
    }
}

/// The component id: the dex type byte followed by the dexId. D3 and D4 pools of the same dex key
/// share a dexId, so the dex type keeps them apart.
pub fn component_id(dex_type: &BigInt, dex_id: &[u8; 32]) -> String {
    let mut bytes = Vec::with_capacity(33);
    bytes.push(dex_type.to_u64() as u8);
    bytes.extend_from_slice(dex_id);
    bytes.to_hex()
}

pub fn pool_store_key(dex_type: &BigInt, dex_id: &[u8; 32]) -> String {
    pool_store_key_for_component(&component_id(dex_type, dex_id))
}

pub fn pool_store_key_for_component(component_id: &str) -> String {
    format!("Pool:{component_id}")
}

#[cfg(test)]
mod tests {
    use super::{component_id, pool_store_key, to_fluid_token, to_tycho_token, FLUID_NATIVE_TOKEN};
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

    #[test]
    fn native_token_maps_between_fluid_and_tycho() {
        let usdc = hex::decode("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

        assert_eq!(to_tycho_token(&FLUID_NATIVE_TOKEN), vec![0u8; 20]);
        assert_eq!(to_fluid_token(&[0u8; 20]), FLUID_NATIVE_TOKEN.to_vec());
        assert_eq!(to_tycho_token(&usdc), usdc);
        assert_eq!(to_fluid_token(&usdc), usdc);
    }
}
