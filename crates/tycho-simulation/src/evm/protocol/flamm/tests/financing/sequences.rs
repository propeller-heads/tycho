// Copyright (c) 2026 Everlong Labs Limited

//! Stateful replay of the financing module. The fixtures under `testdata/edges/` are JSON lines
//! written by `testdata/gen/RouterSequences.t.sol`, `SwapSettlementSequences.t.sol` and
//! `MorphoAccrualGrid.t.sol` on a Base fork at block 51317000 (`forge --isolate`, one transaction
//! per step):
//!   - `router_sequence_a` / `_b`: pseudo-random sequences of Router transactions over four venues
//!     and two loan assets, with time warps, third-party Morpho activity (including calls on the
//!     account's behalf and liquidations), IRM outages, oracle moves and config changes between
//!     them; `router_sequence_liquidation`: a scripted liquidation run;
//!   - `swap_settlement_sequence_a..d`: pseudo-random sequences of real `pool.swap` calls through
//!     the deployed pool (one venue, an inflated book, three venues); `_e`: a scripted run through
//!     `releaseExcess`'s swallowed revert, quarantine and the cross-transaction repay snapshot;
//!   - `mm_accrual_grid`: AdaptiveCurveIrm, Blue accrual and the account views over written market
//!     states.
//!
//! The port REPLAYS each sequence: the Morpho markets, the account positions, `rateAtTarget`, the
//! managed fields and the pool ledger are carried forward from the port's own transitions and
//! compared with the chain before and after every step, so an error that compounds across steps
//! surfaces where it first appears. Config, oracle and IRM readability come from the chain's
//! pre-state (they are tracker inputs).

use alloy::primitives::U256;
use serde::Deserialize;

use super::common::*;
use crate::evm::protocol::flamm::{
    error::FlammError,
    gate, irm,
    morpho::VenueMarket,
    router::{self, Router},
};

#[derive(Deserialize, Default, Clone)]
struct SeqRes {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    ret: String,
    #[serde(default)]
    rets: Vec<String>,
}

impl SeqRes {
    fn call(&self) -> Call {
        Call { ok: self.ok, ret: self.ret.clone() }
    }

    /// The single head word at `slot` of a successful return.
    fn word(&self, slot: usize) -> SeqRes {
        if !self.ok {
            return self.clone();
        }
        let w = abi_words(&hex_bytes(&self.ret));
        SeqRes {
            ok: true,
            ret: format!("0x{}", hex::encode(w[slot].to_be_bytes::<32>())),
            rets: vec![],
        }
    }
}

/// Compares one call: both succeed (then the return words must match) or both fail with the same
/// class.
fn same_result(rep: &mut Report, ctx: &str, chain: &SeqRes, got: Result<Vec<U256>, FlammError>) {
    rep.check_prefix(ctx, &chain.call(), got);
}

/// The `uint256` payload of `InsufficientLiquidity` / `InsufficientCollateral` where the port can
/// name it.
fn check_payload(rep: &mut Report, ctx: &str, chain: &SeqRes, want: U256) {
    if chain.ok {
        return;
    }
    if let Some(got) = revert_payload(&hex_bytes(&chain.ret)) {
        if got != want {
            rep.add(format!("{ctx}: revert payload chain {got} port {want}"));
        }
    }
}

// ------------------------------------------------------------------ Router calldata

struct SeqCall {
    sel: String,
    args: Vec<U256>,
    data: Vec<u8>,
}

fn parse_call(h: &str) -> SeqCall {
    let b = hex_bytes(h);
    SeqCall { sel: hex::encode(&b[..4]), args: abi_words(&b[4..]), data: b[4..].to_vec() }
}

/// The `uint256[]` at head word `slot` of ABI data.
fn dyn_uints(data: &[u8], slot: usize) -> Vec<U256> {
    dyn_array(&abi_words(data), slot)
}

