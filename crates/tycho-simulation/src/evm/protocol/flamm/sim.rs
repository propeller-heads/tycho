// Copyright (c) 2026 Everlong Labs Limited

//! The Tycho [`ProtocolSim`] of one FLAMM component: the pool's swap venue (both directions) or
//! its lever-up venue, quoted natively from the tracked attributes at the execution clock.
//!
//! A quote runs the pool's whole transaction on a copy of the composed state
//! ([`super::state::FlammState`]): the priced context, the plan with its funding passes, the
//! hook fill, the validation and the settlement through the Router and Morpho, exactly as
//! `FLAMM.swap` / `FLAMM.leverUp` would at `block.timestamp == clock`. Only a size the pool fills
//! in full is a quote: the executor (`FLAMMExecutor`) reverts on a partial fill, so a size the
//! pool would clip is refused here, and [`ProtocolSim::get_limits`] locates the largest size per
//! direction that fills in full.
//!
//! The execution clock is what [`ProtocolSim::apply_block`] sets: Morpho's accrual, the IRM's
//! adaptation, the feeds' staleness, the sequencer grace, the spread's age and the Morpho oracle's
//! SVR reveal are all evaluated there, so a quote is exact at the block it executes in rather
//! than at the block the state was read at.
//!
//! Every refusal is typed: a state that cannot be decoded or has drifted from its pinned
//! identity ([`super::decoder::DecodeError`]), a venue whose IRM or oracle cannot be read, a
//! quarantined venue, a scheduled implementation or hook-set change that is executable, and the
//! pool's own reverts ([`super::error::FlammError`]) all surface as
//! [`SimulationError`]s, never as a guessed amount.

use std::{any::Any, collections::HashMap, sync::Arc};

use alloy::primitives::{Address, U256};
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
        },
    },
    Bytes,
};

use super::{
    decoder::{decode_core, Core, DecodeError, Statics},
    error::FlammError,
    feeds::FeedError,
    hook::HookState,
    lever::LeverResult,
    math::PPM,
    morpho::VenueMarket,
    pricefeed::feed_read,
    router::Venue,
    swap::SwapResult,
    words::{address_of, Attributes},
    Flamm,
};
use crate::evm::protocol::{
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    utils::add_fee_markup,
};

/// Gas per venue and direction, the Go port's measured defaults (`constant.go`): the largest
/// receipt gas of the adapter's fills on Base forks plus 25%, rounded up to 10,000. A swap sell
/// grows with its input when the pool clips it: `_maxInForGrossCap` (`EverlongHook.sol:583`)
/// bisects once per plan and once more in the execution, up to 64 curve solves each, and every
/// funding pass after the first (`FLAMMSwapLib.sol:163`, at most four) re-runs the Router's
/// `fundingCeiling`, the fee and the fill; the two slopes were measured over 645 mined clipped
/// sells. A buy runs neither. Lever-down is not quoted.
pub const GAS_SWAP_SELL: u64 = 1_360_000;
pub const GAS_SWAP_SELL_CAP_EVAL: u64 = 10_300;
pub const GAS_SWAP_SELL_PASS: u64 = 210_000;
pub const GAS_SWAP_BUY: u64 = 1_510_000;
pub const GAS_LEVER_UP: u64 = 3_670_000;

/// The doubling scan of [`ProtocolSim::get_limits`] stops here: `2^128` base units is beyond
/// any balance of the pair's tokens.
const LIMIT_SCAN_CEILING_BITS: usize = 128;

/// The lever-up venue's spot is read at the smallest size it fills in full times `2^6`
/// (`ProtocolSim::spot_price`): the smallest fill pays a few hundred loan-asset base units,
/// floor-rounded (`FLAMMLeverLib.sol:103`), so its rate carries a per-mille of rounding, while
/// sixty-four of them move the venue's curve by far less.
const LEVER_SPOT_PROBE_SHIFT: usize = 6;

/// The venue a component quotes: the swap (`FLAMM.swap`, both directions) or the lever-up
/// (`FLAMM.leverUp`, pool asset in, loan asset out). Lever-down is not quoted: its net pay leg is
/// about half the input by construction (`FLAMMLeverLib.sol:135-140`), which the router would
/// strand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum VenueKind {
    Swap,
    LeverUp,
}

/// The direction of a quote on the pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    /// Pool asset in, loan asset out (`poolAssetIn == true`).
    Sell,
    /// Loan asset in, pool asset out.
    Buy,
}

/// One settled fill of the venue, as the pool would return it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fill {
    used: U256,
    out: U256,
    gas: u64,
    post: Flamm,
}

/// Why a size is not a quote.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Refusal {
    /// The pool's own revert.
    Pool(FlammError),
    /// The pool would fill this much of the input: a partial fill, which `FLAMMExecutor`
    /// reverts on.
    Partial(U256),
    /// The fill pays nothing.
    Nothing,
    /// The lever-up venue only sells the pool asset (lever-down is not quoted).
    LeverDown,
}

