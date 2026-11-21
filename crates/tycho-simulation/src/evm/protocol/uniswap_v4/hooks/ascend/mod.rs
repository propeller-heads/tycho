//! The Ascend launchpad hook on Elysium (`AscendLaunchpadV5`): the hook every Ascend launch pool
//! is created with. It charges a constant fee in the pool's quote asset on every swap, keeps an
//! exponential moving average of the price, and fills part of a sell from a bid its pool holds
//! (quote the hook owns) at a discount to that average.
//!
//! Ascend pools are `uniswap_v4_hooks` components. The Elysium manifest of the with-hooks
//! Substreams package (`elysium-uniswap-v4-with-hooks.yaml`) decodes the hook's `PoolData` and
//! `Bid` storage writes into the `ascend_*` state attributes [`AscendPoolState`] reads. The hook
//! has no verified source: the model follows its bytecode and traces recorded on Elysium, and
//! `math.rs` ports the hook's arithmetic and its math library's.
pub mod hook_handler;
pub mod hook_handler_creator;
mod math;

use std::collections::HashMap;

use alloy::primitives::U256;
use math::Curve;
use tycho_common::Bytes;

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

/// The smallest and largest `int24`: a stored tick is one.
const INT24_MIN: i32 = -(1 << 23);
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
    start_tick: i32,
    observation: Observation,
    bid: Bid,
    /// The launch curve of `start_tick` and the orientation, which it also holds.
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
        Self::with_curve(fee_pips, start_tick, observation, bid, curve)
    }

    /// A state on `curve`, which must be the curve of `start_tick`, with the same checks as
    /// [`Self::try_new`] on the other parts.
    fn with_curve(
        fee_pips: u32,
        start_tick: i32,
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
        Ok(Self { fee_pips, start_tick, observation, bid, curve })
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
        let fee_pips = field(attributes, FEE_PIPS_ATTRIBUTE, pips, base.map(|b| b.fee_pips))?;
        let token_is_currency0 = field(
            attributes,
            TOKEN_IS_CURRENCY0_ATTRIBUTE,
            flag,
            base.map(|b| b.token_is_currency0()),
        )?;
        let start_tick = field(attributes, START_TICK_ATTRIBUTE, tick, base.map(|b| b.start_tick))?;
        let observation = Observation {
            ema: field(attributes, EMA_SQRT_PRICE_ATTRIBUTE, uint160, obs.map(|o| o.ema))?,
            updated_at: field(
                attributes,
                EMA_UPDATED_AT_ATTRIBUTE,
                uint64,
                obs.map(|o| o.updated_at),
            )?,
            tick: field(attributes, OBSERVED_TICK_ATTRIBUTE, tick, obs.map(|o| o.tick))?,
            discount: field(attributes, DISCOUNT_ATTRIBUTE, discount, obs.map(|o| o.discount))?,
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
                uint64,
                bid.map(|b| b.window_start),
            )?,
            max_clip: field(attributes, BID_MAX_CLIP_ATTRIBUTE, uint128, bid.map(|b| b.max_clip))?,
        };
        // The curve depends on the orientation and the start tick alone: a delta that leaves both
        // as they were keeps the base's, whose construction is three tick-to-price conversions.
        match base {
            Some(base)
                if base.token_is_currency0() == token_is_currency0 &&
                    base.start_tick == start_tick =>
            {
                Self::with_curve(fee_pips, start_tick, observation, bid, base.curve)
            }
            _ => Self::try_new(fee_pips, token_is_currency0, start_tick, observation, bid),
        }
    }
}

/// The `N` bytes of the attribute `name`, whose value must be exactly that wide; `what` names the
/// type the width stands for.
fn fixed<const N: usize>(
    name: &str,
    value: &Bytes,
    what: &str,
) -> Result<[u8; N], InvalidSnapshotError> {
    <[u8; N]>::try_from(value.as_ref()).map_err(|_wrong_width| {
        InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes ({value}), not the {N} bytes of {what}",
            value.len()
        ))
    })
}

/// An 8-byte big-endian `uint64`: the indexer's format for the timestamps, the fee and the
/// discount.
fn uint64(name: &str, value: &Bytes) -> Result<u64, InvalidSnapshotError> {
    Ok(u64::from_be_bytes(fixed(name, value, "a big-endian u64")?))
}

/// A fee in pips, which the hook stores as a `uint24`, in the indexer's `uint64` format.
/// [`AscendPoolState::with_curve`] checks that it is below 100%; the hook's own bounds on a fee
/// are not known (its source is unverified), so any fee below that decodes, 0 included.
fn pips(name: &str, value: &Bytes) -> Result<u32, InvalidSnapshotError> {
    let pips = uint64(name, value)?;
    u32::try_from(pips).map_err(|_overflow| {
        InvalidSnapshotError::ValueError(format!("{name} = {pips} does not fit a fee in pips"))
    })
}

/// The discount `d`: 8 big-endian bytes of a value below 2^32, which the hook stores as a
/// `uint32`.
fn discount(name: &str, value: &Bytes) -> Result<u32, InvalidSnapshotError> {
    let discount = uint64(name, value)?;
    u32::try_from(discount).map_err(|_overflow| {
        InvalidSnapshotError::ValueError(format!("{name} = {discount} does not fit a uint32"))
    })
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
    let tick = i32::from_be_bytes(fixed(name, value, "a big-endian i32")?);
    if !(INT24_MIN..=INT24_MAX).contains(&tick) {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} = {tick} does not fit an int24"
        )));
    }
    Ok(tick)
}

/// A 20-byte big-endian `uint160`.
fn uint160(name: &str, value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    Ok(U256::from_be_slice(&fixed::<20>(name, value, "a big-endian uint160")?))
}

/// A 16-byte big-endian `uint128`.
fn uint128(name: &str, value: &Bytes) -> Result<u128, InvalidSnapshotError> {
    Ok(u128::from_be_bytes(fixed(name, value, "a big-endian uint128")?))
}

/// A 32-byte big-endian `uint256`.
fn uint256(name: &str, value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    Ok(U256::from_be_bytes(fixed::<32>(name, value, "a big-endian uint256")?))
}

#[cfg(test)]
mod tests;
