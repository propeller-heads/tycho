//! Montra's `LaunchHook` on Elysium: the hook every Montra launch pool is created with, which
//! charges a fee in the pool's quote asset on every swap, an anti-snipe rate on early buys, and
//! switches the pool's LP fee on once the price has held past its LP line.
//!
//! Montra pools are `uniswap_v4_hooks` components. The Elysium manifest of the with-hooks
//! Substreams package (`elysium-uniswap-v4-with-hooks.yaml`) decodes the hook's `PoolConfig`
//! storage writes into the `montra_*` state attributes [`MontraPoolConfig`] reads, and its
//! `map_montra_enriched_block_changes` module decides when a pool is paused.
pub mod hook_handler;
pub mod hook_handler_creator;

use std::collections::HashMap;

use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::protocol::errors::InvalidSnapshotError;

/// `PoolConfig.totalFeePips`: the fee a sell pays, and a buy once its anti-snipe window is over.
pub const TOTAL_FEE_PIPS_ATTRIBUTE: &str = "montra_total_fee_pips";
/// `PoolConfig.lpFeePips`: the LP fee the pool charges once the LP fee is on.
pub const LP_FEE_PIPS_ATTRIBUTE: &str = "montra_lp_fee_pips";
/// `PoolConfig.quoteIsCurrency0`: whether the pool's quote asset, the one the hook charges, is
/// `currency0`.
pub const QUOTE_IS_CURRENCY0_ATTRIBUTE: &str = "montra_quote_is_currency0";
/// `PoolConfig.earlyBuyFeeUntil`: the end of the anti-snipe window in unix seconds, 0 for none.
pub const EARLY_BUY_FEE_UNTIL_ATTRIBUTE: &str = "montra_early_buy_fee_until";
/// `PoolConfig.earlyBuyFeeStartPips`: the anti-snipe rate a buy pays until the window decays.
pub const EARLY_BUY_FEE_START_PIPS_ATTRIBUTE: &str = "montra_early_buy_fee_start_pips";
/// `PoolConfig.earlyBuyFeeWindow`: how long, in seconds, the anti-snipe rate takes to decay.
pub const EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE: &str = "montra_early_buy_fee_window_seconds";
/// `PoolConfig.stage`: 0 to 4, of which only 3 (`LpCrossed`) and 4 (`LpActive`) change a quote.
pub const STAGE_ATTRIBUTE: &str = "montra_stage";
/// `PoolConfig.lpCrossedAt`: when, in unix seconds, the price last crossed the LP line.
pub const LP_CROSSED_AT_ATTRIBUTE: &str = "montra_lp_crossed_at";
/// `PoolConfig.lpHeldSeconds`: how long the price held past the LP line before `lpCrossedAt`.
pub const LP_HELD_SECONDS_ATTRIBUTE: &str = "montra_lp_held_seconds";
/// `PoolConfig.seedTickLower`: the lower tick of the seed position.
pub const SEED_TICK_LOWER_ATTRIBUTE: &str = "montra_seed_tick_lower";
/// `PoolConfig.seedTickUpper`: the upper tick of the seed position.
pub const SEED_TICK_UPPER_ATTRIBUTE: &str = "montra_seed_tick_upper";
/// Tycho's pause flag of a component, which [`decode_paused`] reads.
pub const PAUSED_ATTRIBUTE: &str = "paused";

/// Every `montra_*` attribute, all of which [`MontraPoolConfig::from_attributes`] requires.
const POOL_CONFIG_ATTRIBUTES: [&str; 11] = [
    TOTAL_FEE_PIPS_ATTRIBUTE,
    LP_FEE_PIPS_ATTRIBUTE,
    QUOTE_IS_CURRENCY0_ATTRIBUTE,
    EARLY_BUY_FEE_UNTIL_ATTRIBUTE,
    EARLY_BUY_FEE_START_PIPS_ATTRIBUTE,
    EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE,
    STAGE_ATTRIBUTE,
    LP_CROSSED_AT_ATTRIBUTE,
    LP_HELD_SECONDS_ATTRIBUTE,
    SEED_TICK_LOWER_ATTRIBUTE,
    SEED_TICK_UPPER_ATTRIBUTE,
];

/// 2^24, one past the largest `uint24`: every fee in pips the hook stores is a `uint24`.
const UINT24_LIMIT: u32 = 1 << 24;

