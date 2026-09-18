// Copyright (c) 2026 Everlong Labs Limited

//! Swap hook parity: the curve grid the deployed Base `AlmCurve` library produced, the fee grid
//! from the c104 `EverlongStrategy` source, the hook grid the live Base `EverlongHook` produced
//! under overwritten storage, the pool's first settled swap, and the edge fixtures of
//! `swap_hook_edges_test.go` (generators written separately from the module fixtures':
//! source-compiled harnesses for the internal curve and fee functions, the deployed `AlmCurve`
//! library for `reservesAt` / `swapExactInX96`, the live `EverlongHook` under overwritten storage).
//! Every row is replayed (no sampling), to the wei and by revert class.

use std::collections::HashMap;

use alloy::primitives::U256;
use serde_json::Value;

use super::{
    super::{
        almcurve::{self, Support, MAX_X_WAD, MIN_X_WAD},
        context::{PoolContext, SwapContext},
        fee::{self, FeeParams, FeeState},
        hook::{Book, FillResult, HookState},
        math::{Q96, WAD},
        FlammError,
    },
    fixtures::{arr, b, dec, expect, f, load, s, u, want_err},
};

/// `Support` from a four-word `[aWad, xLo, xHi, yHi]` array.
fn support(v: &Value) -> Support {
    let a = arr(v);
    assert_eq!(a.len(), 4);
    Support { a_wad: a[0], x_lo: a[1], x_hi: a[2], y_hi: a[3] }
}

/// `FeeParams` from the eight-word row `[mid, out, gamma, sigmaRef, volBeta, volMin, volMax,
/// dirSkew]`.
fn fee_params(v: &[U256]) -> FeeParams {
    assert_eq!(v.len(), 8);
    FeeParams {
        mid_fee_wad: v[0],
        out_fee_wad: v[1],
        gamma_wad: v[2],
        sigma_ref_wad: v[3],
        vol_beta_wad: v[4],
        vol_min_wad: v[5],
        vol_max_wad: v[6],
        dir_skew_wad: v[7],
    }
}

/// The hook state of a `{"k":"state", ...}` row of `hook_fill_grid` / `hook_live_swap`.
fn hook_state(row: &Value) -> HookState {
    HookState {
        a_wad: f(row, "aWad"),
        support: support(&row["sup"]),
        anchor_sqrt_x96: f(row, "anchorSqrtX96"),
        reservation_price_wad: f(row, "rp"),
        kappa: f(row, "kappa"),
        x_wad: f(row, "x"),
        reserve_stable: f(row, "rs"),
        idle_stable: f(row, "is"),
        reserve_volatile: f(row, "rv"),
        idle_volatile: f(row, "iv"),
        rv_wad: f(row, "rvWad"),
        fee: fee_params(&arr(&row["fee"])),
        inv_skew_kappa_wad: f(row, "invKappa"),
        inv_skew_band_wad: f(row, "invBand"),
        loan_scale: f(row, "loanScale"),
    }
}

/// A swap context from the pool-asset legs and the fill request.
fn swap_ctx(
    phys: U256,
    posted: U256,
    pool_asset_in: bool,
    amount_in: U256,
    max_out: U256,
) -> SwapContext {
    SwapContext {
        pool: PoolContext {
            physical_pool_asset: phys,
            posted_pool_asset: posted,
            ..Default::default()
        },
        pool_asset_in,
        amount_in,
        max_amount_out: max_out,
        ..Default::default()
    }
}

// ------------------------------------------------------------------ module fixtures

