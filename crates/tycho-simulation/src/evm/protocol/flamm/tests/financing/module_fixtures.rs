// Copyright (c) 2026 Everlong Labs Limited

//! Module fixtures of the financing stack (`testdata/gen/GateMathFixture.t.sol`,
//! `MMFinancingFixture.t.sol`, `GateIntEdges.t.sol`): OpenZeppelin `Math.mulDiv` edges, the
//! deployed `AdaptiveCurveIrm` grid, `FLAMMGateLib` over 400 seeded books and its `int256` edges,
//! the live Router / account / Morpho views at block 51317000 and warped, the first settled swap
//! (block 51302916), the fork's settlement sequence, and the deployed Router bytecode driven over
//! three venues. Every row is compared to the wei, reverts by class.

use alloy::primitives::U256;
use serde::Deserialize;

use super::common::*;
use crate::evm::protocol::flamm::{
    error::FlammError,
    gate::{self, Book, GateInt, Leg, LoanCfg, Pool},
    irm,
    math::{mul_div, mul_div_up, WAD},
    morpho::{Market, VenueMarket, VIRTUAL_SHARES},
    router::{self, Quarantine, Router},
};

// ------------------------------------------------------------------ Math.mulDiv

#[derive(Deserialize)]
struct MulDivRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    x: Dec,
    #[serde(default)]
    y: Dec,
    #[serde(default)]
    d: Dec,
    #[serde(default)]
    down: Call,
    #[serde(default)]
    up: Call,
}

/// OpenZeppelin (compat v4) `Math.mulDiv`, floor and `Rounding.Up`, over every triple of 15
/// boundary values plus 400 seeded ones: a zero denominator is `Panic(0x12)` under a product that
/// fits and the bare `require(denominator > prod1)` otherwise, and the rounded-up `result += 1` is
/// `Panic(0x11)` at `type(uint256).max`.
#[test]
fn mm_muldiv_oz() {
    let rows: Vec<MulDivRow> = load("mm_muldiv_edges.json.gz");
    assert_eq!(rows.len() - 1, 15 * 15 * 15 + 400);
    let mut rep = Report::new("mulDiv");
    for r in rows.iter().filter(|r| !r.header) {
        let ctx = format!("mulDiv({},{},{})", r.x.0, r.y.0, r.d.0);
        rep.check_words(&ctx, &r.down, mul_div(r.x.0, r.y.0, r.d.0).map(|z| vec![z]));
        rep.check_words(
            &format!("{ctx} up"),
            &r.up,
            mul_div_up(r.x.0, r.y.0, r.d.0).map(|z| vec![z]),
        );
    }
    rep.finish(rows.len() - 1);
}

// ------------------------------------------------------------------ AdaptiveCurveIrm

#[derive(Deserialize)]
struct IrmRow {
    #[serde(rename = "rateAtTarget")]
    rate_at_target: Dec,
    tsa: Dec,
    tba: Dec,
    #[serde(rename = "lastUpdate")]
    last_update: Dec,
    #[serde(default)]
    rate: Dec,
    #[serde(default, rename = "endRateAtTarget")]
    end_rate_at_target: Dec,
    #[serde(default)]
    err: String,
}

#[derive(Deserialize)]
struct IrmGrid {
    timestamp: u64,
    rows: Vec<IrmRow>,
}

/// `AdaptiveCurveIrm.borrowRateView` / `borrowRate` on the deployed Base IRM over `rateAtTarget` x
/// supply x utilisation x elapsed, including the clipped `wExp` and the timestamp underflow.
#[test]
fn mm_irm_grid() {
    let fx: IrmGrid = load("mm_irm_grid.json.gz");
    assert!(fx.rows.len() > 1000);
    for (i, r) in fx.rows.iter().enumerate() {
        let m = Market {
            total_supply_assets: r.tsa.0,
            total_supply_shares: r.tsa.0 * VIRTUAL_SHARES,
            total_borrow_assets: r.tba.0,
            total_borrow_shares: r.tba.0 * VIRTUAL_SHARES,
            last_update: r.last_update.0,
            fee: U256::ZERO,
        };
        let got = irm::borrow_rate(&m, r.rate_at_target.0, fx.timestamp);
        if !r.err.is_empty() {
            // The recorded revert data decides the class, as `irm_edges` does it; a row whose
            // data maps to nothing fails rather than passing against a hard-coded guess.
            let want = revert_of_hex(&r.err)
                .unwrap_or_else(|| panic!("row {i}: unmapped revert {}", r.err));
            assert_eq!(got, Err(want), "row {i}");
            continue;
        }
        let (rate, end) = got.unwrap_or_else(|e| panic!("row {i}: {e}"));
        assert_eq!(rate, r.rate.0, "rate row {i}");
        assert_eq!(end, r.end_rate_at_target.0, "end row {i}");
    }
}

// ------------------------------------------------------------------ FLAMMGateLib

#[derive(Deserialize, Default)]
struct GateFxRes {
    #[serde(default)]
    v: Option<Dec>,
    #[serde(default)]
    err: String,
}

#[derive(Deserialize, Default)]
struct GateFxNet {
    #[serde(default)]
    v: Option<SignedDec>,
    #[serde(default)]
    err: String,
}

