// Copyright (c) 2026 Everlong Labs Limited

//! The composed pool core against the deployed pool's recorded behaviour, every row and every word
//! (`testdata/README.md` section 4; the Go suite's `core_e2e_test.go`, `core_edges_test.go`,
//! `core_edge_sequences_test.go`, replayed here without sampling):
//!
//! - every recorded state's deployed views the core reproduces from its own reads: `peekCross`,
//!   `pegOk(loan0)`, `positions`, `loanPosition(0)`, `poolAssetPosition().gross`, `totalAssets`;
//! - every recorded `previewSwap` / `previewLever` and `router.fundingCeiling`: the same words, or
//!   the same revert class;
//! - every executed `swap` / `leverUp` / `leverDown` replayed from the port's own post-state: the
//!   return words, the event words, the post-state field for field (the hook storage, the Router
//!   record and Morpho markets, the pool ledger) and the post-state's views; a reverted step leaves
//!   the state untouched;
//! - sensitivity: perturbing an input a scenario exercises must break rows, and does.

use std::collections::{BTreeMap, HashMap};

use alloy::primitives::{Address, U256};
use serde::Deserialize;

use super::common::{
    attest, bool_word, class_tag, decode_reads, e2e_diff, edge_diff, fixture_lines, outcome,
    outcome_hex, read_fixture, return_words, revert_class, Chain, Report, State, Word, E2E_BLOCKS,
    EDGE_BLOCKS,
};
use crate::evm::protocol::flamm::FlammError;

fn preview_swap_words(s: &State, sell: bool, a: U256, now: u64) -> Result<Vec<U256>, FlammError> {
    let p = s.preview_swap(sell, a, now)?;
    Ok(vec![p.used_native, p.net_native, p.fee_wad])
}

fn preview_lever_words(s: &State, up: bool, a: U256, now: u64) -> Result<Vec<U256>, FlammError> {
    let r = s.preview_lever(up, a, now)?;
    Ok(vec![r.amount_in_used, r.amount_out, r.spread_ppm, r.cr_after_wad])
}

/// The `swap` step of a sequence: the return words, the `Swap` event's words and the post-state.
fn exec_swap(
    s: &State,
    tin: Address,
    tout: Address,
    a: U256,
    min: U256,
    deadline: u64,
    now: u64,
) -> Result<(Vec<U256>, Vec<U256>, State), FlammError> {
    let (r, post) = s.execute_swap(tin, tout, a, min, deadline, now)?;
    Ok((
        vec![r.amount_in_used, r.amount_out],
        vec![
            bool_word(r.pool_asset_in),
            r.amount_in_used,
            r.amount_out,
            r.fee_out,
            r.fee_wad,
            r.spot_after_wad,
        ],
        post,
    ))
}

/// The `leverUp` / `leverDown` step: the return words, the event's words and the post-state.
fn exec_lever(
    s: &State,
    up: bool,
    a: U256,
    min: U256,
    deadline: u64,
    now: u64,
) -> Result<(Vec<U256>, Vec<U256>, State), FlammError> {
    let (r, post) = s.execute_lever(up, a, min, deadline, now)?;
    Ok((
        vec![r.amount_in_used, r.amount_out],
        vec![r.amount_in_used, r.amount_out, r.spread_ppm, r.cr_after_wad],
        post,
    ))
}

// ------------------------------------------------------------------ e2e grids

#[derive(Deserialize)]
struct GridRow {
    k: String,
    #[serde(default)]
    tag: String,
    s: Option<serde_json::Value>,
    #[serde(default)]
    d: i64,
    a: Option<Word>,
    r: Option<Vec<Word>>,
    e: Option<String>,
}

/// One e2e grid fixture (`TestCoreE2EPreviewGrids`): the states' views and every preview row.
/// `only` restricts the replay to one scenario and `mutate` perturbs a freshly built state (the
/// sensitivity runs). Returns the class tallies.
fn replay_e2e_grid(
    name: &str,
    rep: &mut Report,
    only: Option<&str>,
    mutate: Option<&dyn Fn(&mut State)>,
) -> BTreeMap<String, usize> {
    let mut states: HashMap<String, (State, u64)> = Default::default();
    let mut classes: BTreeMap<String, usize> = Default::default();
    for line in fixture_lines(name) {
        if rep.stop() {
            break;
        }
        let row: GridRow = serde_json::from_str(&line).expect("row");
        if only.is_some_and(|t| t != row.tag) {
            continue;
        }
        match row.k.as_str() {
            "state" => {
                let reads = decode_reads(row.s.as_ref().expect("s"));
                let mut s = super::common::build_state(&reads);
                attest(&format!("{name}/{}", row.tag), &s, &reads, rep);
                if let Some(m) = mutate {
                    m(&mut s);
                }
                states.insert(row.tag.clone(), (s, reads.timestamp));
            }
            "sw" | "lv" => {
                let (s, now) = states
                    .get(&row.tag)
                    .unwrap_or_else(|| panic!("{name}: rows before the state of {}", row.tag));
                let a = row.a.expect("a").0;
                let where_ = format!("{name}/{} {} d={} a={a}", row.tag, row.k, row.d);
                let got = if row.k == "sw" {
                    preview_swap_words(s, row.d == 1, a, *now)
                } else {
                    preview_lever_words(s, row.d == 1, a, *now)
                };
                rep.compare(&where_, outcome(row.r.as_ref(), row.e.as_ref()), got);
                *classes
                    .entry(format!("{} {}", row.k, class_tag(row.e.as_ref())))
                    .or_default() += 1;
            }
            "note" | "fc" => {}
            other => panic!("{name}: row kind {other}"),
        }
    }
    classes
}