/// `TestAlmCurveGrid`: supportFor, yAtX (also read through reservesAt on a full-domain support),
/// priceAtX, reservesAt and swapExactInX96 over amplifications, spans, coordinates across and
/// beyond the domain, anchors, scales and log-spaced amounts, reverts included.
#[test]
fn alm_curve_grid() {
    let rows = load("alm_curve_grid.json.gz");
    let rows = rows.as_array().unwrap();
    assert!(rows.len() > 10_000);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in rows {
        let k = s(r, "k");
        *counts.entry(k).or_default() += 1;
        let ok = b(r, "ok");
        let err = s(r, "err");
        match k {
            "sup" => {
                let got = almcurve::support_for(f(r, "a"), f(r, "up"), f(r, "dn"));
                if let Some(sup) = expect(ok, err, got, r) {
                    assert_eq!(sup, support(&r["sup"]), "{r}");
                }
            }
            "y" => {
                let (x, a) = (f(r, "x"), f(r, "a"));
                if let Some(y) = expect(ok, err, almcurve::y_at_x(x, a), r) {
                    assert_eq!(dec(y), s(r, "y"), "{r}");
                }
                let full = Support { a_wad: a, x_lo: MIN_X_WAD, x_hi: MAX_X_WAD, y_hi: U256::ZERO };
                if let Some((st, vol)) =
                    expect(ok, err, almcurve::reserves_at(&full, Q96, WAD, x), r)
                {
                    assert_eq!(dec(st), s(r, "y"), "{r}");
                    assert_eq!(dec(vol), s(r, "held"), "{r}");
                }
            }
            "p" => {
                if let Some(p) = expect(ok, err, almcurve::price_at_x(f(r, "x"), f(r, "a")), r) {
                    assert_eq!(dec(p), s(r, "p"), "{r}");
                }
            }
            "res" => {
                let sup = support(&r["sup"]);
                let got = almcurve::reserves_at(&sup, f(r, "anchor"), f(r, "kappa"), f(r, "x"));
                if let Some((st, vol)) = expect(ok, err, got, r) {
                    assert_eq!(dec(st), s(r, "stable"), "{r}");
                    assert_eq!(dec(vol), s(r, "volatile"), "{r}");
                }
            }
            "swap" => {
                let sup = support(&r["sup"]);
                let got = almcurve::swap_exact_in_x96(
                    &sup,
                    f(r, "anchor"),
                    f(r, "kappa"),
                    f(r, "x"),
                    b(r, "stableIn"),
                    f(r, "amt"),
                );
                if let Some(fill) = expect(ok, err, got, r) {
                    assert_eq!(dec(fill.amount_out), s(r, "out"), "{r}");
                    assert_eq!(dec(fill.x_after), s(r, "xAfter"), "{r}");
                    assert_eq!(dec(fill.amount_in_unspent), s(r, "unspent"), "{r}");
                }
            }
            other => panic!("unknown row kind {other}"),
        }
    }
    eprintln!("alm_curve_grid rows by kind: {counts:?}");
}

/// `TestFeeFillGrid`: fillFee over weights at, one wei around, and ramp-multiples away from the
/// tie, both directions, curvature and volatility on and off, surcharge rows, one-sided books,
/// dislocated spots and malformed rows; plus the lnWad ratio and volMultiplier sub-grids.
#[test]
fn fee_fill_grid() {
    let rows = load("fee_fill_grid.json.gz");
    let rows = rows.as_array().unwrap();
    let mut params = HashMap::new();
    let (mut n, mut lns, mut vols) = (0, 0, 0);
    for r in rows {
        match s(r, "k") {
            "params" => {
                params.insert(s(r, "i").to_string(), fee_params(&arr(&r["v"])));
                continue;
            }
            "ln" => {
                let got = fee::log_ratio_abs_wad(f(r, "a"), f(r, "b"));
                if let Some(v) = expect(b(r, "ok"), s(r, "err"), got, r) {
                    assert_eq!(dec(v), s(r, "v"), "{r}");
                }
                lns += 1;
                continue;
            }
            "vol" => {
                let p = params[s(r, "p")];
                let got = fee::vol_multiplier(&p, f(r, "rvw"), f(r, "d"));
                if let Some(v) = expect(b(r, "ok"), s(r, "err"), got, r) {
                    assert_eq!(dec(v), s(r, "v"), "{r}");
                }
                vols += 1;
                continue;
            }
            "fee" => {}
            other => panic!("unknown row kind {other}"),
        }
        let p = params[s(r, "p")];
        let st = FeeState {
            reserve_stable: f(r, "rs"),
            reserve_volatile: f(r, "rv"),
            anchor_wad: f(r, "an"),
            spot_wad: f(r, "sp"),
            rv_wad: f(r, "rvw"),
        };
        let got = fee::fill_fee(&p, &st, b(r, "loanIn"), f(r, "kap"), f(r, "band"));
        if let Some(fee) = expect(b(r, "ok"), s(r, "err"), got, r) {
            assert_eq!(dec(fee), s(r, "f"), "{r}");
        }
        let g = fee::reduction_g(&st, p.gamma_wad);
        if b(r, "gOk") {
            let (g, w) = g.unwrap_or_else(|e| panic!("{e}: {r}"));
            assert_eq!(dec(g), s(r, "g"), "{r}");
            assert_eq!(dec(w), s(r, "w"), "{r}");
        } else {
            assert!(g.is_err(), "{r}");
        }
        n += 1;
    }
    assert!(n > 6000, "{n}");
    assert_eq!(lns, 600);
    assert_eq!(vols, 300);
}

