//! The Ascend launchpad hook on Elysium (`AscendLaunchpadV5`): the hook every Ascend launch pool
//! is created with. It charges a constant fee in the pool's quote asset on every swap, keeps an
//! exponential moving average of the price, and fills part of a sell from a bid its pool holds
//! (quote the hook owns) at a discount to that average.
//!
//! Ascend pools are `uniswap_v4_hooks` components. The Elysium manifest of the with-hooks
//! Substreams package (`elysium-uniswap-v4-with-hooks.yaml`) decodes the hook's `PoolData` and
//! `Bid` storage writes into the `ascend_*` state attributes [`AscendPoolState`] reads. The hook
//! has no verified source: the model follows the bytecode and the traces recorded in the
//! integration spec, and `math.rs` documents each rule next to the spec fact it ports.
pub mod hook_handler;
pub mod hook_handler_creator;
mod math;

use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_common::Bytes;

use crate::protocol::errors::InvalidSnapshotError;

/// `PoolData.fee`: the swap fee in pips (1e6 = 100%), constant for the life of the pool.
pub const FEE_PIPS_ATTRIBUTE: &str = "ascend_fee_pips";
/// `PoolData.tokenIs0`: whether the launched token is `currency0`.
pub const TOKEN_IS_CURRENCY0_ATTRIBUTE: &str = "ascend_token_is_currency0";
/// `PoolData.startTick`: the tick the launch position's curve starts at.
pub const START_TICK_ATTRIBUTE: &str = "ascend_start_tick";
/// `PoolData.emaSqrtPriceX96`: the moving average of the pool's square-root price.
pub const EMA_SQRT_PRICE_ATTRIBUTE: &str = "ascend_ema_sqrt_price_x96";
/// `PoolData.emaUpdatedAt`: when, in unix seconds, the observation last ran.
pub const EMA_UPDATED_AT_ATTRIBUTE: &str = "ascend_ema_updated_at";
/// `PoolData.observedTick`: the pool's tick at the last observation.
pub const OBSERVED_TICK_ATTRIBUTE: &str = "ascend_observed_tick";
/// `PoolData.discount`: the bid's discount `d`, out of 2^32.
pub const DISCOUNT_ATTRIBUTE: &str = "ascend_discount";
/// `Bid.inventory`: the quote the hook holds to fill this pool's sells.
pub const BID_INVENTORY_ATTRIBUTE: &str = "ascend_bid_inventory";
/// `Bid.filledInWindow`: the quote filled since the window started.
pub const BID_FILLED_IN_WINDOW_ATTRIBUTE: &str = "ascend_bid_filled_in_window";
/// `Bid.windowStart`: when, in unix seconds, the current fill window started.
pub const BID_WINDOW_START_ATTRIBUTE: &str = "ascend_bid_window_start";
/// `Bid.maxClip`: the most quote one window fills.
pub const BID_MAX_CLIP_ATTRIBUTE: &str = "ascend_bid_max_clip";

/// Every `ascend_*` attribute, all of which [`AscendPoolState::from_attributes`] requires and
/// none of which the indexer deletes.
pub(super) const POOL_ATTRIBUTES: [&str; 11] = [
    FEE_PIPS_ATTRIBUTE,
    TOKEN_IS_CURRENCY0_ATTRIBUTE,
    START_TICK_ATTRIBUTE,
    EMA_SQRT_PRICE_ATTRIBUTE,
    EMA_UPDATED_AT_ATTRIBUTE,
    OBSERVED_TICK_ATTRIBUTE,
    DISCOUNT_ATTRIBUTE,
    BID_INVENTORY_ATTRIBUTE,
    BID_FILLED_IN_WINDOW_ATTRIBUTE,
    BID_WINDOW_START_ATTRIBUTE,
    BID_MAX_CLIP_ATTRIBUTE,
];