/// Runs one Router entry (as the pool) on `r`, writing through; returns the ABI return words.
fn exec_router(r: &mut Router, c: &SeqCall, now: u64) -> Result<Vec<U256>, FlammError> {
    let a = &c.args;
    let u8_of = |x: U256| u8::try_from(x).expect("u8");
    let u16_of = |x: U256| u16::try_from(x).expect("u16");
    match c.sel.as_str() {
        // fund(idx, assets, to, collateralIn, priceWad)
        "80066e9c" => r
            .fund(u8_of(a[0]), a[1], a[3], a[4], now)
            .map(|(w, b, p)| vec![w, b, p]),
        "5fee443f" => r
            .repay_cascade(u8_of(a[0]), a[1], now)
            .map(|x| vec![x]),
        "c412f8c5" => r
            .supply_cascade(u8_of(a[0]), a[1], now)
            .map(|x| vec![x]),
        "7490faea" | "4d8e8fb2" => r
            .reclaim(a[0], &dyn_uints(&c.data, 1), c.sel == "7490faea", now)
            .map(|x| vec![x]),
        "71840b97" => r
            .borrow(u16_of(a[0]), a[1], a[3], now)
            .map(|_| vec![]),
        "6b738d98" => r
            .repay(u16_of(a[0]), a[1], now)
            .map(|x| vec![x]),
        "eb669e44" => r
            .supply(u16_of(a[0]), a[1], now)
            .map(|x| vec![x]),
        "940c2e93" => r
            .withdraw_supplied_entry(u16_of(a[0]), a[1], now)
            .map(|x| vec![x]),
        "335efc32" => r
            .post_collateral(u16_of(a[0]), a[1])
            .map(|_| vec![]),
        "9f82c026" => r
            .withdraw_collateral(u16_of(a[0]), a[1], a[2], !a[3].is_zero(), now)
            .map(|_| vec![]),
        other => panic!("unknown selector {other}"),
    }
}

// ------------------------------------------------------------------ the environment

#[derive(Deserialize, Default)]
struct SeqEnv {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    venue: usize,
    #[serde(default)]
    op: i64,
    #[serde(default)]
    a: Dec,
    #[serde(default)]
    s: Dec,
    #[serde(default)]
    wpre: PositionFx,
    #[serde(default)]
    wpost: PositionFx,
    #[serde(default)]
    mpre: MarketFx,
    #[serde(default)]
    mpost: MarketFx,
    #[serde(default)]
    rpre: Dec,
    #[serde(default)]
    rpost: Dec,
    #[serde(default)]
    res: SeqRes,
    #[serde(default)]
    who: String,
    #[serde(default)]
    data: String,
}

/// Checks the carried market (and, for a call on the account's behalf, its position) against the
/// chain before a third-party Morpho call, then takes the chain's post-call figures so a modelling
/// error there is reported once by `third_party` rather than cascading.
fn carry_third_party(rep: &mut Report, ctx: &str, e: &SeqEnv, cv: &mut VenueMarket) {
    if cv.market != e.mpre.state() || cv.rate_at_target != e.rpre.0 {
        rep.add(format!(
            "{ctx} carried market drifted before third-party op: port {:?} rat {}, chain {:?} rat {}",
            cv.market, cv.rate_at_target, e.mpre, e.rpre.0
        ));
    }
    cv.market = e.mpost.state();
    cv.rate_at_target = e.rpost.0;
    if e.who == "account" {
        if cv.position != e.wpre.state() {
            rep.add(format!(
                "{ctx} carried position drifted before a call on the account's behalf: port {:?}, chain {:?}",
                cv.position, e.wpre
            ));
        }
        cv.position = e.wpost.state();
    }
}

