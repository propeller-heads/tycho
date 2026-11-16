use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams_helper::hex::Hexable;

use crate::storage::utils::{calc_map_slot, left_pad};

/// Key for the fee the factory charges on a tick spacing.
pub fn tick_spacing_fee_key(factory: &[u8], tick_spacing: i32) -> String {
    format!("{}:tick_spacing_{tick_spacing}", factory.to_hex())
}

#[derive(Debug, Deserialize)]
struct RawParams {
    factory: String,
    hook: String,
}

/// Contract addresses of one chain's AERO CL deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    pub factory: Vec<u8>,
    /// The `DynamicSwapFeeHook` whose fee state is indexed.
    pub hook: Vec<u8>,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        let raw: RawParams = serde_qs::from_str(input)
            .map_err(|err| anyhow!("Failed to parse query params `{input}`: {err}"))?;
        Ok(Self {
            factory: parse_address("factory", &raw.factory)?,
            hook: parse_address("hook", &raw.hook)?,
        })
    }
}

fn parse_address(name: &str, value: &str) -> Result<Vec<u8>> {
    let bytes = hex::decode(value.trim_start_matches("0x"))
        .map_err(|err| anyhow!("param `{name}` is not hex: {err}"))?;
    if bytes.len() != 20 {
        return Err(anyhow!("param `{name}` must be a 20-byte address, got {} bytes", bytes.len()));
    }
    Ok(bytes)
}

/// Storage slot numbers of the AERO v1.1 `CLFactory` and `DynamicSwapFeeHook`.
pub mod slots {
    pub const FACTORY_DEFAULT_SWAP_HOOK: u8 = 9;
    pub const FACTORY_POOL_SWAP_HOOK: u8 = 10;
    pub const HOOK_DEFAULT_SCALING_FACTOR: u8 = 0;
    pub const HOOK_DEFAULT_FEE_CAP: u8 = 1;
    /// `secondsAgo` (u32) in the low bytes, `recordCallGasLimit` above it.
    pub const HOOK_SECONDS_AGO: u8 = 2;
    pub const HOOK_DYNAMIC_FEE_CONFIG: u8 = 3;
    pub const HOOK_BLOCK_FEE: u8 = 4;
}

/// `slot` as a 32-byte storage key.
pub fn plain_slot(slot: u8) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[31] = slot;
    word
}

fn mapping_slot(key: &[u8], base: u8) -> [u8; 32] {
    calc_map_slot(&left_pad(key, 0), &plain_slot(base))
}

/// Hook slot of `dynamicFeeConfig[pool]`.
pub fn dynamic_fee_config_slot(pool: &[u8]) -> [u8; 32] {
    mapping_slot(pool, slots::HOOK_DYNAMIC_FEE_CONFIG)
}

/// Factory slot of `poolSwapHook[pool]`.
pub fn pool_swap_hook_slot(pool: &[u8]) -> [u8; 32] {
    mapping_slot(pool, slots::FACTORY_POOL_SWAP_HOOK)
}

/// Hook slot of `blockFee[pool][block_number]`.
pub fn block_fee_slot(pool: &[u8], block_number: u64) -> [u8; 32] {
    let inner = mapping_slot(pool, slots::HOOK_BLOCK_FEE);
    calc_map_slot(&left_pad(&block_number.to_be_bytes(), 0), &inner)
}

/// Bytes `[offset, offset + len)` counted from the low-order end of a 32-byte word, as Solidity
/// packs variables into a slot. Missing bytes of a short word read as zero.
pub fn packed_field(word: &[u8], offset: usize, len: usize) -> Vec<u8> {
    let word = left_pad(word, 0);
    word[32 - offset - len..32 - offset].to_vec()
}

/// Minimal big-endian encoding of an unsigned value, as Tycho attributes carry integers.
pub fn unsigned_attribute(bytes: &[u8]) -> Vec<u8> {
    substreams::scalar::BigInt::from_unsigned_bytes_be(bytes).to_signed_bytes_be()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_spacing_fee_keys_are_scoped_by_factory() {
        assert_eq!(
            tick_spacing_fee_key(&[0x11; 20], 200),
            "0x1111111111111111111111111111111111111111:tick_spacing_200"
        );
    }

    #[test]
    fn parses_params() {
        let params = Params::parse_from_query(
            "factory=0x9CB220581b91b0C83223012FE69688b92aD07a76\
             &hook=C0ed888AfE87ed1AcF01D9f19bB850159e3ce434",
        )
        .unwrap();

        assert_eq!(params.hook, hex::decode("C0ed888AfE87ed1AcF01D9f19bB850159e3ce434").unwrap());
        assert!(Params::parse_from_query("factory=0x12&hook=0x34").is_err());
        assert!(
            Params::parse_from_query("factory=0x9CB220581b91b0C83223012FE69688b92aD07a76").is_err()
        );
    }

    #[test]
    fn packed_fields_count_from_the_low_end() {
        let mut word = [0u8; 32];
        word[31] = 0x01;
        word[30] = 0x02;
        word[29] = 0x03;
        word[28] = 0x04;

        assert_eq!(packed_field(&word, 0, 2), vec![0x02, 0x01]);
        assert_eq!(packed_field(&word, 2, 2), vec![0x04, 0x03]);
        assert_eq!(packed_field(&[0x05], 0, 3), vec![0, 0, 0x05]);
    }
}
