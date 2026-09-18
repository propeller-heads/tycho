// Copyright (c) 2026 Everlong Labs Limited

//! `FLAMMLeverLib` (c104 @ `80abd43`, `src/core/flamm/FLAMMLeverLib.sol`): the leverage venue. Core
//! builds the frame and the credit-zero room, resolves the spread through a low-level staticcall to
//! the spread hook, asks the stateless leverage hook for the fill, and checks it against the
//! checked feed (value leak, the taker band, the room, the CR floor and the Router's funding for a
//! lever-up; the ceiling, the concession and the band for a lever-down) before settling through the
//! swap route's Router bundles. The swap hook's fee and invariant are never called and its stored
//! book never moves: the next swap's lazy rescale absorbs the fill. Port of `lever.go`.

use alloy::primitives::U256;

use super::{
    context::{LeverContext, PoolContext},
    deps::{LeverageHook, Router, SwapHook},
    error::FlammError,
    gate::{self, Book, Pool},
    levhook::{LevBook, LevFill},
    math::{checked_add, checked_mul, div_ceil, mul_div, PPM, WAD},
    state::{priced, FlammState, FEATURE_LEVERAGE},
};

/// `FLAMMLeverLib.PHYSICAL_CR_FLOOR_WAD = 1.82e18` (`FLAMMLeverLib.sol:22`).
pub const LEVER_CR_FLOOR_WAD: U256 = U256::from_limbs([1_820_000_000_000_000_000, 0, 0, 0]);
/// `FLAMMLeverLib.LEV_SPREAD_FLOOR_PPM` (`FLAMMLeverLib.sol:23`).
pub const LEVER_SPREAD_FLOOR_PPM: U256 = U256::from_limbs([2_500, 0, 0, 0]);
/// `FLAMMLeverLib.LEV_SPREAD_CEILING_PPM` (`FLAMMLeverLib.sol:24`).
pub const LEVER_SPREAD_CEILING_PPM: U256 = U256::from_limbs([100_000, 0, 0, 0]);
/// `PPM + FLAMMLeverLib.LEV_MAX_CONCESSION_PPM` (`FLAMMLeverLib.sol:25`, `:138`).
pub const LEVER_MAX_CONCESSION_PPM: U256 = U256::from_limbs([1_010_000, 0, 0, 0]);
/// `swapPriceBandWad / 1e12` is the band in ppm (`FLAMMLeverLib.sol:175`).
const LEVER_BAND_TO_PPM: U256 = U256::from_limbs([1_000_000_000_000, 0, 0, 0]);

/// `FLAMMLeverLib.Plan` (`FLAMMLeverLib.sol:27`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeverPlan {
    pub price_wad: U256,
    pub ctx: LeverContext,
    pub fill: LevFill,
    /// The taker's real loan leg, L18: paid to them (up) or by them (down).
    pub pay_l18: U256,
    /// Up: loan asset native; down: poolAsset native.
    pub out: U256,
    /// Down only: the loan asset pulled from the taker.
    pub pay_native: U256,
    /// The hook answered; a degraded fill never becomes the next degrade value.
    pub spread_live: bool,
}

/// What `leverUp` / `leverDown` return and their `LeverUp` / `LeverDown` events carry
/// (`FLAMMStore.sol:141`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeverResult {
    pub up: bool,
    /// Up: poolAsset used; down: `payNative`.
    pub amount_in_used: U256,
    pub amount_out: U256,
    pub spread_ppm: U256,
    pub cr_after_wad: U256,
    /// The taker's loan leg the band was checked on (not part of the return).
    pub pay_l18: U256,
    /// The checked cross (not part of the return).
    pub price_wad: U256,
}

impl LeverPlan {
    /// The `(amountInUsed, amountOut, spreadPpm, crAfterWad)` of `preview`
    /// (`FLAMMLeverLib.sol:44`).
    pub fn result(&self, up: bool) -> LeverResult {
        LeverResult {
            up,
            amount_in_used: if up { self.fill.amount_in_used } else { self.pay_native },
            amount_out: self.out,
            spread_ppm: self.ctx.spread_ppm,
            cr_after_wad: self.fill.cr_after_wad,
            pay_l18: self.pay_l18,
            price_wad: self.price_wad,
        }
    }
}