#[test]
fn e2e_preview_grids() {
    // Every revert class the pool's swap and leverage previews can reach on this deployment must
    // have fired somewhere in the grids (the Go suite's closing check; FeeMismatch /
    // FillMismatch / PriceUnchecked for loan 0 cannot).
    let mut seen: BTreeMap<String, usize> = Default::default();
    for blk in E2E_BLOCKS {
        let name = format!("core_e2e_grid_{blk}.jsonl.gz");
        let mut rep = Report::default();
        let classes = replay_e2e_grid(&name, &mut rep, None, None);
        assert!(rep.checks > 20_000, "{name}: only {} checks", rep.checks);
        rep.finish(&format!("{name} classes {classes:?}"));
        for (k, n) in classes {
            *seen
                .entry(k.split(' ').nth(1).unwrap().to_string())
                .or_default() += n;
        }
    }
    for sel in [
        "0xfe85bb51",
        "0x77417454",
        "0xf9b4678a",
        "0x26363b73",
        "0x84f5270a",
        "0xc6520de3",
        "0x78d612f2",
        "0xc81f1209",
        "0x28851730",
        "0x7c3fa3af",
        "0x9e87fac8",
        "0x6d2d9f49",
        "0x81927929",
        "0x032b3d00",
        "0xc3734dc2",
        "0x85c2be22",
        "0xcd215006",
        "0x2c5211c6",
        "0xd6f8f89c",
        "0x2ea2dce8",
        "0x4e487b71",
        "0x00000000",
        "ok",
    ] {
        assert!(seen.contains_key(sel), "no grid row reached {sel}");
    }
}

// ------------------------------------------------------------------ e2e sequences

#[derive(Deserialize)]
struct E2eStep {
    k: String,
    #[serde(default)]
    seq: String,
    #[serde(default)]
    i: i64,
    #[serde(default)]
    op: String,
    #[serde(default)]
    args: Vec<Word>,
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    ok: bool,
    r: Option<Vec<Word>>,
    e: Option<String>,
    #[serde(default)]
    ev: Vec<E2eEvent>,
    post: Option<serde_json::Value>,
    s: Option<serde_json::Value>,
    #[serde(default)]
    d: i64,
    a: Option<Word>,
}

#[derive(Deserialize, Debug)]
struct E2eEvent {
    name: String,
    w: Vec<Word>,
}

