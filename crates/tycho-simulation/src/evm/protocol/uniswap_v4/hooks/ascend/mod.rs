//! The Ascend launchpad hook on Elysium (`AscendLaunchpadV5`): the hook every Ascend launch pool
//! is created with. It charges a constant fee in the pool's quote asset on every swap, keeps an
//! exponential moving average of the price, and fills part of a sell from a bid its pool holds
//! (quote the hook owns) at a discount to that average.
//!
//! Ascend pools are `uniswap_v4_hooks` components. The Elysium manifest of the with-hooks
//! Substreams package (`elysium-uniswap-v4-with-hooks.yaml`) decodes the hook's `PoolData` and
//! `Bid` storage writes into the `ascend_*` state attributes [`AscendPoolState`] reads. The hook
//! has no verified source: the model follows its bytecode and traces recorded on Elysium, and
//! ports the hook's arithmetic and its math library's.
pub mod hook_handler;
pub mod hook_handler_creator;
mod math;

use std::collections::HashMap;

use alloy::primitives::U256;
use math::Curve;
use tycho_common::Bytes;

use super::launchpad::{field, fixed, flag, tick_i32, u32_be, u64_be};
use crate::{
    evm::protocol::utils::uniswap::FEE_PIPS_DENOMINATOR, protocol::errors::InvalidSnapshotError,
};

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
const POOL_ATTRIBUTES: [&str; 11] = [
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

/// The smallest `int24`: a stored tick is at least this.
const INT24_MIN: i32 = -(1 << 23);
/// The largest `int24`: a stored tick is at most this.
const INT24_MAX: i32 = (1 << 23) - 1;

/// What the hook keeps for one pool's price observation (`PoolData` word 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observation {
    /// `emaSqrtPriceX96`.
    ema: U256,
    /// `emaUpdatedAt`, unix seconds.
    updated_at: u64,
    /// `observedTick`.
    tick: i32,
    /// `discount`, a `uint32`.
    discount: u32,
}

impl Observation {
    /// Whether the observation is later than the block timestamp `now`. The hook's subtraction
    /// of the two reverts then, so every swap refuses: a delta can carry `updated_at` ahead of
    /// the block the handler is at.
    fn is_ahead_of(&self, now: u64) -> bool {
        now < self.updated_at
    }
}

/// What the hook keeps for one pool's bid (`Bid` words 0 to 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bid {
    /// `inventory`, quote raw units.
    inventory: U256,
    /// `filledInWindow`, quote raw units.
    filled: U256,
    /// `windowStart`, unix seconds.
    window_start: u64,
    /// `maxClip`.
    max_clip: u128,
}

/// Everything a quote of one Ascend pool depends on, as the indexer publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AscendPoolState {
    /// The swap fee in pips, below 1e6.
    fee_pips: u32,
    observation: Observation,
    bid: Bid,
    /// The launch curve, which carries the orientation and the start tick.
    curve: Curve,
}

