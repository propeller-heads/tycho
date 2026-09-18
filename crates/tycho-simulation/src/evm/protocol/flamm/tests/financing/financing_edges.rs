// Copyright (c) 2026 Everlong Labs Limited

//! Edge replays for the financing module. The fixtures under `testdata/edges/` come from
//! `testdata/gen/{Morpho,Account,Router,Gate,Settle}Edges.t.sol`: threshold edges and seeded grids
//! replayed against the DEPLOYED Morpho Blue, AdaptiveCurveIrm, MorphoBlueAccount and MMRouter on a
//! Base fork (block 51317000), plus `FLAMMGateLib` compiled from 80abd43 behind a harness. Every
//! array opens with a header element. A row compares the full result: revert class or returned
//! values and the written post-state.

use alloy::primitives::U256;
use serde::Deserialize;

use super::common::*;
use crate::evm::protocol::flamm::{
    error::FlammError,
    gate::{self, LoanCfg, Pool},
    irm,
    math::WAD,
    morpho::{Market, VenueMarket},
    router::{self, Router},
};

fn want_err(got: &Result<impl std::fmt::Debug, FlammError>, want: Option<FlammError>) -> bool {
    match want {
        None => got.is_ok(),
        Some(w) => got.as_ref().err() == Some(&w),
    }
}

// ------------------------------------------------------------------ Morpho transitions

#[derive(Deserialize)]
struct MorphoRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    tag: String,
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default, rename = "noIrm")]
    no_irm: bool,
    #[serde(default)]
    lltv: Dec,
    #[serde(default)]
    rat: SignedDec,
    #[serde(default)]
    price: Dec,
    #[serde(default)]
    dead: bool,
    #[serde(default, deserialize_with = "de_int_default")]
    op: u64,
    #[serde(default)]
    a: Dec,
    #[serde(default)]
    s: Dec,
    #[serde(default)]
    market: MarketFx,
    #[serde(default)]
    position: PositionFx,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    revert: String,
    #[serde(default)]
    r0: Dec,
    #[serde(default)]
    r1: Dec,
    #[serde(default, rename = "postMarket")]
    post_market: MarketFx,
    #[serde(default, rename = "postPosition")]
    post_position: PositionFx,
    #[serde(default, rename = "postRat")]
    post_rat: SignedDec,
}

/// Every Blue transition row against `morpho` / `irm`.
#[test]
fn morpho_edges() {
    let rows: Vec<MorphoRow> = load("edges/mm_morpho_edges.json.gz");
    assert!(rows.len() > 500);
    let mut rep = Report::new("morpho");
    for (i, r) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.header)
    {
        let mut v = VenueMarket {
            market: r.market.state(),
            position: r.position.state(),
            lltv: r.lltv.0,
            has_irm: !r.no_irm,
            irm_readable: true,
            rate_at_target: r.rat.0.abs,
            oracle_ok: !r.dead && !r.price.0.is_zero(),
            oracle_price: r.price.0,
            oracle_zero: !r.dead && r.price.0.is_zero(),
        };
        let (mut r0, mut r1) = (U256::ZERO, U256::ZERO);
        let res: Result<(), FlammError> = match r.op {
            0 => v.accrue(r.now),
            1 => {
                r0 = r.a.0;
                v.supply(r.a.0, r.now).map(|s| r1 = s)
            }
            2 => v
                .withdraw(r.a.0, r.s.0, r.now)
                .map(|(a, s)| {
                    r0 = a;
                    r1 = s;
                }),
            3 => {
                r0 = r.a.0;
                v.borrow(r.a.0, r.now).map(|s| r1 = s)
            }
            4 => v
                .repay(r.a.0, r.s.0, r.now)
                .map(|(a, s)| {
                    r0 = a;
                    r1 = s;
                }),
            5 => v.supply_collateral(r.a.0),
            6 => v.withdraw_collateral(r.a.0, r.now),
            _ => panic!("op {}", r.op),
        };
        let ctx = format!(
            "row {i} tag={} op={} a={} s={} market={:?} pos={:?} rat={} price={} dead={} noIrm={}",
            r.tag,
            r.op,
            r.a.0,
            r.s.0,
            r.market,
            r.position,
            r.rat.0.abs,
            r.price.0,
            r.dead,
            r.no_irm
        );
        if !r.ok {
            let want = revert_of_hex(&r.revert);
            if want.is_none() || !want_err(&res, want) {
                rep.add(format!("{ctx}: port={} sol={:?}", err_class(&res), want));
            }
            continue;
        }
        if let Err(e) = res {
            rep.add(format!("{ctx}: port={e} sol=ok"));
            continue;
        }
        if (1..=4).contains(&r.op) && (r0 != r.r0.0 || r1 != r.r1.0) {
            rep.add(format!("{ctx}: returns port=({r0},{r1}) sol=({},{})", r.r0.0, r.r1.0));
        }
        if v.market != r.post_market.state() {
            rep.add(format!("{ctx}: post market port={:?} sol={:?}", v.market, r.post_market));
        }
        if v.position != r.post_position.state() {
            rep.add(format!(
                "{ctx}: post position port={:?} sol={:?}",
                v.position, r.post_position
            ));
        }
        if v.rate_at_target != r.post_rat.0.abs {
            rep.add(format!(
                "{ctx}: post rateAtTarget port={} sol={}",
                v.rate_at_target, r.post_rat.0.abs
            ));
        }
    }
    rep.finish(rows.len() - 1);
}