/// One executed sequence fixture (`TestCoreE2ESequences`), replayed from the port's own
/// post-states: the previews interleaved, every step's return and event words, the post-state
/// against the chain's dump and that dump's views. Governance moves and tracker inputs are
/// applied to the carried state as the Go replay applies them. `mutate` perturbs every state
/// built from a dump, the carried one and the chain's post-states alike, so only a behavioural
/// divergence can fail a sensitivity run. Returns the executed counts per op.
fn replay_e2e_seq(
    name: &str,
    rep: &mut Report,
    mutate: Option<&dyn Fn(&mut State)>,
) -> BTreeMap<String, usize> {
    let build = |reads: &super::common::Reads| {
        let mut s = super::common::build_state(reads);
        if let Some(m) = mutate {
            m(&mut s);
        }
        s
    };
    let mut executed: BTreeMap<String, usize> = Default::default();
    let mut cur: Option<State> = None;
    let mut now = 0u64;
    let mut seq = String::new();
    for line in fixture_lines(name) {
        if rep.stop() {
            break;
        }
        let st: E2eStep = serde_json::from_str(&line).expect("step");
        match st.k.as_str() {
            "seq" => {
                let reads = decode_reads(st.s.as_ref().expect("s"));
                let s = build(&reads);
                now = reads.timestamp;
                seq = st.seq.clone();
                attest(&format!("{name}/{seq} start"), &s, &reads, rep);
                cur = Some(s);
                continue;
            }
            "sw" | "lv" => {
                let s = cur
                    .as_ref()
                    .expect("a preview before its sequence");
                let a = st.a.expect("a").0;
                let where_ = format!("{name}/{seq} preview {} d={} a={a}", st.k, st.d);
                let got = if st.k == "sw" {
                    preview_swap_words(s, st.d == 1, a, now)
                } else {
                    preview_lever_words(s, st.d == 1, a, now)
                };
                rep.compare(&where_, outcome(st.r.as_ref(), st.e.as_ref()), got);
                continue;
            }
            "step" => {}
            other => panic!("{name}: row kind {other}"),
        }
        let s = cur
            .take()
            .expect("a step before its sequence");
        let where_ = format!(
            "{name}/{seq} step {} {} {:?}",
            st.i,
            st.op,
            st.args
                .iter()
                .map(|w| w.0)
                .collect::<Vec<_>>()
        );
        now = st.ts;
        let want_reads = decode_reads(st.post.as_ref().expect("post"));
        let want = build(&want_reads);
        let args: Vec<U256> = st.args.iter().map(|w| w.0).collect();
        // The port's answer: Ok((return words, event words, post-state)) or the revert.
        let result: Result<(Vec<U256>, Vec<U256>, State), FlammError> = match st.op.as_str() {
            "swap" => {
                let sell = !args[0].is_zero();
                let (mut tin, mut tout) = (s.pool_asset, s.pool.loans[0].token);
                if !sell {
                    std::mem::swap(&mut tin, &mut tout);
                }
                if !args[4].is_zero() {
                    tin = s.pool.loans[0].token;
                    tout = tin;
                }
                let late = u64::try_from(args[3]).expect("late");
                let r = exec_swap(&s, tin, tout, args[1], args[2], now - late, now);
                if let Ok((words, _, _)) = &r {
                    if name.contains("51302915") && seq == "real_sell" {
                        assert_eq!(words[0], U256::from(15_000u64));
                        assert_eq!(
                            words[1],
                            U256::from(11_301_759u64),
                            "the real sell of tx 0x46c3cd72..."
                        );
                    }
                }
                r
            }
            "leverUp" | "leverDown" => {
                exec_lever(&s, st.op == "leverUp", args[0], args[1], now, now)
            }
            _ => {
                let mut post = s.clone();
                match st.op.as_str() {
                    "setLoanConfig" => {
                        let l = &mut post.pool.loans[0];
                        l.swap_price_band_wad = args[0];
                        l.fee_floor_wad = args[1];
                        l.max_swap_notional = args[2];
                        l.reserve_target = args[3];
                    }
                    "setLevPaused" => post.lev_paused = !args[0].is_zero(),
                    "setPaused" => post.paused = !args[0].is_zero(),
                    "setFeeBounds" => {
                        post.fee_floor_wad = args[0];
                        post.fee_cap_wad = args[1];
                    }
                    "setSpread" => {
                        let sp = post
                            .hooks
                            .spread
                            .everlong_spread
                            .as_mut()
                            .expect("a spread post");
                        sp.spread = args[0];
                        sp.last_set_ts = U256::from(now);
                    }
                    "setMaxSpreadAge" => {
                        post.hooks
                            .spread
                            .everlong_spread
                            .as_mut()
                            .expect("a spread post")
                            .max_spread_age = args[0];
                    }
                    "warp" => {}
                    "mockFeeds" | "irmDown" | "storePin" => {
                        // Tracker inputs: a new aggregator round, a rate model that stopped
                        // answering, a pin.
                        post.feed = want.feed.clone();
                        post.router.pin_ltv_wad = want.router.pin_ltv_wad;
                        for (v, w) in post
                            .router
                            .venues
                            .iter_mut()
                            .zip(&want.router.venues)
                        {
                            v.morpho.irm_readable = w.morpho.irm_readable;
                            v.morpho.oracle_ok = w.morpho.oracle_ok;
                            v.morpho.oracle_price = w.morpho.oracle_price;
                            v.morpho.oracle_zero = w.morpho.oracle_zero;
                        }
                    }
                    other => panic!("{where_}: unknown op {other}"),
                }
                Ok((Vec::new(), Vec::new(), post))
            }
        };
        rep.checks += 1;
        let chain: Chain =
            if st.ok { outcome(st.r.as_ref(), None) } else { outcome(None, st.e.as_ref()) };
        let mut post = match result {
            Err(e) => {
                match &chain {
                    Err(Some(c)) if *c == e => {}
                    other => rep.fail(format!("{where_}: chain {other:?}, port err {e:?}")),
                }
                s.clone()
            }
            Ok((words, ev, post)) => {
                match &chain {
                    Ok(c) => {
                        if c.len() != words.len() ||
                            c.iter()
                                .zip(&words)
                                .any(|(a, b)| a != b)
                        {
                            rep.fail(format!("{where_}: return chain {c:?} port {words:?}"));
                        }
                    }
                    other => rep.fail(format!("{where_}: chain {other:?}, port answers {words:?}")),
                }
                if matches!(st.op.as_str(), "swap" | "leverUp" | "leverDown") {
                    let want_name = match st.op.as_str() {
                        "swap" => "Swap",
                        "leverUp" => "LeverUp",
                        _ => "LeverDown",
                    };
                    if st.ev.len() != 1 ||
                        st.ev[0].name != want_name ||
                        st.ev[0]
                            .w
                            .iter()
                            .map(|w| w.0)
                            .collect::<Vec<_>>() !=
                            ev
                    {
                        rep.fail(format!(
                            "{where_}: event chain {:?} port {want_name} {ev:?}",
                            st.ev
                        ));
                    }
                    *executed
                        .entry(st.op.clone())
                        .or_default() += 1;
                } else if !st.ev.is_empty() {
                    rep.fail(format!("{where_}: a move emitted {:?}", st.ev));
                }
                post
            }
        };
        let d = e2e_diff(&post, &want);
        if !d.is_empty() {
            rep.fail(format!("{where_}: post-state differs:\n  {}", d.join("\n  ")));
            // Keep replaying from the chain's state so one divergence reports once.
            post = want.clone();
        }
        post.timestamp = want.timestamp;
        attest(&format!("{where_} post"), &post, &want_reads, rep);
        cur = Some(post);
    }
    executed
}