/// The smallest and largest `int24`: a stored tick is one.
const INT24_MIN: i32 = -(1 << 23);
const INT24_MAX: i32 = (1 << 23) - 1;
/// 2^32, one past the largest `uint32` the hook stores `d` in.
const UINT32_LIMIT: u64 = 1 << 32;
/// 1e6 pips, 100%.
const PIPS: u32 = 1_000_000;

/// What the hook keeps for one pool's price observation (`PoolData` word 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Observation {
    /// `emaSqrtPriceX96`.
    pub(super) ema: U256,
    /// `emaUpdatedAt`, unix seconds.
    pub(super) updated_at: u64,
    /// `observedTick`.
    pub(super) tick: i32,
    /// `discount`, a `uint32`.
    pub(super) discount: u64,
}

/// What the hook keeps for one pool's bid (`Bid` words 0 to 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Bid {
    /// `inventory`, quote raw units.
    pub(super) inventory: U256,
    /// `filledInWindow`, quote raw units.
    pub(super) filled: U256,
    /// `windowStart`, unix seconds.
    pub(super) window_start: u64,
    /// `maxClip`.
    pub(super) max_clip: u128,
}

/// Everything a quote of one Ascend pool depends on, as the indexer publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AscendPoolState {
    /// The swap fee in pips, below 1e6.
    pub(super) fee_pips: u32,
    pub(super) token_is_currency0: bool,
    pub(super) start_tick: i32,
    pub(super) observation: Observation,
    pub(super) bid: Bid,
}

impl AscendPoolState {
    /// Reads every `ascend_*` attribute out of `attributes`.
    ///
    /// Fails with `MissingAttribute` naming the first absent attribute, and with `ValueError` for
    /// a value of the wrong width or one the hook could not have stored: a fee of 1e6 pips or
    /// more, an orientation other than 0 or 1, a tick outside `int24`, a discount outside
    /// `uint32`.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        Self::decode(attributes, None)
    }

    /// This state with the `ascend_*` attributes of `updated` applied; other names are ignored.
    ///
    /// Fails as [`Self::from_attributes`] does for a value it cannot decode, and then leaves
    /// `self` as it was.
    pub fn with_updates(
        &self,
        updated: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        Self::decode(updated, Some(self))
    }

    /// Decodes each attribute of `attributes` in the format the indexer writes it, taking an
    /// absent one from `base`, or failing with `MissingAttribute` without a `base`.
    fn decode(
        attributes: &HashMap<String, Bytes>,
        base: Option<&Self>,
    ) -> Result<Self, InvalidSnapshotError> {
        fn field<T>(
            attributes: &HashMap<String, Bytes>,
            name: &str,
            decode: impl FnOnce(&str, &Bytes) -> Result<T, InvalidSnapshotError>,
            base: Option<T>,
        ) -> Result<T, InvalidSnapshotError> {
            match (attributes.get(name), base) {
                (Some(value), _) => decode(name, value),
                (None, Some(base)) => Ok(base),
                (None, None) => Err(InvalidSnapshotError::MissingAttribute(name.to_string())),
            }
        }
        let obs = base.map(|b| b.observation);
        let bid = base.map(|b| b.bid);
        Ok(Self {
            fee_pips: field(attributes, FEE_PIPS_ATTRIBUTE, pips, base.map(|b| b.fee_pips))?,
            token_is_currency0: field(
                attributes,
                TOKEN_IS_CURRENCY0_ATTRIBUTE,
                flag,
                base.map(|b| b.token_is_currency0),
            )?,
            start_tick: field(attributes, START_TICK_ATTRIBUTE, tick, base.map(|b| b.start_tick))?,
            observation: Observation {
                ema: field(attributes, EMA_SQRT_PRICE_ATTRIBUTE, uint160, obs.map(|o| o.ema))?,
                updated_at: field(
                    attributes,
                    EMA_UPDATED_AT_ATTRIBUTE,
                    u64_be,
                    obs.map(|o| o.updated_at),
                )?,
                tick: field(attributes, OBSERVED_TICK_ATTRIBUTE, tick, obs.map(|o| o.tick))?,
                discount: field(attributes, DISCOUNT_ATTRIBUTE, discount, obs.map(|o| o.discount))?,
            },
            bid: Bid {
                inventory: field(
                    attributes,
                    BID_INVENTORY_ATTRIBUTE,
                    uint256,
                    bid.map(|b| b.inventory),
                )?,
                filled: field(
                    attributes,
                    BID_FILLED_IN_WINDOW_ATTRIBUTE,
                    uint256,
                    bid.map(|b| b.filled),
                )?,
                window_start: field(
                    attributes,
                    BID_WINDOW_START_ATTRIBUTE,
                    u64_be,
                    bid.map(|b| b.window_start),
                )?,
                max_clip: field(
                    attributes,
                    BID_MAX_CLIP_ATTRIBUTE,
                    uint128,
                    bid.map(|b| b.max_clip),
                )?,
            },
        })
    }
}