fn book_of(v: &Value) -> Book {
    Book {
        kappa: f(v, "kappa"),
        rs: f(v, "rs"),
        is: f(v, "is"),
        rv: f(v, "rv"),
        iv: f(v, "iv"),
        x: f(v, "x"),
    }
}

fn post_book(st: &HookState) -> Book {
    Book {
        kappa: st.kappa,
        rs: st.reserve_stable,
        is: st.idle_stable,
        rv: st.reserve_volatile,
        iv: st.idle_volatile,
        x: st.x_wad,
    }
}

fn require_fill(want: &Value, got: &FillResult, ctx: &Value) {
    assert_eq!(
        [dec(got.amount_in_used), dec(got.gross_out), dec(got.fee_out), dec(got.spot_after_wad)],
        [s(want, "used"), s(want, "gross"), s(want, "feeOut"), s(want, "spot")],
        "{ctx}"
    );
}

/// `hookReplay`: one recorded context against bookFor, previewFeeWad, previewExactIn and the
/// post-state executeExactIn commits, reverts included, and that execute writes only the book.
/// Returns whether the fill consumed input.
fn hook_replay(st: &HookState, r: &Value) -> bool {
    let ctx = swap_ctx(f(r, "phys"), f(r, "posted"), b(r, "in"), f(r, "amt"), f(r, "cap"));
    let fee_wad = f(r, "fee");
    let (book, pf, px, ex) = (&r["book"], &r["pf"], &r["px"], &r["ex"]);

    if let Some(got) = expect(b(book, "ok"), s(book, "err"), st.book_for(&ctx.pool), r) {
        assert_eq!(got, book_of(book), "{r}");
    }
    if let Some(got) = expect(b(pf, "ok"), s(pf, "err"), st.preview_fee_wad(&ctx), r) {
        assert_eq!(dec(got), s(pf, "v"), "{r}");
    }
    if let Some(got) = expect(b(px, "ok"), s(px, "err"), st.preview_exact_in(&ctx, fee_wad), r) {
        require_fill(px, &got, r);
    }
    let mut filled = false;
    if let Some((fr, post)) =
        expect(b(ex, "ok"), s(ex, "err"), st.execute_exact_in(&ctx, fee_wad), r)
    {
        require_fill(ex, &fr, r);
        assert_eq!(post_book(&post), book_of(&r["post"]), "{r}");
        // execute writes only the book
        let mut rest = post;
        rest.commit(&post_book(st));
        assert_eq!(rest, *st, "{r}");
        filled = !fr.amount_in_used.is_zero();
    }
    filled
}

/// `TestHookFillGrid`: the live Base EverlongHook (storage overwritten per state: curve rows,
/// anchors, books on and off the curve, empty and retracted books, fee rows) over gross !=
/// accounted, both directions, dust-to-overflow amounts, binding and non-binding output caps and
/// fee rates.
#[test]
fn hook_fill_grid() {
    let rows = load("hook_fill_grid.json.gz");
    let rows = rows.as_array().unwrap();
    let mut states: HashMap<String, HookState> = HashMap::new();
    let (mut fills, mut filled, mut reverted, mut spots) = (0, 0, 0, 0);
    for r in rows {
        match s(r, "k") {
            "state" => {
                states.insert(s(r, "id").to_string(), hook_state(r));
            }
            "spot" => {
                let st = &states[s(r, "s")];
                if let Some(sp) = expect(b(r, "ok"), s(r, "err"), st.spot(), r) {
                    assert_eq!(dec(sp), s(r, "v"), "{r}");
                }
                spots += 1;
            }
            "fill" => {
                let st = &states[s(r, "s")];
                if hook_replay(st, r) {
                    filled += 1;
                }
                if !b(&r["px"], "ok") {
                    reverted += 1;
                }
                fills += 1;
            }
            other => panic!("unknown row kind {other}"),
        }
    }
    assert_eq!(states.len(), spots);
    assert!(fills > 9000, "{fills}");
    assert!(filled > 3000, "{filled}");
    assert!(reverted > 300, "{reverted}");
    eprintln!("hook_fill_grid contexts {fills}, filled {filled}, reverted {reverted}");
}