impl From<Refusal> for SimulationError {
    fn from(r: Refusal) -> Self {
        match r {
            Refusal::Pool(e) => Self::InvalidInput(format!("flamm: the pool reverts {e}"), None),
            Refusal::Partial(used) => Self::InvalidInput(
                format!(
                    "flamm: the pool would fill {used} of the input (a partial fill is not quoted)"
                ),
                None,
            ),
            Refusal::Nothing => Self::InvalidInput("flamm: the fill pays nothing".into(), None),
            Refusal::LeverDown => Self::InvalidInput(
                "flamm: the lever-up venue only sells the pool asset (lever-down is not quoted)"
                    .into(),
                None,
            ),
        }
    }
}

/// The verdict of a venue's borrow-rate ceiling at a clock. `MMRouterLib._slice`
/// (`MMRouterLib.sol:619-623`) and `requireRate` (`:702-704`) refuse a borrow slice whose
/// post-borrow rate, read from the `AdaptiveCurveIrm` at the clock, exceeds `maxBorrowRateWad`.
/// The rate rises with the borrowed amount (utilization) and adapts with the time elapsed since
/// the market's last update, so the verdict at the two ends of the borrowable range decides
/// whether the clock can move any quote through the ceiling: when every size up to the market's
/// cash passes, or none does, no size's outcome depends on the clock.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RateCap {
    /// No ceiling is armed, or the venue never borrows: the rate is not read.
    Unread,
    /// The rate cannot be read at the clock (an unreadable IRM): every borrow is refused.
    Unreadable,
    /// The largest borrow (the market's cash) passes, so every smaller one does.
    AllPass,
    /// The smallest borrow fails, so every larger one does.
    AllFail,
    /// The ceiling falls inside the borrowable range: the largest slice it admits moves with the
    /// IRM's adaptation every second, so this verdict never compares equal across clocks.
    Binding(u64),
}

impl RateCap {
    fn at(v: &Venue, now: u64) -> Self {
        if v.retired || !v.borrow_enabled || v.max_borrow_rate_wad.is_zero() {
            return Self::Unread;
        }
        let cap = v.max_borrow_rate_wad;
        let passes = |delta_borrow: U256| -> Option<bool> {
            match v
                .morpho
                .borrow_rate_after(delta_borrow, U256::ZERO, now)
            {
                Ok((true, rate)) => Some(rate <= cap),
                Ok((false, _)) | Err(_) => None,
            }
        };
        match (passes(v.morpho.free_liquidity()), passes(U256::ZERO)) {
            (None, _) | (_, None) => Self::Unreadable,
            (Some(true), _) => Self::AllPass,
            (_, Some(false)) => Self::AllFail,
            (Some(false), Some(true)) => Self::Binding(now),
        }
    }
}

/// One live venue's reads that depend on the clock.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VenueClock {
    /// `MorphoBlueAccount.tryPosition`'s readability: false past `IRM_STALE_GRACE` while the
    /// IRM cannot be read (`MorphoBlueAccount.sol:476-500`).
    readable: Result<bool, FlammError>,
    /// The clock itself while the pool's position in the venue accrues: Morpho's interest
    /// (`Morpho._accrueInterest`) moves the market's totals every second the market is borrowed
    /// from, and the pool's debt and supplied assets are its shares valued at those totals
    /// (`MorphoBlueAccount.sol:294-309`). They enter the gate's book, the funding plan and the
    /// settlement's share arithmetic, so every quote of a positioned pool is a function of the
    /// clock; a pool with no shares in the market reads zero at any clock.
    accrual: Option<u64>,
    rate_cap: RateCap,
}

impl VenueClock {
    fn at(v: &Venue, now: u64) -> Self {
        let m: &VenueMarket = &v.morpho;
        let positioned = !m.position.borrow_shares.is_zero() || !m.position.supply_shares.is_zero();
        let accrues = m.has_irm &&
            !m.market.total_borrow_assets.is_zero() &&
            U256::from(now) > m.market.last_update;
        Self {
            readable: m.try_position(now).map(|r| r.readable),
            accrual: (positioned && accrues).then_some(now),
            rate_cap: RateCap::at(v, now),
        }
    }
}

/// Everything a quote reads at the execution clock and nowhere else: what
/// [`ProtocolSim::apply_block`] compares to decide whether quoting changed between two clocks.
/// Every entry is either a deadline the clock crosses (a feed's heartbeat, the sequencer's
/// grace, the spread's age, the Morpho oracle's reveal cutoff, a scheduled change's
/// `executableAt`, the account's IRM grace, the rate ceiling's verdict) or the clock itself
/// where a quote is a continuous function of it (a positioned venue's accrual, a binding rate
/// ceiling).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClockSignature {
    venues: Vec<VenueClock>,
    /// The Morpho market oracle's answer at the clock (`MorphoBlueAccount.oraclePrice` over the
    /// DualAggregator's secondary path), or why it has none.
    oracle: Result<(bool, U256, bool), FeedError>,
    /// The pool asset's and loan asset 0's feed reads (`PriceFeed._usd`, stale past the
    /// heartbeat) and the sequencer check (`PriceFeed._requireSequencer`).
    feeds: Vec<Result<(U256, u64), FlammError>>,
    sequencer: Result<(), FlammError>,
    /// The spread hook's answer (`LeverageSpreadHook.spreadPpm`: live while inside
    /// `maxSpreadAge`).
    spread: Result<(bool, U256), FlammError>,
    /// Whether a scheduled implementation, hook-set, venue or loan-asset change is executable.
    scheduled: bool,
}

