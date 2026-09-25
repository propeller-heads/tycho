// Copyright (c) 2026 Everlong Labs Limited

//! Settlement edges for the financing module (`testdata/gen/RouterSettlementEdges.t.sol`,
//! `MorphoMarketEdges.t.sol`, on a Base fork at block 51317000):
//!   - `router_settlement_edges_one_loan` / `_two_loans`: the settlement legs
//!     (`FLAMMSwapLib.settleSell`, the buy branch of `execute`, `payLoan`, `takeLoan`,
//!     `releaseExcess`), the gate composites and the Router entries, executed by DELEGATECALL into
//!     the libraries the deployed pool implementation links, over the live pool record extended to
//!     three USDC venues (and a fourth venue on a second loan asset in `_two_loans`), with amounts
//!     placed on the thresholds each state implies;
//!   - `mm_market_edges`: Morpho Blue transitions, the AdaptiveCurveIrm and the MorphoBlueAccount
//!     views and mutators on realistic-magnitude states with 1-wei neighbours of every branch
//!     threshold.
//!
//! Every array opens with a header element.

use std::collections::BTreeMap;

use alloy::primitives::U256;
use serde::Deserialize;

use super::common::*;
use crate::evm::protocol::flamm::{
    error::FlammError,
    gate::{self, GateInt, LoanCfg, Pool},
    irm,
    morpho::{Market, Position, VenueMarket},
    router::{self, Router},
};

// ------------------------------------------------------------------ state

#[derive(Deserialize, Default)]
struct SettlePool {
    #[serde(default)]
    physical: Dec,
    #[serde(default)]
    features: Dec,
    #[serde(default)]
    ltv: Dec,
    #[serde(default)]
    phi: Dec,
    #[serde(default)]
    eps: Dec,
    #[serde(default)]
    liquid: Vec<Dec>,
    #[serde(default)]
    reserve: Vec<Dec>,
    #[serde(default)]
    scale: Vec<Dec>,
    #[serde(default, rename = "priceWad")]
    price_wad: Vec<Dec>,
    #[serde(default, rename = "crossWad")]
    cross_wad: Vec<Dec>,
}

#[derive(Deserialize, Default)]
struct SettleState {
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default)]
    pool: SettlePool,
    #[serde(default)]
    router: FinRouterState,
}

impl SettleState {
    fn build(&self) -> (Pool, Router) {
        let p = Pool {
            physical: self.pool.physical.0,
            ltv_wad: self.pool.ltv.0,
            phi_wad: self.pool.phi.0,
            room_epsilon_wad: self.pool.eps.0,
            features: self.pool.features.0,
            price_wad: self
                .pool
                .price_wad
                .iter()
                .map(|d| d.0)
                .collect(),
            cross_wad: self
                .pool
                .cross_wad
                .iter()
                .map(|d| d.0)
                .collect(),
            loans: (0..self.pool.liquid.len())
                .map(|i| LoanCfg {
                    scale: self.pool.scale[i].0,
                    liquid: self.pool.liquid[i].0,
                    reserve_target: self.pool.reserve[i].0,
                    ..Default::default()
                })
                .collect(),
        };
        (p, self.router.router())
    }
}

/// The written parts of a post-state, as a diff string (empty when equal).
fn settle_diff(p: &Pool, r: &Router, want: &SettleState) -> String {
    let mut d = Vec::new();
    if p.physical != want.pool.physical.0 {
        d.push(format!("physical port={} sol={}", p.physical, want.pool.physical.0));
    }
    for i in 0..p.loans.len() {
        if p.loans[i].liquid != want.pool.liquid[i].0 {
            d.push(format!("liquid[{i}] port={} sol={}", p.loans[i].liquid, want.pool.liquid[i].0));
        }
    }
    let wr = want.router.router();
    for i in 0..r.venues.len() {
        if let Some(s) = venue_diff(&r.venues[i], &wr.venues[i], i) {
            d.push(s);
        }
    }
    d.join("; ")
}

// ------------------------------------------------------------------ settlement fixtures

