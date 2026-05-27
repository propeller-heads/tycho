use std::collections::HashMap;

use lunarbase_pmm_math::U256;

use super::state::Address;

pub type AttributeMap = HashMap<String, Vec<u8>>;

pub mod attrs {
    pub const POOL: &str = "pool";
    pub const TOKEN_X: &str = "token_x";
    pub const TOKEN_Y: &str = "token_y";
    pub const ANCHOR_PRICE_X96: &str = "anchor_price_x96";
    pub const FEE_ASK_X24: &str = "fee_ask_x24";
    pub const FEE_BID_X24: &str = "fee_bid_x24";
    pub const LATEST_UPDATE_BLOCK: &str = "latest_update_block";
    pub const RESERVE_X: &str = "reserve_x";
    pub const RESERVE_Y: &str = "reserve_y";
    pub const CONCENTRATION_K: &str = "concentration_k";
    pub const BLOCK_DELAY: &str = "block_delay";
    pub const PAUSED: &str = "paused";
    pub const BLACKLIST_FEE_MULTIPLIER: &str = "blacklist_fee_multiplier";
    pub const EXECUTOR_WHITELISTED: &str = "executor_whitelisted";
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeError {
    Missing(&'static str),
    InvalidLength { name: &'static str, expected: usize, actual: usize },
    IntegerOverflow(&'static str),
}

pub fn insert_address(attrs: &mut AttributeMap, name: &'static str, value: Address) {
    attrs.insert(name.to_owned(), value.to_vec());
}

pub fn insert_bool(attrs: &mut AttributeMap, name: &'static str, value: bool) {
    attrs.insert(name.to_owned(), vec![u8::from(value)]);
}

pub fn insert_u32(attrs: &mut AttributeMap, name: &'static str, value: u32) {
    attrs.insert(name.to_owned(), value.to_be_bytes().to_vec());
}

pub fn insert_u64(attrs: &mut AttributeMap, name: &'static str, value: u64) {
    attrs.insert(name.to_owned(), value.to_be_bytes().to_vec());
}

pub fn insert_u128(attrs: &mut AttributeMap, name: &'static str, value: u128) {
    attrs.insert(name.to_owned(), value.to_be_bytes().to_vec());
}

pub fn insert_u256(attrs: &mut AttributeMap, name: &'static str, value: U256) {
    let mut out = vec![0u8; 32];
    value
        .to_be_bytes_vec()
        .iter()
        .rev()
        .take(32)
        .enumerate()
        .for_each(|(idx, byte)| {
            out[31 - idx] = *byte;
        });
    attrs.insert(name.to_owned(), out);
}

pub fn require_address(
    attrs: &AttributeMap,
    name: &'static str,
) -> Result<Address, AttributeError> {
    let value = require(attrs, name)?;
    if value.len() != 20 {
        return Err(AttributeError::InvalidLength { name, expected: 20, actual: value.len() });
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(value);
    Ok(out)
}

pub fn require_bool(attrs: &AttributeMap, name: &'static str) -> Result<bool, AttributeError> {
    let value = require(attrs, name)?;
    if value.len() != 1 {
        return Err(AttributeError::InvalidLength { name, expected: 1, actual: value.len() });
    }
    Ok(value[0] != 0)
}

pub fn require_u32(attrs: &AttributeMap, name: &'static str) -> Result<u32, AttributeError> {
    let value = require(attrs, name)?;
    decode_u32(name, value)
}

pub fn require_u64(attrs: &AttributeMap, name: &'static str) -> Result<u64, AttributeError> {
    let value = require(attrs, name)?;
    decode_u64(name, value)
}

pub fn require_u128(attrs: &AttributeMap, name: &'static str) -> Result<u128, AttributeError> {
    let value = require(attrs, name)?;
    decode_u128(name, value)
}

pub fn require_u256(attrs: &AttributeMap, name: &'static str) -> Result<U256, AttributeError> {
    let value = require(attrs, name)?;
    if value.len() > 32 {
        return Err(AttributeError::InvalidLength { name, expected: 32, actual: value.len() });
    }
    Ok(U256::from_be_slice(value))
}

fn require<'a>(attrs: &'a AttributeMap, name: &'static str) -> Result<&'a [u8], AttributeError> {
    attrs
        .get(name)
        .map(Vec::as_slice)
        .ok_or(AttributeError::Missing(name))
}

fn decode_u32(name: &'static str, value: &[u8]) -> Result<u32, AttributeError> {
    if value.len() > 4 {
        return Err(AttributeError::IntegerOverflow(name));
    }
    let mut out = [0u8; 4];
    out[4 - value.len()..].copy_from_slice(value);
    Ok(u32::from_be_bytes(out))
}

fn decode_u64(name: &'static str, value: &[u8]) -> Result<u64, AttributeError> {
    if value.len() > 8 {
        return Err(AttributeError::IntegerOverflow(name));
    }
    let mut out = [0u8; 8];
    out[8 - value.len()..].copy_from_slice(value);
    Ok(u64::from_be_bytes(out))
}

fn decode_u128(name: &'static str, value: &[u8]) -> Result<u128, AttributeError> {
    if value.len() > 16 {
        return Err(AttributeError::IntegerOverflow(name));
    }
    let mut out = [0u8; 16];
    out[16 - value.len()..].copy_from_slice(value);
    Ok(u128::from_be_bytes(out))
}