impl AscendPoolState {
    /// A state whose parts the hook could have stored: the fee below 100%, an EMA above 0 and a
    /// start tick whose launch curve exists.
    fn try_new(
        fee_pips: u32,
        token_is_currency0: bool,
        start_tick: i32,
        observation: Observation,
        bid: Bid,
    ) -> Result<Self, InvalidSnapshotError> {
        let curve = Curve::try_new(token_is_currency0, start_tick).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "{START_TICK_ATTRIBUTE} = {start_tick} has no launch curve: {e}"
            ))
        })?;
        Self::with_curve(fee_pips, observation, bid, curve)
    }

    /// A state on `curve`, with the same checks as [`Self::try_new`] on the other parts.
    fn with_curve(
        fee_pips: u32,
        observation: Observation,
        bid: Bid,
        curve: Curve,
    ) -> Result<Self, InvalidSnapshotError> {
        if fee_pips >= FEE_PIPS_DENOMINATOR {
            return Err(InvalidSnapshotError::ValueError(format!(
                "{FEE_PIPS_ATTRIBUTE} = {fee_pips} is not a fee below 1e6 pips"
            )));
        }
        if observation.ema.is_zero() {
            return Err(InvalidSnapshotError::ValueError(format!(
                "{EMA_SQRT_PRICE_ATTRIBUTE} is 0, which no observation leaves"
            )));
        }
        Ok(Self { fee_pips, observation, bid, curve })
    }

    /// Whether the launched token is `currency0`.
    fn token_is_currency0(&self) -> bool {
        self.curve.token_is_currency0
    }

    /// Reads every `ascend_*` attribute out of `attributes`.
    ///
    /// Fails with `MissingAttribute` naming the first absent attribute, and with `ValueError` for
    /// a value of the wrong width or one the hook could not have stored: a fee of 1e6 pips or
    /// more, an orientation other than 0 or 1, a tick outside `int24` or one with no launch
    /// curve, a discount outside `uint32`, an EMA of 0.
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
        let obs = base.map(|b| b.observation);
        let bid = base.map(|b| b.bid);
        let fee_pips = field(attributes, FEE_PIPS_ATTRIBUTE, pips, base.map(|b| b.fee_pips))?;
        let token_is_currency0 = field(
            attributes,
            TOKEN_IS_CURRENCY0_ATTRIBUTE,
            flag,
            base.map(|b| b.token_is_currency0()),
        )?;
        let start_tick =
            field(attributes, START_TICK_ATTRIBUTE, tick, base.map(|b| b.curve.start_tick))?;
        let observation = Observation {
            ema: field(attributes, EMA_SQRT_PRICE_ATTRIBUTE, uint160, obs.map(|o| o.ema))?,
            updated_at: field(
                attributes,
                EMA_UPDATED_AT_ATTRIBUTE,
                u64_be,
                obs.map(|o| o.updated_at),
            )?,
            tick: field(attributes, OBSERVED_TICK_ATTRIBUTE, tick, obs.map(|o| o.tick))?,
            discount: field(attributes, DISCOUNT_ATTRIBUTE, u32_be, obs.map(|o| o.discount))?,
        };
        let bid = Bid {
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
            max_clip: field(attributes, BID_MAX_CLIP_ATTRIBUTE, uint128, bid.map(|b| b.max_clip))?,
        };
        // The curve depends on the orientation and the start tick alone: a delta that leaves both
        // as they were keeps the base's, whose construction is three tick-to-price conversions.
        match base {
            Some(base)
                if base.token_is_currency0() == token_is_currency0 &&
                    base.curve.start_tick == start_tick =>
            {
                Self::with_curve(fee_pips, observation, bid, base.curve)
            }
            _ => Self::try_new(fee_pips, token_is_currency0, start_tick, observation, bid),
        }
    }
}

/// A fee in pips, which the hook stores as a `uint24`, in the indexer's `u64_be` format.
/// [`AscendPoolState::try_new`] checks that it is below 100%; the hook's own bounds on a fee
/// are not known (its source is unverified), so any fee below that decodes, 0 included.
fn pips(name: &str, value: &Bytes) -> Result<u32, InvalidSnapshotError> {
    u32_be(name, value)
}

/// A 4-byte big-endian two's-complement `int24`, sign-extended to 32 bits, refused outside the
/// `int24` range.
fn tick(name: &str, value: &Bytes) -> Result<i32, InvalidSnapshotError> {
    let tick = tick_i32(name, value)?;
    if !(INT24_MIN..=INT24_MAX).contains(&tick) {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} = {tick} does not fit an int24"
        )));
    }
    Ok(tick)
}

/// A 20-byte big-endian `uint160`.
fn uint160(name: &str, value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    Ok(U256::from_be_slice(&fixed::<20>(name, value, "big-endian uint160")?))
}

/// A 16-byte big-endian `uint128`.
fn uint128(name: &str, value: &Bytes) -> Result<u128, InvalidSnapshotError> {
    Ok(u128::from_be_bytes(fixed(name, value, "big-endian uint128")?))
}

/// A 32-byte big-endian `uint256`.
fn uint256(name: &str, value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    Ok(U256::from_be_bytes(fixed::<32>(name, value, "big-endian uint256")?))
}

#[cfg(test)]
mod tests;