/// Models a third party's Morpho call on venue `v`'s market with the port's Blue transitions and
/// compares the market, the caller's position and `rateAtTarget` with the chain.
fn third_party(rep: &mut Report, ctx: &str, e: &SeqEnv, flags: VenueMarket, now: u64) {
    let mut m = flags;
    m.market = e.mpre.state();
    m.position = e.wpre.state();
    m.rate_at_target = e.rpre.0;
    // 0-4 the third party's own position; 5-7 on the financing account's behalf
    let res: Result<(), FlammError> = match e.op {
        0 | 5 => m.supply(e.a.0, now).map(|_| ()),
        1 => m
            .withdraw(e.a.0, e.s.0, now)
            .map(|_| ()),
        2 => m.borrow(e.a.0, now).map(|_| ()),
        3 | 7 => m.repay(e.a.0, e.s.0, now).map(|_| ()),
        4 | 6 => m.supply_collateral(e.a.0),
        op => panic!("unknown third-party op {op}"),
    };
    if e.res.ok != res.is_ok() {
        rep.add(format!(
            "{ctx} third-party op {} a={} s={}: chain ok={} ret={}, port {}",
            e.op,
            e.a.0,
            e.s.0,
            e.res.ok,
            e.res.ret,
            err_class(&res)
        ));
        return;
    }
    if !e.res.ok {
        let want = revert_of_hex(&e.res.ret);
        if want.is_none() || res.as_ref().err() != want.as_ref() {
            rep.add(format!(
                "{ctx} third-party op {}: chain {}, port {}",
                e.op,
                e.res.ret,
                err_class(&res)
            ));
        }
        return;
    }
    if m.market != e.mpost.state() || m.position != e.wpost.state() || m.rate_at_target != e.rpost.0
    {
        rep.add(format!(
            "{ctx} third-party op {} a={} s={}: port market {:?} pos {:?} rat {}, chain {:?} {:?} {}",
            e.op, e.a.0, e.s.0, m.market, m.position, m.rate_at_target, e.mpost, e.wpost, e.rpost.0
        ));
    }
}

// ------------------------------------------------------------------ the Router views per step

#[derive(Deserialize, Default)]
struct SeqAcctV {
    #[serde(default, rename = "tryPosition")]
    try_position: SeqRes,
    #[serde(default, rename = "debtOf")]
    debt_of: SeqRes,
    #[serde(default, rename = "suppliedOf")]
    supplied_of: SeqRes,
    #[serde(default, rename = "freeLiquidity")]
    free_liquidity: SeqRes,
    #[serde(default, rename = "dB")]
    db: Vec<Dec>,
    #[serde(default, rename = "dS")]
    ds: Vec<Dec>,
    #[serde(default)]
    rates: Vec<SeqRes>,
}

#[derive(Deserialize, Default)]
struct SeqAcct {
    #[serde(default)]
    venue: usize,
    #[serde(default)]
    v: SeqAcctV,
}

#[derive(Deserialize, Default)]
struct SeqRouterV {
    #[serde(default)]
    positions: SeqRes,
    #[serde(default)]
    drawn: SeqRes,
    #[serde(default, rename = "minLltv")]
    min_lltv: SeqRes,
    #[serde(default)]
    reclaimable: SeqRes,
    #[serde(default)]
    quarantine: Vec<SeqRes>,
    #[serde(default)]
    ceiling: Vec<SeqRes>,
    #[serde(default, rename = "venuePosition")]
    venue_position: Vec<SeqRes>,
    #[serde(default)]
    health: Vec<SeqRes>,
}

#[derive(Deserialize, Default)]
struct SeqRouterViews {
    #[serde(default)]
    prices: Vec<Dec>,
    #[serde(default, rename = "collIns")]
    coll_ins: Vec<Dec>,
    #[serde(default)]
    router: SeqRouterV,
    #[serde(default)]
    acct: Option<SeqAcct>,
    // the swap sequences' extra views
    #[serde(default, rename = "totalAssets")]
    total_assets: SeqRes,
    #[serde(default, rename = "loanPosition")]
    loan_position: SeqRes,
    #[serde(default, rename = "poolAssetPosition")]
    pool_asset_position: SeqRes,
}