#[derive(Deserialize, Default)]
struct GateFxIn {
    scale: Vec<Dec>,
    liquid: Vec<Dec>,
    supplied: Vec<Dec>,
    debt: Vec<Dec>,
    #[serde(rename = "priceWad")]
    price_wad: Vec<Dec>,
    #[serde(rename = "crossWad")]
    cross_wad: Vec<Dec>,
    physical: Dec,
    posted: Dec,
    #[serde(rename = "ltvWad")]
    ltv_wad: Dec,
    #[serde(rename = "phiWad")]
    phi_wad: Dec,
    #[serde(rename = "roomEpsilonWad")]
    room_epsilon_wad: Dec,
    #[serde(rename = "qAny")]
    q_any: Vec<bool>,
    #[serde(rename = "qDebt")]
    q_debt: Vec<Dec>,
    #[serde(rename = "qColl")]
    q_coll: Vec<Dec>,
}

#[derive(Deserialize, Default)]
struct GateFxContext {
    #[serde(default)]
    liquid: Dec,
    #[serde(default)]
    supplied: Dec,
    #[serde(default)]
    debt: Dec,
    #[serde(default, rename = "loanCount")]
    loan_count: Dec,
    #[serde(default)]
    err: String,
}

#[derive(Deserialize, Default)]
struct GateFxAnchor {
    u0: Vec<SignedDec>,
    gross0: Dec,
    quarantined: bool,
}

#[derive(Deserialize, Default)]
struct GateFxPost {
    physical: Dec,
    #[serde(rename = "postedAdj")]
    posted_adj: Dec,
    #[serde(rename = "debtAdj")]
    debt_adj: Vec<SignedDec>,
}

#[derive(Deserialize)]
struct GateFxCase {
    #[serde(rename = "in")]
    input: GateFxIn,
    #[serde(rename = "netPW")]
    net_pw: Vec<GateFxNet>,
    #[serde(rename = "exposurePW")]
    exposure_pw: GateFxRes,
    #[serde(rename = "headOf")]
    head_of: Vec<GateFxRes>,
    #[serde(rename = "roomNative")]
    room_native: Vec<GateFxRes>,
    #[serde(rename = "requiredPostedAll")]
    required_posted_all: GateFxRes,
    #[serde(rename = "navAt")]
    nav_at: GateFxRes,
    context: GateFxContext,
    #[serde(rename = "structuralDistWad")]
    structural_dist_wad: Dec,
    #[serde(rename = "roomWadNeg")]
    room_wad_neg: Dec,
    #[serde(rename = "assertGate")]
    assert_gate: GateFxRes,
    anchor: GateFxAnchor,
    entry: GateFxRes,
    exit: GateFxRes,
    post: GateFxPost,
}

#[derive(Deserialize)]
struct GateFx {
    cases: Vec<GateFxCase>,
}

fn gate_check(want: &GateFxRes, got: Result<Option<U256>, FlammError>, msg: &str) {
    if !want.err.is_empty() {
        let w = revert_of_hex(&want.err)
            .unwrap_or_else(|| panic!("{msg}: unmapped revert {}", want.err));
        assert_eq!(got.err(), Some(w), "{msg}");
        return;
    }
    let got = got.unwrap_or_else(|e| panic!("{msg}: {e}"));
    if let (Some(g), Some(v)) = (got, want.v) {
        assert_eq!(g, v.0, "{msg}");
    }
}

