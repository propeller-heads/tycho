//! Montra's `LaunchHook` on Elysium: the hook every Montra launch pool is created with, which
//! charges a fee in the pool's quote asset on every swap, an anti-snipe rate on early buys, and
//! switches the pool's LP fee on once the price has held past its LP line.
//!
//! Montra pools are `uniswap_v4_hooks` components. The Elysium manifest of the with-hooks
//! Substreams package (`elysium-uniswap-v4-with-hooks.yaml`) decodes the hook's `PoolConfig`
//! storage writes into the `montra_*` state attributes [`MontraPoolConfig`] reads, and its
//! `map_launchpad_enriched_block_changes` module decides when a pool is paused.
pub mod hook_handler;
pub mod hook_handler_creator;

use std::collections::HashMap;

use tycho_common::{simulation::errors::SimulationError, Bytes};

use super::launchpad::{field, flag, tick_i32 as tick, u64_be};
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

/// Every `montra_*` attribute, all of which [`MontraPoolConfig::from_attributes`] requires (a
/// test checks that each one is), and none of which the indexer deletes.
pub(super) const POOL_CONFIG_ATTRIBUTES: [&str; 11] = [
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
pub(crate) const MAX_TOTAL_FEE_PIPS_ELYSIUM: u32 = 100_000;
/// `MAX_EARLY_BUY_FEE_PIPS()` of the same hook at block 6002000: the largest
/// `earlyBuyFeeStartPips` its `register` accepts.
pub(crate) const MAX_EARLY_BUY_FEE_PIPS_ELYSIUM: u32 = 990_000;
/// `MAX_EARLY_BUY_FEE_WINDOW()` of the same hook at block 6002000: the longest
/// `earlyBuyFeeWindow`, in seconds, its `register` accepts.
pub(crate) const MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM: u64 = 1_800;

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
/// such pool does not fail the stream. Construction checks the terms once and keeps the refusal,
/// which [`MontraPoolConfig::check_terms`] and every quoting method return; on terms it accepts
/// the quoting arithmetic cannot fail.
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
    /// Why the hook refuses these terms, naming them; `None` when it accepts them.
    refusal: Option<String>,
}