#[test]
fn e2e_sequences() {
    for blk in E2E_BLOCKS {
        let name = format!("core_e2e_seq_{blk}.jsonl.gz");
        let mut rep = Report::default();
        let executed = replay_e2e_seq(&name, &mut rep, None);
        rep.finish(&format!("{name} executed {executed:?}"));
        assert!(
            executed
                .get("swap")
                .copied()
                .unwrap_or(0) >=
                30,
            "{name}: {executed:?}"
        );
        assert!(
            executed
                .get("leverUp")
                .copied()
                .unwrap_or(0) >=
                2,
            "{name}: {executed:?}"
        );
        assert!(
            executed
                .get("leverDown")
                .copied()
                .unwrap_or(0) >=
                3,
            "{name}: {executed:?}"
        );
    }
}

// ------------------------------------------------------------------ edge grids

#[derive(Deserialize)]
struct EdgeGridRow {
    k: String,
    #[serde(default)]
    tag: String,
    s: Option<serde_json::Value>,
    #[serde(default)]
    d: i64,
    a: Option<Word>,
    c: Option<Word>,
    p: Option<Word>,
    r: Option<String>,
    e: Option<String>,
}

/// A sensitivity perturbation of one scenario's freshly built state, by tag.
type TagMutation<'a> = &'a dyn Fn(&str, &mut State);

/// One edge grid fixture (`TestCoreEdgeGrid`): pool `previewSwap` / `previewLever` and
/// `router.fundingCeiling` at scenario states, every row. Returns the class tallies.
fn replay_edge_grid(
    blk: &str,
    rep: &mut Report,
    only: Option<&str>,
    mutate: Option<TagMutation<'_>>,
) -> BTreeMap<String, usize> {
    let name = format!("edges/core_edge_grid_{blk}.jsonl.gz");
    let mut states: HashMap<String, (State, u64)> = Default::default();
    let mut classes: BTreeMap<String, usize> = Default::default();
    for line in fixture_lines(&name) {
        if rep.stop() {
            break;
        }
        let row: EdgeGridRow = serde_json::from_str(&line).expect("row");
        if only.is_some_and(|t| t != row.tag) {
            continue;
        }
        match row.k.as_str() {
            "note" => continue,
            "state" => {
                let reads = decode_reads(row.s.as_ref().expect("s"));
                let mut s = super::common::build_state(&reads);
                rep.checks += 1;
                match s.router.positions(reads.timestamp) {
                    Err(e) => rep.fail(format!("{blk}/{}: positions {e:?}", row.tag)),
                    Ok(pos) => {
                        let g = s.pool.physical + pos.total_coll;
                        if g != reads.pool.gross.0 {
                            rep.fail(format!(
                                "{blk}/{}: gross chain {} port {g}",
                                row.tag, reads.pool.gross.0
                            ));
                        }
                    }
                }
                if let Some(m) = mutate {
                    m(&row.tag, &mut s);
                }
                states.insert(row.tag.clone(), (s, reads.timestamp));
                continue;
            }
            _ => {}
        }
        let (s, now) = states
            .get(&row.tag)
            .unwrap_or_else(|| panic!("{name}: rows before the state of {}", row.tag));
        let (where_, got) = match row.k.as_str() {
            "sw" => {
                let a = row.a.expect("a").0;
                (
                    format!("{blk}/{} sw d={} a={a}", row.tag, row.d),
                    preview_swap_words(s, row.d == 1, a, *now),
                )
            }
            "lv" => {
                let a = row.a.expect("a").0;
                (
                    format!("{blk}/{} lv d={} a={a}", row.tag, row.d),
                    preview_lever_words(s, row.d == 1, a, *now),
                )
            }
            "fc" => {
                let (c, p) = (row.c.expect("c").0, row.p.expect("p").0);
                (
                    format!("{blk}/{} fc c={c} p={p}", row.tag),
                    s.router
                        .funding_ceiling(0, c, p, *now)
                        .map(|f| vec![f]),
                )
            }
            other => panic!("{name}: row kind {other}"),
        };
        rep.compare(&where_, outcome_hex(row.r.as_ref(), row.e.as_ref()), got);
        *classes
            .entry(format!("{} {}", row.k, class_tag(row.e.as_ref())))
            .or_default() += 1;
    }
    classes
}