/// One FLAMM component as Tycho quotes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlammPoolState {
    /// The component id (`0x` + the pool address for the swap venue, `pool || 0x00000000 ||
    /// uint64(1)` for lever-up).
    id: String,
    statics: Arc<Statics>,
    /// The component's attributes as last streamed: the one source the state is decoded from.
    /// Shared between a state and the post-states its quotes return (a quote moves the pool,
    /// not the stream), so a later delta on any of them rebuilds from the chain's words.
    attrs: Arc<Attributes>,
    /// The block the attributes were observed at (the snapshot's, then each delta's).
    block: u64,
    /// The execution clock: the timestamp quotes run at ([`ProtocolSim::apply_block`]).
    clock: u64,
    /// The decoded state, or why the attributes do not decode into one. A refusal keeps the
    /// component alive: the next delta may restore it (a feed's rounds after a rotation).
    core: Result<Core, DecodeError>,
    /// Why the Morpho market oracle could not be evaluated at `clock`, if it could not.
    oracle_refusal: Option<String>,
}

impl FlammPoolState {
    /// A state from its decoded parts at `clock`; the Morpho oracle is evaluated there.
    pub fn new(
        id: String,
        statics: Arc<Statics>,
        attrs: Arc<Attributes>,
        core: Result<Core, DecodeError>,
        block: u64,
        clock: u64,
    ) -> Self {
        let mut s = Self { id, statics, attrs, block, clock, core, oracle_refusal: None };
        s.refresh_oracle();
        s
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn venue(&self) -> VenueKind {
        self.statics.kind
    }

    pub fn statics(&self) -> &Statics {
        &self.statics
    }

    pub fn attributes(&self) -> &Attributes {
        &self.attrs
    }

    /// The block the attributes were observed at.
    pub fn block(&self) -> u64 {
        self.block
    }

    /// The execution clock.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// The decoded state, or why there is none.
    pub fn core(&self) -> Result<&Core, &DecodeError> {
        self.core.as_ref()
    }

    /// The composed pool state at the current clock, when decoded.
    pub fn flamm(&self) -> Option<&Flamm> {
        self.core
            .as_ref()
            .ok()
            .map(|c| &c.flamm)
    }

    /// The state over another composed pool state (a fixture scenario of the same pool), the
    /// oracle re-evaluated at the clock.
    #[cfg(test)]
    pub(crate) fn with_flamm(mut self, flamm: Flamm) -> Self {
        if let Ok(core) = self.core.as_mut() {
            core.flamm = flamm;
        }
        self.refresh_oracle();
        self
    }

    fn pool_asset(&self) -> Address {
        self.statics.pool_asset
    }

    fn loan_asset(&self) -> Address {
        self.statics.loan_asset_0
    }

    /// Evaluates every venue's Morpho market oracle at the clock
    /// (`MorphoBlueAccount.oraclePrice`, [`super::feeds::Feed::oracle_price`]) into the composed
    /// state; a ring that cannot answer at this clock is recorded as a refusal.
    fn refresh_oracle(&mut self) {
        let clock = self.clock;
        let block = self.block;
        let st = self.statics.clone();
        let Ok(core) = self.core.as_mut() else {
            self.oracle_refusal = None;
            return;
        };
        core.flamm.block = block;
        core.flamm.timestamp = clock;
        match core.feeds.mo0.oracle_price(
            st.venue_0_oracle_scale_factor,
            clock,
            st.feed_mo0_max_sync_iterations,
        ) {
            Ok((ok, price, zero)) => {
                for v in core.flamm.router.venues.iter_mut() {
                    v.morpho.oracle_ok = ok;
                    v.morpho.oracle_price = price;
                    v.morpho.oracle_zero = zero;
                }
                self.oracle_refusal = None;
            }
            Err(e) => {
                for v in core.flamm.router.venues.iter_mut() {
                    v.morpho.oracle_ok = false;
                    v.morpho.oracle_price = U256::ZERO;
                    v.morpho.oracle_zero = false;
                }
                self.oracle_refusal =
                    Some(format!("the Morpho oracle cannot be read at {clock}: {e:?}"));
            }
        }
    }

    /// The composed state a quote may run on, or the refusal that stands in its way: the
    /// attributes decode, the oracle reads at the clock, no scheduled change is executable, and
    /// every venue is inside the quotable envelope (`pool_simulator.go` `envelope`): readable
    /// (not quarantined past the account's grace) with an oracle that answers. A donation to the
    /// venue account beyond what the Router manages is quoted as the Router prices it
    /// (recognized `= min(actual, managed)`), which the settlement fixtures replay exactly.
    fn quotable(&self) -> Result<&Flamm, SimulationError> {
        let core = match &self.core {
            Ok(c) => c,
            Err(e) => return Err(SimulationError::RecoverableError(format!("flamm: {e}"))),
        };
        if let Some(r) = &self.oracle_refusal {
            return Err(SimulationError::RecoverableError(format!("flamm: {r}")));
        }
        if core.scheduled_at != 0 && self.clock >= core.scheduled_at {
            return Err(SimulationError::RecoverableError(format!(
                "flamm: a scheduled implementation, hook-set, venue or loan-asset change is executable at {} (clock {})",
                core.scheduled_at, self.clock
            )));
        }
        for (i, v) in core
            .flamm
            .router
            .venues
            .iter()
            .enumerate()
        {
            if v.retired {
                continue;
            }
            match v.morpho.try_position(self.clock) {
                Ok(r) if r.readable => {}
                Ok(_) => {
                    return Err(SimulationError::RecoverableError(format!(
                        "flamm: venue {i} is quarantined (its rate model is unreadable past the account's grace)"
                    )))
                }
                Err(e) => {
                    return Err(SimulationError::RecoverableError(format!(
                        "flamm: venue {i} cannot be read at {}: {e}",
                        self.clock
                    )))
                }
            }
            if !v.morpho.oracle_ok {
                return Err(SimulationError::RecoverableError(format!(
                    "flamm: venue {i}'s Morpho market oracle does not answer"
                )));
            }
        }
        Ok(&core.flamm)
    }

    fn direction(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> Result<Direction, SimulationError> {
        if token_in == self.pool_asset() && token_out == self.loan_asset() {
            Ok(Direction::Sell)
        } else if token_in == self.loan_asset() && token_out == self.pool_asset() {
            Ok(Direction::Buy)
        } else {
            Err(SimulationError::InvalidInput(
                format!("flamm: {token_in} -> {token_out} is not the pool's pair"),
                None,
            ))
        }
    }

    /// The venue's transaction for `amount_in` at the clock: `FLAMM.swap(tokenIn, tokenOut,
    /// amountIn, 1, to, block.timestamp)` or `FLAMM.leverUp(amountIn, 1, to, block.timestamp)`,
    /// with the pool's own reverts as refusals. The fill may be partial: [`Self::full_fill`]
    /// refuses those.
    fn settle(&self, flamm: &Flamm, dir: Direction, amount_in: U256) -> Result<Fill, Refusal> {
        let now = self.clock;
        match (self.venue(), dir) {
            (VenueKind::Swap, _) => {
                let (token_in, token_out) = match dir {
                    Direction::Sell => (self.pool_asset(), self.loan_asset()),
                    Direction::Buy => (self.loan_asset(), self.pool_asset()),
                };
                let (r, post) = flamm
                    .execute_swap(token_in, token_out, amount_in, U256::from(1u8), now, now)
                    .map_err(Refusal::Pool)?;
                Ok(Fill { used: r.amount_in_used, out: r.amount_out, gas: swap_gas(&r), post })
            }
            (VenueKind::LeverUp, Direction::Sell) => {
                let (r, post) = flamm
                    .execute_lever(true, amount_in, U256::from(1u8), now, now)
                    .map_err(Refusal::Pool)?;
                Ok(Fill { used: r.amount_in_used, out: r.amount_out, gas: lever_gas(&r), post })
            }
            (VenueKind::LeverUp, Direction::Buy) => Err(Refusal::LeverDown),
        }
    }

    /// [`Self::settle`], refusing a fill the pool would clip (`amountInUsed < amountIn`) or that
    /// pays nothing: `FLAMMExecutor` reverts on a partial fill, so such a size is not a quote.
    fn full_fill(&self, flamm: &Flamm, dir: Direction, amount_in: U256) -> Result<Fill, Refusal> {
        let f = self.settle(flamm, dir, amount_in)?;
        if f.used != amount_in {
            return Err(Refusal::Partial(f.used));
        }
        if f.out.is_zero() {
            return Err(Refusal::Nothing);
        }
        Ok(f)
    }

    /// The smallest power of two that fills in full in `dir`, and its output: doubling from one
    /// base unit. `None` when no size up to `2^128` fills in full.
    fn smallest_full_fill(&self, flamm: &Flamm, dir: Direction) -> Option<(U256, U256)> {
        let mut a = U256::from(1u8);
        for _ in 0..LIMIT_SCAN_CEILING_BITS {
            if let Ok(f) = self.full_fill(flamm, dir, a) {
                return Some((a, f.out));
            }
            a <<= 1;
        }
        None
    }

    /// The largest size that fills in full in `dir`, and its output: doubling from one base unit
    /// to the first size that fills in full, doubling on to the first that does not, then
    /// bisection between the two. Past the first fully filled size the predicate is monotone:
    /// the clip (the gate room, the notional cap, the Router's funding at the pin, which grows
    /// slower than the payout) and the band binding only tighten with size. Every probe is the
    /// pool's whole transaction. `(0, 0)` when no size up to `2^128` fills in full.
    fn limit(&self, flamm: &Flamm, dir: Direction) -> Result<(U256, U256), SimulationError> {
        let probe = |a: U256| -> Option<U256> {
            self.full_fill(flamm, dir, a)
                .ok()
                .map(|f| f.out)
        };
        let Some((mut lo, mut lo_out)) = self.smallest_full_fill(flamm, dir) else {
            return Ok((U256::ZERO, U256::ZERO));
        };
        let mut hi = lo << 1;
        while let Some(out) = probe(hi) {
            lo = hi;
            lo_out = out;
            if hi.bit_len() > LIMIT_SCAN_CEILING_BITS {
                return Ok((lo, lo_out));
            }
            hi <<= 1;
        }
        while hi - lo > U256::from(1u8) {
            let mid = lo + ((hi - lo) >> 1);
            match probe(mid) {
                Some(out) => {
                    lo = mid;
                    lo_out = out;
                }
                None => hi = mid,
            }
        }
        Ok((lo, lo_out))
    }

    /// The swap hook's spot (`EverlongHook.spot`, N18 per pool-asset base unit) in the token
    /// frame: the amount of loan asset one whole pool asset buys, as `f64`.
    fn hook_spot_frame(&self, flamm: &Flamm, pool_decimals: u32) -> Result<f64, SimulationError> {
        let hook: &HookState = flamm
            .hooks
            .swap
            .port()
            .map_err(|e| SimulationError::RecoverableError(format!("flamm: {e}")))?;
        let spot = hook
            .spot()
            .map_err(|e| SimulationError::RecoverableError(format!("flamm: spot: {e}")))?;
        // N18 per base unit times base units per whole pool asset, over 1e18 loan units per
        // whole loan asset (N18 is loan asset 0 at 18 decimals).
        Ok(u256_to_f64(spot)? * 10f64.powi(pool_decimals as i32) / 1e18)
    }

    /// The lever-up venue's rate at negligible size: the loan asset one whole pool asset is
    /// paid, read off a fill of the smallest size the venue fills in full times
    /// `2^LEVER_SPOT_PROBE_SHIFT` (halving back to a size that fills when the venue is that
    /// shallow). The frozen `CollRebalancerMath` curve has no closed-form spot; the read carries
    /// the payout's floor rounding (under one base unit of the output over the whole payout) and
    /// the curve's move over the probed size, both bounded at the constant.
    fn lever_rate(
        &self,
        flamm: &Flamm,
        pool_decimals: u32,
        loan_decimals: u32,
    ) -> Result<f64, SimulationError> {
        let Some((a0, out0)) = self.smallest_full_fill(flamm, Direction::Sell) else {
            return Err(SimulationError::RecoverableError(
                "flamm: the lever-up venue fills no size".into(),
            ));
        };
        let (a, out) = (1..=LEVER_SPOT_PROBE_SHIFT)
            .rev()
            .find_map(|k| {
                let a = a0 << k;
                self.full_fill(flamm, Direction::Sell, a)
                    .ok()
                    .map(|f| (a, f.out))
            })
            .unwrap_or((a0, out0));
        let in_whole = u256_to_f64(a)? / 10f64.powi(pool_decimals as i32);
        let out_whole = u256_to_f64(out)? / 10f64.powi(loan_decimals as i32);
        Ok(out_whole / in_whole)
    }

    /// Whether a size the pool refuses is dust: at or below the venue's limit in `dir`, so a
    /// larger size fills in full and the refusal is the payout's quantization (a buy whose
    /// output rounds to nothing, `FillInvalid`, or whose few-unit output rounds past the band
    /// around the checked cross, `FLAMMSwapLib.sol:233`, `PriceBand`; a lever-up whose net leg
    /// rounds to nothing, `FLAMMLeverLib.sol:104`), not the venue's depth. A partial fill is
    /// never dust: the pool clips only above the limit.
    fn is_dust(&self, flamm: &Flamm, dir: Direction, amount: U256, r: &Refusal) -> bool {
        match r {
            Refusal::Pool(_) | Refusal::Nothing => {}
            Refusal::Partial(_) | Refusal::LeverDown => return false,
        }
        match self.limit(flamm, dir) {
            Ok((limit, _)) => amount <= limit,
            Err(_) => false,
        }
    }

    /// What a quote reads at the clock `now` (`ClockSignature`); `None` for a state that does
    /// not decode (nothing is quoted at any clock).
    fn clock_signature(&self, now: u64) -> Option<ClockSignature> {
        let core = self.core.as_ref().ok()?;
        let st = &self.statics;
        let f = &core.flamm;
        Some(ClockSignature {
            venues: f
                .router
                .venues
                .iter()
                .filter(|v| !v.retired)
                .map(|v| VenueClock::at(v, now))
                .collect(),
            oracle: core.feeds.mo0.oracle_price(
                st.venue_0_oracle_scale_factor,
                now,
                st.feed_mo0_max_sync_iterations,
            ),
            feeds: std::iter::once(&f.feed.asset)
                .chain(f.feed.loans.iter())
                .map(|t| feed_read(t, now))
                .collect(),
            sequencer: f.feed.require_sequencer(now),
            spread: f.hooks.spread.spread_ppm(now),
            scheduled: core.scheduled_at != 0 && now >= core.scheduled_at,
        })
    }
}

/// The gas of a settled swap (`pool_simulator.go` `gas`).
fn swap_gas(r: &SwapResult) -> u64 {
    if !r.pool_asset_in {
        return GAS_SWAP_BUY;
    }
    let mut gas = GAS_SWAP_SELL.saturating_add(
        r.cap_evals
            .saturating_mul(GAS_SWAP_SELL_CAP_EVAL),
    );
    if r.passes > 1 {
        gas = gas.saturating_add((r.passes - 1).saturating_mul(GAS_SWAP_SELL_PASS));
    }
    gas
}

fn lever_gas(_r: &LeverResult) -> u64 {
    GAS_LEVER_UP
}

fn token_address(t: &Token) -> Result<Address, SimulationError> {
    address_of("token", &t.address)
        .map_err(|e| SimulationError::InvalidInput(format!("flamm: {e:?}"), None))
}

fn amount_to_u256(a: &BigUint) -> Result<U256, SimulationError> {
    if a.bits() > 256 {
        return Err(SimulationError::InvalidInput("flamm: amount_in exceeds uint256".into(), None));
    }
    Ok(biguint_to_u256(a))
}

#[typetag::serde]
impl ProtocolSim for FlammPoolState {
    /// The swap venue: the fee a sell of the pool asset fills at right now
    /// (`FLAMMSwapLib._fill`'s bounded `previewFeeWad`, the `feeWad` every `previewSwap(true, .)`
    /// returns; the hook's fee law reads the direction and the book, never the size, and a buy's
    /// fee differs by the direction skew). The lever-up venue: the spread a lever-up fills at
    /// right now as a ratio (`FLAMMLeverLib._spread`, `FLAMMLeverLib.sol:158-178`: the hook's
    /// live post clamped into `[LEV_SPREAD_FLOOR_PPM, min(swapPriceBandWad / 1e12,
    /// LEV_SPREAD_CEILING_PPM)]`, the `spreadPpm` every `previewLever(true, .)` returns, never
    /// the raw post). `1.0` when the venue cannot fill at all (a fee the floor refuses, an
    /// unreadable state, no live spread): no price survives it.
    fn fee(&self) -> f64 {
        let Ok(flamm) = self.quotable() else {
            return 1.0;
        };
        match self.venue() {
            VenueKind::Swap => match flamm.swap_fee_wad(true, self.clock) {
                Ok(fee) => u256_to_f64(fee).map_or(1.0, |f| f / 1e18),
                Err(_) => 1.0,
            },
            VenueKind::LeverUp => match flamm.lever_spread(&flamm.pool, true, self.clock) {
                Ok((ppm, _)) if ppm < PPM => u256_to_f64(ppm).map_or(1.0, |f| f / 1e6),
                _ => 1.0,
            },
        }
    }

    /// The amount of `quote` that buys one whole `base` at zero size, gross of the fee of the
    /// direction that buys `base` (`pre_fee / (1 - fee)`, the trait's definition and
    /// `add_fee_markup`'s form): the price of the `quote -> base` trade, which is what a
    /// consumer who reads `spot_price(base, quote)` as the trait defines it expects. The swap
    /// venue prices both orderings at the hook's spot (`EverlongHook.spot`, the reservation
    /// curve at the stored coordinate, the rate a fill of either direction starts from): buying
    /// the pool asset is the loan-asset-in direction and pays that direction's fee, buying the
    /// loan asset is the pool-asset-in direction and pays that one's (the two differ by the
    /// direction skew, `EverlongStrategy.sol:169-196`).
    ///
    /// The lever-up venue has no closed-form spot (its curve is the frozen `CollRebalancerMath`)
    /// and one rate: the loan asset it pays per pool asset at negligible size, read off a fill
    /// (`lever_rate`), net of the spread as the pool pays it. It answers both orderings from
    /// that rate. `spot_price(loan asset, pool asset)`, the trait's ordering for the direction
    /// the venue trades (the pool asset buys the loan asset), is the rate's reciprocal, gross of
    /// the spread since the payout is net of it. `spot_price(pool asset, loan asset)` is the
    /// rate itself: the venue never sells the pool asset (lever-down is not quoted, so there is
    /// no buy side to price gross of anything), and the consumers in this repository ask
    /// `spot_price(token_in, token_out)` for the price of a `token_in -> token_out` trade
    /// (`query_pool_swap`, which needs it as the zero-size bound of that trade's execution
    /// price, and the protocol test harness, which requires it for every direction a venue
    /// trades), so that ordering carries the one price the venue has for the pool asset. The
    /// two orderings are reciprocals; neither is a price for buying the pool asset here, which
    /// `get_amount_out` and `get_limits` refuse as a direction.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let flamm = self.quotable()?;
        // The direction that buys `base`: `quote` in, `base` out.
        let dir = self.direction(token_address(quote)?, token_address(base)?)?;
        let (pool_decimals, loan_decimals) = match dir {
            Direction::Sell => (quote.decimals, base.decimals),
            Direction::Buy => (base.decimals, quote.decimals),
        };
        match self.venue() {
            VenueKind::Swap => {
                let pool_in_loan = self.hook_spot_frame(flamm, pool_decimals)?;
                let fee = flamm
                    .swap_fee_wad(dir == Direction::Sell, self.clock)
                    .map_err(|e| SimulationError::RecoverableError(format!("flamm: fee: {e}")))?;
                let fee = u256_to_f64(fee)? / 1e18;
                if fee >= 1.0 {
                    return Err(SimulationError::RecoverableError(
                        "flamm: the fee takes the whole fill, there is no price".into(),
                    ));
                }
                let pre_fee = match dir {
                    // Loan asset per pool asset: the loan asset buys the pool asset.
                    Direction::Buy => pool_in_loan,
                    // Pool asset per loan asset: the pool asset buys the loan asset.
                    Direction::Sell => 1.0 / pool_in_loan,
                };
                Ok(add_fee_markup(pre_fee, fee))
            }
            VenueKind::LeverUp => {
                let rate = self.lever_rate(flamm, pool_decimals, loan_decimals)?;
                Ok(match dir {
                    // Pool asset per loan asset: what the pool asset buys the loan asset at.
                    Direction::Sell => 1.0 / rate,
                    // Loan asset per pool asset: what the venue pays for the pool asset.
                    Direction::Buy => rate,
                })
            }
        }
    }

    /// The venue's fill of `amount_in` at the execution clock, refused unless the pool fills it
    /// in full; the returned state is the pool after that transaction. A zero input is the empty
    /// trade (the pool itself reverts `InvalidAmount()` on it), and so is dust: a size the pool
    /// refuses although a larger one fills in full (`is_dust`), which pays nothing and is quoted
    /// as nothing on the unchanged state, so that every size in the trait's `[0, limit]` domain
    /// answers. A size above the limit is refused as the pool refuses it: a partial fill or the
    /// pool's own revert.
    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let dir = self.direction(token_address(token_in)?, token_address(token_out)?)?;
        let gas = match (self.venue(), dir) {
            (VenueKind::Swap, Direction::Sell) => GAS_SWAP_SELL,
            (VenueKind::Swap, Direction::Buy) => GAS_SWAP_BUY,
            (VenueKind::LeverUp, Direction::Sell) => GAS_LEVER_UP,
            (VenueKind::LeverUp, Direction::Buy) => return Err(Refusal::LeverDown.into()),
        };
        let empty =
            || GetAmountOutResult::new(BigUint::ZERO, BigUint::from(gas), Box::new(self.clone()));
        if amount_in == BigUint::ZERO {
            return Ok(empty());
        }
        let flamm = self.quotable()?;
        let amount = amount_to_u256(&amount_in)?;
        let f = match self.full_fill(flamm, dir, amount) {
            Ok(f) => f,
            Err(r) if self.is_dust(flamm, dir, amount, &r) => return Ok(empty()),
            Err(r) => return Err(r.into()),
        };
        let mut next = self.clone();
        if let Ok(core) = next.core.as_mut() {
            core.flamm = f.post;
        }
        Ok(GetAmountOutResult::new(u256_to_biguint(f.out), BigUint::from(f.gas), Box::new(next)))
    }