// ------------------------------------------------------------------ AdaptiveCurveIrm

#[derive(Deserialize)]
struct IrmRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    tag: String,
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default)]
    tsa: Dec,
    #[serde(default)]
    tba: Dec,
    #[serde(default, rename = "lastUpdate")]
    last_update: Dec,
    #[serde(default)]
    rat: SignedDec,
    #[serde(default, rename = "viewOk")]
    view_ok: bool,
    #[serde(default)]
    view: String,
    #[serde(default, rename = "mutOk")]
    mut_ok: bool,
    #[serde(default, rename = "mut")]
    mutated: String,
    #[serde(default)]
    end: SignedDec,
}

/// `borrow_rate` against `borrowRateView` and the stored `endRateAtTarget` of `borrowRate`.
#[test]
fn irm_edges() {
    let rows: Vec<IrmRow> = load("edges/mm_irm_edges.json.gz");
    assert!(rows.len() > 600);
    let mut rep = Report::new("irm");
    for (i, r) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.header)
    {
        let m = Market {
            total_supply_assets: r.tsa.0,
            total_supply_shares: r.tsa.0,
            total_borrow_assets: r.tba.0,
            total_borrow_shares: r.tba.0,
            last_update: r.last_update.0,
            fee: U256::ZERO,
        };
        let res = irm::borrow_rate(&m, r.rat.0.abs, r.now);
        let ctx = format!(
            "row {i} tag={} tsa={} tba={} lastUpdate={} rat={}",
            r.tag, r.tsa.0, r.tba.0, r.last_update.0, r.rat.0.abs
        );
        if !r.view_ok {
            let want = revert_of_hex(&r.view);
            if want.is_none() || !want_err(&res, want) {
                rep.add(format!("{ctx}: port={} sol={:?}", err_class(&res), want));
            }
            continue;
        }
        let (avg, end) = match res {
            Ok(x) => x,
            Err(e) => {
                rep.add(format!("{ctx}: port={e} sol=ok"));
                continue;
            }
        };
        if avg.to_string() != r.view ||
            !r.mut_ok ||
            avg.to_string() != r.mutated ||
            end != r.end.0.abs
        {
            rep.add(format!(
                "{ctx}: port=({avg},{end}) sol=({},{},{})",
                r.view, r.mutated, r.end.0.abs
            ));
        }
    }
    rep.finish(rows.len() - 1);
}

// ------------------------------------------------------------------ MorphoBlueAccount

#[derive(Deserialize, Default)]
struct FinRes {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    v: Dec,
    #[serde(default)]
    revert: String,
}