#[test]
fn edge_grids() {
    for blk in EDGE_BLOCKS {
        let mut rep = Report::default();
        let classes = replay_edge_grid(blk, &mut rep, None, None);
        assert!(rep.checks > 100, "{blk}: only {} checks", rep.checks);
        rep.finish(&format!("edges/core_edge_grid_{blk} classes {classes:?}"));
    }
}

/// `TestCoreEdgeGridSensitivity`: each small perturbation of a state input the scenario exercises
/// must produce mismatches (the unperturbed replay requires none).
#[test]
fn edge_grid_sensitivity() {
    let one = U256::from(1u64);
    type Mutation = Box<dyn Fn(&mut State)>;
    let cases: Vec<(&str, Mutation)> = vec![
        ("seq_grace_eq", Box::new(move |s| s.feed.sequencer_grace -= one)),
        ("cb_age_eq", Box::new(move |s| s.feed.asset.heartbeat -= one)),
        ("usdc_age_eq", Box::new(move |s| s.feed.loans[0].heartbeat -= one)),
        ("peg_lo_eq", Box::new(move |s| s.feed.loans[0].peg_band_wad -= one)),
        (
            "armed_min",
            Box::new(move |s| {
                s.hooks
                    .spread
                    .everlong_spread
                    .as_mut()
                    .unwrap()
                    .spread += one;
            }),
        ),
        (
            "spread_age_eq",
            Box::new(move |s| {
                s.hooks
                    .spread
                    .everlong_spread
                    .as_mut()
                    .unwrap()
                    .max_spread_age -= one;
            }),
        ),
        ("degrade_zero", Box::new(move |s| s.last_lever_spread_ppm = U256::from(17_500u64))),
        (
            "live",
            Box::new(move |s| {
                s.hooks
                    .swap
                    .everlong_swap
                    .as_mut()
                    .unwrap()
                    .reserve_volatile += one;
            }),
        ),
        ("ratecap_0", Box::new(move |s| s.router.venues[0].max_borrow_rate_wad -= one)),
        ("irm_dt_3600", Box::new(move |s| s.router.venues[0].morpho.irm_readable = true)),
        (
            "morpho_fee",
            Box::new(move |s| {
                s.router.venues[0]
                    .morpho
                    .market
                    .last_update -= U256::from(86_400u64)
            }),
        ),
        ("debtcap_tight", Box::new(move |s| s.router.venues[0].debt_cap += one)),
        (
            "ltv_low_1",
            Box::new(move |s| s.pool.room_epsilon_wad += U256::from(1_000_000_000_000_000u64)),
        ),
        (
            "whale_dt1",
            Box::new(move |s| {
                s.router.venues[0]
                    .morpho
                    .market
                    .total_supply_assets += one
            }),
        ),
    ];
    for (tag, mutate) in &cases {
        let mut rep = Report { quiet: true, ..Default::default() };
        let m = |t: &str, s: &mut State| {
            if t == *tag {
                mutate(s);
            }
        };
        let classes = replay_edge_grid("51324800", &mut rep, Some(tag), Some(&m));
        assert!(rep.checks > 0, "{tag}: no rows");
        assert!(!rep.failures.is_empty(), "perturbing {tag} changed no row ({classes:?})");
        eprintln!("sensitivity {tag}: broke at {}", rep.failures[0]);
    }
}

// ------------------------------------------------------------------ edge sequences