impl MontraPoolConfig {
    /// Reads every `montra_*` attribute out of `attributes`.
    ///
    /// Fails with `MissingAttribute` naming the first absent attribute, and with `ValueError` for
    /// a value of the wrong width. Terms the hook would refuse decode; quoting refuses them.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        Self::decode(attributes, None)
    }

    /// This config with the `montra_*` attributes of `updated` applied; other names are ignored.
    ///
    /// Fails with `ValueError` for a value of the wrong width, and then leaves `self` as it was.
    /// Terms the hook would refuse apply; quoting refuses them.
    pub fn with_updates(
        &self,
        updated: &HashMap<String, Bytes>,
    ) -> Result<Self, InvalidSnapshotError> {
        Self::decode(updated, Some(self))
    }

    /// Decodes each `montra_*` attribute of `attributes` in the format the indexer writes it,
    /// taking an absent one from `base`, or failing with `MissingAttribute` without a `base`.
    fn decode(
        attributes: &HashMap<String, Bytes>,
        base: Option<&Self>,
    ) -> Result<Self, InvalidSnapshotError> {
        let mut config = Self {
            total_fee_pips: field(
                attributes,
                TOTAL_FEE_PIPS_ATTRIBUTE,
                pips,
                base.map(|b| b.total_fee_pips),
            )?,
            lp_fee_pips: field(
                attributes,
                LP_FEE_PIPS_ATTRIBUTE,
                pips,
                base.map(|b| b.lp_fee_pips),
            )?,
            quote_is_currency0: field(
                attributes,
                QUOTE_IS_CURRENCY0_ATTRIBUTE,
                flag,
                base.map(|b| b.quote_is_currency0),
            )?,
            anti_snipe: AntiSnipeTerms {
                until: field(
                    attributes,
                    EARLY_BUY_FEE_UNTIL_ATTRIBUTE,
                    u64_be,
                    base.map(|b| b.anti_snipe.until),
                )?,
                start_pips: field(
                    attributes,
                    EARLY_BUY_FEE_START_PIPS_ATTRIBUTE,
                    pips,
                    base.map(|b| b.anti_snipe.start_pips),
                )?,
                seconds: field(
                    attributes,
                    EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE,
                    u64_be,
                    base.map(|b| b.anti_snipe.seconds),
                )?,
            },
            stage: field(attributes, STAGE_ATTRIBUTE, stage, base.map(|b| b.stage))?,
            lp_crossed_at: field(
                attributes,
                LP_CROSSED_AT_ATTRIBUTE,
                u64_be,
                base.map(|b| b.lp_crossed_at),
            )?,
            lp_held_seconds: field(
                attributes,
                LP_HELD_SECONDS_ATTRIBUTE,
                u64_be,
                base.map(|b| b.lp_held_seconds),
            )?,
            seed_tick_lower: field(
                attributes,
                SEED_TICK_LOWER_ATTRIBUTE,
                tick,
                base.map(|b| b.seed_tick_lower),
            )?,
            seed_tick_upper: field(
                attributes,
                SEED_TICK_UPPER_ATTRIBUTE,
                tick,
                base.map(|b| b.seed_tick_upper),
            )?,
            refusal: None,
        };
        config.refusal = config.refusal();
        Ok(config)
    }

    /// Why the hook's `register` and `lowerTotalFee` refuse these terms, naming them, or `None`:
    /// - `totalFeePips` above [`MAX_TOTAL_FEE_PIPS_ELYSIUM`];
    /// - `lpFeePips` above `totalFeePips`;
    /// - while an anti-snipe window is set, a start rate above [`MAX_EARLY_BUY_FEE_PIPS_ELYSIUM`]
    ///   or at or below the total, and a window longer than
    ///   [`MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM`] or than `earlyBuyFeeUntil`.
    ///
    /// The start rate at or below the total is the hook's own refusal, not a conservative one:
    /// on a fork of Elysium at block 7964700, `register` reverted `InvalidFeeConfig()` for every
    /// window from 1 to 1800 s with a start rate equal to or below totals of 5000, 20000 and
    /// 100000 pips, and accepted a start of total + 1. `lowerTotalFee` only lowers the total, so
    /// no later write brings the start down to it. Without a window `register` accepts only a
    /// start rate of 0, and no quote reads the start rate or the window, so they are not checked
    /// then.
    fn refusal(&self) -> Option<String> {
        if self.total_fee_pips > MAX_TOTAL_FEE_PIPS_ELYSIUM {
            return Some(format!(
                "{TOTAL_FEE_PIPS_ATTRIBUTE} {} above its cap {MAX_TOTAL_FEE_PIPS_ELYSIUM}",
                self.total_fee_pips
            ));
        }
        if self.lp_fee_pips > self.total_fee_pips {
            return Some(format!(
                "{LP_FEE_PIPS_ATTRIBUTE} {} above {TOTAL_FEE_PIPS_ATTRIBUTE} {}",
                self.lp_fee_pips, self.total_fee_pips
            ));
        }
        let window = self.anti_snipe.window()?;
        if window.start_pips > MAX_EARLY_BUY_FEE_PIPS_ELYSIUM {
            return Some(format!(
                "{EARLY_BUY_FEE_START_PIPS_ATTRIBUTE} {} above its cap \
                 {MAX_EARLY_BUY_FEE_PIPS_ELYSIUM}",
                window.start_pips
            ));
        }
        if window.seconds > MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM {
            return Some(format!(
                "{EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE} {} above its cap \
                 {MAX_EARLY_BUY_FEE_WINDOW_SECONDS_ELYSIUM}",
                window.seconds
            ));
        }
        if window.start_pips <= self.total_fee_pips {
            return Some(format!(
                "{EARLY_BUY_FEE_START_PIPS_ATTRIBUTE} {} at or below {TOTAL_FEE_PIPS_ATTRIBUTE} {}",
                window.start_pips, self.total_fee_pips
            ));
        }
        if window.seconds > window.until {
            return Some(format!(
                "{EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE} {} longer than \
                 {EARLY_BUY_FEE_UNTIL_ATTRIBUTE} {}",
                window.seconds, window.until
            ));
        }
        None
    }

    /// Refuses with `InvalidInput` terms the hook refuses, naming them (see [`Self::refusal`]
    /// for the rules).
    pub fn check_terms(&self) -> Result<(), SimulationError> {
        match &self.refusal {
            None => Ok(()),
            Some(terms) => Err(SimulationError::InvalidInput(
                format!("montra pool config: the hook refuses {terms}"),
                None,
            )),
        }
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
    /// set. On terms the hook accepts the rate is never below `totalFeePips`.
    ///
    /// Refuses the terms [`Self::check_terms`] refuses.
    pub fn buy_fee_pips(&self, timestamp: u64) -> Result<u32, SimulationError> {
        self.check_terms()?;
        let total = self.total_fee_pips;
        let Some(window) = self.anti_snipe.window() else { return Ok(total) };
        if timestamp >= window.until {
            return Ok(total);
        }
        // Accepted terms keep the window inside `until` and the start above the total, so
        // neither subtraction saturates, and the decay, a share of the span below one since
        // window_start < timestamp < until, never reaches the total.
        let window_start = window
            .until
            .saturating_sub(window.seconds);
        if timestamp <= window_start {
            return Ok(window.start_pips);
        }
        let span = window.start_pips.saturating_sub(total);
        let decay = u64::from(span) * (timestamp - window_start) / window.seconds;
        let decay = u32::try_from(decay).unwrap_or(u32::MAX);
        Ok(window
            .start_pips
            .saturating_sub(decay)
            .max(total))
    }

    /// The LP fee in pips the pool charges at `timestamp`, `None` while it charges none:
    /// `lpFeePips` at `LpActive`, and at `LpCrossed` once the price has held past the LP line for
    /// [`hook_handler::MONTRA_LP_LINE_HOLD_SECONDS_ELYSIUM`] in total. An `lpFeePips` of 0 is
    /// none too: the hook's `beforeSwap` then sets no override and the pool charges its own
    /// slot0 fee, which is 0 for a Montra pool.
    pub fn lp_fee_in_force_pips(&self, timestamp: u64) -> Option<u32> {
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
        (lp_on && self.lp_fee_pips != 0).then_some(self.lp_fee_pips)
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

/// A one-byte stage, one of [`Stage`]'s variants.
fn stage(_name: &str, value: &Bytes) -> Result<Stage, InvalidSnapshotError> {
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

#[cfg(test)]
mod tests;