fn router_views_check(
    rep: &mut Report,
    ctx: &str,
    st: &Router,
    v: &SeqRouterViews,
    now: u64,
) -> usize {
    let mut n = 0usize;
    let r = st.clone();
    let prices: Vec<U256> = v.prices.iter().map(|d| d.0).collect();
    let pos = r.positions(now);
    same_result(
        rep,
        &format!("{ctx} positions.total"),
        &v.router.positions.word(3),
        pos.clone().map(|p| vec![p.total_coll]),
    );
    if let (Ok(pos), true) = (&pos, v.router.positions.ok) {
        let data = hex_bytes(&v.router.positions.ret);
        for (k, want) in [&pos.coll, &pos.sup, &pos.debt]
            .into_iter()
            .enumerate()
        {
            let got = dyn_uints(&data, k);
            if got != *want {
                rep.add(format!("{ctx} positions[{k}] chain {got:?} port {want:?}"));
            }
        }
    }
    n += 1;
    same_result(
        rep,
        &format!("{ctx} drawn"),
        &v.router.drawn,
        r.drawn(now)
            .map(|(c, m)| vec![U256::from(c), U256::from(m)]),
    );
    same_result(
        rep,
        &format!("{ctx} minLltv"),
        &v.router.min_lltv,
        r.min_lltv(now).map(|l| vec![l]),
    );
    same_result(
        rep,
        &format!("{ctx} reclaimable"),
        &v.router.reclaimable,
        r.reclaimable(&prices, now)
            .map(|x| vec![x]),
    );
    n += 3;
    for (i, q) in v.router.quarantine.iter().enumerate() {
        same_result(
            rep,
            &format!("{ctx} quarantine({i})"),
            q,
            r.quarantine(i as u8, now)
                .map(|q| vec![bool_word(q.any), q.frozen_debt, q.frozen_coll]),
        );
        n += 1;
    }
    let mut k = 0usize;
    for (i, price) in prices
        .iter()
        .enumerate()
        .take(st.loans.len())
    {
        for c in &v.coll_ins {
            same_result(
                rep,
                &format!("{ctx} fundingCeiling({i},{},{price})", c.0),
                &v.router.ceiling[k],
                r.funding_ceiling(i as u8, c.0, *price, now)
                    .map(|x| vec![x]),
            );
            k += 1;
            n += 1;
        }
    }
    for id in 0..st.venues.len() {
        same_result(
            rep,
            &format!("{ctx} venuePosition({id})"),
            &v.router.venue_position[id],
            r.venue_position(id as u16, now)
                .map(|p| p.to_vec()),
        );
        let price = prices[st.venues[id].loan_index as usize];
        same_result(
            rep,
            &format!("{ctx} venueHealth({id})"),
            &v.router.health[id],
            r.venue_health(id as u16, price, now)
                .map(|h| vec![h]),
        );
        n += 2;
    }
    if let Some(a) = &v.acct {
        let vm = &r.venues[a.venue].morpho;
        same_result(
            rep,
            &format!("{ctx} tryPosition({})", a.venue),
            &a.v.try_position,
            vm.try_position(now).map(|tp| {
                vec![bool_word(tp.readable), tp.collateral, tp.supply_shares, tp.supplied, tp.debt]
            }),
        );
        same_result(
            rep,
            &format!("{ctx} debtOf({})", a.venue),
            &a.v.debt_of,
            vm.debt_of(now).map(|d| vec![d]),
        );
        same_result(
            rep,
            &format!("{ctx} suppliedOf({})", a.venue),
            &a.v.supplied_of,
            vm.supplied_of(now).map(|s| vec![s]),
        );
        same_result(
            rep,
            &format!("{ctx} freeLiquidity({})", a.venue),
            &a.v.free_liquidity,
            Ok(vec![vm.free_liquidity()]),
        );
        n += 4;
        for (i, rate) in a.v.rates.iter().enumerate() {
            same_result(
                rep,
                &format!("{ctx} borrowRateAfter({},{},{})", a.venue, a.v.db[i].0, a.v.ds[i].0),
                rate,
                vm.borrow_rate_after(a.v.db[i].0, a.v.ds[i].0, now)
                    .map(|(ok, x)| vec![bool_word(ok), x]),
            );
            n += 1;
        }
    }
    n
}

// ------------------------------------------------------------------ the Router sequence