/// `MAX_TOTAL_FEE_PIPS()` of [`hook_handler::MONTRA_HOOK_ELYSIUM`] at block 6002000: the largest
/// `totalFeePips` its `register` accepts.
pub const MAX_TOTAL_FEE_PIPS_ELYSIUM: u32 = 100_000;
/// `MAX_EARLY_BUY_FEE_PIPS()` of the same hook at block 6002000: the largest
/// `earlyBuyFeeStartPips` its `register` accepts.
pub const MAX_EARLY_BUY_FEE_PIPS_ELYSIUM: u32 = 990_000;
/// `MAX_EARLY_BUY_FEE_WINDOW()` of the same hook at block 6002000: the longest
/// `earlyBuyFeeWindow`, in seconds, its `register` accepts.
pub const MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM: u64 = 1_800;

/// `PoolConfig.stage`, the hook's `Stage` enum, stored as one byte from 0 to 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// 0: launched, the price below the graduation line.
    Launched,
    /// 1: the price crossed the graduation line, not yet confirmed.
    Crossed,
    /// 2: graduated.
    Graduated,
    /// 3: the price crossed the LP line and is holding past it.
    LpCrossed,
    /// 4: the LP fee is on.
    LpActive,
}

/// A pool's anti-snipe terms. `earlyBuyFeeUntil` sits in `PoolConfig` word 0 and the other two
/// in word 2, so the indexer can update them apart, and all three are kept whether or not a
/// window is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AntiSnipeTerms {
    /// `earlyBuyFeeUntil`: the end of the window in unix seconds, 0 for a pool without one.
    until: u64,
    /// `earlyBuyFeeStartPips`: the rate a buy pays until the window starts to decay.
    start_pips: u32,
    /// `earlyBuyFeeWindow`: the seconds the rate takes to decay to the total.
    seconds: u64,
}

impl AntiSnipeTerms {
    /// The terms while a window is set; `None` when `earlyBuyFeeUntil` is 0, which the hook
    /// stores for a pool launched without one.
    fn window(&self) -> Option<&Self> {
        (self.until != 0).then_some(self)
    }
}

/// The terms of one Montra pool's `PoolConfig` that a quote depends on, as the indexer
/// publishes them.
///
/// The decoder keeps terms the hook's `register` and `lowerTotalFee` would refuse, so that one
/// such pool does not fail the stream; [`MontraPoolConfig::check_terms`] refuses them when the
/// pool is quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MontraPoolConfig {
    total_fee_pips: u32,
    lp_fee_pips: u32,
    quote_is_currency0: bool,
    anti_snipe: AntiSnipeTerms,
    stage: Stage,
    lp_crossed_at: u64,
    lp_held_seconds: u64,
    seed_tick_lower: i32,
    seed_tick_upper: i32,
}