#[derive(Deserialize)]
struct EdgeSeqRow {
    k: String,
    #[serde(default)]
    seq: String,
    #[serde(default)]
    i: i64,
    #[serde(default)]
    ts: u64,
    s: Option<serde_json::Value>,
    #[serde(default)]
    lever: bool,
    #[serde(default)]
    d: i64,
    a: Option<Word>,
    m: Option<Word>,
    #[serde(default)]
    late: u64,
    r: Option<String>,
    e: Option<String>,
    #[serde(default)]
    ev: Vec<EdgeEvent>,
    post: Option<serde_json::Value>,
    #[serde(default)]
    op: String,
}

#[derive(Deserialize, Debug)]
struct EdgeEvent {
    t: String,
    data: String,
}

const CBBTC: &str = "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf";
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

/// `coreEdgeReSkip`: the fields a non-core move may change (a tracker re-read); every other field
/// must already equal the port's carried state. `None` is a snapshot rewind (nothing to carry).
fn edge_re_skip(op: &str) -> Option<Vec<&'static str>> {
    Some(match op {
        "setSpread" | "setMaxSpreadAge" => vec!["s.Hooks.Spread"],
        "setLoanConfig" => vec![
            "s.Pool.Loans[0].SwapPriceBandWad",
            "s.Pool.Loans[0].FeeFloorWad",
            "s.Pool.Loans[0].MaxSwapNotional",
            "s.Pool.Loans[0].ReserveTarget",
        ],
        "setFeeBounds" => vec!["s.FeeFloorWad", "s.FeeCapWad"],
        "setPaused" => vec!["s.Paused"],
        "setLevPaused" => vec!["s.LevPaused"],
        "setFeatures" => vec!["s.Pool.Features"],
        "setDials" => vec!["s.Pool.LtvWad", "s.Pool.PhiWad", "s.Router.PinLtvWad"],
        "setVenueCaps" => vec![
            "s.Router.Venues[0].DebtCap",
            "s.Router.Venues[0].SupplyCap",
            "s.Router.Venues[0].MaxBorrowRateWad",
        ],
        "setVenueFlags" => {
            vec!["s.Router.Venues[0].BorrowEnabled", "s.Router.Venues[0].SupplyEnabled"]
        }
        "feeds" | "sequencer" => vec!["s.Feed"],
        "irm" => vec!["s.Router.Venues[0].Morpho.IrmReadable"],
        "oracle" => vec![
            "s.Router.Venues[0].Morpho.OracleOk",
            "s.Router.Venues[0].Morpho.OraclePrice",
            "s.Router.Venues[0].Morpho.OracleZero",
        ],
        "morpho" => {
            vec!["s.Router.Venues[0].Morpho.Market", "s.Router.Venues[0].Morpho.RateAtTarget"]
        }
        "loanCaps" => vec![
            "s.Router.Loans[0].DebtCap",
            "s.Router.Loans[0].SupplyCap",
            "s.Router.Loans[0].BorrowEnabled",
        ],
        "donate" => vec![
            "s.Router.Venues[0].Morpho.Market",
            "s.Router.Venues[0].Morpho.RateAtTarget",
            "s.Router.Venues[0].Morpho.Position",
        ],
        "warpquiet" => vec![],
        "rewind" | "deposit" => return None,
        other => panic!("unknown re-read op {other}"),
    })
}

/// `coreEdgePaths`: which settlement legs an executed step moved.
fn edge_paths(pre: &State, post: &State) -> Vec<String> {
    let mut tags = Vec::new();
    let (a, b) = (&pre.router.venues[0], &post.router.venues[0]);
    let mut cmp = |name: &str, x: U256, y: U256| {
        if y > x {
            tags.push(format!("{name}+"));
        } else if y < x {
            tags.push(format!("{name}-"));
        }
    };
    cmp("borrowShares", a.morpho.position.borrow_shares, b.morpho.position.borrow_shares);
    cmp("collateral", a.morpho.position.collateral, b.morpho.position.collateral);
    cmp("supplyShares", a.morpho.position.supply_shares, b.morpho.position.supply_shares);
    cmp("liquid", pre.pool.loans[0].liquid, post.pool.loans[0].liquid);
    if post.last_lever_spread_ppm != pre.last_lever_spread_ppm {
        tags.push("spreadStored".into());
    }
    if b.morpho.market.last_update > a.morpho.market.last_update {
        tags.push("accrued".into());
    }
    tags
}

