use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    abi::pool::events as pool_events,
    lunarbase::state::{attribute, attrs},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LunarBaseEvent {
    StateUpdated { anchor_price_x96: u128, fee_ask_x24: u32, fee_bid_x24: u32 },
    Sync { reserve_x: u128, reserve_y: u128 },
    BlockDelaySet { block_delay: u64 },
    ConcentrationKSet { concentration_k: u32 },
    Paused,
    Unpaused,
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

    if pool_events::Paused::match_log(log) {
        return Ok(Some(LunarBaseEvent::Paused));
    }

    if pool_events::Unpaused::match_log(log) {
        return Ok(Some(LunarBaseEvent::Unpaused));
    }

    Ok(None)
}

pub fn event_attributes(event: &LunarBaseEvent, block_number: u64) -> Vec<tycho::Attribute> {
    match event {
        LunarBaseEvent::StateUpdated { anchor_price_x96, fee_ask_x24, fee_bid_x24 } => vec![
            attribute(attrs::ANCHOR_PRICE_X96, anchor_price_x96.to_be_bytes().to_vec()),
            attribute(attrs::FEE_ASK_X24, fee_ask_x24.to_be_bytes().to_vec()),
            attribute(attrs::FEE_BID_X24, fee_bid_x24.to_be_bytes().to_vec()),
            attribute(attrs::LATEST_UPDATE_BLOCK, block_number.to_be_bytes().to_vec()),
        ],
        LunarBaseEvent::Sync { reserve_x, reserve_y } => vec![
            attribute(attrs::RESERVE_X, reserve_x.to_be_bytes().to_vec()),
            attribute(attrs::RESERVE_Y, reserve_y.to_be_bytes().to_vec()),
        ],
        LunarBaseEvent::BlockDelaySet { block_delay } => {
            vec![attribute(attrs::BLOCK_DELAY, block_delay.to_be_bytes().to_vec())]
        }
        LunarBaseEvent::ConcentrationKSet { concentration_k } => {
            vec![attribute(attrs::CONCENTRATION_K, concentration_k.to_be_bytes().to_vec())]
        }
        LunarBaseEvent::Paused => vec![attribute(attrs::PAUSED, vec![1u8])],
        LunarBaseEvent::Unpaused => vec![attribute(attrs::PAUSED, vec![0u8])],
    }
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