impl FinRes {
    fn check(&self, rep: &mut Report, ctx: &str, got: Result<U256, FlammError>) {
        if !self.ok {
            let want = revert_of_hex(&self.revert);
            if want.is_none() || !want_err(&got, want) {
                rep.add(format!("{ctx}: port={} sol={:?}", err_class(&got), want));
            }
            return;
        }
        match got {
            Err(e) => rep.add(format!("{ctx}: port={e} sol={}", self.v.0)),
            Ok(g) if g != self.v.0 => rep.add(format!("{ctx}: port={g} sol={}", self.v.0)),
            _ => {}
        }
    }
}

#[derive(Deserialize, Default)]
struct AcctTryPosition {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    revert: String,
    #[serde(default)]
    readable: bool,
    #[serde(default)]
    collateral: Dec,
    #[serde(default, rename = "supplyShares")]
    supply_shares: Dec,
    #[serde(default)]
    supplied: Dec,
    #[serde(default)]
    debt: Dec,
}

#[derive(Deserialize, Default)]
struct AcctShares {
    shares: Dec,
    res: FinRes,
}

#[derive(Deserialize, Default)]
struct AcctRateRes {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    revert: String,
    #[serde(default)]
    rok: bool,
    #[serde(default)]
    rate: Dec,
}

#[derive(Deserialize, Default)]
struct AcctRate {
    #[serde(rename = "dB")]
    db: Dec,
    #[serde(rename = "dS")]
    ds: Dec,
    res: AcctRateRes,
}

#[derive(Deserialize, Default)]
struct AcctOracle {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    v: Dec,
}

#[derive(Deserialize)]
struct AccountRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    tag: String,
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default)]
    lltv: Dec,
    #[serde(default)]
    rat: SignedDec,
    #[serde(default)]
    price: Dec,
    #[serde(default, rename = "oracleDead")]
    oracle_dead: bool,
    #[serde(default, rename = "irmDead")]
    irm_dead: bool,
    #[serde(default)]
    market: MarketFx,
    #[serde(default)]
    position: PositionFx,
    #[serde(default, rename = "tryPosition")]
    try_position: AcctTryPosition,
    #[serde(default, rename = "debtOf")]
    debt_of: FinRes,
    #[serde(default, rename = "suppliedOf")]
    supplied_of: FinRes,
    #[serde(default, rename = "freeLiquidity")]
    free_liquidity: FinRes,
    #[serde(default, rename = "sharesToAssets")]
    shares_to_assets: Vec<AcctShares>,
    #[serde(default, rename = "rateAfter")]
    rate_after: Vec<AcctRate>,
    #[serde(default, rename = "oraclePrice")]
    oracle_price: AcctOracle,
    #[serde(default, deserialize_with = "de_int_default")]
    op: u64,
    #[serde(default)]
    a: Dec,
    #[serde(default)]
    s: Dec,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    revert: String,
    #[serde(default)]
    r0: Dec,
    #[serde(default)]
    r1: Dec,
    #[serde(default, rename = "postMarket")]
    post_market: MarketFx,
    #[serde(default, rename = "postPosition")]
    post_position: PositionFx,
    #[serde(default, rename = "postRat")]
    post_rat: SignedDec,
}