impl MontraPoolConfig {
    /// Reads every `montra_*` attribute out of `attributes`.
    ///
    /// Fails with `MissingAttribute` naming the first absent attribute, and with `ValueError` for
    /// a value of the wrong width. Terms the hook would refuse decode; quoting refuses them.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        let mut config = Self {
            total_fee_pips: 0,
            lp_fee_pips: 0,
            quote_is_currency0: false,
            anti_snipe: AntiSnipeTerms { until: 0, start_pips: 0, seconds: 0 },
            stage: Stage::Launched,
            lp_crossed_at: 0,
            lp_held_seconds: 0,
            seed_tick_lower: 0,
            seed_tick_upper: 0,
        };
        for name in POOL_CONFIG_ATTRIBUTES {
            let value = attributes
                .get(name)
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))?;
            config.set(name, value)?;
        }
        Ok(config)
    }

    /// This config with the `montra_*` attributes of `updated` applied; other names are ignored.
    ///
    /// Fails with `ValueError` for a value of the wrong width, and then leaves `self` as it was.
    /// Terms the hook would refuse apply; quoting refuses them.
    pub fn with_updates(
        &self,
        updated: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        let mut config = self.clone();
        for name in POOL_CONFIG_ATTRIBUTES {
            if let Some(value) = updated.get(name) {
                config.set(name, value)?;
            }
        }
        Ok(config)
    }

    /// Decodes `value` as the attribute `name`, in the format the indexer writes it.
    fn set(&mut self, name: &str, value: &Bytes) -> Result<(), InvalidSnapshotError> {
        match name {
            TOTAL_FEE_PIPS_ATTRIBUTE => self.total_fee_pips = pips(name, value)?,
            LP_FEE_PIPS_ATTRIBUTE => self.lp_fee_pips = pips(name, value)?,
            QUOTE_IS_CURRENCY0_ATTRIBUTE => self.quote_is_currency0 = flag(name, value)?,
            EARLY_BUY_FEE_UNTIL_ATTRIBUTE => self.anti_snipe.until = u64_be(name, value)?,
            EARLY_BUY_FEE_START_PIPS_ATTRIBUTE => self.anti_snipe.start_pips = pips(name, value)?,
            EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE => {
                self.anti_snipe.seconds = u64_be(name, value)?
            }
            STAGE_ATTRIBUTE => self.stage = stage(value)?,
            LP_CROSSED_AT_ATTRIBUTE => self.lp_crossed_at = u64_be(name, value)?,
            LP_HELD_SECONDS_ATTRIBUTE => self.lp_held_seconds = u64_be(name, value)?,
            SEED_TICK_LOWER_ATTRIBUTE => self.seed_tick_lower = tick(name, value)?,
            SEED_TICK_UPPER_ATTRIBUTE => self.seed_tick_upper = tick(name, value)?,
            other => {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "{other} is not a montra pool config attribute"
                )))
            }
        }
        Ok(())
    }

    /// Refuses with `InvalidInput`, naming them, the terms the hook's `register` and
    /// `lowerTotalFee` refuse:
    /// - `totalFeePips` above [`MAX_TOTAL_FEE_PIPS_ELYSIUM`];
    /// - `lpFeePips` above `totalFeePips`;
    /// - while an anti-snipe window is set, a start rate above [`MAX_EARLY_BUY_FEE_PIPS_ELYSIUM`]
    ///   or at or below the total, and a window longer than
    ///   [`MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM`] or than `earlyBuyFeeUntil`.
    ///
    /// Without a window no quote reads the start rate or the window, so `check_terms` does not
    /// check them.
    pub fn check_terms(&self) -> Result<(), SimulationError> {
        let refuse = |terms: String| {
            SimulationError::InvalidInput(
                format!("montra pool config: the hook refuses {terms}"),
                None,
            )
        };
        if self.total_fee_pips > MAX_TOTAL_FEE_PIPS_ELYSIUM {
            return Err(refuse(format!(
                "{TOTAL_FEE_PIPS_ATTRIBUTE} {} above its cap {MAX_TOTAL_FEE_PIPS_ELYSIUM}",
                self.total_fee_pips
            )));
        }
        if self.lp_fee_pips > self.total_fee_pips {
            return Err(refuse(format!(
                "{LP_FEE_PIPS_ATTRIBUTE} {} above {TOTAL_FEE_PIPS_ATTRIBUTE} {}",
                self.lp_fee_pips, self.total_fee_pips
            )));
        }
        let Some(window) = self.anti_snipe.window() else { return Ok(()) };
        if window.start_pips > MAX_EARLY_BUY_FEE_PIPS_ELYSIUM {
            return Err(refuse(format!(
                "{EARLY_BUY_FEE_START_PIPS_ATTRIBUTE} {} above its cap \
                 {MAX_EARLY_BUY_FEE_PIPS_ELYSIUM}",
                window.start_pips
            )));
        }
        if window.seconds > MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM {
            return Err(refuse(format!(
                "{EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE} {} above its cap \
                 {MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM}",
                window.seconds
            )));
        }
        if window.start_pips <= self.total_fee_pips {
            return Err(refuse(format!(
                "{EARLY_BUY_FEE_START_PIPS_ATTRIBUTE} {} at or below {TOTAL_FEE_PIPS_ATTRIBUTE} {}",
                window.start_pips, self.total_fee_pips
            )));
        }
        if window.seconds > window.until {
            return Err(refuse(format!(
                "{EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE} {} longer than \
                 {EARLY_BUY_FEE_UNTIL_ATTRIBUTE} {}",
                window.seconds, window.until
            )));
        }
        Ok(())
    }

    /// `totalFeePips`.
    pub fn total_fee_pips(&self) -> u32 {
        self.total_fee_pips
    }

    /// `quoteIsCurrency0`.
    pub fn quote_is_currency0(&self) -> bool {
        self.quote_is_currency0
    }

    /// The anti-snipe rate in pips a buy pays at `timestamp`: `earlyBuyFeeStartPips` until the
    /// window starts, decaying linearly with the decay floored (so the rate rounds up) to
    /// `totalFeePips` at `earlyBuyFeeUntil`, and `totalFeePips` from then on or when no window is
    /// set.
    ///
    /// Refuses the terms [`Self::check_terms`] refuses. On terms it accepts, the decay stays
    /// below `earlyBuyFeeStartPips - totalFeePips`, since the elapsed part of the window is
    /// shorter than the window; a decay outside that range fails with `FatalError` naming it.
    pub fn buy_fee_pips(&self, timestamp: u64) -> Result<u32, SimulationError> {
        self.check_terms()?;
        let total = self.total_fee_pips;
        let Some(window) = self.anti_snipe.window() else { return Ok(total) };
        if timestamp >= window.until {
            return Ok(total);
        }
        // `check_terms` keeps the window inside `until` and the start above the total.
        let window_start = window.until - window.seconds;
        if timestamp <= window_start {
            return Ok(window.start_pips);
        }
        // Here window_start < timestamp < until, so the window is at least one second long.
        let span = window.start_pips - total;
        let decay = u64::from(span) * (timestamp - window_start) / window.seconds;
        u32::try_from(decay)
            .ok()
            .filter(|decay| *decay < span)
            .map(|decay| window.start_pips - decay)
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "montra pool config: the anti-snipe decay {decay} at {timestamp} is not \
                     below the span {span} from the start rate to the total"
                ))
            })
    }

    /// The LP fee in pips the pool charges at `timestamp`: `lpFeePips` at `LpActive`, and at
    /// `LpCrossed` once the price has held past the LP line for
    /// [`hook_handler::MONTRA_LP_LINE_HOLD_SECONDS_ELYSIUM`] in total; 0 otherwise.
    pub fn lp_fee_in_force_pips(&self, timestamp: u64) -> u32 {
        let held_long_enough = || {
            // `lpCrossedAt` is the timestamp of an earlier block, so on chain the hook's
            // `block.timestamp - lpCrossedAt` never goes negative. A sum past `u64::MAX` has
            // held long enough, so it saturates.
            let held = self
                .lp_held_seconds
                .saturating_add(timestamp.saturating_sub(self.lp_crossed_at));
            held >= hook_handler::MONTRA_LP_LINE_HOLD_SECONDS_ELYSIUM
        };
        let lp_on = match self.stage {
            Stage::LpActive => true,
            Stage::LpCrossed => held_long_enough(),
            Stage::Launched | Stage::Crossed | Stage::Graduated => false,
        };
        if lp_on {
            self.lp_fee_pips
        } else {
            0
        }
    }

    /// The tick the seed position starts at, which a sell may not take the price past:
    /// `seedTickUpper` when the quote is `currency0`, `seedTickLower` otherwise.
    pub fn opening_tick(&self) -> i32 {
        if self.quote_is_currency0 {
            self.seed_tick_upper
        } else {
            self.seed_tick_lower
        }
    }

    /// The far end of the seed position, the furthest a buy can take the price: v4's usable
    /// extreme for the pool's tick spacing.
    pub fn seed_extreme_tick(&self) -> i32 {
        if self.quote_is_currency0 {
            self.seed_tick_lower
        } else {
            self.seed_tick_upper
        }
    }
}

