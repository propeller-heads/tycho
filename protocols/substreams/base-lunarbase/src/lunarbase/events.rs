use ethabi::ethereum_types::U256;

use crate::lunarbase::{
    attributes::{
        attrs, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64, AttributeMap,
    },
    decoder::StateDelta,
    Address,
};

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

    Ok(StateDelta { updated_attributes })
}

fn uint24_max() -> u32 {
    (1u32 << 24) - 1
}