/// `TestHookLiveSwap`: the first settled Swap on the Base pool, the 15000-sat sell of tx
/// `0x46c3cd72a5860b2fe546e5a2130e066314e3777027151661e1e4f19a935901fa` against the hook at block
/// 51302915 (the constructor seed book rescaled to 219423 sats), fee 17499999999999999, netting
/// 11301759 USDC.
#[test]
fn hook_live_swap() {
    let rows = load("hook_live_swap.json.gz");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let st = hook_state(&rows[0]);
    assert!(hook_replay(&st, &rows[1]));

    let ctx = swap_ctx(
        U256::from(219_423u64),
        U256::ZERO,
        true,
        U256::from(15_000u64),
        u(&Value::from("101043362000000000000")),
    );
    let fee = st.preview_fee_wad(&ctx).unwrap();
    assert_eq!(dec(fee), "17499999999999999");
    let (fr, post) = st.execute_exact_in(&ctx, fee).unwrap();
    let net = (fr.gross_out - fr.fee_out) / st.loan_scale;
    assert_eq!(dec(net), "11301759");
    assert_eq!(dec(fr.amount_in_used), "15000");
    // the committed book is the one the chain stored (read back at block 51310000)
    assert_eq!(dec(post.kappa), "12779948558379");
    assert_eq!(dec(post.reserve_stable), "157080117684030704791");
    assert_eq!(dec(post.idle_stable), "201304154388180895");
    assert_eq!(dec(post.reserve_volatile), "234423");
    assert_eq!(dec(post.x_wad), "532533306204662064");
}

// ------------------------------------------------------------------ edge fixtures

/// Accumulates mismatches so a run reports all of them.
struct Checker {
    name: &'static str,
    fails: usize,
    checks: usize,
}

impl Checker {
    fn new(name: &'static str) -> Self {
        Self { name, fails: 0, checks: 0 }
    }

    fn fail(&mut self, msg: String) {
        self.fails += 1;
        if self.fails <= 60 {
            eprintln!("{}: {msg}", self.name);
        }
    }

