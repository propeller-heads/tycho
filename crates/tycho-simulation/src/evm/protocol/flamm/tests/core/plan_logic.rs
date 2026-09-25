// Copyright (c) 2026 Everlong Labs Limited

//! The composed flows over scripted seam doubles: `FLAMMSwapLib._plan`'s funding passes and
//! ceilings, `_fill`'s fee floor / cap and edge conversions, `_validate`'s check order, `execute`'s
//! commit and settlement plumbing, and `FLAMMLeverLib._planUp` / `_planDown`'s checks in their
//! revert order. The numbers are chosen so that the numeraire is the native unit (`scale = 1`,
//! `crossWad = WAD`) and the checked cross is exactly `10` N18 per poolAsset base unit; nothing
//! here is a wei-exact claim about the deployed hooks (that is the fixture replays), only about the
//! core's own arithmetic and ordering.

use alloy::primitives::{Address, U256};

use crate::evm::protocol::flamm::{
    context::{LeverContext, PoolContext, SwapContext},
    deps::{LeverageHook, Router, SwapHook},
    gate::{GateReads, LoanCfg, Pool},
    hook::{Book, FillResult},
    levhook::{LevBook, LevFill},
    math::{mul_div, PPM, WAD},
    pricefeed::{FeedRound, FeedToken, PriceFeedState},
    router::{Positions, Quarantine},
    state::{HookKind, LeverageHookSlot, PoolHooks, SpreadHookSlot, SpreadHookState, SwapHookSlot},
    FlammError, FlammState,
};

fn w(x: u64) -> U256 {
    U256::from(x)
}

/// The checked cross: 10 N18 per poolAsset base unit.
const PRICE: u64 = 10;

/// A hook whose fill is a flat price of `PRICE` sliced to the ceiling: a sell of `a` yields
/// `min(a * PRICE, maxAmountOut)` gross and uses `ceil(gross / PRICE)`; a buy of `a` (N18) yields
/// `min(a / PRICE, maxAmountOut)` gross and uses `gross * PRICE`. `fee_wad` is what the fee role
/// quotes; `ignore_ceiling` makes the invariant role misbehave; `book` is what a commit must store.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScriptedHook {
    fee_wad: U256,
    ignore_ceiling: bool,
    committed: Option<Book>,
    reservation: U256,
}

impl SwapHook for ScriptedHook {
    fn spot(&self) -> Result<U256, FlammError> {
        Ok(w(PRICE))
    }

    fn preview_fee_wad(&self, _ctx: &SwapContext) -> Result<U256, FlammError> {
        Ok(self.fee_wad)
    }

    fn fill(&self, ctx: &SwapContext, fee_wad: U256) -> Result<(FillResult, Book), FlammError> {
        let cap = if self.ignore_ceiling { U256::MAX } else { ctx.max_amount_out };
        let (used, gross) = if ctx.pool_asset_in {
            let gross = (ctx.amount_in * w(PRICE)).min(cap);
            (gross.div_ceil(w(PRICE)), gross)
        } else {
            let gross = (ctx.amount_in / w(PRICE)).min(cap);
            (gross * w(PRICE), gross)
        };
        let fee_out = mul_div(gross, fee_wad, WAD)?;
        let book = Book { kappa: w(7), rs: gross, is: used, rv: fee_out, iv: w(1), x: w(2) };
        Ok((
            FillResult {
                amount_in_used: used,
                gross_out: gross,
                fee_out,
                spot_after_wad: w(PRICE),
                cap_evals: 3,
            },
            book,
        ))
    }

    fn commit(&mut self, book: &Book) {
        self.committed = Some(*book);
    }

    fn book_for(&self, ctx: &PoolContext) -> Result<Book, FlammError> {
        Ok(Book {
            kappa: w(1),
            rs: ctx.liquid_loan_asset,
            is: w(0),
            rv: ctx.physical_pool_asset,
            iv: w(0),
            x: w(0),
        })
    }

    fn reservation_price(&self) -> U256 {
        self.reservation
    }
}

/// A leverage hook that answers a scripted fill and records the book it was handed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScriptedLever {
    fill: Result<LevFill, FlammError>,
}

impl LeverageHook for ScriptedLever {
    fn preview_lever(&self, _ctx: &LeverContext, _book: &LevBook) -> Result<LevFill, FlammError> {
        self.fill
    }
}