/// The deployed account's views and Router-driven mutators.
#[test]
fn account_edges() {
    let rows: Vec<AccountRow> = load("edges/mm_account_edges.json.gz");
    assert!(rows.len() > 400);
    let mut rep = Report::new("account");
    for (i, r) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.header)
    {
        let base = VenueMarket {
            market: r.market.state(),
            position: r.position.state(),
            lltv: r.lltv.0,
            has_irm: true,
            irm_readable: !r.irm_dead,
            rate_at_target: r.rat.0.abs,
            oracle_ok: r.oracle_price.ok,
            oracle_price: r.oracle_price.v.0,
            oracle_zero: !r.oracle_dead && r.price.0.is_zero(),
        };
        let ctx = format!(
            "row {i} tag={} now={} market={:?} pos={:?} rat={} irmDead={} oracle=({},{})",
            r.tag,
            r.now,
            r.market,
            r.position,
            r.rat.0.abs,
            r.irm_dead,
            r.oracle_price.ok,
            r.oracle_price.v.0
        );
        let mut v = base;
        let tp = v.try_position(r.now);
        if !r.try_position.ok {
            let want = revert_of_hex(&r.try_position.revert);
            if want.is_none() || !want_err(&tp, want) {
                rep.add(format!("{ctx} tryPosition: port={} sol={:?}", err_class(&tp), want));
            }
        } else {
            match tp {
                Err(e) => rep.add(format!("{ctx} tryPosition: port={e} sol=ok")),
                Ok(tp) => {
                    let w = &r.try_position;
                    if tp.readable != w.readable ||
                        tp.collateral != w.collateral.0 ||
                        tp.supply_shares != w.supply_shares.0 ||
                        tp.supplied != w.supplied.0 ||
                        tp.debt != w.debt.0
                    {
                        rep.add(format!(
                            "{ctx} tryPosition: port={tp:?} sol=({},{},{},{},{})",
                            w.readable, w.collateral.0, w.supply_shares.0, w.supplied.0, w.debt.0
                        ));
                    }
                }
            }
        }
        r.debt_of
            .check(&mut rep, &format!("{ctx} debtOf"), v.debt_of(r.now));
        r.supplied_of
            .check(&mut rep, &format!("{ctx} suppliedOf"), v.supplied_of(r.now));
        r.free_liquidity
            .check(&mut rep, &format!("{ctx} freeLiquidity"), Ok(v.free_liquidity()));
        for s in &r.shares_to_assets {
            s.res.check(
                &mut rep,
                &format!("{ctx} supplySharesToAssets({})", s.shares.0),
                v.supply_shares_to_assets(s.shares.0, r.now),
            );
        }
        for d in &r.rate_after {
            let got = v.borrow_rate_after(d.db.0, d.ds.0, r.now);
            let dctx = format!("{ctx} borrowRateAfter({},{})", d.db.0, d.ds.0);
            if !d.res.ok {
                let want = revert_of_hex(&d.res.revert);
                if want.is_none() || !want_err(&got, want) {
                    rep.add(format!("{dctx}: port={} sol={:?}", err_class(&got), want));
                }
            } else {
                match got {
                    Ok((ok, rate)) if ok == d.res.rok && rate == d.res.rate.0 => {}
                    other => rep.add(format!(
                        "{dctx}: port={other:?} sol=({},{})",
                        d.res.rok, d.res.rate.0
                    )),
                }
            }
        }
        if v != base {
            rep.add(format!("{ctx}: a view wrote state"));
        }
        if r.op == 0 {
            continue;
        }
        let (mut r0, mut r1) = (U256::ZERO, U256::ZERO);
        let res: Result<(), FlammError> = match r.op {
            1 => v
                .account_repay(r.a.0, r.now)
                .map(|x| r0 = x),
            2 => v
                .account_withdraw(r.a.0, r.s.0, r.now)
                .map(|(a, s)| {
                    r0 = a;
                    r1 = s;
                }),
            3 => v.account_borrow(r.a.0, r.now),
            4 => v
                .account_supply(r.a.0, r.now)
                .map(|x| r0 = x),
            5 => v.account_supply_collateral(r.a.0),
            6 => v.account_withdraw_collateral(r.a.0, r.now),
            _ => panic!("op {}", r.op),
        };
        let mctx = format!("{ctx} op={} a={} s={}", r.op, r.a.0, r.s.0);
        if !r.ok {
            let want = revert_of_hex(&r.revert);
            if want.is_none() || !want_err(&res, want) {
                rep.add(format!("{mctx}: port={} sol={:?}", err_class(&res), want));
            }
            continue;
        }
        if let Err(e) = res {
            rep.add(format!("{mctx}: port={e} sol=ok"));
            continue;
        }
        if r0 != r.r0.0 || r1 != r.r1.0 {
            rep.add(format!("{mctx}: returns port=({r0},{r1}) sol=({},{})", r.r0.0, r.r1.0));
        }
        if v.market != r.post_market.state() {
            rep.add(format!("{mctx}: post market port={:?} sol={:?}", v.market, r.post_market));
        }
        if v.position != r.post_position.state() {
            rep.add(format!(
                "{mctx}: post position port={:?} sol={:?}",
                v.position, r.post_position
            ));
        }
        if v.rate_at_target != r.post_rat.0.abs {
            rep.add(format!(
                "{mctx}: post rateAtTarget port={} sol={}",
                v.rate_at_target, r.post_rat.0.abs
            ));
        }
    }
    rep.finish(rows.len() - 1);
}

