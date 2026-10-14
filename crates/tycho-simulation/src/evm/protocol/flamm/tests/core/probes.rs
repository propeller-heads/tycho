// Copyright (c) 2026 Everlong Labs Limited

//! Semantic probes over scripted doubles, beside the fixture replays: the degrade value is NOT
//! written on a degraded lever-down when it is still zero (`FLAMMLeverLib.sol:182`); a two-loan
//! pool's swap uses the pair's own peg / cross (`PegBroken` before `PriceUnchecked` on a sell,
//! `PriceUnchecked` alone on a buy); the feed ages with `now`, not with the snapshot timestamp;
//! the spread ceiling floors the band; the lever-up pay leg is floored to the native grid and
//! `payNative` is ceiled then capped.
//!
//! The chain's revert class against the port's outcome is not tabulated here: `e2e_preview_grids`
//! (`core/e2e.rs`) already walks every row of the same three grids through `Report::compare`,
//! which fails on any class disagreement and on the returned words as well.

use alloy::primitives::{Address, U256};

use crate::evm::protocol::flamm::{
    context::{LeverContext, PoolContext, SwapContext},
    deps::{LeverageHook, Router, SwapHook},
    gate::{GateReads, LoanCfg, Pool},
    hook::{Book, FillResult},
    levhook::{LevBook, LevFill},
    math::WAD,
    pricefeed::{FeedRound, FeedToken, PriceFeedState},
    router::{Positions, Quarantine},
    state::{HookKind, LeverageHookSlot, PoolHooks, SpreadHookSlot, SpreadHookState, SwapHookSlot},
    FlammError, FlammState,
};

fn w(x: u64) -> U256 {
    U256::from(x)
}

const PRICE: u64 = 10;
const NOW: u64 = 2_000_000;
const CBBTC: Address = Address::repeat_byte(0xcb);
const USDC: Address = Address::repeat_byte(0x83);
const USDT: Address = Address::repeat_byte(0x84);

#[derive(Clone, Debug, PartialEq, Eq)]
struct FlatHook;

impl SwapHook for FlatHook {
    fn spot(&self) -> Result<U256, FlammError> {
        Ok(w(PRICE))
    }

    fn preview_fee_wad(&self, _ctx: &SwapContext) -> Result<U256, FlammError> {
        Ok(U256::ZERO)
    }

    fn fill(&self, ctx: &SwapContext, _fee_wad: U256) -> Result<(FillResult, Book), FlammError> {
        let (used, gross) = if ctx.pool_asset_in {
            let gross = (ctx.amount_in * w(PRICE)).min(ctx.max_amount_out);
            (gross.div_ceil(w(PRICE)), gross)
        } else {
            let gross = (ctx.amount_in / w(PRICE)).min(ctx.max_amount_out);
            (gross * w(PRICE), gross)
        };
        Ok((
            FillResult {
                amount_in_used: used,
                gross_out: gross,
                fee_out: U256::ZERO,
                spot_after_wad: w(PRICE),
                cap_evals: 0,
            },
            Book::default(),
        ))
    }

    fn commit(&mut self, _book: &Book) {}

    fn book_for(&self, ctx: &PoolContext) -> Result<Book, FlammError> {
        Ok(Book { rs: ctx.liquid_loan_asset, rv: ctx.physical_pool_asset, ..Default::default() })
    }