    /// One row: an error exactly where the contract reverted (the right class), else every return
    /// word equal.
    fn compare(&mut self, r: &Value, got: Result<Vec<U256>, FlammError>) {
        self.checks += 1;
        let (fn_name, args) = (s(r, "f"), &r["a"]);
        if !b(r, "ok") {
            let want = want_err(s(r, "e"));
            match got {
                Err(e) if e == want => {}
                other => self.fail(format!(
                    "{fn_name}{args}: solidity reverted {want} ({}), port returned {other:?}",
                    s(r, "e")
                )),
            }
            return;
        }
        let want: Vec<&str> = r["r"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        match got {
            Err(e) => self.fail(format!("{fn_name}{args}: solidity ok {want:?}, port refused {e}")),
            Ok(words) => {
                let got: Vec<String> = words.iter().map(|w| dec(*w)).collect();
                if got != want {
                    self.fail(format!("{fn_name}{args}: port {got:?} solidity {want:?}"));
                }
            }
        }
    }

    fn finish(self, seen: &HashMap<String, usize>) {
        eprintln!(
            "{} checks {} mismatches {} by fn {:?}",
            self.name, self.checks, self.fails, seen
        );
        assert_eq!(self.fails, 0, "{}: {} mismatches", self.name, self.fails);
    }
}

fn sup_of(a: &[U256]) -> Support {
    Support { a_wad: a[0], x_lo: a[1], x_hi: a[2], y_hi: a[3] }
}

fn words<const N: usize>(w: [U256; N]) -> Vec<U256> {
    w.to_vec()
}

/// `TestAlmCurveEdges`: the internal AlmCurve functions through a source-compiled harness, and
/// reservesAt / swapExactInX96 against the deployed library: domain and amplification edges with
/// 1-wei neighbours, the `b = 0` branch point, clamp gates, seed-skip gates on `yTarget`, band
/// truncation, malformed supports, and a keyed random grid.
#[test]
fn alm_curve_edges() {
    let rows = load("edges/alm_curve_edges.json.gz");
    let mut c = Checker::new("alm_curve_edges");
    let mut seen: HashMap<String, usize> = HashMap::new();
    for r in rows.as_array().unwrap() {
        let a = arr(&r["a"]);
        *seen
            .entry(s(r, "f").to_string())
            .or_default() += 1;
        let got = match s(r, "f") {
            // cWad = fourK - WAD as int256, compared in two's complement.
            "cWad" => almcurve::c_wad_raw(a[0]).map(|c| words([c])),
            "yAtX" => almcurve::y_at_x(a[0], a[1]).map(|y| words([y])),
            "priceAtX" => almcurve::price_at_x(a[0], a[1]).map(|p| words([p])),
            "xAtPrice" => almcurve::x_at_price(a[0], a[1]).map(|x| words([x])),
            "supportFor" => almcurve::support_for(a[0], a[1], a[2])
                .map(|s| words([s.a_wad, s.x_lo, s.x_hi, s.y_hi])),
            "heldAt" => almcurve::held_at(&sup_of(&a), a[4]).map(|(v, s)| words([v, s])),
            "swapExactIn" => almcurve::swap_exact_in(&sup_of(&a), a[4], !a[5].is_zero(), a[6])
                .map(|f| words([f.amount_out, f.x_after, f.input_unused])),
            "reservesAt" => {
                almcurve::reserves_at(&sup_of(&a), a[4], a[5], a[6]).map(|(s, v)| words([s, v]))
            }
            "swapExactInX96" => {
                almcurve::swap_exact_in_x96(&sup_of(&a), a[4], a[5], a[6], !a[7].is_zero(), a[8])
                    .map(|f| words([f.amount_out, f.x_after, f.amount_in_unspent]))
            }
            other => panic!("unknown fn {other}"),
        };
        c.compare(r, got);
    }
    c.finish(&seen);
}

/// `TestFeeEdges`: Solady lnWad (every power-of-two boundary and every top-byte lookup pattern) and
/// the EverlongStrategy fee law through a source-compiled harness, including the reductionG /
/// volMultiplier / fillFee overflow and zero-denominator panics and the tie, ramp and band
/// thresholds.
#[test]
fn fee_edges() {
    let rows = load("edges/fee_edges.json.gz");
    let mut c = Checker::new("fee_edges");
    let mut seen: HashMap<String, usize> = HashMap::new();
    for r in rows.as_array().unwrap() {
        let a = arr(&r["a"]);
        *seen
            .entry(s(r, "f").to_string())
            .or_default() += 1;
        let got = match s(r, "f") {
            // int256 in two's complement, as the harness returned it
            "lnWad" => fee::ln_wad(a[0]).map(|v| words([v])),
            "logRatioAbsWad" => fee::log_ratio_abs_wad(a[0], a[1]).map(|v| words([v])),
            "reductionG" => {
                let st = FeeState {
                    reserve_stable: a[0],
                    reserve_volatile: a[1],
                    anchor_wad: a[2],
                    ..Default::default()
                };
                fee::reduction_g(&st, a[3]).map(|(g, w)| words([g, w]))
            }
            "volMultiplier" => {
                fee::vol_multiplier(&fee_params(&a[..8]), a[8], a[9]).map(|v| words([v]))
            }
            "fillFee" => {
                let st = FeeState {
                    reserve_stable: a[8],
                    reserve_volatile: a[9],
                    anchor_wad: a[10],
                    spot_wad: a[11],
                    rv_wad: a[12],
                };
                fee::fill_fee(&fee_params(&a[..8]), &st, !a[13].is_zero(), a[14], a[15])
                    .map(|f| words([f]))
            }
            other => panic!("unknown fn {other}"),
        };
        c.compare(r, got);
    }
    c.finish(&seen);
}

/// The hook storage and context of an edge row: `a[0..25]` state, `a[25..31]` context.
fn hook_row(a: &[U256]) -> (HookState, SwapContext, U256) {
    assert_eq!(a.len(), 31);
    let st = HookState {
        a_wad: a[0],
        support: Support { a_wad: a[1], x_lo: a[2], x_hi: a[3], y_hi: a[4] },
        anchor_sqrt_x96: a[5],
        reservation_price_wad: a[6],
        kappa: a[7],
        x_wad: a[8],
        reserve_stable: a[9],
        idle_stable: a[10],
        reserve_volatile: a[11],
        idle_volatile: a[12],
        rv_wad: a[13],
        fee: fee_params(&a[14..22]),
        inv_skew_kappa_wad: a[22],
        inv_skew_band_wad: a[23],
        loan_scale: a[24],
    };
    let ctx = SwapContext {
        pool: PoolContext {
            physical_pool_asset: a[25],
            posted_pool_asset: a[26],
            ..Default::default()
        },
        pool_asset_in: !a[27].is_zero(),
        amount_in: a[28],
        max_amount_out: a[29],
        ..Default::default()
    };
    (st, ctx, a[30])
}

fn hook_rows(name: &'static str, path: &str) {
    let rows = load(path);
    let mut c = Checker::new(name);
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut filled = 0;
    for r in rows.as_array().unwrap() {
        let a = arr(&r["a"]);
        let (st, ctx, fee_wad) = hook_row(&a);
        *seen
            .entry(s(r, "f").to_string())
            .or_default() += 1;
        let got = match s(r, "f") {
            "hook.spot" => st.spot().map(|v| words([v])),
            "hook.bookFor" => st
                .book_for(&ctx.pool)
                .map(|b| words([b.kappa, b.rs, b.is, b.rv, b.iv, b.x])),
            "hook.previewFeeWad" => st
                .preview_fee_wad(&ctx)
                .map(|v| words([v])),
            "hook.previewExactIn" => st
                .preview_exact_in(&ctx, fee_wad)
                .map(|fr| words([fr.amount_in_used, fr.gross_out, fr.fee_out, fr.spot_after_wad])),
            "hook.executeExactIn" => st
                .execute_exact_in(&ctx, fee_wad)
                .map(|(fr, post)| {
                    if !fr.amount_in_used.is_zero() {
                        filled += 1;
                    }
                    // the rest of the storage is untouched
                    let mut rest = post;
                    rest.kappa = st.kappa;
                    rest.x_wad = st.x_wad;
                    rest.reserve_stable = st.reserve_stable;
                    rest.idle_stable = st.idle_stable;
                    rest.reserve_volatile = st.reserve_volatile;
                    rest.idle_volatile = st.idle_volatile;
                    assert_eq!(rest, st, "executeExactIn changed non-book fields: {r}");
                    // storage read back in slot order: kappa(16), x(17), rs(18), is(19), rv(20),
                    // iv(21)
                    words([
                        fr.amount_in_used,
                        fr.gross_out,
                        fr.fee_out,
                        fr.spot_after_wad,
                        post.kappa,
                        post.x_wad,
                        post.reserve_stable,
                        post.idle_stable,
                        post.reserve_volatile,
                        post.idle_volatile,
                    ])
                }),
            other => panic!("unknown fn {other}"),
        };
        c.compare(r, got);
    }
    eprintln!("{name}: filled {filled}");
    c.finish(&seen);
}

/// `TestHookEdges`: the live EverlongHook at block 51310000 with its storage overwritten per state
/// (the lazy-rescale branches, retracted and invalid books, invalid `_p.aWad` or support, spot
/// overflow, fee-row edges); caps drawn at 1-wei neighbours of the realised net.
#[test]
fn hook_fill_edges() {
    hook_rows("hook_fill_edges", "edges/hook_fill_edges.json.gz");
}

/// `TestHookEdgesLoanScale`: LOAN_SCALE is immutable on the deployed hook (1e12); these rows patch
/// its PUSH32 sites to 1 and 1e10.
#[test]
fn hook_fill_loan_scale_edges() {
    hook_rows("hook_fill_loan_scale_edges", "edges/hook_fill_loan_scale_edges.json.gz");
}

/// `TestHookEdgesLiveTx`: the real 15000-sat sell (tx `0x46c3cd72...`, block 51302916): the context
/// captured by a calldata tap on a re-driven swap whose committed book equals the transaction's.
#[test]
fn hook_live_tx_edges() {
    hook_rows("hook_live_tx_edges", "edges/hook_live_tx_edges.json.gz");
}