/// One block's edge sequences (`TestCoreEdgeSequences`), replayed from the port's own
/// post-states; `mutate` perturbs every post-state the port carries forward.
fn replay_edge_seq(
    blk: &str,
    rep: &mut Report,
    mutate: Option<&dyn Fn(&mut State)>,
) -> BTreeMap<String, usize> {
    let name = format!("edges/core_edge_seq_{blk}.jsonl.gz");
    let cbbtc = super::common::addr(CBBTC);
    let usdc = super::common::addr(USDC);
    let mut executed: BTreeMap<String, usize> = Default::default();
    let mut cur: Option<State> = None;
    let mut now = 0u64;
    for line in fixture_lines(&name) {
        if rep.stop() {
            break;
        }
        let row: EdgeSeqRow = serde_json::from_str(&line).expect("row");
        let where_ = format!("{blk}/{}#{}", row.seq, row.i);
        match row.k.as_str() {
            "begin" => {
                let reads = decode_reads(row.s.as_ref().expect("s"));
                cur = Some(super::common::build_state(&reads));
                now = reads.timestamp;
                continue;
            }
            "pv" => {
                let s = cur.as_ref().expect("state");
                assert_eq!(now, row.ts, "{where_}");
                let a = row.a.expect("a").0;
                let before = s.clone();
                let got = if row.lever {
                    preview_lever_words(s, row.d == 1, a, now)
                } else {
                    preview_swap_words(s, row.d == 1, a, now)
                };
                rep.compare(
                    &format!("{where_} preview lever={} d={} a={a}", row.lever, row.d),
                    outcome_hex(row.r.as_ref(), row.e.as_ref()),
                    got,
                );
                let d = edge_diff(s, &before, &[]);
                if !d.is_empty() {
                    rep.fail(format!("{where_}: preview mutated the state: {d:?}"));
                }
                continue;
            }
            "warp" => {
                now = row.ts;
                let want =
                    super::common::build_state(&decode_reads(row.post.as_ref().expect("post")));
                rep.checks += 1;
                let d = edge_diff(cur.as_ref().expect("state"), &want, &[]);
                if !d.is_empty() {
                    rep.fail(format!(
                        "{where_} warp: reads moved without a transaction:\n  {}",
                        d.join("\n  ")
                    ));
                }
                continue;
            }
            "re" => {
                now = row.ts;
                let want =
                    super::common::build_state(&decode_reads(row.post.as_ref().expect("post")));
                rep.checks += 1;
                if let Some(skip) = edge_re_skip(&row.op) {
                    let d = edge_diff(cur.as_ref().expect("state"), &want, &skip);
                    if !d.is_empty() {
                        rep.fail(format!(
                            "{where_} re-read {}: carried state drifted:\n  {}",
                            row.op,
                            d.join("\n  ")
                        ));
                    }
                }
                cur = Some(want);
                continue;
            }
            "x" => {}
            other => panic!("{name}: row kind {other}"),
        }
        now = row.ts;
        let s = cur.take().expect("state");
        let want = super::common::build_state(&decode_reads(row.post.as_ref().expect("post")));
        let a = row.a.expect("a").0;
        let m = row.m.expect("m").0;
        let label = format!("{where_} exec lever={} d={} a={a} min={m}", row.lever, row.d);
        let pre = s.clone();
        let (result, ev_name) = if row.lever {
            (
                exec_lever(&s, row.d == 1, a, m, now - row.late, now),
                if row.d == 1 { "LeverUp" } else { "LeverDown" },
            )
        } else {
            let (tin, tout) = if row.d == 1 { (cbbtc, usdc) } else { (usdc, cbbtc) };
            (exec_swap(&s, tin, tout, a, m, now - row.late, now), "Swap")
        };
        let words = result
            .as_ref()
            .map(|(w, _, _)| w.clone())
            .map_err(|e| *e);
        rep.compare(&label, outcome_hex(row.r.as_ref(), row.e.as_ref()), words);
        rep.checks += 1;
        let d = edge_diff(&s, &pre, &[]);
        if !d.is_empty() {
            rep.fail(format!("{label}: the execution wrote its receiver:\n  {}", d.join("\n  ")));
        }
        if row.r.is_none() {
            rep.checks += 1;
            if !row.ev.is_empty() {
                rep.fail(format!("{label}: reverted call emitted {:?}", row.ev));
            }
            let d = edge_diff(&s, &want, &[]);
            if !d.is_empty() {
                rep.fail(format!("{label}: chain state moved on a revert:\n  {}", d.join("\n  ")));
            }
            cur = Some(s);
            continue;
        }
        let Ok((_, evw, mut post)) = result else {
            cur = Some(want);
            continue;
        };
        *executed
            .entry(ev_name.to_string())
            .or_default() += 1;
        for tag in edge_paths(&s, &want) {
            *executed
                .entry(format!("{ev_name} {tag}"))
                .or_default() += 1;
        }
        rep.checks += 1;
        if row.ev.len() != 1 || row.ev[0].t != ev_name {
            rep.fail(format!("{label}: chain events {:?}, port {ev_name}", row.ev));
        } else if return_words(&row.ev[0].data) != evw {
            rep.fail(format!(
                "{label}: event chain {:?} port {evw:?}",
                return_words(&row.ev[0].data)
            ));
        }
        rep.checks += 1;
        let d = edge_diff(&post, &want, &[]);
        if !d.is_empty() {
            rep.fail(format!("{label}: post-state differs:\n  {}", d.join("\n  ")));
            post = want;
        }
        if let Some(m) = mutate {
            m(&mut post);
        }
        cur = Some(post);
    }
    executed
}