/// A Router with no position, whose funding ceiling is `cap_hi` while the offered collateral beyond
/// the physical balance is at least `threshold`, else `cap_lo`; it records every settlement call.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScriptedRouter {
    physical: U256,
    threshold: U256,
    cap_hi: U256,
    cap_lo: U256,
    calls: Vec<String>,
    ended: bool,
}

impl GateReads for ScriptedRouter {
    fn positions(&self, _now: u64) -> Result<Positions, FlammError> {
        Ok(Positions { coll: vec![w(0)], sup: vec![w(0)], debt: vec![w(0)], total_coll: w(0) })
    }

    fn quarantine(&self, _idx: u8, _now: u64) -> Result<Quarantine, FlammError> {
        Ok(Quarantine::default())
    }
}

impl Router for ScriptedRouter {
    fn funding_ceiling(
        &self,
        idx: u8,
        collateral_in: U256,
        price_wad: U256,
        _now: u64,
    ) -> Result<U256, FlammError> {
        assert_eq!(idx, 0);
        assert_eq!(price_wad, w(PRICE));
        let extra = collateral_in
            .checked_sub(self.physical)
            .expect("the whole physical balance is offered");
        Ok(if extra >= self.threshold { self.cap_hi } else { self.cap_lo })
    }

    fn settle_sell_legs(
        &mut self,
        pool: &mut Pool,
        idx: u8,
        price_wad: U256,
        used: U256,
        net: U256,
        now: u64,
    ) -> Result<(), FlammError> {
        assert!(!pool.price_wad.is_empty(), "the settlement reads the plan's price frame");
        self.calls
            .push(format!("sell idx={idx} price={price_wad} used={used} net={net} now={now}"));
        pool.physical += used;
        Ok(())
    }

    fn settle_buy_legs(
        &mut self,
        pool: &mut Pool,
        idx: u8,
        used: U256,
        net: U256,
        now: u64,
    ) -> Result<(), FlammError> {
        assert!(!pool.price_wad.is_empty(), "the settlement reads the plan's price frame");
        self.calls
            .push(format!("buy idx={idx} used={used} net={net} now={now}"));
        pool.physical -= net;
        Ok(())
    }

    fn end_transaction(&mut self) {
        self.ended = true;
    }
}

type State = FlammState<ScriptedHook, ScriptedLever, ScriptedRouter>;

const NOW: u64 = 2_000_000;
const CBBTC: Address = Address::repeat_byte(0xcb);
const USDC: Address = Address::repeat_byte(0x83);

