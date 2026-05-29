use ethabi::ethereum_types::U256;
use substreams_ethereum::pb::eth;

use crate::{
    abi::pool::events as pool_events,
    lunarbase::{events::LunarBaseEvent, Address},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogDecodeError {
    Decode { event: &'static str, message: String },
    InvalidAddress { event: &'static str, field: &'static str, len: usize },
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
        let event = pool_events::WhitelistSet::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "WhitelistSet", message })?;
        return Ok(Some(LunarBaseEvent::WhitelistSet {
            account: vec_to_address("WhitelistSet", "account", &event.account)?,
            whitelisted: event.whitelisted,
        }));
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

fn vec_to_address(
    event: &'static str,
    field: &'static str,
    value: &[u8],
) -> Result<Address, LogDecodeError> {
    value
        .try_into()
        .map_err(|_| LogDecodeError::InvalidAddress { event, field, len: value.len() })
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