// ------------------------------------------------------------------ MMRouter

#[derive(Deserialize, Default)]
struct RouterVenueViews {
    #[serde(default, rename = "venuePosition")]
    venue_position: Call,
    #[serde(default)]
    health: Call,
    #[serde(default, rename = "healthLow")]
    health_low: Call,
    #[serde(default)]
    readable: Call,
    #[serde(default, rename = "freeLiquidity")]
    free_liquidity: Call,
}

#[derive(Deserialize, Default)]
struct RouterViews {
    #[serde(default)]
    positions: Call,
    #[serde(default)]
    position0: Call,
    #[serde(default)]
    position1: Call,
    #[serde(default)]
    position2: Call,
    #[serde(default)]
    quarantine0: Call,
    #[serde(default)]
    quarantine1: Call,
    #[serde(default)]
    drawn: Call,
    #[serde(default, rename = "minLltv")]
    min_lltv: Call,
    #[serde(default)]
    reclaimable: Call,
    #[serde(default, rename = "reclaimableP1zero")]
    reclaimable_p1zero: Call,
    #[serde(default, rename = "reclaimableBadLen")]
    reclaimable_bad_len: Call,
    #[serde(default, rename = "venueViews")]
    venue_views: Vec<RouterVenueViews>,
}

#[derive(Deserialize, Default)]
struct RouterCeiling {
    #[serde(default, deserialize_with = "de_int_default")]
    idx: u8,
    #[serde(default)]
    coll: Dec,
    #[serde(default)]
    price: Dec,
    #[serde(default)]
    res: Call,
}

#[derive(Deserialize)]
struct RouterRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    scenario: String,
    #[serde(default, deserialize_with = "de_int_default")]
    kind: u8,
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default)]
    state: FinRouterState,
    #[serde(default)]
    views: RouterViews,
    #[serde(default)]
    ceilings: Vec<RouterCeiling>,
    #[serde(default, deserialize_with = "de_int_default")]
    id: u16,
    #[serde(default, deserialize_with = "de_int_default")]
    idx: u8,
    #[serde(default)]
    a: Dec,
    #[serde(default, rename = "collIn")]
    coll_in: Dec,
    #[serde(default)]
    price: Dec,
    #[serde(default)]
    price1: Dec,
    #[serde(default)]
    prop: bool,
    #[serde(default)]
    pre: Dec,
    #[serde(default, rename = "preOk")]
    pre_ok: bool,
    #[serde(default, rename = "preRet")]
    pre_ret: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    ret: String,
    #[serde(default)]
    post: FinRouterState,
}

const FIN_P0: u64 = 788_432_322_552_395;
const FIN_P1: u64 = 262_810_774_184_131;

