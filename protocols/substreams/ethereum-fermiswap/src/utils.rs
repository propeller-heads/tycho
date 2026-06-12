use serde::Deserialize;
use substreams::scalar::BigInt;
use tiny_keccak::{Hasher, Keccak};

use num_bigint::Sign;

pub const PAUSED_ATTRIBUTE: &str = "paused";
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

/// Computes the Fermi registry lane index for an ordered token pair.
///
/// This mirrors the on-chain calculation used by the registry and engine:
/// `keccak256(abi.encode(tokenA, tokenB))`. Solidity ABI encoding pads each address to
/// 32 bytes, so both 20-byte token addresses are left-padded before hashing.
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

/// Converts a decoded registry `laneIndex` uint256 into the store key used by `lane_index`.
///
/// The ABI decoder returns uint256 values as `BigInt`. Store keys use the canonical 32-byte
/// big-endian representation emitted by `keccak256`, prefixed with `0x`.
pub fn lane_index_store_key(value: &BigInt) -> Option<String> {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 32 {
        return None;
    }

    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    Some(format!("0x{}", hex::encode(padded)))
}