impl<H: SwapHook, L: LeverageHook, R: Router> FlammState<H, L, R> {
    /// `FLAMMLeverLib.preview` (`FLAMMLeverLib.sol:37`), `FLAMM.previewLever`'s body.
    /// `amount_in_used` is the hook's used input up and `payNative` down.
    pub fn preview_lever(
        &self,
        up: bool,
        amount_in: U256,
        now: u64,
    ) -> Result<LeverResult, FlammError> {
        if amount_in.is_zero() {
            return Err(FlammError::InvalidAmount);
        }
        let mut pool = self.pool.clone();
        let p = self.lever_plan_for(&mut pool, up, amount_in, now)?;
        Ok(p.result(up))
    }

    fn lever_plan_for(
        &self,
        pool: &mut Pool,
        up: bool,
        amount_in: U256,
        now: u64,
    ) -> Result<LeverPlan, FlammError> {
        if up {
            self.lever_plan_up(pool, amount_in, now)
        } else {
            self.lever_plan_down(pool, amount_in, now)
        }
    }

    /// `FLAMMLeverLib.leverUp` (`FLAMMLeverLib.sol:47`) / `leverDown` (`:62`) as one transaction:
    /// the post-state is returned and the receiver is never written. A live spread is stored as
    /// the next degrade value before the fill; `executeLever` is `previewLever` on the same
    /// context (the hook is stateless), so `FillMismatch` cannot fire. A lever-up settles as a
    /// sell of the used poolAsset for `out`; a lever-down anchors the entry gate, settles as a
    /// buy of `out` poolAsset for `payNative` and re-asserts it.
    pub fn execute_lever(
        &self,
        up: bool,
        amount_in: U256,
        min_out: U256,
        deadline: u64,
        now: u64,
    ) -> Result<(LeverResult, Self), FlammError> {
        if now > deadline {
            return Err(FlammError::Expired);
        }
        if amount_in.is_zero() {
            return Err(FlammError::InvalidAmount);
        }
        let mut post = self.clone();
        // The plan prices the post-state's own ledger: the frame it writes is what the settlement
        // legs read.
        let mut pool = post.pool.clone();
        let p = post.lever_plan_for(&mut pool, up, amount_in, now)?;
        post.pool = pool;
        if p.out < min_out {
            return Err(FlammError::Slippage);
        }
        if p.spread_live {
            // `uint32(p.ctx.spreadPpm)` (FLAMMLeverLib.sol:182).
            post.last_lever_spread_ppm = p.ctx.spread_ppm & U256::from(u32::MAX);
        }
        if up {
            post.router.settle_sell_legs(
                &mut post.pool,
                0,
                p.price_wad,
                p.fill.amount_in_used,
                p.out,
                now,
            )?;
        } else {
            post.router
                .settle_buy_legs(&mut post.pool, 0, p.pay_native, p.out, now)?;
        }
        post.router.end_transaction();
        // A frame of this timestamp, not state.
        post.pool.price_wad.clear();
        post.pool.cross_wad.clear();
        Ok((p.result(up), post))
    }

    /// `FLAMMLeverLib._open` (`FLAMMLeverLib.sol:80`).
    pub fn lever_open(&self, up: bool, now: u64) -> Result<(), FlammError> {
        if !self.hooks.has_leverage() {
            return Err(FlammError::LeverageDisabled);
        }
        if self.lev_paused {
            return Err(FlammError::LevPaused);
        }
        self.feature(FEATURE_LEVERAGE)?;
        if up {
            if self.paused {
                return Err(FlammError::Paused);
            }
            if !self.peg_ok(0, now)? {
                return Err(FlammError::PegBroken);
            }
        }
        Ok(())
    }

    /// The shared head of `_planUp` / `_planDown` (`FLAMMLeverLib.sol:92-95`, `:121-124`): the
    /// checked cross, the priced book and its context.
    fn lever_frame(
        &self,
        pool: &mut Pool,
        p: &mut LeverPlan,
        now: u64,
    ) -> Result<(Book, PoolContext), FlammError> {
        let (price_wad, price_ts) = self.price(now)?;
        p.price_wad = price_wad;
        let b = priced(&self.feed, &self.router, pool, now)?;
        if b.legs.is_empty() {
            return Err(FlammError::PanicIndex);
        }
        let ctx = gate::context(&b, price_wad, price_ts, self.share_supply)?;
        Ok((b, ctx))
    }