#[derive(Deserialize, Default)]
struct SeqOp {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct SeqRouterRow {
    #[serde(default)]
    i: i64,
    #[serde(default)]
    t: Dec,
    #[serde(default)]
    env: Vec<SeqEnv>,
    #[serde(default)]
    pre: GoRouter,
    #[serde(default)]
    op: SeqOp,
    #[serde(default)]
    res: SeqRes,
    #[serde(default)]
    post: GoRouter,
    #[serde(default)]
    views: SeqRouterViews,
}

fn router_sequence(name: &str) {
    let rows: Vec<SeqRouterRow> = load_lines(&format!("edges/{name}"));
    let mut rep = Report::new(name);
    rep.limit = 80;
    let mut cur: Option<Router> = None;
    let (mut n_ops, mut n_ok, mut n_views) = (0usize, 0usize, 0usize);
    for row in &rows {
        let now = u64::try_from(row.t.0).expect("timestamp");
        let pre = row.pre.state();
        let post = row.post.state();
        let ctx = format!("step {}", row.i);

        // third-party Morpho activity, replayed on the carried market
        for e in &row.env {
            match e.kind.as_str() {
                "tp" => {
                    if let Some(c) = cur.as_mut() {
                        carry_third_party(&mut rep, &ctx, e, &mut c.venues[e.venue].morpho);
                    }
                    third_party(&mut rep, &ctx, e, pre.venues[e.venue].morpho, now);
                }
                "liq" => {
                    // Blue liquidation is not ported: check the carried figures, then take the
                    // chain's
                    if let Some(c) = cur.as_mut() {
                        let mut e2 = SeqEnv { who: "account".to_string(), ..Default::default() };
                        e2.mpre = e.mpre;
                        e2.mpost = e.mpost;
                        e2.rpre = e.rpre;
                        e2.rpost = e.rpost;
                        e2.wpre = e.wpre;
                        e2.wpost = e.wpost;
                        carry_third_party(&mut rep, &ctx, &e2, &mut c.venues[e.venue].morpho);
                    }
                }
                "poolcall" => {
                    if let Some(c) = cur.as_mut() {
                        let call = parse_call(&e.data);
                        let mut trial = c.clone();
                        let words = exec_router(&mut trial, &call, now);
                        let ok = words.is_ok();
                        same_result(
                            &mut rep,
                            &format!("{ctx} poolcall {}", call.sel),
                            &e.res,
                            words,
                        );
                        if ok {
                            *c = trial;
                        }
                    }
                }
                _ => {}
            }
        }

        // the carried state against the chain's pre-state
        let mut g = pre.clone();
        if let Some(c) = &cur {
            router_carry(&mut g, c);
            let d = router_diff(&g, &pre);
            if !d.is_empty() {
                rep.add(format!("{ctx} carried state drifted from chain pre-state: {d:?}"));
                g = pre.clone();
            }
        }

        // the transaction
        n_ops += 1;
        let mut trial = g.clone();
        match row.op.kind.as_str() {
            "call" => {
                let h: String = serde_json::from_value(row.op.data.clone()).expect("call data");
                let c = parse_call(&h);
                let words = exec_router(&mut trial, &c, now);
                same_result(&mut rep, &format!("{ctx} {}", c.sel), &row.res, words.clone());
                match (&c.sel[..], &words) {
                    ("80066e9c", Err(FlammError::InsufficientLiquidity)) => {
                        let plan = g.clone().build_plan(
                            u8::try_from(c.args[0]).expect("idx"),
                            c.args[1],
                            c.args[3],
                            c.args[4],
                            now,
                        );
                        if let Ok(plan) = plan {
                            if !plan.remaining.is_zero() {
                                check_payload(
                                    &mut rep,
                                    &format!("{ctx} fund"),
                                    &row.res,
                                    plan.remaining,
                                );
                            }
                        }
                    }
                    (
                        "940c2e93" | "9f82c026",
                        Err(FlammError::InsufficientLiquidity | FlammError::InsufficientCollateral),
                    ) => {
                        check_payload(&mut rep, &format!("{ctx} {}", c.sel), &row.res, c.args[1]);
                    }
                    _ => {}
                }
                if words.is_ok() {
                    g = trial;
                    n_ok += 1;
                }
            }
            "bundle" => {
                let hs: Vec<String> =
                    serde_json::from_value(row.op.data.clone()).expect("bundle data");
                let (c0, c1) = (parse_call(&hs[0]), parse_call(&hs[1]));
                let w0 = exec_router(&mut trial, &c0, now);
                let res = w0
                    .clone()
                    .and_then(|_| exec_router(&mut trial, &c1, now));
                if row.res.ok != res.is_ok() {
                    rep.add(format!(
                        "{ctx} bundle: chain ok={} ret={}, port {}",
                        row.res.ok,
                        row.res.ret,
                        err_class(&res)
                    ));
                } else if !row.res.ok {
                    let want = revert_of_hex(&row.res.ret);
                    if want.is_none() || res.as_ref().err() != want.as_ref() {
                        rep.add(format!(
                            "{ctx} bundle: chain {}, port {}",
                            row.res.ret,
                            err_class(&res)
                        ));
                    }
                } else {
                    let first = SeqRes { ok: true, ret: row.res.rets[0].clone(), rets: vec![] };
                    same_result(&mut rep, &format!("{ctx} bundle repay"), &first, w0);
                    g = trial;
                    n_ok += 1;
                }
            }
            other => panic!("unknown op kind {other}"),
        }
        g.end_transaction();
        let d = router_diff(&g, &post);
        if !d.is_empty() {
            rep.add(format!("{ctx} post-state: {d:?}"));
            g = post.clone();
        }
        cur = Some(g);

        // views on the chain's post-state
        n_views += router_views_check(&mut rep, &ctx, &post, &row.views, now);
    }
    eprintln!("[{name}] {n_ops} steps, {n_ok} transactions committed, {n_views} view rows");
    rep.finish(rows.len());
}

#[test]
fn router_sequence_a() {
    router_sequence("router_sequence_a.jsonl.gz");
}

#[test]
fn router_sequence_b() {
    router_sequence("router_sequence_b.jsonl.gz");
}

#[test]
fn router_sequence_liquidation() {
    router_sequence("router_sequence_liquidation.jsonl.gz");
}

// ------------------------------------------------------------------ the swap sequence

#[derive(Deserialize, Default)]
struct SeqSwapState {
    #[serde(default)]
    pool: GoPool,
    #[serde(default)]
    router: GoRouter,
}

#[derive(Deserialize, Default)]
struct SeqSwapOp {
    #[serde(default)]
    sell: bool,
    #[serde(default, rename = "amountIn")]
    amount_in: Dec,
}

#[derive(Deserialize)]
struct SeqSwapRow {
    #[serde(default)]
    i: i64,
    #[serde(default)]
    t: Dec,
    #[serde(default)]
    env: Vec<SeqEnv>,
    #[serde(default)]
    pre: SeqSwapState,
    #[serde(default)]
    op: SeqSwapOp,
    #[serde(default)]
    preview: SeqRes,
    #[serde(default)]
    res: SeqRes,
    #[serde(default)]
    post: SeqSwapState,
    #[serde(default)]
    views: SeqRouterViews,
}

fn swap_sequence(name: &str) {
    let rows: Vec<SeqSwapRow> = load_lines(&format!("edges/{name}"));
    let mut rep = Report::new(name);
    rep.limit = 80;
    let mut cur: Option<(gate::Pool, Router)> = None;
    let (mut n_swaps, mut n_ok, mut n_fail, mut n_views) = (0usize, 0usize, 0usize, 0usize);
    for row in &rows {
        let now = u64::try_from(row.t.0).expect("timestamp");
        let ctx = format!("step {}", row.i);
        let pre_router = row.pre.router.state();
        let pre_pool = row.pre.pool.state();
        let mut resync_physical = false;
        for e in &row.env {
            match e.kind.as_str() {
                "tp" => {
                    if let Some((_, cr)) = cur.as_mut() {
                        carry_third_party(&mut rep, &ctx, e, &mut cr.venues[e.venue].morpho);
                    }
                    third_party(&mut rep, &ctx, e, pre_router.venues[e.venue].morpho, now);
                }
                "poolcall" => {
                    // a Router entry called as the pool between swaps (custody outside the tracked
                    // ledger)
                    if let Some((_, cr)) = cur.as_mut() {
                        let call = parse_call(&e.data);
                        let mut trial = cr.clone();
                        let words = exec_router(&mut trial, &call, now);
                        let ok = words.is_ok();
                        same_result(
                            &mut rep,
                            &format!("{ctx} poolcall {}", call.sel),
                            &e.res,
                            words,
                        );
                        if ok {
                            trial.end_transaction();
                            *cr = trial;
                        }
                    }
                }
                "phys" => resync_physical = true,
                _ => {}
            }
        }

        let mut pool = pre_pool.clone();
        let mut r = pre_router.clone();
        if let Some((cp, cr)) = &cur {
            router_carry(&mut r, cr);
            let d = router_diff(&r, &pre_router);
            if !d.is_empty() {
                rep.add(format!("{ctx} carried router drifted: {d:?}"));
                r = pre_router.clone();
            }
            pool.physical = cp.physical;
            if resync_physical {
                pool.physical = pre_pool.physical;
            }
            for i in 0..pool.loans.len() {
                pool.loans[i].liquid = cp.loans[i].liquid;
            }
            let d = pool_diff(&pool, &pre_pool);
            if !d.is_empty() {
                rep.add(format!("{ctx} carried pool drifted: {d:?}"));
                pool = pre_pool.clone();
            }
        }

        if row.preview.ok {
            n_swaps += 1;
            let src = if row.res.ok { &row.res } else { &row.preview };
            let w = abi_words(&hex_bytes(&src.ret));
            let (used, net) = (w[0], w[1]);
            let res = if row.op.sell {
                let price = pool.price_wad[0];
                router::settle_sell(&mut pool, &mut r, 0, price, used, net, now)
            } else {
                router::settle_buy(&mut pool, &mut r, 0, used, net, now)
            };
            if row.res.ok != res.is_ok() {
                rep.add(format!(
                    "{ctx} {} amountIn={} used={used} net={net}: chain ok={} ret={}, port {}",
                    if row.op.sell { "sell" } else { "buy" },
                    row.op.amount_in.0,
                    row.res.ok,
                    row.res.ret,
                    err_class(&res)
                ));
            } else if !row.res.ok {
                n_fail += 1;
                let want = revert_of_hex(&row.res.ret);
                if want.is_none() || res.as_ref().err() != want.as_ref() {
                    rep.add(format!(
                        "{ctx} settlement revert: chain {} ({want:?}), port {}",
                        row.res.ret,
                        err_class(&res)
                    ));
                }
            } else {
                n_ok += 1;
            }
        } else if row.res.ok {
            rep.add(format!("{ctx} preview reverted {} but the swap succeeded", row.preview.ret));
        }
        let post_pool = row.post.pool.state();
        let post_router = row.post.router.state();
        let d = pool_diff(&pool, &post_pool);
        if !d.is_empty() {
            rep.add(format!("{ctx} post pool: {d:?}"));
            pool = post_pool.clone();
        }
        let d = router_diff(&r, &post_router);
        if !d.is_empty() {
            rep.add(format!("{ctx} post router: {d:?}"));
            r = post_router.clone();
        }
        cur = Some((pool, r));

        // views on the chain post-state
        let (pp, pr) = (post_pool, post_router);
        n_views += router_views_check(&mut rep, &ctx, &pr, &row.views, now);
        same_result(
            &mut rep,
            &format!("{ctx} totalAssets"),
            &row.views.total_assets,
            gate::total_assets(&pp, &pr, now).map(|x| vec![x]),
        );
        same_result(
            &mut rep,
            &format!("{ctx} loanPosition"),
            &row.views.loan_position,
            pr.position(0, now)
                .map(|(_, sup, debt)| vec![pp.loans[0].liquid, sup, debt]),
        );
        same_result(
            &mut rep,
            &format!("{ctx} poolAssetPosition"),
            &row.views.pool_asset_position,
            pr.positions(now)
                .map(|pos| vec![pp.physical, pp.physical + pos.total_coll]),
        );
        n_views += 3;
    }
    eprintln!(
        "[{name}] {} steps, {n_swaps} swaps, {n_ok} settled, {n_fail} settlement reverts, {n_views} view rows",
        rows.len()
    );
    rep.finish(rows.len());
}

#[test]
fn swap_settlement_sequence_a() {
    swap_sequence("swap_settlement_sequence_a.jsonl.gz");
}

#[test]
fn swap_settlement_sequence_b() {
    swap_sequence("swap_settlement_sequence_b.jsonl.gz");
}

#[test]
fn swap_settlement_sequence_c() {
    swap_sequence("swap_settlement_sequence_c.jsonl.gz");
}

#[test]
fn swap_settlement_sequence_d() {
    swap_sequence("swap_settlement_sequence_d.jsonl.gz");
}

#[test]
fn swap_settlement_sequence_e() {
    swap_sequence("swap_settlement_sequence_e.jsonl.gz");
}

// ------------------------------------------------------------------ the IRM / Blue / account grid

#[derive(Deserialize, Default)]
struct GridAcct {
    #[serde(default, rename = "tryPosition")]
    try_position: SeqRes,
    #[serde(default, rename = "debtOf")]
    debt_of: SeqRes,
    #[serde(default, rename = "suppliedOf")]
    supplied_of: SeqRes,
    #[serde(default)]
    shares: Dec,
    #[serde(default)]
    s2a: SeqRes,
    #[serde(default, rename = "dB")]
    db: Dec,
    #[serde(default, rename = "dS")]
    ds: Dec,
    #[serde(default)]
    rate: SeqRes,
}

#[derive(Deserialize)]
struct GridRow {
    #[serde(default)]
    i: i64,
    #[serde(default)]
    t: Dec,
    #[serde(default)]
    down: bool,
    #[serde(default)]
    state: VenueMarketFx,
    #[serde(default)]
    view: SeqRes,
    #[serde(default)]
    acct: GridAcct,
    #[serde(default)]
    rate: SeqRes,
    #[serde(default, rename = "ratAfter")]
    rat_after: Dec,
    #[serde(default)]
    accrue: SeqRes,
    #[serde(default)]
    after: VenueMarketFx,
}

#[test]
fn morpho_accrual_grid() {
    let rows: Vec<GridRow> = load_lines("edges/mm_accrual_grid.jsonl.gz");
    let mut rep = Report::new("accrual-grid");
    rep.limit = 80;
    for row in &rows {
        let now = u64::try_from(row.t.0).expect("timestamp");
        let ctx = format!("grid {}", row.i);
        let st = row.state.state();
        if st.irm_readable == row.down {
            rep.add(format!(
                "{ctx}: fixture readability flag {} with outage {}",
                st.irm_readable, row.down
            ));
        }

        // AdaptiveCurveIrm: the view, and borrowRate's stored endRateAtTarget
        let res = irm::borrow_rate(&st.market, st.rate_at_target, now);
        let res = if row.down { Err(FlammError::MorphoIrmReverted) } else { res };
        same_result(
            &mut rep,
            &format!("{ctx} borrowRateView"),
            &row.view,
            res.map(|(avg, _)| vec![avg]),
        );
        same_result(
            &mut rep,
            &format!("{ctx} borrowRate"),
            &row.rate,
            res.map(|(avg, _)| vec![avg]),
        );
        if let (true, Ok((_, end))) = (row.rate.ok, &res) {
            if *end != row.rat_after.0 {
                rep.add(format!("{ctx} endRateAtTarget chain {} port {end}", row.rat_after.0));
            }
        }

        // account views
        let vm = st;
        same_result(
            &mut rep,
            &format!("{ctx} tryPosition"),
            &row.acct.try_position,
            vm.try_position(now).map(|tp| {
                vec![bool_word(tp.readable), tp.collateral, tp.supply_shares, tp.supplied, tp.debt]
            }),
        );
        same_result(
            &mut rep,
            &format!("{ctx} debtOf"),
            &row.acct.debt_of,
            vm.debt_of(now).map(|d| vec![d]),
        );
        same_result(
            &mut rep,
            &format!("{ctx} suppliedOf"),
            &row.acct.supplied_of,
            vm.supplied_of(now).map(|s| vec![s]),
        );
        same_result(
            &mut rep,
            &format!("{ctx} supplySharesToAssets"),
            &row.acct.s2a,
            vm.supply_shares_to_assets(row.acct.shares.0, now)
                .map(|a| vec![a]),
        );
        same_result(
            &mut rep,
            &format!("{ctx} borrowRateAfter({},{})", row.acct.db.0, row.acct.ds.0),
            &row.acct.rate,
            vm.borrow_rate_after(row.acct.db.0, row.acct.ds.0, now)
                .map(|(ok, rate)| vec![bool_word(ok), rate]),
        );

        // Morpho accrueInterest
        let mut acc = st;
        let res = acc.accrue(now);
        same_result(&mut rep, &format!("{ctx} accrueInterest"), &row.accrue, res.map(|_| vec![]));
        let after = row.after.state();
        if row.accrue.ok &&
            res.is_ok() &&
            (acc.market != after.market || acc.rate_at_target != after.rate_at_target)
        {
            rep.add(format!(
                "{ctx} accrue: port {:?} rat {}, chain {:?} rat {}",
                acc.market, acc.rate_at_target, after.market, after.rate_at_target
            ));
        }
    }
    rep.finish(rows.len());
}