#[derive(Deserialize)]
struct SettleRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    scenario: String,
    #[serde(default)]
    s: i64,
    #[serde(default)]
    state: Option<SettleState>,
    #[serde(default)]
    op: String,
    #[serde(default)]
    args: serde_json::Value,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    ret: String,
    #[serde(default)]
    post: Option<SettleState>,
}

#[derive(Deserialize, Default)]
struct GateArgs {
    #[serde(default)]
    u0: Vec<SignedDec>,
    #[serde(default)]
    g0: Dec,
    #[serde(default)]
    q0: bool,
}

fn settle_replay(file: &str) {
    let rows: Vec<SettleRow> = load(file);
    assert!(rows.len() > 300);
    let mut rep = Report::new(file);
    rep.limit = 60;
    let mut states: BTreeMap<i64, &SettleState> = BTreeMap::new();
    let mut tags: BTreeMap<i64, &str> = BTreeMap::new();
    let mut n_ops = 0usize;
    for (i, row) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.header)
    {
        if let Some(st) = &row.state {
            states.insert(row.s, st);
            tags.insert(row.s, &row.scenario);
            continue;
        }
        let st = states
            .get(&row.s)
            .unwrap_or_else(|| panic!("row {i}: no state {}", row.s));
        n_ops += 1;
        let (mut pool, mut r) = st.build();
        let now = st.now;
        let mut args: Vec<U256> = Vec::new();
        let mut gargs = GateArgs::default();
        if row.args.is_array() {
            let ss: Vec<String> = serde_json::from_value(row.args.clone()).expect("args");
            args = ss
                .iter()
                .map(|s| parse_u256(s).expect("arg"))
                .collect();
        } else if row.args.is_object() {
            gargs = serde_json::from_value(row.args.clone()).expect("gate args");
        }
        let ret = hex_bytes(&row.ret);
        let ctx = format!("row {i} scenario={} op={} args={}", tags[&row.s], row.op, row.args);
        let a = |k: usize| args[k];
        let idx = |k: usize| u8::try_from(args[k]).expect("idx");
        let id = |k: usize| u16::try_from(args[k]).expect("id");
        let u0: Vec<GateInt> = gargs.u0.iter().map(|x| x.0).collect();
        let mut mutates = false;
        let res: Result<Vec<U256>, FlammError> = match row.op.as_str() {
            "positions" => r.positions(now).map(|ps| {
                let mut o = ps.coll;
                o.extend(ps.sup);
                o.extend(ps.debt);
                o.push(ps.total_coll);
                o
            }),
            "drawn" => r
                .drawn(now)
                .map(|(c, m)| vec![U256::from(c), U256::from(m)]),
            "minLltv" => r.min_lltv(now).map(|l| vec![l]),
            "reclaimable" => r
                .reclaimable(&pool.price_wad, now)
                .map(|x| vec![x]),
            "quarantine" => r
                .quarantine(idx(0), now)
                .map(|q| vec![bool_word(q.any), q.frozen_debt, q.frozen_coll]),
            "fundingCeiling" => r
                .funding_ceiling(idx(0), a(1), a(2), now)
                .map(|x| vec![x]),
            "totalAssets" => gate::total_assets(&pool, &r, now).map(|x| vec![x]),
            "assertGate" => gate::assert_gate(&pool, &r, now).map(|_| vec![]),
            "anchor" => {
                let res = gate::anchor(&pool, &r, now);
                if let (Ok((u0, g0, q0)), true) = (&res, row.ok) {
                    let inner = unwrap_bytes(&ret);
                    let w = abi_words(&inner);
                    let arr = dyn_array(&w, 0);
                    if arr.len() != u0.len() {
                        rep.add(format!(
                            "{ctx}: anchor length port={} sol={}",
                            u0.len(),
                            arr.len()
                        ));
                        continue;
                    }
                    for (k, word) in arr.iter().enumerate() {
                        let want = GateInt::cast(*word);
                        if want != u0[k] {
                            rep.add(format!(
                                "{ctx}: anchor u0[{k}] port={:?} sol={:?}",
                                u0[k], want
                            ));
                        }
                    }
                    if *g0 != w[1] || *q0 != !w[2].is_zero() {
                        rep.add(format!(
                            "{ctx}: anchor port=({g0},{q0}) sol=({},{})",
                            w[1],
                            !w[2].is_zero()
                        ));
                    }
                }
                res.map(|_| vec![])
            }
            "entryGate" => {
                gate::assert_entry_gate(&pool, &r, now, &u0, gargs.g0.0, gargs.q0).map(|_| vec![])
            }
            "exitGate" => {
                gate::assert_exit_not_worsened(&pool, &r, now, &u0, gargs.g0.0).map(|_| vec![])
            }
            "sell" => {
                mutates = true;
                router::settle_sell(&mut pool, &mut r, idx(0), a(1), a(2), a(3), now)
                    .map(|_| vec![])
            }
            "buy" => {
                mutates = true;
                router::settle_buy(&mut pool, &mut r, idx(0), a(1), a(2), now).map(|_| vec![])
            }
            "release" => {
                mutates = true;
                router::release_excess(&mut pool, &mut r, now).map(|_| vec![])
            }
            "take" => {
                mutates = true;
                router::take_loan(&mut pool, &mut r, idx(0), a(1), now).map(|_| vec![])
            }
            "pay" => {
                mutates = true;
                router::pay_loan(&mut pool, &mut r, idx(0), a(1), a(2), now).map(|_| vec![])
            }
            "fund" => {
                mutates = true;
                r.fund(idx(0), a(1), a(2), a(3), now)
                    .map(|(w, b, p)| vec![w, b, p])
            }
            "repayCascade" => {
                mutates = true;
                r.repay_cascade(idx(0), a(1), now)
                    .map(|x| vec![x])
            }
            "supplyCascade" => {
                mutates = true;
                r.supply_cascade(idx(0), a(1), now)
                    .map(|x| vec![x])
            }
            "reclaim" | "reclaimBestEffort" => {
                mutates = true;
                let pw = pool.price_wad.clone();
                r.reclaim(a(0), &pw, row.op == "reclaim", now)
                    .map(|x| vec![x])
            }
            "borrow" => {
                mutates = true;
                r.borrow(id(0), a(1), a(2), now)
                    .map(|_| vec![])
            }
            "withdrawSupplied" => {
                mutates = true;
                r.withdraw_supplied_entry(id(0), a(1), now)
                    .map(|x| vec![x])
            }
            "postCollateral" => {
                mutates = true;
                r.post_collateral(id(0), a(1))
                    .map(|_| vec![])
            }
            "withdrawCollateral" => {
                mutates = true;
                r.withdraw_collateral(id(0), a(1), a(2), !args[3].is_zero(), now)
                    .map(|_| vec![])
            }
            "supply" => {
                mutates = true;
                r.supply(id(0), a(1), now)
                    .map(|x| vec![x])
            }
            "repayWithdraw" => {
                // MMRouter.repay then a proportional withdrawCollateral inside one transaction
                mutates = true;
                r.repay(id(0), a(1), now)
                    .and_then(|_| r.withdraw_collateral(id(0), a(2), U256::ZERO, true, now))
                    .map(|_| vec![])
            }
            other => panic!("unknown op {other}"),
        };
        if !row.ok {
            let want = revert_of(&ret);
            if want.is_none() || res.as_ref().err() != want.as_ref() {
                rep.add(format!(
                    "{ctx}: port={} sol=revert {} ({want:?})",
                    err_class(&res),
                    row.ret
                ));
            }
            continue;
        }
        let outs = match res {
            Ok(o) => o,
            Err(e) => {
                rep.add(format!("{ctx}: port={e} sol=ok"));
                continue;
            }
        };
        if !outs.is_empty() && row.op != "anchor" {
            let data = if row.op == "totalAssets" { unwrap_bytes(&ret) } else { ret.clone() };
            let want: Vec<U256> = if row.op == "positions" {
                let mut w = dyn_array(&abi_words(&data), 0);
                w.extend(dyn_array(&abi_words(&data), 1));
                w.extend(dyn_array(&abi_words(&data), 2));
                w.push(abi_words(&data)[3]);
                w
            } else {
                abi_words(&data)
            };
            if want.len() != outs.len() {
                rep.add(format!("{ctx}: outs port={outs:?} sol={want:?}"));
            } else {
                for k in 0..outs.len() {
                    if outs[k] != want[k] {
                        rep.add(format!("{ctx}: out[{k}] port={} sol={}", outs[k], want[k]));
                    }
                }
            }
        }
        if mutates {
            let post = row
                .post
                .as_ref()
                .unwrap_or_else(|| panic!("{ctx}: no post"));
            let d = settle_diff(&pool, &r, post);
            if !d.is_empty() {
                rep.add(format!("{ctx}: post-state {d}"));
            }
        }
    }
    eprintln!("[{file}] {n_ops} ops over {} scenarios", states.len());
    rep.finish(n_ops);
}