/// Reads Tycho's pause flag of a component: an empty value or `[0]` is not paused, and any other
/// value is paused, whichever component paused it and for which reason.
pub fn decode_paused(value: &Bytes) -> bool {
    !matches!(value.as_ref(), [] | [0])
}

/// An 8-byte big-endian `u64`.
fn u64_be(name: &str, value: &Bytes) -> Result<u64, InvalidSnapshotError> {
    let bytes = <[u8; 8]>::try_from(value.as_ref()).map_err(|_wrong_width| {
        InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes ({value}), not an 8-byte big-endian u64",
            value.len()
        ))
    })?;
    Ok(u64::from_be_bytes(bytes))
}

/// An 8-byte big-endian fee in pips, which the hook stores as a `uint24`.
fn pips(name: &str, value: &Bytes) -> Result<u32, InvalidSnapshotError> {
    let pips = u64_be(name, value)?;
    u32::try_from(pips)
        .ok()
        .filter(|pips| *pips < UINT24_LIMIT)
        .ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!("{name} = {pips} does not fit a uint24"))
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

/// A one-byte stage, one of [`Stage`]'s variants.
fn stage(value: &Bytes) -> Result<Stage, InvalidSnapshotError> {
    match value.as_ref() {
        [0] => Ok(Stage::Launched),
        [1] => Ok(Stage::Crossed),
        [2] => Ok(Stage::Graduated),
        [3] => Ok(Stage::LpCrossed),
        [4] => Ok(Stage::LpActive),
        _ => Err(InvalidSnapshotError::ValueError(format!(
            "{STAGE_ATTRIBUTE} is {value}, not one byte naming a stage"
        ))),
    }
}

/// A 4-byte big-endian two's-complement tick.
fn tick(name: &str, value: &Bytes) -> Result<i32, InvalidSnapshotError> {
    let bytes = <[u8; 4]>::try_from(value.as_ref()).map_err(|_wrong_width| {
        InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes ({value}), not a 4-byte big-endian i32",
            value.len()
        ))
    })?;
    Ok(i32::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests;