/// `FLAMMGateLib` (commit 80abd43) over 400 seeded books: `netPW`, exposure, `headOf`,
/// `roomNative`, the posting law, NAV, the hook context, the structural floor, `roomWad` on a
/// surplus, and the storage composites (`assertGate`, `anchor`, `assertEntryGate` including the
/// quarantined frame, `assertExitNotWorsened`) against a post-flow book.
#[test]
fn gate_math() {
    let fx: GateFx = load("gate_math.json.gz");
    assert_eq!(fx.cases.len(), 400);
    for (k, c) in fx.cases.iter().enumerate() {
        let inp = &c.input;
        let n = inp.scale.len();
        let mut b = Book { physical: inp.physical.0, posted: inp.posted.0, legs: Vec::new() };
        let mut pool = Pool {
            physical: inp.physical.0,
            ltv_wad: inp.ltv_wad.0,
            phi_wad: inp.phi_wad.0,
            room_epsilon_wad: inp.room_epsilon_wad.0,
            ..Default::default()
        };
        let mut fr = FakeRouter {
            posted: inp.posted.0,
            sup: inp
                .supplied
                .iter()
                .map(|d| d.0)
                .collect(),
            debt: inp.debt.iter().map(|d| d.0).collect(),
            q: Vec::new(),
        };
        for i in 0..n {
            b.legs.push(Leg {
                liquid: inp.liquid[i].0,
                supplied: inp.supplied[i].0,
                debt: inp.debt[i].0,
                scale: inp.scale[i].0,
                price_wad: inp.price_wad[i].0,
                cross_wad: inp.cross_wad[i].0,
            });
            pool.loans.push(LoanCfg {
                scale: inp.scale[i].0,
                liquid: inp.liquid[i].0,
                ..Default::default()
            });
            pool.price_wad.push(inp.price_wad[i].0);
            pool.cross_wad.push(inp.cross_wad[i].0);
            fr.q.push(Quarantine {
                any: inp.q_any[i],
                frozen_debt: inp.q_debt[i].0,
                frozen_coll: inp.q_coll[i].0,
            });
        }
        let name = format!("case {k}");
        for i in 0..n {
            let got = gate::net_pw(&b.legs[i]);
            let want = &c.net_pw[i];
            if !want.err.is_empty() {
                assert_eq!(got.err(), revert_of_hex(&want.err), "{name} netPW");
                continue;
            }
            let got = got.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(gate_int_string(&got), gate_int_string(&want.v.unwrap().0), "{name} netPW");
        }
        let u = gate::exposure_pw(&b);
        gate_check(&c.exposure_pw, u.map(Some), &format!("{name} exposurePW"));
        let u = u.unwrap_or(U256::ZERO);
        for i in 0..n {
            gate_check(
                &c.head_of[i],
                gate::head_of(&b, i, u, inp.ltv_wad.0).map(Some),
                &format!("{name} headOf[{i}]"),
            );
            gate_check(
                &c.room_native[i],
                gate::room_native(&pool, &b, i, u).map(Some),
                &format!("{name} roomNative[{i}]"),
            );
        }
        gate_check(
            &c.required_posted_all,
            gate::required_posted_all(&b, inp.ltv_wad.0).map(Some),
            &format!("{name} requiredPostedAll"),
        );
        gate_check(&c.nav_at, gate::nav_at(&b).map(Some), &format!("{name} navAt"));
        let ctx =
            gate::context(&b, inp.price_wad[0].0, 1234, U256::from(5_000_000_000_000_000_000u64));
        if !c.context.err.is_empty() {
            assert_eq!(ctx.err(), revert_of_hex(&c.context.err), "{name} context");
        } else {
            let ctx = ctx.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(ctx.liquid_loan_asset, c.context.liquid.0, "{name} context.liquid");
            assert_eq!(ctx.supplied_loan_asset, c.context.supplied.0, "{name} context.supplied");
            assert_eq!(ctx.debt_loan_asset, c.context.debt.0, "{name} context.debt");
            assert_eq!(U256::from(ctx.loan_count), c.context.loan_count.0, "{name} loanCount");
        }
        let lltv = ((inp.ltv_wad.0 + inp.phi_wad.0) >> 1) + U256::from(100_000_000_000_000_000u64);
        assert_eq!(
            gate::structural_dist_wad(inp.ltv_wad.0, lltv),
            c.structural_dist_wad.0,
            "{name} structuralDistWad"
        );
        let abs = inp.physical.0 * U256::from(10_000_000_000u64);
        let neg = GateInt { neg: !abs.is_zero(), abs };
        let rw = gate::room_wad(
            neg,
            inp.physical.0,
            U256::from(700_000_000_000_000u64),
            inp.ltv_wad.0,
            inp.phi_wad.0,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(rw, c.room_wad_neg.0, "{name} roomWadNeg");

        // storage composites against the mock router
        gate_check(
            &c.assert_gate,
            gate::assert_gate(&pool, &fr, 0).map(|_| None),
            &format!("{name} assertGate"),
        );
        let (u0, g0, q0) = gate::anchor(&pool, &fr, 0).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(q0, c.anchor.quarantined, "{name} quarantined");
        assert_eq!(g0, c.anchor.gross0.0, "{name} gross0");
        for (i, u) in u0.iter().enumerate() {
            assert_eq!(gate_int_string(u), gate_int_string(&c.anchor.u0[i].0), "{name} u0[{i}]");
        }
        let mut post = pool.clone();
        post.physical = c.post.physical.0;
        let mut pr = FakeRouter {
            sup: inp
                .supplied
                .iter()
                .map(|d| d.0)
                .collect(),
            q: fr.q.clone(),
            debt: vec![U256::ZERO; n],
            posted: inp.posted.0 + c.post.posted_adj.0,
        };
        for i in 0..n {
            let d = GateInt { neg: false, abs: inp.debt[i].0 }
                .checked_add(c.post.debt_adj[i].0)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            if !d.neg {
                pr.debt[i] = d.abs;
            }
        }
        gate_check(
            &c.entry,
            gate::assert_entry_gate(&post, &pr, 0, &u0, g0, q0).map(|_| None),
            &format!("{name} assertEntryGate"),
        );
        gate_check(
            &c.exit,
            gate::assert_exit_not_worsened(&post, &pr, 0, &u0, g0).map(|_| None),
            &format!("{name} assertExitNotWorsened"),
        );
    }
}

/// `FLAMMGateLib` (80abd43, via-IR) over the `int256` edges: casts wrapping at `2^255`, checked add
/// / sub / negation at `type(int256).min` in `netL18`, `netPW`, `roomWad`, `headOf`, `_readableU`
/// and the quarantined entry frame, `Math.mulDiv`'s rounded-up `+= 1` overflow, and the checked
/// epsilon shave.
#[test]
fn gate_int_edges() {
    let rows: Vec<GateRow> = load("gate_int_edges.json.gz");
    assert_eq!(rows.len() - 1, 20);
    gate_replay("gate-int-edges", &rows);
}

// ------------------------------------------------------------------ live views and settlements

#[derive(Deserialize, Default)]
struct FxRate {
    #[serde(rename = "dBorrow")]
    d_borrow: Dec,
    #[serde(rename = "dSupplyDown")]
    d_supply_down: Dec,
    ok: bool,
    rate: Dec,
}

#[derive(Deserialize, Default)]
struct FxShares {
    shares: Dec,
    #[serde(default)]
    v: Option<Dec>,
    #[serde(default)]
    err: String,
}

#[derive(Deserialize, Default)]
struct FxTryPosition {
    readable: bool,
    collateral: Dec,
    #[serde(rename = "supplyShares")]
    supply_shares: Dec,
    supplied: Dec,
    debt: Dec,
}

#[derive(Deserialize, Default)]
struct FxAccount {
    #[serde(rename = "tryPosition")]
    try_position: FxTryPosition,
    #[serde(rename = "debtOf")]
    debt_of: MmFxResult,
    #[serde(rename = "suppliedOf")]
    supplied_of: MmFxResult,
    #[serde(rename = "collateralOf")]
    collateral_of: Dec,
    #[serde(rename = "freeLiquidity")]
    free_liquidity: Dec,
    #[serde(rename = "supplySharesToAssets")]
    supply_shares_to_assets: Vec<FxShares>,
    #[serde(rename = "borrowRateAfter")]
    borrow_rate_after: Vec<FxRate>,
}

#[derive(Deserialize, Default)]
struct FxCeiling {
    #[serde(rename = "collIn")]
    coll_in: Dec,
    #[serde(rename = "priceWad")]
    price_wad: Dec,
    #[serde(default)]
    v: Option<Dec>,
    #[serde(default)]
    err: String,
}

#[derive(Deserialize, Default)]
struct FxQuarantine {
    any: bool,
    #[serde(rename = "frozenDebt")]
    frozen_debt: Dec,
    #[serde(rename = "frozenColl")]
    frozen_coll: Dec,
}

#[derive(Deserialize, Default)]
struct FxLoanViews {
    position: Vec<Dec>,
    quarantine: FxQuarantine,
    #[serde(rename = "fundingCeiling")]
    funding_ceiling: Vec<FxCeiling>,
}

#[derive(Deserialize, Default)]
struct FxVenueViews {
    #[serde(rename = "venuePosition")]
    venue_position: Vec<Dec>,
    readable: bool,
    health: MmFxResult,
    account: FxAccount,
}

#[derive(Deserialize, Default)]
struct FxPositions {
    coll: Vec<Dec>,
    sup: Vec<Dec>,
    debt: Vec<Dec>,
    #[serde(rename = "totalColl")]
    total_coll: Dec,
}

#[derive(Deserialize, Default)]
struct FxViews {
    positions: FxPositions,
    drawn: Vec<u8>,
    #[serde(rename = "minLltv")]
    min_lltv: Dec,
    reclaimable: MmFxResult,
    #[serde(rename = "reclaimableUnpriced")]
    reclaimable_unpriced: MmFxResult,
    loans: Vec<FxLoanViews>,
    venues: Vec<FxVenueViews>,
}

#[derive(Deserialize, Default)]
struct FxLens {
    facts: Vec<Dec>,
    venues: Vec<Vec<Dec>>,
}

#[derive(Deserialize, Default)]
struct FxSnap {
    #[serde(default)]
    timestamp: u64,
    #[serde(default)]
    router: MmFxRouter,
    #[serde(default)]
    pool: Option<MmFxPool>,
    #[serde(default)]
    prices: Vec<Dec>,
    #[serde(default)]
    views: Option<FxViews>,
    #[serde(default)]
    lens: Option<FxLens>,
}

fn require_result(want: &MmFxResult, got: Result<U256, FlammError>, msg: &str) {
    if !want.err.is_empty() {
        let w = revert_of_hex(&want.err)
            .unwrap_or_else(|| panic!("{msg}: unmapped revert {}", want.err));
        assert_eq!(got.err(), Some(w), "{msg}");
        return;
    }
    let got = got.unwrap_or_else(|e| panic!("{msg}: {e}"));
    assert_eq!(got, want.v.expect("value").0, "{msg}");
}

/// Recomputes every recorded router / account view from the loaded state at `now` (the Go
/// `mmCheckViews`).
fn check_views(name: &str, r: &Router, prices: &[U256], now: u64, w: &FxViews) {
    let pos = r
        .positions(now)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    for i in 0..pos.coll.len() {
        assert_eq!(pos.coll[i], w.positions.coll[i].0, "{name} positions.coll[{i}]");
        assert_eq!(pos.sup[i], w.positions.sup[i].0, "{name} positions.sup[{i}]");
        assert_eq!(pos.debt[i], w.positions.debt[i].0, "{name} positions.debt[{i}]");
    }
    assert_eq!(pos.total_coll, w.positions.total_coll.0, "{name} positions.totalColl");
    let (count, mask) = r.drawn(now).unwrap();
    assert_eq!(vec![count, mask], w.drawn, "{name} drawn");
    assert_eq!(r.min_lltv(now).unwrap(), w.min_lltv.0, "{name} minLltv");
    require_result(&w.reclaimable, r.reclaimable(prices, now), &format!("{name} reclaimable"));
    require_result(
        &w.reclaimable_unpriced,
        r.reclaimable(&vec![U256::ZERO; prices.len()], now),
        &format!("{name} reclaimableUnpriced"),
    );
    for (i, lw) in w.loans.iter().enumerate() {
        let (c, s, d) = r.position(i as u8, now).unwrap();
        assert_eq!(c, lw.position[0].0, "{name} loan {i} position coll");
        assert_eq!(s, lw.position[1].0, "{name} loan {i} position sup");
        assert_eq!(d, lw.position[2].0, "{name} loan {i} position debt");
        let q = r.quarantine(i as u8, now).unwrap();
        assert_eq!(q.any, lw.quarantine.any, "{name} quarantine any");
        assert_eq!(q.frozen_debt, lw.quarantine.frozen_debt.0, "{name} frozenDebt");
        assert_eq!(q.frozen_coll, lw.quarantine.frozen_coll.0, "{name} frozenColl");
        for row in &lw.funding_ceiling {
            let got = r.funding_ceiling(i as u8, row.coll_in.0, row.price_wad.0, now);
            let msg = format!(
                "{name} loan {i} fundingCeiling(coll {}, price {})",
                row.coll_in.0, row.price_wad.0
            );
            require_result(&MmFxResult { v: row.v, err: row.err.clone() }, got, &msg);
        }
    }
    for (i, vw) in w.venues.iter().enumerate() {
        let v = &r.venues[i];
        let vp = r.venue_position(i as u16, now).unwrap();
        for (k, got) in vp.iter().enumerate() {
            assert_eq!(*got, vw.venue_position[k].0, "{name} venue {i} venuePosition[{k}]");
        }
        let rd = r.read(v, now).unwrap();
        assert_eq!(rd.readable, vw.readable, "{name} venue {i} readable");
        let h = r.venue_health(i as u16, prices[v.loan_index as usize], now);
        require_result(&vw.health, h, &format!("{name} venue {i} health"));
        check_account(&format!("{name} venue {i}"), &v.morpho, now, &vw.account);
    }
}

fn check_account(name: &str, m: &VenueMarket, now: u64, w: &FxAccount) {
    let tp = m
        .try_position(now)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(tp.readable, w.try_position.readable, "{name} tryPosition.readable");
    assert_eq!(tp.collateral, w.try_position.collateral.0, "{name} tryPosition.collateral");
    assert_eq!(tp.supply_shares, w.try_position.supply_shares.0, "{name} tryPosition.supplyShares");
    assert_eq!(tp.supplied, w.try_position.supplied.0, "{name} tryPosition.supplied");
    assert_eq!(tp.debt, w.try_position.debt.0, "{name} tryPosition.debt");
    require_result(&w.debt_of, m.debt_of(now), &format!("{name} debtOf"));
    require_result(&w.supplied_of, m.supplied_of(now), &format!("{name} suppliedOf"));
    assert_eq!(m.position.collateral, w.collateral_of.0, "{name} collateralOf");
    assert_eq!(m.free_liquidity(), w.free_liquidity.0, "{name} freeLiquidity");
    for row in &w.supply_shares_to_assets {
        require_result(
            &MmFxResult { v: row.v, err: row.err.clone() },
            m.supply_shares_to_assets(row.shares.0, now),
            &format!("{name} supplySharesToAssets {}", row.shares.0),
        );
    }
    for row in &w.borrow_rate_after {
        let (ok, rate) = m
            .try_borrow_rate(row.d_borrow.0, row.d_supply_down.0, now)
            .unwrap();
        let msg = format!("{name} borrowRateAfter({}, {})", row.d_borrow.0, row.d_supply_down.0);
        assert_eq!(ok, row.ok, "{msg}");
        assert_eq!(rate, row.rate.0, "{msg}");
    }
}

/// Compares the port's router (and pool) after a transition with the chain's (the Go
/// `mmCheckState`).
fn check_state(name: &str, r: &Router, p: Option<&Pool>, w: &FxSnap) {
    let want = w.router.state();
    assert_eq!(want.venues.len(), r.venues.len(), "{name} venue count");
    assert_eq!(want.max_drawn_assets, r.max_drawn_assets, "{name} maxDrawnAssets");
    for i in 0..want.venues.len() {
        let (a, b) = (&want.venues[i], &r.venues[i]);
        let msg = format!("{name} venue {i}");
        assert_eq!(b.morpho.market, a.morpho.market, "{msg} market");
        assert_eq!(b.morpho.position, a.morpho.position, "{msg} position");
        assert_eq!(b.morpho.rate_at_target, a.morpho.rate_at_target, "{msg} rateAtTarget");
        assert_eq!(b.managed_collateral, a.managed_collateral, "{msg} managedCollateral");
        assert_eq!(b.managed_supply_shares, a.managed_supply_shares, "{msg} managedSupplyShares");
        assert_eq!(b.max_borrow_rate_wad, a.max_borrow_rate_wad, "{msg} maxBorrowRateWad");
    }
    if let (Some(p), Some(wp)) = (p, &w.pool) {
        assert_eq!(p.physical, wp.physical.0, "{name} physical");
        for i in 0..p.loans.len() {
            assert_eq!(p.loans[i].liquid, wp.loans[i].liquid.0, "{name} liquid[{i}]");
        }
    }
}

/// Recomputes `FLAMMLens.facts` and the numeric part of `FLAMMLens.venues` (health only where the
/// Lens could price it).
fn check_lens(name: &str, s: &FxSnap) {
    let (r, p, now) = (s.router.state(), s.pool.as_ref().unwrap().state(), s.timestamp);
    let lens = s.lens.as_ref().unwrap();
    let b = gate::book_of(&p, &r, now).unwrap_or_else(|e| panic!("{name}: {e}"));
    let gross = gate::gross(&b).unwrap();
    let f = &lens.facts;
    assert_eq!(f[0].0, b.physical, "{name} lens physical");
    assert_eq!(f[1].0, b.posted, "{name} lens posted");
    assert_eq!(f[2].0, gross, "{name} lens gross");
    assert_eq!(f[3].0, b.legs[0].liquid, "{name} lens liquid");
    assert_eq!(f[4].0, b.legs[0].supplied, "{name} lens supplied");
    assert_eq!(f[5].0, b.legs[0].debt, "{name} lens debt");
    for (i, lv) in lens.venues.iter().enumerate() {
        let vp = r.venue_position(i as u16, now).unwrap();
        for k in 0..5 {
            assert_eq!(lv[k].0, vp[k], "{name} lens venue {i} [{k}]");
        }
        assert_eq!(
            lv[5].0,
            r.venues[i].morpho.free_liquidity(),
            "{name} lens venue {i} freeLiquidity"
        );
        if !lv[6].0.is_zero() {
            let h = r
                .venue_health(i as u16, s.prices[r.venues[i].loan_index as usize].0, now)
                .unwrap();
            assert_eq!(lv[6].0, h, "{name} lens venue {i} health");
        }
    }
}

/// Recomputes the pool-level gate reads recorded with a live snapshot.
fn check_pool_views(name: &str, r: &Router, p: &Pool, now: u64, w: &MmFxPool) {
    let b = gate::book_of(p, r, now).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(gate::gross(&b).unwrap(), w.gross.0, "{name} gross");
    assert_eq!(b.legs[0].liquid, w.loan_position[0].0, "{name} loanPosition.liquid");
    assert_eq!(b.legs[0].supplied, w.loan_position[1].0, "{name} loanPosition.supplied");
    assert_eq!(b.legs[0].debt, w.loan_position[2].0, "{name} loanPosition.debt");
    require_result(&w.total_assets, gate::total_assets(p, r, now), &format!("{name} totalAssets"));
}

fn snap_prices(s: &FxSnap) -> Vec<U256> {
    s.prices.iter().map(|d| d.0).collect()
}

#[derive(Deserialize)]
struct NamedSnap {
    name: String,
    snap: FxSnap,
}

#[derive(Deserialize)]
struct LiveViews {
    snaps: Vec<NamedSnap>,
}

/// The deployed c104 router / account / Morpho views at block 51317000 and warped to +1s, +1h, +1d,
/// +30d, +365d (accrual and IRM adaptation), with managed figures below the position, a binding
/// rate ceiling (the non-monotone `fundingCeiling` branch) and an unreadable IRM inside and beyond
/// the grace.
#[test]
fn mm_live_views() {
    let fx: LiveViews = load("mm_live_views.json.gz");
    assert_eq!(fx.snaps.len(), 11);
    for s in &fx.snaps {
        let r = s.snap.router.state();
        let p = s.snap.pool.as_ref().unwrap().state();
        check_views(
            &s.name,
            &r,
            &snap_prices(&s.snap),
            s.snap.timestamp,
            s.snap.views.as_ref().unwrap(),
        );
        check_pool_views(&s.name, &r, &p, s.snap.timestamp, s.snap.pool.as_ref().unwrap());
        check_lens(&s.name, &s.snap);
    }
}

#[derive(Deserialize)]
struct RealSell {
    #[serde(rename = "preBlock")]
    pre_block: FxSnap,
    pre: FxSnap,
    post: FxSnap,
}

/// The first settled swap (block 51302916, tx `0x46c3cd72…`: 15000 sats in, 11301759 USDC out)
/// replayed through the settlement port from the in-block pre-state, matching the chain's
/// post-state and views.
#[test]
fn mm_real_sell() {
    let fx: RealSell = load("mm_real_sell.json.gz");
    for (name, s) in [("preBlock", &fx.pre_block), ("pre", &fx.pre), ("post", &fx.post)] {
        let r = s.router.state();
        let p = s.pool.as_ref().unwrap().state();
        check_views(name, &r, &snap_prices(s), s.timestamp, s.views.as_ref().unwrap());
        check_pool_views(name, &r, &p, s.timestamp, s.pool.as_ref().unwrap());
        check_lens(name, s);
    }
    let (mut r, mut p) = (fx.pre.router.state(), fx.pre.pool.as_ref().unwrap().state());
    let price = p.price_wad[0];
    router::settle_sell(
        &mut p,
        &mut r,
        0,
        price,
        U256::from(15000),
        U256::from(11_301_759),
        fx.pre.timestamp,
    )
    .expect("realSell");
    check_state("realSell", &r, Some(&p), &fx.post);
}

#[derive(Deserialize)]
struct SettleStep {
    name: String,
    sell: bool,
    ok: bool,
    used: Dec,
    out: Dec,
    pre: FxSnap,
    post: FxSnap,
}

#[derive(Deserialize)]
struct LiveSettle {
    steps: Vec<SettleStep>,
}

/// The fork's settlement sequence on the live pool: sells that borrow and post, a buy that repays
/// and reclaims while indebted, a buy that repays everything and lends the surplus, a sell that
/// withdraws the supply before borrowing, a debt-free reclaim, each from the chain's pre-state.
#[test]
fn mm_live_settle() {
    let fx: LiveSettle = load("mm_live_settle.json.gz");
    let mut settled = 0;
    for s in &fx.steps {
        let (mut r, mut p) = (s.pre.router.state(), s.pre.pool.as_ref().unwrap().state());
        let now = s.pre.timestamp;
        if s.ok {
            let res = if s.sell {
                let price = p.price_wad[0];
                router::settle_sell(&mut p, &mut r, 0, price, s.used.0, s.out.0, now)
            } else {
                router::settle_buy(&mut p, &mut r, 0, s.used.0, s.out.0, now)
            };
            res.unwrap_or_else(|e| panic!("{}: {e}", s.name));
            settled += 1;
        }
        check_state(&s.name, &r, Some(&p), &s.post);
        let pr = s.post.router.state();
        let pp = s.post.pool.as_ref().unwrap().state();
        check_views(
            &s.name,
            &pr,
            &snap_prices(&s.post),
            s.post.timestamp,
            s.post.views.as_ref().unwrap(),
        );
        check_pool_views(&s.name, &pr, &pp, s.post.timestamp, s.post.pool.as_ref().unwrap());
        check_lens(&s.name, &s.post);
    }
    assert!(settled >= 6);
}

// ------------------------------------------------------------------ the multi-venue router

#[derive(Deserialize, Default)]
struct FxOp {
    op: String,
    #[serde(default)]
    idx: u8,
    #[serde(default)]
    assets: Dec,
    #[serde(default, rename = "collateralIn")]
    collateral_in: Dec,
    #[serde(default, rename = "priceWad")]
    price_wad: Dec,
    #[serde(default, rename = "priceWads")]
    price_wads: Vec<Dec>,
    #[serde(default)]
    n: u8,
    #[serde(default)]
    id: u16,
    #[serde(default, rename = "debtCap")]
    debt_cap: Dec,
    #[serde(default, rename = "supplyCap")]
    supply_cap: Dec,
    #[serde(default, rename = "maxBorrowRateWad")]
    max_borrow_rate_wad: Dec,
    #[serde(default)]
    proportional: bool,
}

/// Runs one pool-scoped Router call on a clone, committing only on success, and returns its words.
fn apply_op(r: &mut Router, op: &FxOp, now: u64) -> Result<Vec<U256>, FlammError> {
    let mut c = r.clone();
    let out = match op.op.as_str() {
        "fund" => {
            let (w, b, p) = c.fund(op.idx, op.assets.0, op.collateral_in.0, op.price_wad.0, now)?;
            vec![w, b, p]
        }
        "repayCascade" => vec![c.repay_cascade(op.idx, op.assets.0, now)?],
        "supplyCascade" => vec![c.supply_cascade(op.idx, op.assets.0, now)?],
        "reclaim" | "reclaimBestEffort" => {
            let pw: Vec<U256> = op
                .price_wads
                .iter()
                .map(|d| d.0)
                .collect();
            vec![c.reclaim(op.assets.0, &pw, op.op == "reclaim", now)?]
        }
        "supply" => vec![c.supply(op.id, op.assets.0, now)?],
        "withdrawSupplied" => vec![c.withdraw_supplied_entry(op.id, op.assets.0, now)?],
        "repay" => vec![c.repay(op.id, op.assets.0, now)?],
        "postCollateral" => {
            c.post_collateral(op.id, op.assets.0)?;
            vec![]
        }
        "borrow" => {
            c.borrow(op.id, op.assets.0, op.price_wad.0, now)?;
            vec![]
        }
        "withdrawCollateral" => {
            c.withdraw_collateral(op.id, op.assets.0, op.price_wad.0, op.proportional, now)?;
            vec![]
        }
        "setMaxDrawnAssets" => {
            if op.n == 0 || op.n as usize > c.loans.len() {
                return Err(FlammError::InvalidConfig);
            }
            c.max_drawn_assets = op.n;
            vec![]
        }
        "setVenueCaps" => {
            let v = &mut c.venues[op.id as usize];
            v.debt_cap = op.debt_cap.0;
            v.supply_cap = op.supply_cap.0;
            v.max_borrow_rate_wad = op.max_borrow_rate_wad.0;
            vec![]
        }
        other => panic!("unknown op {other}"),
    };
    *r = c;
    Ok(out)
}

#[derive(Deserialize)]
struct MultiStep {
    name: String,
    #[serde(default)]
    args: Option<FxOp>,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    ret: String,
    #[serde(default)]
    err: String,
    #[serde(default)]
    pre: FxSnap,
    #[serde(default)]
    post: FxSnap,
}

#[derive(Deserialize)]
struct MultiVenue {
    setup: FxSnap,
    steps: Vec<MultiStep>,
}

/// The per-asset crosses the generator passed to the views (the live grid's fifth price column is
/// the unscaled cross).
fn multi_prices(s: &FxSnap) -> Vec<U256> {
    s.views
        .as_ref()
        .unwrap()
        .loans
        .iter()
        .map(|l| l.funding_ceiling[4].price_wad.0)
        .collect()
}

/// The DEPLOYED router bytecode (etched on a fork, fresh storage) over three venues: two USDC
/// markets (the live 86% market and a seeded 77% market sharing its oracle and IRM) and a WETH
/// market behind a constant oracle, with venue debt / supply caps, distinct borrow / supply /
/// withdraw / repay orders, the drawn set, a binding rate ceiling on the small market, and an IRM
/// outage on one venue inside and beyond the grace. Every call is replayed from the chain's
/// pre-state; post-state and every view must match, reverts by selector (and
/// `InsufficientLiquidity`'s shortfall by the plan's remainder).
#[test]
fn mm_multi_venue() {
    let fx: MultiVenue = load("mm_multi_venue.json.gz");
    check_views(
        "setup",
        &fx.setup.router.state(),
        &multi_prices(&fx.setup),
        fx.setup.timestamp,
        fx.setup.views.as_ref().unwrap(),
    );
    let (mut ops, mut fails) = (0, 0);
    // transient storage outlives each call: the whole generator test is one transaction
    let mut transient = Default::default();
    for s in &fx.steps {
        let Some(args) = &s.args else { continue };
        let mut r = s.pre.router.state();
        r.transient_repay = transient;
        let now = s.pre.timestamp;
        let out = apply_op(&mut r, args, now);
        if s.ok {
            let out = out.unwrap_or_else(|e| panic!("{}: {e}", s.name));
            let raw = hex_bytes(&s.ret);
            assert_eq!(raw.len(), out.len() * 32, "{}", s.name);
            for (i, w) in abi_words(&raw).iter().enumerate() {
                assert_eq!(out[i], *w, "{} ret[{i}]", s.name);
            }
            ops += 1;
        } else {
            let want =
                revert_of_hex(&s.err).unwrap_or_else(|| panic!("{}: unmapped {}", s.name, s.err));
            assert_eq!(out.err(), Some(want), "{}", s.name);
            if s.err.starts_with("0xc730333f") && args.op == "fund" {
                let plan = s
                    .pre
                    .router
                    .state()
                    .build_plan(
                        args.idx,
                        args.assets.0,
                        args.collateral_in.0,
                        args.price_wad.0,
                        now,
                    )
                    .unwrap();
                let raw = hex_bytes(&s.err);
                assert_eq!(revert_payload(&raw), Some(plan.remaining), "{} shortfall", s.name);
            }
            fails += 1;
        }
        transient = r.transient_repay.clone();
        check_state(&s.name, &r, None, &s.post);
        check_views(
            &s.name,
            &s.post.router.state(),
            &multi_prices(&s.post),
            s.post.timestamp,
            s.post.views.as_ref().unwrap(),
        );
    }
    assert!(ops >= 25, "ops {ops}");
    assert!(fails >= 5, "fails {fails}");
}

/// The repay snapshot is EIP-1153 transient storage, so it survives within one transaction and is
/// gone in the next. From the live buy that repays an indebted venue and then reclaims its
/// collateral (still indebted after): a proportional `withdrawCollateral` in the same transaction
/// is judged against the cascade's pre-repay snapshot (85209356 debt / 196499 coll, now 64209356 /
/// 148072: `Unhealthy` even for one wei), while in a later transaction it reverts `NoRepaySnapshot`
/// (`MMRouterLib.sol:200-201`). A raw repay leaves its snapshot for the caller's transaction until
/// `end_transaction`.
#[test]
fn mm_transient_repay_scope() {
    let fx: LiveSettle = load("mm_live_settle.json.gz");
    let s = fx
        .steps
        .iter()
        .find(|s| s.name == "buyReclaimIndebted")
        .expect("step");
    let now = s.pre.timestamp;
    let withdraw = U256::from(1000);

    // the buy's own transaction: settleBuy's repayCascade snapshot is still readable
    let (mut rc, mut pc) = (s.pre.router.state(), s.pre.pool.as_ref().unwrap().state());
    router::settle_buy_legs(&mut pc, &mut rc, 0, s.used.0, s.out.0, now).expect("legs");
    let debt = rc.venues[0]
        .morpho
        .debt_of(now)
        .unwrap();
    assert!(!debt.is_zero());
    assert_eq!(
        rc.clone()
            .withdraw_collateral(0, U256::from(1), U256::ZERO, true, now),
        Err(FlammError::Unhealthy)
    );

    // a later transaction: the committed settlement leaves no snapshot behind
    let (mut r, mut p) = (s.pre.router.state(), s.pre.pool.as_ref().unwrap().state());
    router::settle_buy(&mut p, &mut r, 0, s.used.0, s.out.0, now).expect("settle");
    assert!(r.transient_repay.is_empty());
    assert_eq!(
        r.clone()
            .withdraw_collateral(0, withdraw, U256::ZERO, true, now),
        Err(FlammError::NoRepaySnapshot)
    );

    // raw entries share the caller's transaction until it ends
    let mut c = r.clone();
    c.repay(0, U256::from(5_000_000), now)
        .expect("repay");
    assert_eq!(
        c.clone()
            .withdraw_collateral(0, withdraw, U256::ZERO, true, now),
        Ok(())
    );
    c.end_transaction();
    assert_eq!(
        c.withdraw_collateral(0, withdraw, U256::ZERO, true, now),
        Err(FlammError::NoRepaySnapshot)
    );
}

/// The `WAD` the gate scales with is the one the module fixtures were generated against.
#[test]
fn wad_is_1e18() {
    assert_eq!(WAD, U256::from(10u64).pow(U256::from(18)));
}