/// The settlement / gate / Router op grid of both runs.
#[test]
fn router_settlement_edges_one_loan() {
    settle_replay("edges/router_settlement_edges_one_loan.json.gz");
}

#[test]
fn router_settlement_edges_two_loans() {
    settle_replay("edges/router_settlement_edges_two_loans.json.gz");
}

// ------------------------------------------------------------------ Blue / IRM / account fixtures

#[derive(Deserialize, Default)]
struct BlueState {
    #[serde(default)]
    tsa: Dec,
    #[serde(default)]
    tss: Dec,
    #[serde(default)]
    tba: Dec,
    #[serde(default)]
    tbs: Dec,
    #[serde(default, deserialize_with = "de_int_default")]
    elapsed: u64,
    #[serde(default)]
    fee: Dec,
    #[serde(default)]
    sup: Dec,
    #[serde(default)]
    bor: Dec,
    #[serde(default)]
    coll: Dec,
    #[serde(default)]
    rat: SignedDec,
    #[serde(default, rename = "oracleP")]
    oracle_p: Dec,
    #[serde(default, rename = "oracleMode", deserialize_with = "de_int_default")]
    oracle_mode: u8,
    #[serde(default, rename = "irmDead")]
    irm_dead: bool,
    #[serde(default, rename = "noIrm")]
    no_irm: bool,
}