    /// The largest size of `sell_token` the venue fills in full and its output (`Self::limit`).
    ///
    /// The contract, exactly: every size the venue fills in full lies in `[1, limit]`; a size
    /// above `limit` is refused by [`ProtocolSim::get_amount_out`] (the pool clips it, a partial
    /// fill, or reverts `PriceBand`); inside `[1, limit]` every size answers, a fill or, for
    /// the dust the pool refuses, the empty trade. A sell of the pool asset fills from one base
    /// unit at every recorded block. A buy with the loan asset is dust below a few thousand
    /// base units (tenths of a cent): a payout that rounds to zero pool asset is `FillInvalid`,
    /// and a payout of one to six units whose quantization leaves the band around the checked
    /// cross (`FLAMMSwapLib.sol:233`) is `PriceBand`; the refused sizes are not an interval (at
    /// 51409000 the pool fills 4096 units and refuses 5000), so `get_amount_out` is the only
    /// test of a dust size. At every recorded block the largest refused buy is below a
    /// hundredth of a percent of the limit, so the sizes a consumer derives from the limit (the
    /// protocol test harness quotes 0.1%, 1% and 10% of it) fill in full. `(0, 0)` for a
    /// direction the venue does not trade (the lever-up venue's loan asset in) and for a state
    /// that fills no size.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let flamm = self.quotable()?;
        let sell = address_of("sell_token", &sell_token)
            .map_err(|e| SimulationError::InvalidInput(format!("flamm: {e:?}"), None))?;
        let buy = address_of("buy_token", &buy_token)
            .map_err(|e| SimulationError::InvalidInput(format!("flamm: {e:?}"), None))?;
        let dir = self.direction(sell, buy)?;
        if self.venue() == VenueKind::LeverUp && dir != Direction::Sell {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let (a, out) = self.limit(flamm, dir)?;
        Ok((u256_to_biguint(a), u256_to_biguint(out)))
    }