    /// `ILeverageInvariantHook(leverageHook).previewLever(p.ctx)` (`FLAMMLeverLib.sol:100`,
    /// `:128`): the leverage hook's quote over the pool's swap hook's book (`hook_kinds.go`
    /// `everlongLeverageV1.previewLever`: `EverlongHook.bookFor(ctx.pool)` and
    /// `reservationPriceWad()`, `EverlongLeverageHook.sol:37`, `:48-52`).
    fn lever_fill(&self, ctx: &LeverContext) -> Result<LevFill, FlammError> {
        let lev = self.hooks.leverage.port()?;
        let swap = self.hooks.swap.port()?;
        let hb = swap.book_for(&ctx.pool)?;
        let book = LevBook {
            rs: hb.rs,
            is: hb.is,
            rv: hb.rv,
            iv: hb.iv,
            reservation_price_wad: swap.reservation_price(),
        };
        lev.preview_lever(ctx, &book)
    }

    /// `FLAMMLeverLib._planUp` (`FLAMMLeverLib.sol:90`).
    pub fn lever_plan_up(
        &self,
        pool: &mut Pool,
        vol_in: U256,
        now: u64,
    ) -> Result<LeverPlan, FlammError> {
        self.lever_open(true, now)?;
        let mut p = LeverPlan::default();
        let (b, ctx) = self.lever_frame(pool, &mut p, now)?;
        let leg = &b.legs[0];
        let head = self.lever_room(pool, &b)?;
        p.ctx = LeverContext {
            pool: ctx,
            up: true,
            spread_ppm: U256::ZERO,
            amount_in: vol_in,
            max_out: head,
        };
        let (spread, live) = self.lever_spread(pool, true, now)?;
        p.ctx.spread_ppm = spread;
        p.spread_live = live;
        let f = self.lever_fill(&p.ctx)?;
        if f.amount_in_used.is_zero() ||
            f.amount_in_used > vol_in ||
            f.gross_out <= f.virtual_leg_l18
        {
            return Err(FlammError::FillInvalid);
        }
        p.fill = f;
        let f = &p.fill;
        if leg.scale.is_zero() {
            return Err(FlammError::PanicDivZero);
        }
        p.out = (f.gross_out - f.virtual_leg_l18) / leg.scale;
        if p.out.is_zero() {
            return Err(FlammError::FillInvalid);
        }
        p.pay_l18 = p.out * leg.scale; // <= grossOut - virtualLeg
        let value = checked_mul(f.amount_in_used, p.price_wad)?;
        // The book's NAV change at the feed is `volIn * price - paid`: never less than the spread.
        let leak = mul_div(value, PPM - p.ctx.spread_ppm, PPM)?; // spreadPpm < PPM
        if p.pay_l18 > leak {
            return Err(FlammError::LevValueLeak);
        }
        lever_band_floor(p.pay_l18, value, pool.loans[0].swap_price_band_wad)?;
        // Credit zero, all or nothing: the incoming poolAsset earns no room.
        if p.pay_l18 > head {
            return Err(FlammError::RoomExceeded);
        }
        if f.cr_after_wad < LEVER_CR_FLOOR_WAD {
            return Err(FlammError::LevBelowFloor);
        }
        let rest = if p.out > leg.liquid { p.out - leg.liquid } else { U256::ZERO };
        let coll = checked_add(ctx.physical_pool_asset, f.amount_in_used)?;
        let funding = self
            .router
            .funding_ceiling(0, coll, p.price_wad, now)?;
        if rest > funding {
            return Err(FlammError::OutputAboveCeiling);
        }
        Ok(p)
    }