#[test]
fn edge_sequences() {
    for blk in EDGE_BLOCKS {
        let mut rep = Report::default();
        let executed = replay_edge_seq(blk, &mut rep, None);
        assert!(rep.checks > 50, "{blk}: only {} checks", rep.checks);
        rep.finish(&format!("edges/core_edge_seq_{blk} executed {executed:?}"));
    }
}

/// `TestCoreEdgeSequenceSensitivity`: carrying a Morpho accrual stamp one second stale must break
/// the replay.
#[test]
fn edge_sequence_sensitivity() {
    let mut rep = Report { quiet: true, ..Default::default() };
    replay_edge_seq(
        "51324800",
        &mut rep,
        Some(&|s: &mut State| {
            s.router.venues[0]
                .morpho
                .market
                .last_update -= U256::from(1u64)
        }),
    );
    assert!(!rep.failures.is_empty(), "a stale accrual stamp changed no step");
    eprintln!("sensitivity accrual stamp: broke at {}", rep.failures[0]);
}

/// The degrade value: forcing a stored `lastLeverSpreadPpm` (on the carried state and on every
/// chain dump alike) must break the degraded lever-down steps of the `lever` sequence (a stale
/// post degrades to the stored value, `FLAMMLeverLib.sol:168-169`), clearing the spread post's
/// staleness window must break the steps that saw it stale, and one wei on the swap hook's
/// stored reserve must break a fill.
#[test]
fn e2e_sequence_sensitivity() {
    let name = "core_e2e_seq_51302915.jsonl.gz";
    let one = U256::from(1u64);
    type Mutation = Box<dyn Fn(&mut State)>;
    let cases: Vec<(&str, Mutation)> = vec![
        ("degrade value", Box::new(|s| s.last_lever_spread_ppm = U256::from(12_345u64))),
        (
            "spread age",
            Box::new(|s| {
                if let Some(p) = s.hooks.spread.everlong_spread.as_mut() {
                    p.max_spread_age = U256::ZERO;
                }
            }),
        ),
        (
            "hook reserve",
            Box::new(move |s| {
                s.hooks
                    .swap
                    .everlong_swap
                    .as_mut()
                    .unwrap()
                    .reserve_stable += one;
            }),
        ),
    ];
    for (tag, mutate) in &cases {
        let mut rep = Report { quiet: true, ..Default::default() };
        replay_e2e_seq(name, &mut rep, Some(mutate.as_ref()));
        assert!(!rep.failures.is_empty(), "perturbing the {tag} changed no step");
        eprintln!("sensitivity {tag}: broke at {}", rep.failures[0]);
    }
}

/// Every revert class the recorded rows carry is mapped (no row is compared against unmapped
/// data), and the contracts' own `Unsupported()` never appears on chain.
#[test]
fn every_recorded_revert_class_is_mapped() {
    let mut seen: std::collections::HashSet<FlammError> = Default::default();
    for blk in E2E_BLOCKS {
        for line in fixture_lines(&format!("core_e2e_grid_{blk}.jsonl.gz")) {
            let row: GridRow = serde_json::from_str(&line).expect("row");
            if let Some(e) = &row.e {
                let c = revert_class(e).unwrap_or_else(|| panic!("{blk}: unmapped {e}"));
                seen.insert(c);
            }
        }
        for line in fixture_lines(&format!("core_e2e_seq_{blk}.jsonl.gz")) {
            let st: E2eStep = serde_json::from_str(&line).expect("step");
            if let Some(e) = &st.e {
                let c = revert_class(e).unwrap_or_else(|| panic!("{blk}: unmapped {e}"));
                seen.insert(c);
            }
        }
    }
    for want in [
        FlammError::InvalidAmount,
        FlammError::Expired,
        FlammError::InvalidPair,
        FlammError::Paused,
        FlammError::FeatureDisabled,
        FlammError::StalePrice,
        FlammError::InvalidPrice,
        FlammError::SequencerDown,
        FlammError::SequencerGrace,
        FlammError::PegBroken,
        FlammError::LevPaused,
        FlammError::SpreadUnavailable,
        FlammError::PanicArithmetic,
        FlammError::PriceBand,
        FlammError::NothingToFill,
        FlammError::LevValueLeak,
        FlammError::MulDivOverflow,
    ] {
        assert!(seen.contains(&want), "no recorded row reaches {want:?}");
    }
    assert!(!seen.contains(&FlammError::Unsupported));
    let _ = read_fixture("core_e2e_grid_51302915.jsonl.gz");
}