    /// Applies the streamed attribute changes (updates and deletions) and rebuilds the state
    /// from the attributes. The decoder's `block_number` / `block_timestamp` carry the observed
    /// block. A value of the wrong width is a transition error; an attribute set that no longer
    /// decodes (a feed's rounds deleted by a rotation, a pin no longer met) leaves the component
    /// alive and refusing until a later delta restores it.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let attrs = Arc::make_mut(&mut self.attrs);
        for (name, value) in delta.updated_attributes {
            match name.as_str() {
                "block_number" => self.block = u64::from(value),
                "block_timestamp" => {}
                _ => {
                    attrs.insert(name, value);
                }
            }
        }
        for name in delta.deleted_attributes {
            attrs.remove(&name);
        }
        self.core = match decode_core(&self.statics, attrs) {
            Ok(core) => Ok(core),
            Err(DecodeError::Malformed(m)) => {
                return Err(TransitionError::DecodeError(format!("flamm: malformed attribute: {m}")))
            }
            Err(e) => Err(e),
        };
        self.refresh_oracle();
        Ok(())
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    /// Moves the execution clock to the block's timestamp and re-evaluates the Morpho market
    /// oracle there. Returns whether a quote can observe the move, which is when something a
    /// quote reads at the clock differs between the two clocks (`ClockSignature`):
    ///
    /// - a deadline lies between them: a feed's heartbeat (`PriceFeed._usd`), the sequencer's
    ///   grace, the spread's `maxSpreadAge`, the Morpho oracle's reveal cutoff (a different round
    ///   answers), a scheduled change's `executableAt`, the account's IRM grace, or the rate
    ///   ceiling's verdict flipping between admitting every borrow and none;
    /// - or a quote is a continuous function of the clock: a live venue in which the pool holds
    ///   debt or supply shares of a market that is borrowed from (the accrued totals value them
    ///   anew every second, and they enter the gate's book, the funding plan and the settlement),
    ///   or a rate ceiling that binds inside the borrowable range (the admitted slice moves with
    ///   the IRM's adaptation every second). For such a state every clock advance is a change, and
    ///   the state is re-emitted on every block.
    ///
    /// A pool with no position in any venue and no binding ceiling is quiet between deadlines:
    /// its fills are the hook's arithmetic over the pool's own words, which the clock does not
    /// touch, and the Morpho market's own accrual (the other users' interest) changes no read of
    /// its quotes. What the flag does not cover is the post-state of a quote that touched
    /// Morpho: it carries the market accrued at the clock, and a second quote on it reads its
    /// then-position there. A repeated timestamp (the flashblocks of one block) is a no-op; a
    /// state that does not decode compares equal at every clock.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let timestamp = block.timestamp();
        if timestamp == self.clock {
            return false;
        }
        let before = self.clock_signature(self.clock);
        self.clock = timestamp;
        self.refresh_oracle();
        let after = self.clock_signature(timestamp);
        before != after
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_gas_grows_with_solves_and_passes() {
        let r = SwapResult { pool_asset_in: true, cap_evals: 3, passes: 1, ..Default::default() };
        assert_eq!(swap_gas(&r), GAS_SWAP_SELL + 3 * GAS_SWAP_SELL_CAP_EVAL);
        let r = SwapResult { pool_asset_in: true, cap_evals: 0, passes: 4, ..Default::default() };
        assert_eq!(swap_gas(&r), GAS_SWAP_SELL + 3 * GAS_SWAP_SELL_PASS);
        let r = SwapResult { pool_asset_in: false, cap_evals: 9, passes: 4, ..Default::default() };
        assert_eq!(swap_gas(&r), GAS_SWAP_BUY);
        assert_eq!(lever_gas(&LeverResult::default()), GAS_LEVER_UP);
    }
}