    /// `FLAMMLeverLib._planDown` (`FLAMMLeverLib.sol:119`).
    pub fn lever_plan_down(
        &self,
        pool: &mut Pool,
        loan_in: U256,
        now: u64,
    ) -> Result<LeverPlan, FlammError> {
        self.lever_open(false, now)?;
        let mut p = LeverPlan::default();
        let (b, ctx) = self.lever_frame(pool, &mut p, now)?;
        let leg = &b.legs[0];
        let in_l18 = checked_mul(loan_in, leg.scale)?;
        let max_out = checked_add(ctx.physical_pool_asset, ctx.posted_pool_asset)?;
        p.ctx = LeverContext {
            pool: ctx,
            up: false,
            spread_ppm: U256::ZERO,
            amount_in: in_l18,
            max_out,
        };
        let (spread, live) = self.lever_spread(pool, false, now)?;
        p.ctx.spread_ppm = spread;
        p.spread_live = live;
        let f = self.lever_fill(&p.ctx)?;
        if f.amount_in_used.is_zero() ||
            f.amount_in_used > in_l18 ||
            f.amount_in_used <= f.virtual_leg_l18 ||
            f.gross_out.is_zero()
        {
            return Err(FlammError::FillInvalid);
        }
        if f.gross_out > max_out {
            return Err(FlammError::OutputAboveCeiling);
        }
        p.fill = f;
        let f = &p.fill;
        p.out = f.gross_out;
        p.pay_l18 = f.amount_in_used - f.virtual_leg_l18;
        let out_value = checked_mul(f.gross_out, p.price_wad)?;
        // The venue never releases more than the concession beyond the value it takes in, at the
        // feed.
        let concession = mul_div(p.pay_l18, LEVER_MAX_CONCESSION_PPM, PPM)?;
        if out_value > concession {
            return Err(FlammError::LevValueLeak);
        }
        lever_band_floor(out_value, p.pay_l18, pool.loans[0].swap_price_band_wad)?;
        p.pay_native = div_ceil(p.pay_l18, leg.scale)?;
        if p.pay_native > loan_in {
            p.pay_native = loan_in;
        }
        Ok(p)
    }

    /// `FLAMMLeverLib._room` (`FLAMMLeverLib.sol:147`): loan asset 0's credit-zero headroom (L18)
    /// less the epsilon shave; no phi lift, since the incoming poolAsset is not credited.
    pub fn lever_room(&self, pool: &Pool, b: &Book) -> Result<U256, FlammError> {
        let u = gate::exposure_pw(b)?;
        let head = gate::head_of(b, 0, u, pool.ltv_wad)?;
        let shave = mul_div(head, pool.room_epsilon_wad, WAD)?;
        if shave > head {
            return Err(FlammError::PanicArithmetic);
        }
        Ok(head - shave)
    }

    /// `FLAMMLeverLib._spread` (`FLAMMLeverLib.sol:158`). No answer (no spread hook, a stale post,
    /// or `ppm >= PPM`) fails a lever-up closed and degrades a lever-down to the last live
    /// spread (the venue ceiling before any live fill), unclamped. A live answer is clamped
    /// into `[LEV_SPREAD_FLOOR_PPM, min(swapPriceBandWad / 1e12, LEV_SPREAD_CEILING_PPM)]`.
    /// Returns `(spreadPpm, live)`.
    pub fn lever_spread(
        &self,
        pool: &Pool,
        up: bool,
        now: u64,
    ) -> Result<(U256, bool), FlammError> {
        let (ok, mut sp) = self.hooks.spread.spread_ppm(now)?;
        if !ok || sp >= PPM {
            if up {
                return Err(FlammError::SpreadUnavailable);
            }
            if self.last_lever_spread_ppm.is_zero() {
                return Ok((LEVER_SPREAD_CEILING_PPM, false));
            }
            return Ok((self.last_lever_spread_ppm, false));
        }
        if sp < LEVER_SPREAD_FLOOR_PPM {
            sp = LEVER_SPREAD_FLOOR_PPM;
        }
        let Some(loan0) = pool.loans.first() else {
            return Err(FlammError::PanicIndex);
        };
        let mut ceiling = loan0.swap_price_band_wad / LEVER_BAND_TO_PPM;
        if ceiling > LEVER_SPREAD_CEILING_PPM {
            ceiling = LEVER_SPREAD_CEILING_PPM;
        }
        if sp > ceiling {
            sp = ceiling;
        }
        Ok((sp, true))
    }
}