fn router_views(rep: &mut Report, r: &RouterRow, p: &Router) {
    let now = r.now;
    let ctx = format!("scenario {}", r.scenario);
    let v = &r.views;
    let pos = p.positions(now);
    match (&pos, v.positions.ok) {
        (Ok(pos), true) => {
            let w = v.positions.words();
            let cmp = |rep: &mut Report, name: &str, sol: Vec<U256>, got: &[U256]| {
                for (i, s) in sol.iter().enumerate() {
                    if i >= got.len() || *s != got[i] {
                        rep.add(format!("{ctx} positions.{name}[{i}]: port={got:?} sol={s}"));
                        return;
                    }
                }
            };
            cmp(rep, "coll", dyn_array(&w, 0), &pos.coll);
            cmp(rep, "sup", dyn_array(&w, 1), &pos.sup);
            cmp(rep, "debt", dyn_array(&w, 2), &pos.debt);
            if w[3] != pos.total_coll {
                rep.add(format!("{ctx} positions.total: port={} sol={}", pos.total_coll, w[3]));
            }
        }
        _ => {
            rep.check_words(&format!("{ctx} positions"), &v.positions, pos.clone().map(|_| vec![]))
        }
    }
    for (i, c) in [&v.position0, &v.position1, &v.position2]
        .into_iter()
        .enumerate()
    {
        rep.check_words(
            &format!("{ctx} position({i})"),
            c,
            p.position(i as u8, now)
                .map(|(a, b, d)| vec![a, b, d]),
        );
    }
    for (i, c) in [&v.quarantine0, &v.quarantine1]
        .into_iter()
        .enumerate()
    {
        rep.check_words(
            &format!("{ctx} quarantine({i})"),
            c,
            p.quarantine(i as u8, now)
                .map(|q| vec![bool_word(q.any), q.frozen_debt, q.frozen_coll]),
        );
    }
    rep.check_words(
        &format!("{ctx} drawn"),
        &v.drawn,
        p.drawn(now)
            .map(|(c, m)| vec![U256::from(c), U256::from(m)]),
    );
    rep.check_words(&format!("{ctx} minLltv"), &v.min_lltv, p.min_lltv(now).map(|x| vec![x]));
    rep.check_words(
        &format!("{ctx} reclaimable"),
        &v.reclaimable,
        p.reclaimable(&[U256::from(FIN_P0), U256::from(FIN_P1)], now)
            .map(|x| vec![x]),
    );
    rep.check_words(
        &format!("{ctx} reclaimable(P1=0)"),
        &v.reclaimable_p1zero,
        p.reclaimable(&[U256::from(FIN_P0), U256::ZERO], now)
            .map(|x| vec![x]),
    );
    rep.check_words(
        &format!("{ctx} reclaimable(len 1)"),
        &v.reclaimable_bad_len,
        p.reclaimable(&[U256::ZERO], now)
            .map(|x| vec![x]),
    );
    for (i, vv) in v.venue_views.iter().enumerate() {
        let id = i as u16;
        let price = U256::from(if i == 3 { FIN_P1 } else { FIN_P0 });
        rep.check_words(
            &format!("{ctx} venuePosition({i})"),
            &vv.venue_position,
            p.venue_position(id, now)
                .map(|a| a.to_vec()),
        );
        rep.check_words(
            &format!("{ctx} venueHealth({i})"),
            &vv.health,
            p.venue_health(id, price, now)
                .map(|h| vec![h]),
        );
        rep.check_words(
            &format!("{ctx} venueHealth({i}, p/2)"),
            &vv.health_low,
            p.venue_health(id, price >> 1, now)
                .map(|h| vec![h]),
        );
        rep.check_words(
            &format!("{ctx} venueReadable({i})"),
            &vv.readable,
            p.read(&p.venues[i], now)
                .map(|rd| vec![bool_word(rd.readable)]),
        );
        rep.check_words(
            &format!("{ctx} venueFreeLiquidity({i})"),
            &vv.free_liquidity,
            Ok(vec![p.venues[i].morpho.free_liquidity()]),
        );
    }
    for c in &r.ceilings {
        rep.check_words(
            &format!("{ctx} fundingCeiling({},{},{})", c.idx, c.coll.0, c.price.0),
            &c.res,
            p.funding_ceiling(c.idx, c.coll.0, c.price.0, now)
                .map(|x| vec![x]),
        );
    }
}

