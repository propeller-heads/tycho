use std::collections::HashSet;

use ethabi::ethereum_types::U256;

use crate::lunarbase::{
    attributes::{
        attrs, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64, AttributeMap,
    },
    decoder::StateDelta,
    Address,
};

pub mod topics {
    pub const SWAP_EXECUTED: &str =
        "0x1b43ddf90e971181a7faf41549e512675072e84befadbba7873086509dec1fdc";
    pub const STATE_UPDATED: &str =
        "0x8acb811d2c5106785f847faf03ce160d2eb124b8632eb42d466f46c087033d61";
    pub const BLOCK_DELAY_SET: &str =
        "0x673f9280467ef1d677edd6a21630cf328068a1dc8da64205c1bc79855c6b2307";
    pub const CONCENTRATION_K_SET: &str =
        "0xcf34ec77e4a73dc1b2fdbb6eaec360819374b6412a8bf8096f91c4fdb76db3a8";
    pub const WHITELIST_SET: &str =
        "0x0aa5ec5ffdc7f6f9c4d0dded489d7450297155cb2f71cb771e02427f7dff4f51";
    pub const BLACKLIST_FEE_MULTIPLIER_SET: &str =
        "0xa15057886e6ebcdf47294bcb091d686031124d1041cafe00740e93667bacd186";
    pub const SYNC: &str = "0x99e93fd94a51b80d7dd7ec3f69c4f09a43e7523f5a45ca09b88a178d9daaed1e";
    pub const PAUSED: &str = "0x62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258";
    pub const UNPAUSED: &str = "0x5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventApplyContext {
    pub block_number: u64,
    pub tycho_executor: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LunarBaseEvent {
    StateUpdated { anchor_price_x96: u128, fee_ask_x24: u32, fee_bid_x24: u32 },
    Sync { reserve_x: u128, reserve_y: u128 },
    BlockDelaySet { block_delay: u64 },
    ConcentrationKSet { concentration_k: u32 },
    WhitelistSet { account: Address, whitelisted: bool },
    BlacklistFeeMultiplierSet { multiplier: U256 },
    Paused,
    Unpaused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventApplyError {
    FeeOverflow,
}

pub fn event_to_delta(
    event: &LunarBaseEvent,
    context: EventApplyContext,
) -> Result<StateDelta, EventApplyError> {
    let mut updated_attributes = AttributeMap::new();

    match event {
        LunarBaseEvent::StateUpdated { anchor_price_x96, fee_ask_x24, fee_bid_x24 } => {
            if *fee_ask_x24 > uint24_max() || *fee_bid_x24 > uint24_max() {
                return Err(EventApplyError::FeeOverflow);
            }
            insert_u128(&mut updated_attributes, attrs::ANCHOR_PRICE_X96, *anchor_price_x96);
            insert_u32(&mut updated_attributes, attrs::FEE_ASK_X24, *fee_ask_x24);
            insert_u32(&mut updated_attributes, attrs::FEE_BID_X24, *fee_bid_x24);
            insert_u64(&mut updated_attributes, attrs::LATEST_UPDATE_BLOCK, context.block_number);
        }
        LunarBaseEvent::Sync { reserve_x, reserve_y } => {
            insert_u128(&mut updated_attributes, attrs::RESERVE_X, *reserve_x);
            insert_u128(&mut updated_attributes, attrs::RESERVE_Y, *reserve_y);
        }
        LunarBaseEvent::BlockDelaySet { block_delay } => {
            insert_u64(&mut updated_attributes, attrs::BLOCK_DELAY, *block_delay);
        }
        LunarBaseEvent::ConcentrationKSet { concentration_k } => {
            insert_u32(&mut updated_attributes, attrs::CONCENTRATION_K, *concentration_k);
        }
        LunarBaseEvent::WhitelistSet { account, whitelisted } => {
            if *account == context.tycho_executor {
                insert_bool(&mut updated_attributes, attrs::EXECUTOR_WHITELISTED, *whitelisted);
            }
        }
        LunarBaseEvent::BlacklistFeeMultiplierSet { multiplier } => {
            insert_u256(&mut updated_attributes, attrs::BLACKLIST_FEE_MULTIPLIER, *multiplier);
        }
        LunarBaseEvent::Paused => {
            insert_bool(&mut updated_attributes, attrs::PAUSED, true);
        }
        LunarBaseEvent::Unpaused => {
            insert_bool(&mut updated_attributes, attrs::PAUSED, false);
        }
    }

    Ok(StateDelta { updated_attributes, deleted_attributes: HashSet::new() })
}

fn uint24_max() -> u32 {
    (1u32 << 24) - 1
}