    fn reservation_price(&self) -> U256 {
        w(PRICE) * WAD
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FixedLever(Result<LevFill, FlammError>);

impl LeverageHook for FixedLever {
    fn preview_lever(&self, _ctx: &LeverContext, _book: &LevBook) -> Result<LevFill, FlammError> {
        self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NoopRouter {
    loans: usize,
    calls: Vec<String>,
}

impl GateReads for NoopRouter {
    fn positions(&self, _now: u64) -> Result<Positions, FlammError> {
        Ok(Positions {
            coll: vec![U256::ZERO; self.loans],
            sup: vec![U256::ZERO; self.loans],
            debt: vec![U256::ZERO; self.loans],
            total_coll: U256::ZERO,
        })
    }

    fn quarantine(&self, _idx: u8, _now: u64) -> Result<Quarantine, FlammError> {
        Ok(Quarantine::default())
    }
}

impl Router for NoopRouter {
    fn funding_ceiling(&self, _: u8, _: U256, _: U256, _: u64) -> Result<U256, FlammError> {
        Ok(w(1_000_000))
    }

    fn settle_sell_legs(
        &mut self,
        _pool: &mut Pool,
        idx: u8,
        _price_wad: U256,
        used: U256,
        net: U256,
        _now: u64,
    ) -> Result<(), FlammError> {
        self.calls
            .push(format!("sell idx={idx} used={used} net={net}"));
        Ok(())
    }

    fn settle_buy_legs(
        &mut self,
        _pool: &mut Pool,
        idx: u8,
        used: U256,
        net: U256,
        _now: u64,
    ) -> Result<(), FlammError> {
        self.calls
            .push(format!("buy idx={idx} used={used} net={net}"));
        Ok(())
    }

    fn end_transaction(&mut self) {}
}

type State = FlammState<FlatHook, FixedLever, NoopRouter>;

fn round(answer: u64, at: u64) -> FeedRound {
    FeedRound { ok: true, round_id: w(1), answer: w(answer), started_at: w(at), updated_at: w(at) }
}

fn loan_cfg(token: Address) -> LoanCfg {
    LoanCfg {
        token,
        scale: w(1),
        swap_price_band_wad: w(80_000_000_000_000_000),
        fee_floor_wad: U256::ZERO,
        max_swap_notional: U256::ZERO,
        reserve_target: U256::ZERO,
        liquid: w(100),
    }
}

fn feed_token(answer: u64, at: u64, peg_band: u64) -> FeedToken {
    FeedToken {
        known: true,
        heartbeat: w(3600),
        scale: WAD,
        unit: w(1),
        peg_band_wad: w(peg_band),
        round: round(answer, at),
    }
}

/// A pool with `loans` loan assets (USDC, then USDT), feed at exactly `PRICE`, every feature on, a
/// live spread of 17500, a leverage role, `lastLeverSpreadPpm` zero.
fn state(loans: usize) -> State {
    let tokens = [USDC, USDT];
    State {
        block: 1,
        timestamp: NOW,
        pool_asset: CBBTC,
        pool: Pool {
            physical: w(1_000_000),
            loans: tokens[..loans]
                .iter()
                .map(|t| loan_cfg(*t))
                .collect(),
            ltv_wad: WAD / w(2),
            phi_wad: U256::ZERO,
            room_epsilon_wad: U256::ZERO,
            features: w(0b110_1111),
            price_wad: vec![],
            cross_wad: vec![],
        },
        paused: false,
        lev_paused: false,
        fee_floor_wad: U256::ZERO,
        fee_cap_wad: WAD / w(10),
        share_supply: w(1),
        last_lever_spread_ppm: U256::ZERO,
        hooks: PoolHooks {
            swap: SwapHookSlot { kind: HookKind::EverlongSwapV1, everlong_swap: Some(FlatHook) },
            leverage: LeverageHookSlot {
                kind: HookKind::EverlongLeverageV1,
                everlong_leverage: Some(FixedLever(Err(FlammError::Unsupported))),
            },
            spread: SpreadHookSlot {
                kind: HookKind::EverlongSpreadV1,
                everlong_spread: Some(SpreadHookState {
                    spread: w(17_500),
                    max_spread_age: w(3600),
                    last_set_ts: w(NOW - 100),
                }),
            },
        },
        router: NoopRouter { loans, calls: vec![] },
        feed: PriceFeedState {
            has_sequencer: false,
            sequencer_grace: U256::ZERO,
            sequencer: FeedRound::default(),
            asset: FeedToken { scale: w(1), ..feed_token(PRICE, NOW - 60, 0) },
            loans: (0..loans)
                .map(|_| feed_token(1, NOW - 60, 10_000_000_000_000_000))
                .collect(),
        },
    }
}

/// `FLAMMLeverLib._execute` (`:182`): only a live spread is stored. Before the venue has ever
/// filled (`lastLeverSpreadPpm == 0`) a stale post degrades a lever-down to 100000 ppm and the
/// stored value must stay zero afterwards.
#[test]
fn degraded_lever_down_never_writes_the_degrade_value() {
    let mut s = state(1);
    s.hooks
        .spread
        .everlong_spread
        .as_mut()
        .unwrap()
        .last_set_ts = w(NOW - 3601);
    s.hooks.leverage.everlong_leverage = Some(FixedLever(Ok(LevFill {
        amount_in_used: w(500),
        gross_out: w(42),
        virtual_leg_l18: w(50),
        cr_after_wad: w(2_000_000_000_000_000_000),
    })));
    assert_eq!(s.last_lever_spread_ppm, U256::ZERO);
    let (r, post) = s
        .execute_lever(false, w(500), w(0), NOW, NOW)
        .unwrap();
    assert_eq!(r.spread_ppm, w(100_000), "the venue ceiling before any live fill");
    assert_eq!(post.last_lever_spread_ppm, U256::ZERO, "a degraded fill is never stored");
    // And a lever-up on the same stale post fails closed.
    assert_eq!(
        s.execute_lever(true, w(500), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::SpreadUnavailable
    );
}

/// `FLAMMSwapLib._plan` (`:140-147`): the pair's own peg (`pegOk($, idx)`) and its own frame
/// (`b.legs[idx]`). On a two-loan pool with loan 1's round stale: a sell to loan 1 is `PegBroken`
/// (checked before `PriceUnchecked`), a buy with loan 1 is `PriceUnchecked` (no peg check on a
/// buy), and both directions on loan 0 still price.
#[test]
fn two_loan_pool_uses_the_pairs_own_peg_and_frame() {
    let mut s = state(2);
    s.feed.loans[1].round.updated_at = w(NOW - 3601); // stale: usd(loan1) reverts StalePrice
                                                      // A positioned leg at an unchecked cross fails `context` closed (FLAMMGateLib.sol:238) for
                                                      // EVERY route, so leg 1 is left idle here; the positioned case is asserted at the end.
    s.pool.loans[1].liquid = U256::ZERO;
    assert_eq!(s.pair(CBBTC, USDT), Ok((1, true)));
    assert_eq!(s.pair(USDT, CBBTC), Ok((1, false)));
    // loan 0 both ways still prices (cross_wad[0] = WAD, price_wad[0] from the fresh rounds).
    s.execute_swap(CBBTC, USDC, w(10), w(0), NOW, NOW)
        .unwrap();
    s.execute_swap(USDC, CBBTC, w(100), w(0), NOW, NOW)
        .unwrap();
    // loan 1: sell -> PegBroken (pegOk(1) reads loan 1's stale round), buy -> PriceUnchecked.
    assert_eq!(
        s.execute_swap(CBBTC, USDT, w(10), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::PegBroken
    );
    assert_eq!(
        s.execute_swap(USDT, CBBTC, w(100), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::PriceUnchecked
    );
    // With loan 1's peg band zero the sell reaches PriceUnchecked too (pegOk short-circuits).
    s.feed.loans[1].peg_band_wad = U256::ZERO;
    assert_eq!(
        s.execute_swap(CBBTC, USDT, w(10), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::PriceUnchecked
    );
    // A positioned leg 1 at an unchecked cross fails `context` closed on the loan-0 routes too
    // (FLAMMGateLib.sol:238), after `price` and the peg.
    s.pool.loans[1].liquid = w(1);
    assert_eq!(
        s.execute_swap(CBBTC, USDC, w(10), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::PriceUnchecked
    );
    assert_eq!(s.preview_lever(false, w(100), NOW), Err(FlammError::PriceUnchecked));
    s.pool.loans[1].liquid = U256::ZERO;
    // Sanity: the frame itself is computed per leg.
    let mut pool = s.pool.clone();
    let b = crate::evm::protocol::flamm::state::priced(&s.feed, &s.router, &mut pool, NOW).unwrap();
    assert_eq!(b.legs[0].cross_wad, WAD);
    assert!(!b.legs[0].price_wad.is_zero());
    assert!(b.legs[1].price_wad.is_zero());
    assert!(b.legs[1].cross_wad.is_zero());
    assert_eq!(pool.price_wad.len(), 2);
    assert_eq!(pool.cross_wad, vec![WAD, U256::ZERO]);
}

/// The feed ages with the call's `now`, never with the snapshot's `timestamp`: the same state
/// quotes at `timestamp + heartbeat` and refuses `StalePrice` one second later; the spread post
/// answers through `lastSetTs + maxSpreadAge` inclusive.
#[test]
fn quotes_age_with_now_not_with_the_snapshot() {
    let mut s = state(1);
    s.hooks.leverage.everlong_leverage = Some(FixedLever(Ok(LevFill {
        amount_in_used: w(100),
        gross_out: w(1050),
        virtual_leg_l18: w(100),
        cr_after_wad: w(1_820_000_000_000_000_000),
    })));
    let updated = NOW - 60;
    let last_ok = updated + 3600;
    assert!(s
        .preview_swap(true, w(10), last_ok)
        .is_ok());
    assert_eq!(s.preview_swap(true, w(10), last_ok + 1), Err(FlammError::StalePrice));
    assert_eq!(s.preview_swap(false, w(100), last_ok + 1), Err(FlammError::StalePrice));
    // The lever-up: peg first (a stale usd is "not ok" -> PegBroken), before the checked cross.
    assert_eq!(s.preview_lever(true, w(100), last_ok + 1), Err(FlammError::PegBroken));
    // The lever-down has no peg check: the checked cross reverts StalePrice.
    assert_eq!(s.preview_lever(false, w(100), last_ok + 1), Err(FlammError::StalePrice));
    // Spread post: lastSetTs NOW-100, age 3600 -> answers through NOW+3500.
    s.feed.asset.heartbeat = w(1 << 40);
    s.feed.loans[0].heartbeat = w(1 << 40);
    assert!(s
        .preview_lever(true, w(100), NOW + 3500)
        .is_ok());
    assert_eq!(s.preview_lever(true, w(100), NOW + 3501), Err(FlammError::SpreadUnavailable));
    // Expired uses now > deadline, inclusive at equality.
    assert!(s
        .execute_lever(true, w(100), w(0), NOW, NOW)
        .is_ok());
    assert_eq!(
        s.execute_lever(true, w(100), w(0), NOW - 1, NOW)
            .unwrap_err(),
        FlammError::Expired
    );
}

/// `FLAMMLeverLib._spread` (`:175`): the ceiling is `swapPriceBandWad / 1e12`, floored; a band of
/// `2_500_000_000_000_001` clamps a live 17500 to 2500 (floor 2500 first, then the ceiling).
#[test]
fn spread_ceiling_floors_the_band() {
    let mut s = state(1);
    s.pool.loans[0].swap_price_band_wad = w(2_500_000_000_000_001);
    assert_eq!(s.lever_spread(&s.pool, true, NOW), Ok((w(2_500), true)));
    s.pool.loans[0].swap_price_band_wad = w(2_499_999_999_999_999);
    // ceiling 2499 < floor 2500: the ceiling wins (clamped after the floor).
    assert_eq!(s.lever_spread(&s.pool, true, NOW), Ok((w(2_499), true)));
    s.pool.loans[0].swap_price_band_wad = U256::ZERO;
    assert_eq!(s.lever_spread(&s.pool, true, NOW), Ok((U256::ZERO, true)));
}

/// `FLAMMLeverLib._planUp` (`:103-105`): `out = (grossOut - virtualLeg) / scale`,
/// `payL18 = out * scale` -- the taker's leg is floored to the native grid, and the value-leak /
/// band / room checks run on the floored `payL18`, not on `grossOut - virtualLeg`.
#[test]
fn lever_up_pay_leg_is_floored_to_the_native_grid() {
    let mut s = state(1);
    s.pool.loans[0].scale = w(1000);
    // grossOut - virtualLeg = 950_999 -> out = 950, payL18 = 950_000.
    s.hooks.leverage.everlong_leverage = Some(FixedLever(Ok(LevFill {
        amount_in_used: w(100_000),
        gross_out: w(951_099),
        virtual_leg_l18: w(100),
        cr_after_wad: w(1_820_000_000_000_000_000),
    })));
    // value = 100_000 * 10 = 1_000_000; leak ceiling at 17500 ppm = 982_500 >= 950_000; band floor
    // 920_000 <= 950_000; room: bound = 1_000_000 * 0.5 WAD / WAD * price... head large.
    let r = s
        .preview_lever(true, w(100_000), NOW)
        .unwrap();
    assert_eq!(r.amount_out, w(950));
    assert_eq!(r.pay_l18, w(950_000));
    // One grid unit less of gross and the band floor (920_000) still holds; drop gross so that
    // payL18 = 919_000 < 920_000 -> PriceBand.
    s.hooks.leverage.everlong_leverage = Some(FixedLever(Ok(LevFill {
        amount_in_used: w(100_000),
        gross_out: w(919_999 + 100),
        virtual_leg_l18: w(100),
        cr_after_wad: w(1_820_000_000_000_000_000),
    })));
    assert_eq!(s.preview_lever(true, w(100_000), NOW), Err(FlammError::PriceBand));
}

/// `FLAMMLeverLib._planDown` (`:140-141`): `payNative = ceilDiv(payL18, scale)` capped at `loanIn`.
/// With scale 1000 and `payL18 = 450_001`, `payNative = 451`; with `loanIn = 450` the cap binds.
#[test]
fn lever_down_pay_native_is_ceiled_then_capped() {
    let mut s = state(1);
    s.pool.loans[0].scale = w(1000);
    // outValue = 45_000 * 10 = 450_000: within the 1.01x concession of payL18 (454_501) and
    // above the 92% band floor (414_001).
    let fill = LevFill {
        amount_in_used: w(450_051),
        gross_out: w(45_000),
        virtual_leg_l18: w(50),
        cr_after_wad: w(2_000_000_000_000_000_000),
    };
    s.hooks.leverage.everlong_leverage = Some(FixedLever(Ok(fill)));
    // loanIn 500 -> inL18 500_000 >= amountInUsed; payL18 = 450_001 -> payNative 451.
    let r = s
        .preview_lever(false, w(500), NOW)
        .unwrap();
    assert_eq!(r.pay_l18, w(450_001));
    assert_eq!(r.amount_in_used, w(451));
    // loanIn 451 -> inL18 451_000 >= 450_051: payNative 451 (no cap).
    assert_eq!(
        s.preview_lever(false, w(451), NOW)
            .unwrap()
            .amount_in_used,
        w(451)
    );
    // loanIn 450 -> inL18 450_000 < amountInUsed 450_051 -> FillInvalid (the hook overdrew).
    assert_eq!(s.preview_lever(false, w(450), NOW), Err(FlammError::FillInvalid));
    // The cap: amountInUsed 450_000 exactly, payL18 449_950 -> payNative 450 = loanIn.
    s.hooks.leverage.everlong_leverage =
        Some(FixedLever(Ok(LevFill { amount_in_used: w(450_000), ..fill })));
    let r = s
        .preview_lever(false, w(450), NOW)
        .unwrap();
    assert_eq!(r.pay_l18, w(449_950));
    assert_eq!(r.amount_in_used, w(450));
}