fn wrong_width(name: &str, value: &Bytes, what: &str) -> InvalidSnapshotError {
    InvalidSnapshotError::ValueError(format!(
        "{name} is {} bytes ({value}), not {what}",
        value.len()
    ))
}

/// An 8-byte big-endian `u64`.
fn u64_be(name: &str, value: &Bytes) -> Result<u64, InvalidSnapshotError> {
    let bytes = <[u8; 8]>::try_from(value.as_ref())
        .map_err(|_wrong_width| wrong_width(name, value, "an 8-byte big-endian u64"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// An 8-byte big-endian fee in pips, which the hook stores as a `uint24` and never sets to 100%
/// or more.
fn pips(name: &str, value: &Bytes) -> Result<u32, InvalidSnapshotError> {
    let pips = u64_be(name, value)?;
    u32::try_from(pips)
        .ok()
        .filter(|pips| *pips < PIPS)
        .ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!("{name} = {pips} is not a fee below 1e6 pips"))
        })
}

/// An 8-byte big-endian `uint32` the hook stores `d` in.
fn discount(name: &str, value: &Bytes) -> Result<u64, InvalidSnapshotError> {
    let discount = u64_be(name, value)?;
    if discount >= UINT32_LIMIT {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} = {discount} does not fit a uint32"
        )));
    }
    Ok(discount)
}

/// A one-byte boolean, `0` or `1`.
fn flag(name: &str, value: &Bytes) -> Result<bool, InvalidSnapshotError> {
    match value.as_ref() {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(InvalidSnapshotError::ValueError(format!(
            "{name} is {value}, not a one-byte 0 or 1"
        ))),
    }
}

/// A 4-byte big-endian two's-complement `int24`, sign-extended to 32 bits.
fn tick(name: &str, value: &Bytes) -> Result<i32, InvalidSnapshotError> {
    let bytes = <[u8; 4]>::try_from(value.as_ref())
        .map_err(|_wrong_width| wrong_width(name, value, "a 4-byte big-endian i32"))?;
    let tick = i32::from_be_bytes(bytes);
    if !(INT24_MIN..=INT24_MAX).contains(&tick) {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} = {tick} does not fit an int24"
        )));
    }
    Ok(tick)
}

/// A 20-byte big-endian `uint160`.
fn uint160(name: &str, value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    if value.len() != 20 {
        return Err(wrong_width(name, value, "a 20-byte big-endian uint160"));
    }
    Ok(U256::from_be_slice(value))
}

/// A 16-byte big-endian `uint128`.
fn uint128(name: &str, value: &Bytes) -> Result<u128, InvalidSnapshotError> {
    let bytes = <[u8; 16]>::try_from(value.as_ref())
        .map_err(|_wrong_width| wrong_width(name, value, "a 16-byte big-endian uint128"))?;
    Ok(u128::from_be_bytes(bytes))
}

/// A 32-byte big-endian `uint256`.
fn uint256(name: &str, value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    if value.len() != 32 {
        return Err(wrong_width(name, value, "a 32-byte big-endian uint256"));
    }
    Ok(U256::from_be_slice(value))
}

#[cfg(test)]
mod tests;