fn router_op(rep: &mut Report, r: &RouterRow, base: &Router, now: u64) {
    let mut p = base.clone();
    let ctx = format!(
        "scenario {} kind={} idx={} id={} a={} collIn={} price={} price1={} prop={} pre={}",
        r.scenario, r.kind, r.idx, r.id, r.a.0, r.coll_in.0, r.price.0, r.price1.0, r.prop, r.pre.0
    );
    let got: Result<Vec<U256>, FlammError> = match r.kind {
        1 => p
            .fund(r.idx, r.a.0, r.coll_in.0, r.price.0, now)
            .map(|(w, b, po)| vec![w, b, po]),
        2 => p
            .repay_cascade(r.idx, r.a.0, now)
            .map(|x| vec![x]),
        3 => p
            .supply_cascade(r.idx, r.a.0, now)
            .map(|x| vec![x]),
        4 | 5 => p
            .reclaim(r.a.0, &[r.price.0, r.price1.0], r.kind == 4, now)
            .map(|x| vec![x]),
        6 => p
            .borrow(r.id, r.a.0, r.price.0, now)
            .map(|_| vec![]),
        7 => p
            .repay(r.id, r.a.0, now)
            .map(|x| vec![x]),
        8 => p
            .supply(r.id, r.a.0, now)
            .map(|x| vec![x]),
        9 => p
            .withdraw_supplied_entry(r.id, r.a.0, now)
            .map(|x| vec![x]),
        10 => {
            if r.prop {
                let mut q = p.clone();
                let perr = q.repay(r.id, r.pre.0, now);
                if r.pre_ok != perr.is_ok() {
                    rep.add(format!(
                        "{ctx}: pre-repay port={} sol ok={} {}",
                        err_class(&perr),
                        r.pre_ok,
                        r.pre_ret
                    ));
                    return;
                }
                if perr.is_ok() {
                    p = q;
                }
            }
            p.withdraw_collateral(r.id, r.a.0, r.price.0, r.prop, now)
                .map(|_| vec![])
        }
        11 => p
            .post_collateral(r.id, r.a.0)
            .map(|_| vec![]),
        k => panic!("kind {k}"),
    };
    let c = Call { ok: r.ok, ret: r.ret.clone() };
    rep.check_words(&ctx, &c, got.clone());
    if !r.ok || got.is_err() {
        return;
    }
    let want = r.post.router();
    for i in 0..r.post.venues.len() {
        if let Some(d) = venue_diff(&p.venues[i], &want.venues[i], i) {
            rep.add(format!("{ctx}: post {d}"));
        }
    }
}

/// The deployed MMRouter's views, funding ceilings and entries over 26 scenarios.
#[test]
fn router_edges() {
    let rows: Vec<RouterRow> = load("edges/mm_router_edges.json.gz");
    assert!(rows.len() > 1000);
    let mut rep = Report::new("router");
    let mut base: Option<Router> = None;
    let mut now = 0u64;
    let mut ops = 0;
    for r in rows.iter().filter(|r| !r.header) {
        if r.kind == 0 {
            let b = r.state.router();
            now = r.now;
            router_views(&mut rep, r, &b);
            base = Some(b);
            continue;
        }
        ops += 1;
        router_op(
            &mut rep,
            r,
            base.as_ref()
                .expect("scenario before op"),
            now,
        );
    }
    eprintln!("[router] ops {ops}");
    rep.finish(rows.len() - 1);
}

// ------------------------------------------------------------------ FLAMMGateLib

/// `FLAMMGateLib`'s pure law and storage composites.
#[test]
fn gate_edges() {
    let rows: Vec<GateRow> = load("edges/gate_edges.json.gz");
    assert!(rows.len() > 400);
    gate_replay("gate", &rows);
}

// ------------------------------------------------------------------ FLAMMSwapLib settlement

#[derive(Deserialize, Default)]
struct SettlePool {
    #[serde(default)]
    physical: Dec,
    #[serde(default)]
    liquid: Dec,
    #[serde(default, rename = "reserveTarget")]
    reserve_target: Dec,
    #[serde(default)]
    features: Dec,
    #[serde(default)]
    ltv: Dec,
    #[serde(default)]
    phi: Dec,
    #[serde(default)]
    eps: Dec,
    #[serde(default, rename = "priceWad")]
    price_wad: Dec,
}

