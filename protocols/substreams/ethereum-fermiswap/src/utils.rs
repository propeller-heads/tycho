use serde::Deserialize;
use tiny_keccak::{Hasher, Keccak};

pub const PAUSED_ATTRIBUTE: &str = "paused";
pub const OVERRIDE_BLOCK_NUMBER_ATTRIBUTE: &str = "override_block_number";
pub const OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE: &str = "override_block_timestamp";

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(with = "hex::serde")]
    pub engine_address: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub trader_vault: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub swapper_address: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub registry_address: Vec<u8>,
}

pub fn component_id(base_asset: &[u8], quote_asset: &[u8]) -> String {
    let mut input = Vec::with_capacity(40);
    input.extend_from_slice(base_asset);
    input.extend_from_slice(quote_asset);

    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(&input);
    hasher.finalize(&mut out);

    format!("0x{}", hex::encode(out))
}

pub fn lane_index(base_asset: &[u8], quote_asset: &[u8]) -> String {
    let mut input = Vec::with_capacity(64);
    input.extend_from_slice(&[0u8; 12]);
    input.extend_from_slice(base_asset);
    input.extend_from_slice(&[0u8; 12]);
    input.extend_from_slice(quote_asset);

    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(&input);
    hasher.finalize(&mut out);

    format!("0x{}", hex::encode(out))
}