/// A one-loan pool: physical `physical`, liquid `liquid`, ltv 0.5, phi 0, no epsilon, band 8%,
/// every feature on, a fresh feed answering exactly `PRICE`, a live spread post of 17500.
fn state(physical: u64, liquid: u64) -> State {
    let round = |answer: u64| FeedRound {
        ok: true,
        round_id: w(1),
        answer: w(answer),
        started_at: w(NOW - 60),
        updated_at: w(NOW - 60),
    };
    State {
        block: 1,
        timestamp: NOW,
        pool_asset: CBBTC,
        pool: Pool {
            physical: w(physical),
            loans: vec![LoanCfg {
                token: USDC,
                scale: w(1),
                swap_price_band_wad: w(80_000_000_000_000_000),
                fee_floor_wad: w(0),
                max_swap_notional: w(0),
                reserve_target: w(0),
                liquid: w(liquid),
            }],
            ltv_wad: WAD / w(2),
            phi_wad: w(0),
            room_epsilon_wad: w(0),
            features: w(0b110_1111),
            price_wad: vec![],
            cross_wad: vec![],
        },
        paused: false,
        lev_paused: false,
        fee_floor_wad: w(0),
        fee_cap_wad: WAD / w(10),
        share_supply: w(1_000_000),
        last_lever_spread_ppm: w(0),
        hooks: PoolHooks {
            swap: SwapHookSlot {
                kind: HookKind::EverlongSwapV1,
                everlong_swap: Some(ScriptedHook {
                    fee_wad: w(0),
                    ignore_ceiling: false,
                    committed: None,
                    reservation: w(PRICE) * WAD,
                }),
            },
            leverage: LeverageHookSlot {
                kind: HookKind::EverlongLeverageV1,
                everlong_leverage: Some(ScriptedLever { fill: Err(FlammError::Unsupported) }),
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
        router: ScriptedRouter {
            physical: w(physical),
            threshold: w(0),
            cap_hi: w(1000),
            cap_lo: w(1000),
            calls: vec![],
            ended: false,
        },
        feed: PriceFeedState {
            has_sequencer: true,
            sequencer_grace: w(3600),
            sequencer: FeedRound {
                ok: true,
                round_id: w(1),
                answer: w(0),
                started_at: w(NOW - 10_000),
                updated_at: w(NOW - 10_000),
            },
            // baseUsd = PRICE, quoteUsd = 1e18, unit 1: cross = PRICE exactly.
            asset: FeedToken {
                known: true,
                heartbeat: w(3600),
                scale: w(1),
                unit: w(1),
                peg_band_wad: w(0),
                round: round(PRICE),
            },
            loans: vec![FeedToken {
                known: true,
                heartbeat: w(90_000),
                scale: WAD,
                unit: w(1),
                peg_band_wad: w(10_000_000_000_000_000),
                round: round(1),
            }],
        },
    }
}

fn hook(s: &mut State) -> &mut ScriptedHook {
    s.hooks
        .swap
        .everlong_swap
        .as_mut()
        .unwrap()
}

fn lever(s: &mut State, fill: Result<LevFill, FlammError>) {
    s.hooks.leverage.everlong_leverage = Some(ScriptedLever { fill });
}

#[test]
fn sell_converges_on_the_first_pass_when_the_funding_holds() {
    // funding 1000 at any collateral; room 5e6; ceiling = liquid 100 + 1000.
    let s = state(1_000_000, 100);
    let p = s
        .preview_swap(true, w(1000), NOW)
        .unwrap();
    assert_eq!(p.passes, 1);
    assert_eq!(p.ceiling, w(1100));
    assert_eq!(p.sctx.max_amount_out, w(1100));
    assert_eq!(p.used_native, w(110));
    assert_eq!((p.gross_native, p.net_native), (w(1100), w(1100)));
    assert_eq!(p.fill.amount_in_used, w(110));
    assert_eq!(p.spot_wad, w(PRICE));
    assert_eq!((p.price_wad, p.cross_wad, p.scale), (w(PRICE), WAD, w(1)));
    assert_eq!(p.cap_evals, 3);
}

#[test]
fn sell_replans_on_its_own_consumption() {
    // 1000 while the offered collateral beyond physical is >= 200, else 400: pass 1 fills 110 <
    // 200, the re-plan needs 1000 > 400, pass 2 fills 50 with need 400 <= 400.
    let mut s = state(1_000_000, 100);
    s.router.threshold = w(200);
    s.router.cap_lo = w(400);
    let p = s
        .preview_swap(true, w(1000), NOW)
        .unwrap();
    assert_eq!(p.passes, 2);
    assert_eq!(p.ceiling, w(500));
    assert_eq!((p.used_native, p.net_native), (w(50), w(500)));
    // The funding never settles: four passes, then OutputAboveCeiling.
    let mut s = state(1_000_000, 100);
    s.router.threshold = w(1_000_000);
    s.router.cap_hi = w(1000);
    s.router.cap_lo = w(0);
    // pass 1: ceiling 100 (liquid), used 10, need 0 <= 0: returns. Make liquid 0 so need binds.
    s.pool.loans[0].liquid = w(0);
    assert_eq!(s.preview_swap(true, w(1000), NOW), Err(FlammError::RoomExhausted));
    // A partial fill whose need never fits: the hook ignores the ceiling so net stays 10000 >
    // funding.
    let mut s = state(1_000_000, 100);
    hook(&mut s).ignore_ceiling = true;
    s.router.threshold = w(2000); // never met: funding 1000 at every pass
    let err = s
        .preview_swap(true, w(1000), NOW)
        .unwrap_err();
    // used == amountIn on the first pass (the hook fills everything), so the plan returns and
    // validate refuses the output above the ceiling.
    assert_eq!(err, FlammError::OutputAboveCeiling);
}

#[test]
fn sell_exhausts_four_passes() {
    // A hook that uses less than offered every pass and a funding that shrinks with it: never
    // self-consistent, four passes, OutputAboveCeiling from the loop (FLAMMSwapLib.sol:173).
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Shrinking(ScriptedRouter);
    impl GateReads for Shrinking {
        fn positions(&self, now: u64) -> Result<Positions, FlammError> {
            self.0.positions(now)
        }
        fn quarantine(&self, idx: u8, now: u64) -> Result<Quarantine, FlammError> {
            self.0.quarantine(idx, now)
        }
    }
    impl Router for Shrinking {
        fn funding_ceiling(
            &self,
            _idx: u8,
            collateral_in: U256,
            _price_wad: U256,
            _now: u64,
        ) -> Result<U256, FlammError> {
            Ok((collateral_in - self.0.physical) * w(5))
        }
        fn settle_sell_legs(
            &mut self,
            p: &mut Pool,
            i: u8,
            pw: U256,
            u: U256,
            n: U256,
            t: u64,
        ) -> Result<(), FlammError> {
            self.0
                .settle_sell_legs(p, i, pw, u, n, t)
        }
        fn settle_buy_legs(
            &mut self,
            p: &mut Pool,
            i: u8,
            u: U256,
            n: U256,
            t: u64,
        ) -> Result<(), FlammError> {
            self.0.settle_buy_legs(p, i, u, n, t)
        }
        fn end_transaction(&mut self) {
            self.0.end_transaction()
        }
    }
    let base = state(1_000_000, 100);
    let s: FlammState<ScriptedHook, ScriptedLever, Shrinking> = FlammState {
        block: base.block,
        timestamp: base.timestamp,
        pool_asset: base.pool_asset,
        pool: base.pool.clone(),
        paused: false,
        lev_paused: false,
        fee_floor_wad: base.fee_floor_wad,
        fee_cap_wad: base.fee_cap_wad,
        share_supply: base.share_supply,
        last_lever_spread_ppm: base.last_lever_spread_ppm,
        hooks: base.hooks.clone(),
        router: Shrinking(base.router.clone()),
        feed: base.feed.clone(),
    };
    // pass 1: funding 5000, ceiling 5100, used 510 < 1000; need 5000 > 2550; pass 2: 2650, used
    // 265; need 2550 > 1325; pass 3: 1425, used 143; need 1325 > 715; pass 4: 815, used 82;
    // need 715 > 410.
    assert_eq!(s.preview_swap(true, w(1000), NOW), Err(FlammError::OutputAboveCeiling));
}

#[test]
fn sell_room_is_clipped_by_the_notional_cap() {
    let mut s = state(1_000_000, 100);
    s.pool.loans[0].max_swap_notional = w(300);
    let p = s
        .preview_swap(true, w(1000), NOW)
        .unwrap();
    assert_eq!(p.ceiling, w(300));
    assert_eq!((p.used_native, p.net_native), (w(30), w(300)));
    // The gate room binds when smaller than the funding: physical 100 -> bound 100 * 0.5 = 50 PW,
    // plus the asset's own surplus (liquid 100 at price 10 = 10 PW), at price 10: head 600 L18,
    // no lift (phi 0), scale 1: room 600 < 1100.
    let s = state(100, 100);
    let p = s
        .preview_swap(true, w(1000), NOW)
        .unwrap();
    assert_eq!(p.ceiling, w(600));
    let s = state(100, 0);
    assert_eq!(
        s.preview_swap(true, w(1000), NOW)
            .unwrap()
            .ceiling,
        w(500)
    );
}

#[test]
fn fee_floor_rejects_and_cap_clips() {
    let mut s = state(1_000_000, 100);
    hook(&mut s).fee_wad = w(50_000_000_000_000_000); // 5%
    let p = s
        .preview_swap(true, w(1000), NOW)
        .unwrap();
    assert_eq!(p.fee_wad, w(50_000_000_000_000_000));
    assert_eq!(p.fill.fee_out, w(55));
    assert_eq!((p.gross_native, p.net_native), (w(1100), w(1045)));
    // The pool floor above the quote: rejected.
    s.fee_floor_wad = w(50_000_000_000_000_001);
    assert_eq!(s.preview_swap(true, w(1000), NOW), Err(FlammError::FeeOutOfBounds));
    s.fee_floor_wad = w(0);
    // The loan asset's own floor, the larger of the two.
    s.pool.loans[0].fee_floor_wad = w(60_000_000_000_000_000);
    assert_eq!(s.preview_swap(false, w(1000), NOW), Err(FlammError::FeeOutOfBounds));
    s.pool.loans[0].fee_floor_wad = w(0);
    // The cap clips and the fill is built on the clipped fee.
    s.fee_cap_wad = w(40_000_000_000_000_000);
    let p = s
        .preview_swap(true, w(1000), NOW)
        .unwrap();
    assert_eq!(p.fee_wad, w(40_000_000_000_000_000));
    assert_eq!(p.fill.fee_out, w(44));
    assert_eq!(p.net_native, w(1056));
    // Floor == cap == quote passes.
    s.fee_cap_wad = w(50_000_000_000_000_000);
    s.fee_floor_wad = w(50_000_000_000_000_000);
    assert!(s
        .preview_swap(true, w(1000), NOW)
        .is_ok());
}

#[test]
fn buy_ceiling_is_the_gross_pool_asset_and_the_claim_is_ceiled() {
    let mut s = state(1_000_000, 100);
    hook(&mut s).fee_wad = w(50_000_000_000_000_000);
    let p = s
        .preview_swap(false, w(1000), NOW)
        .unwrap();
    assert_eq!(p.ceiling, w(1_000_000));
    assert_eq!(p.passes, 0);
    assert_eq!(p.sctx.amount_in, w(1000));
    assert_eq!(p.sctx.max_amount_out, w(1_000_000));
    assert_eq!((p.used_native, p.gross_native, p.net_native), (w(1000), w(100), w(95)));
    // A buy larger than the pool's poolAsset is sliced to it.
    let p = s
        .preview_swap(false, w(100_000_000), NOW)
        .unwrap();
    assert_eq!(p.gross_native, w(1_000_000));
    assert_eq!(p.used_native, w(10_000_000));
    // The notional cap applies to the used input of a buy, in validate (after the fill).
    s.pool.loans[0].max_swap_notional = w(999);
    assert_eq!(s.preview_swap(false, w(1000), NOW), Err(FlammError::NotionalCap));
    s.pool.loans[0].max_swap_notional = w(1000);
    assert!(s
        .preview_swap(false, w(1000), NOW)
        .is_ok());
    // The band is on the net: 95 * 10 = 950 against 1000, inside 8%, outside 1%.
    s.pool.loans[0].swap_price_band_wad = w(10_000_000_000_000_000);
    assert_eq!(s.preview_swap(false, w(1000), NOW), Err(FlammError::PriceBand));
}

#[test]
fn validate_checks_in_order() {
    // A misbehaving hook: gross above the ceiling.
    let mut s = state(100, 0);
    hook(&mut s).ignore_ceiling = true;
    // Ceiling: room 500 (physical 100), funding 1000: 500. Fill uses 1000, gross 10000 > 500.
    assert_eq!(s.preview_swap(true, w(1000), NOW), Err(FlammError::OutputAboveCeiling));
    // Slippage is checked before the ceiling.
    assert_eq!(
        s.execute_swap(CBBTC, USDC, w(1000), w(20_000), NOW, NOW)
            .unwrap_err(),
        FlammError::Slippage
    );
    // A zero fill is FillInvalid before anything else: amountIn 0 after the numeraire edge cannot
    // happen, so use a fee at 100% (the hook fills nothing) -- clipped by the cap first, so
    // raise the cap.
    let mut s = state(1_000_000, 100);
    hook(&mut s).fee_wad = WAD;
    s.fee_cap_wad = WAD;
    // The scripted hook still fills at fee WAD (fee_out == gross): net 0 -> FillInvalid.
    assert_eq!(s.preview_swap(true, w(1000), NOW), Err(FlammError::FillInvalid));
    assert_eq!(
        s.execute_swap(CBBTC, USDC, w(1000), w(20_000), NOW, NOW)
            .unwrap_err(),
        FlammError::FillInvalid
    );
    // Entry checks: Expired, InvalidAmount, InvalidPair, in that order.
    let s = state(1_000_000, 100);
    assert_eq!(
        s.execute_swap(CBBTC, USDC, w(0), w(0), NOW - 1, NOW)
            .unwrap_err(),
        FlammError::Expired
    );
    assert_eq!(
        s.execute_swap(CBBTC, CBBTC, w(0), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::InvalidAmount
    );
    assert_eq!(
        s.execute_swap(CBBTC, CBBTC, w(1), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::InvalidPair
    );
    assert_eq!(s.preview_swap(true, w(0), NOW), Err(FlammError::InvalidAmount));
}

#[test]
fn execute_swap_commits_settles_and_leaves_the_receiver() {
    let s = state(1_000_000, 100);
    let (r, post) = s
        .execute_swap(CBBTC, USDC, w(1000), w(1100), NOW, NOW)
        .unwrap();
    assert!(r.pool_asset_in);
    assert_eq!((r.amount_in_used, r.amount_out, r.fee_out), (w(110), w(1100), w(0)));
    assert_eq!((r.fee_wad, r.spot_after_wad, r.price_wad), (w(0), w(PRICE), w(PRICE)));
    assert_eq!((r.cap_evals, r.passes), (6, 1));
    let committed = post
        .hooks
        .swap
        .everlong_swap
        .as_ref()
        .unwrap()
        .committed
        .unwrap();
    assert_eq!(committed.rs, w(1100));
    assert_eq!(
        post.router.calls,
        vec!["sell idx=0 price=10 used=110 net=1100 now=2000000".to_string()]
    );
    assert!(post.router.ended);
    assert!(post.pool.price_wad.is_empty() && post.pool.cross_wad.is_empty());
    assert_eq!(post.pool.physical, w(1_000_110));
    // The receiver is untouched.
    assert_eq!(s, state(1_000_000, 100));
    // A buy settles through the buy legs with the used input and the net payout.
    let (r, post) = s
        .execute_swap(USDC, CBBTC, w(1000), w(100), NOW, NOW)
        .unwrap();
    assert!(!r.pool_asset_in);
    assert_eq!((r.amount_in_used, r.amount_out), (w(1000), w(100)));
    assert_eq!(post.router.calls, vec!["buy idx=0 used=1000 net=100 now=2000000".to_string()]);
    assert_eq!(post.pool.physical, w(999_900));
}

#[test]
fn lever_up_plan_checks_in_order() {
    let fill = LevFill {
        amount_in_used: w(100),
        gross_out: w(1050),
        virtual_leg_l18: w(100),
        cr_after_wad: w(1_820_000_000_000_000_000),
    };
    let mut s = state(1_000_000, 100);
    lever(&mut s, Ok(fill));
    let r = s
        .preview_lever(true, w(100), NOW)
        .unwrap();
    // out = (1050 - 100) / 1 = 950; leak floor 1000 * (1 - 0.0175) = 982 >= 950; band 920 <= 950;
    // room 5e6; CR at the floor; rest 950 - 100 = 850 <= funding 1000.
    assert_eq!(
        (r.amount_in_used, r.amount_out, r.spread_ppm, r.cr_after_wad),
        (w(100), w(950), w(17_500), fill.cr_after_wad)
    );
    assert_eq!((r.pay_l18, r.price_wad), (w(950), w(PRICE)));
    let check = |f: LevFill, want: FlammError| {
        let mut s = state(1_000_000, 100);
        lever(&mut s, Ok(f));
        assert_eq!(s.preview_lever(true, w(100), NOW), Err(want));
    };
    check(LevFill { amount_in_used: w(0), ..fill }, FlammError::FillInvalid);
    check(LevFill { amount_in_used: w(101), ..fill }, FlammError::FillInvalid);
    check(LevFill { gross_out: w(100), ..fill }, FlammError::FillInvalid);
    // gross - virtual under one native unit: out 0.
    let mut s = state(1_000_000, 100);
    s.pool.loans[0].scale = w(1000);
    lever(&mut s, Ok(LevFill { gross_out: w(150), ..fill }));
    assert_eq!(s.preview_lever(true, w(100), NOW), Err(FlammError::FillInvalid));
    // paid above value net of the spread: 1100 > 982.
    check(LevFill { gross_out: w(1200), ..fill }, FlammError::LevValueLeak);
    // paid under the taker band: 900 < 920.
    check(LevFill { gross_out: w(1000), ..fill }, FlammError::PriceBand);
    // The room: physical 100 -> head 500 < 950.
    let mut s = state(100, 100);
    lever(&mut s, Ok(fill));
    assert_eq!(s.preview_lever(true, w(100), NOW), Err(FlammError::RoomExceeded));
    // CR one under the floor.
    check(
        LevFill { cr_after_wad: w(1_819_999_999_999_999_999), ..fill },
        FlammError::LevBelowFloor,
    );
    // The funding: rest 850 > 800.
    let mut s = state(1_000_000, 100);
    s.router.cap_hi = w(800);
    s.router.cap_lo = w(800);
    lever(&mut s, Ok(fill));
    assert_eq!(s.preview_lever(true, w(100), NOW), Err(FlammError::OutputAboveCeiling));
    // Liquid covering the whole payout needs no funding.
    let mut s = state(1_000_000, 950);
    s.router.cap_hi = w(0);
    s.router.cap_lo = w(0);
    lever(&mut s, Ok(fill));
    assert!(s
        .preview_lever(true, w(100), NOW)
        .is_ok());
    // The hook's own refusal bubbles as is.
    let mut s = state(1_000_000, 100);
    lever(&mut s, Err(FlammError::NothingToFill));
    assert_eq!(s.preview_lever(true, w(100), NOW), Err(FlammError::NothingToFill));
    // No spread: a lever-up fails closed before the hook is asked.
    let mut s = state(1_000_000, 100);
    s.hooks
        .spread
        .everlong_spread
        .as_mut()
        .unwrap()
        .last_set_ts = w(NOW - 3601);
    lever(&mut s, Ok(fill));
    assert_eq!(s.preview_lever(true, w(100), NOW), Err(FlammError::SpreadUnavailable));
}

#[test]
fn lever_down_plan_checks_in_order() {
    let fill = LevFill {
        amount_in_used: w(500),
        gross_out: w(42),
        virtual_leg_l18: w(50),
        cr_after_wad: w(2_000_000_000_000_000_000),
    };
    let mut s = state(1_000_000, 100);
    lever(&mut s, Ok(fill));
    let r = s
        .preview_lever(false, w(500), NOW)
        .unwrap();
    // pay 450; outValue 420 <= concession 454; band floor 414 <= 420; payNative ceil(450 / 1) =
    // 450.
    assert_eq!((r.amount_in_used, r.amount_out, r.spread_ppm), (w(450), w(42), w(17_500)));
    assert_eq!(r.pay_l18, w(450));
    let check = |f: LevFill, want: FlammError| {
        let mut s = state(1_000_000, 100);
        lever(&mut s, Ok(f));
        assert_eq!(s.preview_lever(false, w(500), NOW), Err(want));
    };
    check(LevFill { amount_in_used: w(0), ..fill }, FlammError::FillInvalid);
    check(LevFill { amount_in_used: w(501), ..fill }, FlammError::FillInvalid);
    check(LevFill { amount_in_used: w(50), ..fill }, FlammError::FillInvalid);
    check(LevFill { gross_out: w(0), ..fill }, FlammError::FillInvalid);
    check(LevFill { gross_out: w(1_000_001), ..fill }, FlammError::OutputAboveCeiling);
    // outValue 460 > concession 454.
    check(LevFill { gross_out: w(46), ..fill }, FlammError::LevValueLeak);
    // outValue 410 < band floor 414.
    check(LevFill { gross_out: w(41), ..fill }, FlammError::PriceBand);
    // payNative is ceiled onto the native grid, then capped at the input.
    let mut s = state(1_000_000, 100);
    s.pool.loans[0].scale = w(100);
    lever(
        &mut s,
        Ok(LevFill {
            amount_in_used: w(50_000),
            gross_out: w(4_200),
            virtual_leg_l18: w(5_001),
            ..fill
        }),
    );
    // inL18 = 500 * 100 = 50000; pay 44999 -> ceil(44999 / 100) = 450.
    assert_eq!(
        s.preview_lever(false, w(500), NOW)
            .unwrap()
            .amount_in_used,
        w(450)
    );
    lever(
        &mut s,
        Ok(LevFill {
            amount_in_used: w(50_000),
            gross_out: w(4_600),
            virtual_leg_l18: w(1),
            ..fill
        }),
    );
    // pay 49999 -> ceil 500 == loanIn (the cap binds at equality, not above).
    assert_eq!(
        s.preview_lever(false, w(500), NOW)
            .unwrap()
            .amount_in_used,
        w(500)
    );
    // A huge input overflows the L18 lift before the spread or the hook are consulted.
    let mut s = state(1_000_000, 100);
    s.pool.loans[0].scale = w(1_000_000_000_000);
    lever(&mut s, Ok(fill));
    assert_eq!(s.preview_lever(false, U256::MAX, NOW), Err(FlammError::PanicArithmetic));
    // A stale post degrades a lever-down to the ceiling; the fill is still checked.
    let mut s = state(1_000_000, 100);
    s.hooks
        .spread
        .everlong_spread
        .as_mut()
        .unwrap()
        .last_set_ts = w(NOW - 3601);
    lever(&mut s, Ok(fill));
    let r = s
        .preview_lever(false, w(500), NOW)
        .unwrap();
    assert_eq!(r.spread_ppm, w(100_000));
}

#[test]
fn execute_lever_stores_a_live_spread_and_settles() {
    let up = LevFill {
        amount_in_used: w(100),
        gross_out: w(1050),
        virtual_leg_l18: w(100),
        cr_after_wad: w(1_820_000_000_000_000_000),
    };
    let mut s = state(1_000_000, 100);
    lever(&mut s, Ok(up));
    assert_eq!(
        s.execute_lever(true, w(100), w(0), NOW - 1, NOW)
            .unwrap_err(),
        FlammError::Expired
    );
    assert_eq!(
        s.execute_lever(true, w(0), w(0), NOW, NOW)
            .unwrap_err(),
        FlammError::InvalidAmount
    );
    assert_eq!(
        s.execute_lever(true, w(100), w(951), NOW, NOW)
            .unwrap_err(),
        FlammError::Slippage
    );
    let (r, post) = s
        .execute_lever(true, w(100), w(950), NOW, NOW)
        .unwrap();
    assert!(r.up);
    assert_eq!((r.amount_in_used, r.amount_out, r.spread_ppm), (w(100), w(950), w(17_500)));
    assert_eq!(post.last_lever_spread_ppm, w(17_500));
    assert_eq!(
        post.router.calls,
        vec!["sell idx=0 price=10 used=100 net=950 now=2000000".to_string()]
    );
    assert!(post.router.ended);
    assert!(post.pool.price_wad.is_empty());
    // The swap hook's book is never committed by the leverage venue.
    assert!(post
        .hooks
        .swap
        .everlong_swap
        .as_ref()
        .unwrap()
        .committed
        .is_none());
    assert_eq!(s.last_lever_spread_ppm, w(0), "the receiver is never written");
    // A live spread above the band is stored clamped (uint32 of the clamped value).
    let mut s = state(1_000_000, 100);
    s.hooks
        .spread
        .everlong_spread
        .as_mut()
        .unwrap()
        .spread = w(90_000);
    lever(&mut s, Ok(LevFill { gross_out: w(1020), ..up }));
    // leak floor 1000 * 0.92 = 920 >= pay 920; band 920 <= 920.
    let (r, post) = s
        .execute_lever(true, w(100), w(0), NOW, NOW)
        .unwrap();
    assert_eq!(r.spread_ppm, w(80_000));
    assert_eq!(post.last_lever_spread_ppm, w(80_000));
    // A degraded lever-down never becomes the degrade value; it settles through the buy legs.
    let down = LevFill {
        amount_in_used: w(500),
        gross_out: w(42),
        virtual_leg_l18: w(50),
        cr_after_wad: w(2_000_000_000_000_000_000),
    };
    let mut s = state(1_000_000, 100);
    s.last_lever_spread_ppm = w(17_500);
    s.hooks
        .spread
        .everlong_spread
        .as_mut()
        .unwrap()
        .last_set_ts = w(NOW - 3601);
    lever(&mut s, Ok(down));
    let (r, post) = s
        .execute_lever(false, w(500), w(42), NOW, NOW)
        .unwrap();
    assert!(!r.up);
    assert_eq!((r.amount_in_used, r.amount_out, r.spread_ppm), (w(450), w(42), w(17_500)));
    assert_eq!(post.last_lever_spread_ppm, w(17_500));
    assert_eq!(post.router.calls, vec!["buy idx=0 used=450 net=42 now=2000000".to_string()]);
    assert_eq!(post.pool.physical, w(999_958));
    // A live lever-down stores the (floored) spread.
    let mut s = state(1_000_000, 100);
    s.hooks
        .spread
        .everlong_spread
        .as_mut()
        .unwrap()
        .spread = w(1);
    lever(&mut s, Ok(down));
    let (r, post) = s
        .execute_lever(false, w(500), w(0), NOW, NOW)
        .unwrap();
    assert_eq!(r.spread_ppm, w(2_500));
    assert_eq!(post.last_lever_spread_ppm, w(2_500));
    let _ = PPM;
}