#[derive(Deserialize)]
struct SettleRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    buy: bool,
    #[serde(default, rename = "amountIn")]
    amount_in: Dec,
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default, rename = "prePool")]
    pre_pool: SettlePool,
    #[serde(default, rename = "preRouter")]
    pre_router: FinRouterState,
    #[serde(default, rename = "previewOk")]
    preview_ok: bool,
    #[serde(default)]
    used: Dec,
    #[serde(default)]
    net: Dec,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    ret: String,
    #[serde(default, rename = "swapUsed")]
    swap_used: Dec,
    #[serde(default, rename = "swapNet")]
    swap_net: Dec,
    #[serde(default, rename = "postPool")]
    post_pool: SettlePool,
    #[serde(default, rename = "postRouter")]
    post_router: FinRouterState,
}

/// Real swaps' settlement legs (the preview's used / net) against `settle_sell` / `settle_buy`.
#[test]
fn settle_edges() {
    let rows: Vec<SettleRow> = load("edges/mm_settle_edges.json.gz");
    assert!(rows.len() > 150);
    let mut rep = Report::new("settle");
    let (mut settled, mut reverted) = (0, 0);
    for (i, r) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.header)
    {
        if !r.preview_ok {
            continue;
        }
        let pp = &r.pre_pool;
        let mut pool = Pool {
            physical: pp.physical.0,
            ltv_wad: pp.ltv.0,
            phi_wad: pp.phi.0,
            room_epsilon_wad: pp.eps.0,
            features: pp.features.0,
            loans: vec![LoanCfg {
                scale: r.pre_router.loans[0].scale.0,
                liquid: pp.liquid.0,
                reserve_target: pp.reserve_target.0,
                ..Default::default()
            }],
            price_wad: vec![pp.price_wad.0],
            cross_wad: vec![WAD],
        };
        let mut rt = r.pre_router.router();
        let res = if r.buy {
            router::settle_buy(&mut pool, &mut rt, 0, r.used.0, r.net.0, r.now)
        } else {
            router::settle_sell(&mut pool, &mut rt, 0, pp.price_wad.0, r.used.0, r.net.0, r.now)
        };
        let ctx = format!(
            "row {i} {} buy={} in={} used={} net={} pre=(physical {}, liquid {})",
            r.tag, r.buy, r.amount_in.0, r.used.0, r.net.0, pp.physical.0, pp.liquid.0
        );
        rep.check_words(&ctx, &Call { ok: r.ok, ret: r.ret.clone() }, res.map(|_| vec![]));
        if !r.ok {
            reverted += 1;
            continue;
        }
        if res.is_err() {
            continue;
        }
        settled += 1;
        assert!(
            r.swap_used.0 == r.used.0 && r.swap_net.0 == r.net.0,
            "preview/execute diverged: {ctx}"
        );
        if pool.physical != r.post_pool.physical.0 || pool.loans[0].liquid != r.post_pool.liquid.0 {
            rep.add(format!(
                "{ctx}: post pool port=(physical {}, liquid {}) sol=(physical {}, liquid {})",
                pool.physical, pool.loans[0].liquid, r.post_pool.physical.0, r.post_pool.liquid.0
            ));
        }
        let want = r.post_router.router();
        for k in 0..want.venues.len() {
            if let Some(d) = venue_diff(&rt.venues[k], &want.venues[k], k) {
                rep.add(format!("{ctx}: post {d}"));
            }
        }
    }
    eprintln!("[settle] settled {settled}, settlement reverts {reverted}");
    rep.finish(rows.len() - 1);
}

/// The gate module the settlement legs assert through is the one the edge rows drive.
#[test]
fn gate_book_shape() {
    let r = Router::default();
    let p = Pool::default();
    assert_eq!(
        gate::book_of(&p, &r, 0)
            .unwrap()
            .legs
            .len(),
        0
    );
}