#[derive(Deserialize)]
struct BlueRow {
    #[serde(default)]
    header: bool,
    #[serde(default)]
    tag: String,
    #[serde(default, deserialize_with = "de_int_default")]
    now: u64,
    #[serde(default)]
    lltv: Dec,
    #[serde(default)]
    st: BlueState,
    #[serde(default, deserialize_with = "de_int_default")]
    op: u8,
    #[serde(default)]
    a: Dec,
    #[serde(default)]
    b: Dec,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    ret: String,
    #[serde(default, rename = "postMarket")]
    post_market: MarketFx,
    #[serde(default, rename = "postPosition")]
    post_position: PositionFx,
    #[serde(default, rename = "postRat")]
    post_rat: SignedDec,
}

/// The Blue / IRM / account rows.
#[test]
fn morpho_market_edges() {
    let rows: Vec<BlueRow> = load("edges/mm_market_edges.json.gz");
    assert!(rows.len() > 1000);
    let mut rep = Report::new("blue");
    rep.limit = 60;
    for (i, r) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.header)
    {
        let s = &r.st;
        let mut v = VenueMarket {
            market: Market {
                total_supply_assets: s.tsa.0,
                total_supply_shares: s.tss.0,
                total_borrow_assets: s.tba.0,
                total_borrow_shares: s.tbs.0,
                last_update: U256::from(r.now - s.elapsed),
                fee: s.fee.0,
            },
            position: Position {
                supply_shares: s.sup.0,
                borrow_shares: s.bor.0,
                collateral: s.coll.0,
            },
            lltv: r.lltv.0,
            has_irm: !s.no_irm,
            irm_readable: !s.irm_dead,
            rate_at_target: s.rat.0.abs,
            oracle_ok: s.oracle_mode == 0 && !s.oracle_p.0.is_zero(),
            oracle_price: U256::ZERO,
            oracle_zero: s.oracle_mode == 2 || (s.oracle_mode == 0 && s.oracle_p.0.is_zero()),
        };
        if v.oracle_ok {
            v.oracle_price = s.oracle_p.0;
        }
        let now = r.now;
        let res: Result<Vec<U256>, FlammError> = match r.op {
            0 => v.accrue(now).map(|_| vec![]),
            1 => v
                .supply(r.a.0, now)
                .map(|sh| vec![r.a.0, sh]),
            2 => v
                .withdraw(r.a.0, r.b.0, now)
                .map(|(x, y)| vec![x, y]),
            3 => v
                .borrow(r.a.0, now)
                .map(|sh| vec![r.a.0, sh]),
            4 => v
                .repay(r.a.0, r.b.0, now)
                .map(|(x, y)| vec![x, y]),
            5 => v
                .supply_collateral(r.a.0)
                .map(|_| vec![]),
            6 => v
                .withdraw_collateral(r.a.0, now)
                .map(|_| vec![]),
            10 => v.try_position(now).map(|p| {
                vec![bool_word(p.readable), p.collateral, p.supply_shares, p.supplied, p.debt]
            }),
            11 => v.debt_of(now).map(|x| vec![x]),
            12 => v.supplied_of(now).map(|x| vec![x]),
            13 => v
                .supply_shares_to_assets(r.a.0, now)
                .map(|x| vec![x]),
            14 => Ok(vec![v.free_liquidity()]),
            15 => v
                .try_borrow_rate(r.a.0, r.b.0, now)
                .map(|(ok, x)| vec![bool_word(ok), x]),
            16 => Ok(vec![bool_word(v.oracle_ok), v.oracle_price]),
            20 => v
                .supply_collateral(r.a.0)
                .map(|_| vec![]),
            21 => v
                .withdraw_collateral(r.a.0, now)
                .map(|_| vec![]),
            22 => v.borrow(r.a.0, now).map(|_| vec![]),
            23 => v
                .account_repay(r.a.0, now)
                .map(|x| vec![x]),
            24 => v.supply(r.a.0, now).map(|x| vec![x]),
            25 => v
                .account_withdraw(r.a.0, r.b.0, now)
                .map(|(x, y)| vec![x, y]),
            30 | 31 => {
                if !v.irm_readable {
                    Err(FlammError::MorphoIrmReverted)
                } else {
                    if s.no_irm {
                        // the IRM read directly for a market Blue never let it touch: rateAtTarget
                        // is unset
                        v.rate_at_target = U256::ZERO;
                    }
                    irm::borrow_rate(&v.market, v.rate_at_target, now).map(|(avg, end)| {
                        if r.op == 31 {
                            v.rate_at_target = end;
                        }
                        vec![avg]
                    })
                }
            }
            op => panic!("unknown op {op}"),
        };
        let ctx = format!(
            "row {i} tag={} op={} a={} b={} st=(tsa {} tss {} tba {} tbs {} elapsed {} fee {} sup {} bor {} coll {} rat {} oracleP {} mode {} irmDead {} noIrm {})",
            r.tag,
            r.op,
            r.a.0,
            r.b.0,
            s.tsa.0,
            s.tss.0,
            s.tba.0,
            s.tbs.0,
            s.elapsed,
            s.fee.0,
            s.sup.0,
            s.bor.0,
            s.coll.0,
            s.rat.0.abs,
            s.oracle_p.0,
            s.oracle_mode,
            s.irm_dead,
            s.no_irm
        );
        let ret = hex_bytes(&r.ret);
        if !r.ok {
            let want = revert_of(&ret);
            if want.is_none() || res.as_ref().err() != want.as_ref() {
                rep.add(format!("{ctx}: port={} sol=revert {} ({want:?})", err_class(&res), r.ret));
            }
            continue;
        }
        let outs = match res {
            Ok(o) => o,
            Err(e) => {
                rep.add(format!("{ctx}: port={e} sol=ok"));
                continue;
            }
        };
        if !outs.is_empty() {
            let w = abi_words(&ret);
            if w.len() < outs.len() {
                rep.add(format!("{ctx}: outs port={outs:?} sol={}", r.ret));
                continue;
            }
            for k in 0..outs.len() {
                if outs[k] != w[k] {
                    rep.add(format!("{ctx}: out[{k}] port={} sol={}", outs[k], w[k]));
                }
            }
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
        if !s.no_irm && v.rate_at_target != r.post_rat.0.abs {
            rep.add(format!("{ctx}: post rat port={} sol={}", v.rate_at_target, r.post_rat.0.abs));
        }
    }
    rep.finish(rows.len() - 1);
}
