use ethabi::ethereum_types::U256;
use substreams_ethereum::pb::eth;

use crate::{
    abi::pool::events as pool_events,
    lunarbase::state::{
        attrs, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64, AttributeMap,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventApplyContext {
    pub block_number: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub updated_attributes: AttributeMap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LunarBaseEvent {
    StateUpdated { anchor_price_x96: u128, fee_ask_x24: u32, fee_bid_x24: u32 },
    Sync { reserve_x: u128, reserve_y: u128 },
    BlockDelaySet { block_delay: u64 },
    ConcentrationKSet { concentration_k: u32 },
    BlacklistFeeMultiplierSet { multiplier: U256 },
    Paused,
    Unpaused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventApplyError {
    FeeOverflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogDecodeError {
    Decode { event: &'static str, message: String },
    IntegerOverflow(&'static str),
}

pub fn decode_lunarbase_state_log(
    log: &eth::v2::Log,
) -> Result<Option<LunarBaseEvent>, LogDecodeError> {
    if log.topics.is_empty() {
        return Ok(None);
    }

    if pool_events::StateUpdated::match_log(log) {
        let event = pool_events::StateUpdated::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "StateUpdated", message })?;
        return Ok(Some(LunarBaseEvent::StateUpdated {
            anchor_price_x96: bigint_to_u128("StateUpdated.anchorPrice", &event.anchor_price)?,
            fee_ask_x24: bigint_to_u32("StateUpdated.feeAskX24", &event.fee_ask_x24)?,
            fee_bid_x24: bigint_to_u32("StateUpdated.feeBidX24", &event.fee_bid_x24)?,
        }));
    }

    if pool_events::Sync::match_log(log) {
        let event = pool_events::Sync::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "Sync", message })?;
        return Ok(Some(LunarBaseEvent::Sync {
            reserve_x: bigint_to_u128("Sync.reserveX", &event.reserve_x)?,
            reserve_y: bigint_to_u128("Sync.reserveY", &event.reserve_y)?,
        }));
    }

    if pool_events::BlockDelaySet::match_log(log) {
        let event = pool_events::BlockDelaySet::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "BlockDelaySet", message })?;
        return Ok(Some(LunarBaseEvent::BlockDelaySet {
            block_delay: bigint_to_u64("BlockDelaySet.blockDelay", &event.block_delay)?,
        }));
    }

    if pool_events::ConcentrationKSet::match_log(log) {
        let event = pool_events::ConcentrationKSet::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "ConcentrationKSet", message })?;
        return Ok(Some(LunarBaseEvent::ConcentrationKSet {
            concentration_k: bigint_to_u32(
                "ConcentrationKSet.concentrationK",
                &event.concentration_k,
            )?,
        }));
    }

    if pool_events::WhitelistSet::match_log(log) {
        return Ok(None);
    }

    if pool_events::BlacklistFeeMultiplierSet::match_log(log) {
        let event = pool_events::BlacklistFeeMultiplierSet::decode(log).map_err(|message| {
            LogDecodeError::Decode { event: "BlacklistFeeMultiplierSet", message }
        })?;
        return Ok(Some(LunarBaseEvent::BlacklistFeeMultiplierSet {
            multiplier: bigint_to_u256("BlacklistFeeMultiplierSet.multiplier", &event.multiplier)?,
        }));
    }

    if pool_events::Paused::match_log(log) {
        return Ok(Some(LunarBaseEvent::Paused));
    }

    if pool_events::Unpaused::match_log(log) {
        return Ok(Some(LunarBaseEvent::Unpaused));
    }

    if pool_events::SwapExecuted::match_log(log) {
        return Ok(None);
    }

    Ok(None)
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

fn bigint_to_u32(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<u32, LogDecodeError> {
    value
        .to_string()
        .parse()
        .map_err(|_| LogDecodeError::IntegerOverflow(name))
}

fn bigint_to_u64(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<u64, LogDecodeError> {
    value
        .to_string()
        .parse()
        .map_err(|_| LogDecodeError::IntegerOverflow(name))
}

fn bigint_to_u128(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<u128, LogDecodeError> {
    value
        .to_string()
        .parse()
        .map_err(|_| LogDecodeError::IntegerOverflow(name))
}

fn bigint_to_u256(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<U256, LogDecodeError> {
    U256::from_dec_str(&value.to_string()).map_err(|_| LogDecodeError::IntegerOverflow(name))
}