/// The one-sided taker band `got < mulDiv(worth, WAD - band, WAD)` reverting `PriceBand`
/// (`FLAMMLeverLib.sol:109`, `:139`).
pub fn lever_band_floor(got: U256, worth: U256, band_wad: U256) -> Result<(), FlammError> {
    if band_wad > WAD {
        return Err(FlammError::PanicArithmetic);
    }
    let floor = mul_div(worth, WAD - band_wad, WAD)?;
    if got < floor {
        return Err(FlammError::PriceBand);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            deps::EverlongLeverageV1,
            gate::LoanCfg,
            hook::HookState,
            router::Router as MmRouter,
            state::{HookKind, SpreadHookSlot, SpreadHookState},
        },
        *,
    };

    type State = FlammState<HookState, EverlongLeverageV1, MmRouter>;

    fn w(x: u64) -> U256 {
        U256::from(x)
    }

    fn state_with_post(spread: u64, age: u64, last: u64, band_wad: u64) -> State {
        let mut s = State::default();
        s.hooks.spread = SpreadHookSlot {
            kind: HookKind::EverlongSpreadV1,
            everlong_spread: Some(SpreadHookState {
                spread: w(spread),
                max_spread_age: w(age),
                last_set_ts: w(last),
            }),
        };
        s.pool
            .loans
            .push(LoanCfg { swap_price_band_wad: w(band_wad), ..Default::default() });
        s
    }

    #[test]
    fn spread_clamps_a_live_answer_into_the_venue_bounds() {
        let band = 80_000_000_000_000_000u64; // 8% -> 80000 ppm ceiling
        let now = 1_003_600u64;
        let s = state_with_post(17_500, 3600, 1_000_000, band);
        assert_eq!(s.lever_spread(&s.pool, true, now), Ok((w(17_500), true)));
        // Above the band: clamped to the band (90000 -> 80000), live.
        let s = state_with_post(90_000, 3600, 1_000_000, band);
        assert_eq!(s.lever_spread(&s.pool, true, now), Ok((w(80_000), true)));
        // A band wider than the venue ceiling: 100000.
        let s = state_with_post(150_000, 3600, 1_000_000, 200_000_000_000_000_000);
        assert_eq!(s.lever_spread(&s.pool, false, now), Ok((w(100_000), true)));
        // Below the floor: 2500. A band whose ceiling (1500) sits under the floor: floor first,
        // then the ceiling wins (armed_band_tiny).
        let s = state_with_post(1, 3600, 1_000_000, band);
        assert_eq!(s.lever_spread(&s.pool, true, now), Ok((w(2_500), true)));
        let s = state_with_post(2_500, 3600, 1_000_000, 1_500_000_000_000_000);
        assert_eq!(s.lever_spread(&s.pool, true, now), Ok((w(1_500), true)));
        // Exactly the band: unchanged.
        let s = state_with_post(80_000, 3600, 1_000_000, band);
        assert_eq!(s.lever_spread(&s.pool, true, now), Ok((w(80_000), true)));
    }

    #[test]
    fn spread_degrades_a_lever_down_and_fails_a_lever_up_closed() {
        let band = 80_000_000_000_000_000u64;
        // Stale by one second: no answer.
        let mut s = state_with_post(17_500, 3600, 1_000_000, band);
        let now = 1_003_601u64;
        assert_eq!(s.lever_spread(&s.pool, true, now), Err(FlammError::SpreadUnavailable));
        assert_eq!(s.lever_spread(&s.pool, false, now), Ok((LEVER_SPREAD_CEILING_PPM, false)));
        s.last_lever_spread_ppm = w(17_500);
        assert_eq!(s.lever_spread(&s.pool, false, now), Ok((w(17_500), false)));
        // The degrade value is unclamped: a stored value under the floor or over the band is
        // returned as is.
        s.last_lever_spread_ppm = w(1);
        assert_eq!(s.lever_spread(&s.pool, false, now), Ok((w(1), false)));
        // A ppm at PPM is no answer even when the post is fresh.
        let s = state_with_post(1_000_000, 0, 1_000_000, band);
        assert_eq!(s.lever_spread(&s.pool, true, now), Err(FlammError::SpreadUnavailable));
        assert_eq!(s.lever_spread(&s.pool, false, now), Ok((LEVER_SPREAD_CEILING_PPM, false)));
        // No spread role at all: the staticcall to address(0) returns nothing.
        let mut s = state_with_post(17_500, 0, 1_000_000, band);
        s.hooks.spread = SpreadHookSlot::default();
        assert_eq!(s.lever_spread(&s.pool, true, now), Err(FlammError::SpreadUnavailable));
        assert_eq!(s.lever_spread(&s.pool, false, now), Ok((LEVER_SPREAD_CEILING_PPM, false)));
        // maxSpreadAge 0 never goes stale.
        let s = state_with_post(17_500, 0, 1, band);
        assert_eq!(s.lever_spread(&s.pool, true, u64::MAX), Ok((w(17_500), true)));
    }

    #[test]
    fn open_gates_in_order() {
        let mut s = state_with_post(17_500, 0, 1, 0);
        // No leverage role.
        assert_eq!(s.lever_open(true, 0), Err(FlammError::LeverageDisabled));
        s.hooks.leverage.kind = HookKind::EverlongLeverageV1;
        s.hooks.leverage.everlong_leverage = Some(EverlongLeverageV1 {});
        s.lev_paused = true;
        assert_eq!(s.lever_open(false, 0), Err(FlammError::LevPaused));
        s.lev_paused = false;
        assert_eq!(s.lever_open(false, 0), Err(FlammError::FeatureDisabled));
        s.pool.features = w(1 << 5);
        assert_eq!(s.lever_open(false, 0), Ok(()));
        // Up: paused, then the peg (loan 0 must exist in the feed).
        s.paused = true;
        assert_eq!(s.lever_open(true, 0), Err(FlammError::Paused));
        s.paused = false;
        assert_eq!(s.lever_open(true, 0), Err(FlammError::PanicIndex));
        s.feed
            .loans
            .push(super::super::pricefeed::FeedToken { known: true, ..Default::default() });
        assert_eq!(s.lever_open(true, 0), Ok(())); // a zero peg band is always ok
    }

    #[test]
    fn pay_native_is_ceiled_then_capped_at_the_input() {
        let scale = w(1_000_000_000_000);
        // payL18 not on the native grid: one more native unit.
        assert_eq!(div_ceil(w(11_301_759) * scale + w(1), scale), Ok(w(11_301_760)));
        assert_eq!(div_ceil(w(11_301_759) * scale, scale), Ok(w(11_301_759)));
        // The cap: `if (p.payNative > loanIn) p.payNative = loanIn` (FLAMMLeverLib.sol:141).
        let loan_in = w(11_301_759);
        let mut pay = div_ceil(loan_in * scale + w(1), scale).unwrap();
        if pay > loan_in {
            pay = loan_in;
        }
        assert_eq!(pay, loan_in);
    }

    #[test]
    fn taker_band_floor_is_inclusive() {
        let band = w(80_000_000_000_000_000);
        let worth = w(1_000_000) * WAD;
        let floor = worth * (WAD - band) / WAD;
        assert!(lever_band_floor(floor, worth, band).is_ok());
        assert_eq!(lever_band_floor(floor - w(1), worth, band), Err(FlammError::PriceBand));
        assert!(lever_band_floor(U256::MAX, worth, band).is_ok());
        assert_eq!(lever_band_floor(w(1), w(1), WAD + w(1)), Err(FlammError::PanicArithmetic));
    }

    #[test]
    fn lever_result_words() {
        let p = LeverPlan {
            fill: LevFill { amount_in_used: w(5), cr_after_wad: w(9), ..Default::default() },
            out: w(7),
            pay_native: w(6),
            ctx: LeverContext { spread_ppm: w(3), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(p.result(true).amount_in_used, w(5));
        assert_eq!(p.result(false).amount_in_used, w(6));
        assert_eq!(p.result(true).amount_out, w(7));
        assert_eq!(p.result(true).spread_ppm, w(3));
        assert_eq!(p.result(true).cr_after_wad, w(9));
    }
}
