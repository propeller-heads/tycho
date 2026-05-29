use std::collections::HashMap;

use ethabi::ethereum_types::U256;

pub type AttributeMap = HashMap<String, Vec<u8>>;

pub mod attrs {
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
    value.to_big_endian(out.as_mut_slice());
    attrs.insert(name.to_owned(), out);
}
