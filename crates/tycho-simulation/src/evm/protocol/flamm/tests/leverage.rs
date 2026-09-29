// Copyright (c) 2026 Everlong Labs Limited

//! Leverage venue parity: the `CollRebalancerMath` grid and keccak-seeded sweep called on the
//! deployed library on a Base fork at block 51317000 (`lev_curve_fork_fixture`), the
//! Rust<->Solidity leverage-curve parity tape (`lev_curve_tape_v1.tar.gz`, all 22,465 rows over
//! the five row classes, the only fixture that reaches the pro-rata branch), the hook-level
//! fixtures recorded by `testdata/gen/LevRecorder.sol` on the local LevBase stack and on the fork
//! against the deployed `EverlongLeverageHook` (`lev_hook_{local,fork,band}_fixture`), and the
//! curve edge fixture of `LevCurveEdges.t.sol` (`edges/lev_curve_edges`). Every row is replayed.

use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
};

use alloy::primitives::U256;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    super::{
        context::{LeverContext, PoolContext},
        levcurve::{
            anchor_and_base, anchor_best_effort, bezier3, bezier4, cv_required_on_anchor,
            debt_cap_on_anchor, debt_norm_wad, deleverage_pro_rata, deleverage_quote,
            deleverage_spread, frozen_params, half_law_anchor, is_state_safe, lerp_floor,
            leverage_quote, marked_value, phi_wad, post_any_anchor_accepted,
            post_strict_anchor_accepted, recovery_debt_at_y, recovery_deleverage, recovery_state,
            root_interval_contains, strict_anchor, DUST_ANCHOR_FLOOR, LEVERAGE_RATIO_WAD,
            TARGET_SPREAD_CAP_PPM,
        },
        levhook::{assert_anchor_and_band, frame, quote, LevBook},
        math::{mul512, mul_div_floor_raw, product_gt, PPM, WAD},
        FlammError,
    },
    fixtures::{digest_of, read_fixture, read_stored},
};

/// `Panic(uint256)`.
const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A decimal word, as the fixtures carry them: a JSON number (arbitrary precision) or a decimal
/// string.
fn word(v: &Value) -> U256 {
    let s = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => panic!("not a word: {other}"),
    };
    U256::from_str_radix(&s, 10).unwrap_or_else(|e| panic!("word {s:?}: {e}"))
}

fn words(v: &Value) -> Vec<U256> {
    v.as_array()
        .unwrap_or_else(|| panic!("not an array: {v}"))
        .iter()
        .map(word)
        .collect()
}

fn u64_of(v: &Value) -> u64 {
    word(v).try_into().expect("fits u64")
}

/// The revert a fixture recorded as `(length, selector, first argument word)`, mapped to the error
/// the port must return: empty data is `Math.mulDiv`'s bare require, `Panic(0x11)` the
/// checked-arithmetic panic, and a 4-byte selector one of the hook's custom errors.
fn recorded_revert(length: U256, selector: U256, arg: U256) -> FlammError {
    if length.is_zero() {
        return FlammError::MulDivOverflow;
    }
    let s: u64 = selector
        .try_into()
        .expect("selector fits u64");
    let sel = [(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8, s as u8];
    if sel == PANIC_SELECTOR {
        return match u64::try_from(arg).expect("panic code fits u64") {
            0x11 => FlammError::PanicArithmetic,
            0x12 => FlammError::PanicDivZero,
            code => panic!("unmodelled Panic(0x{code:x})"),
        };
    }
    FlammError::from_revert_data(&sel)
        .unwrap_or_else(|| panic!("unmapped revert selector {}", hex::encode(sel)))
}

/// One row of an edge fixture: `[op, [inputs], [status, words...]]`, status 0 carrying the ABI
/// return words verbatim and status 1 the revert as `[len, selector, first argument word]`.
struct EdgeRow {
    op: String,
    inputs: Vec<U256>,
    outs: Vec<U256>,
}

/// Loads an edge fixture (`{"block":N,"rows":[[op,[in],[outs]],...,["end",[],[]]],"count":N}`)
/// and returns its `count` rows (the `end` sentinel dropped).
fn load_edges(rel: &str) -> Vec<EdgeRow> {
    let data = read_fixture(rel);
    let doc: Value = serde_json::from_slice(&data).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let count = u64_of(&doc["count"]) as usize;
    let rows: Vec<EdgeRow> = doc["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            let r = r.as_array().expect("row");
            assert_eq!(r.len(), 3, "row shape");
            EdgeRow {
                op: r[0].as_str().expect("op").to_string(),
                inputs: words(&r[1]),
                outs: words(&r[2]),
            }
        })
        .collect();
    assert_eq!(rows.len(), count + 1, "{rel}: row count");
    assert_eq!(rows[count].op, "end");
    let mut rows = rows;
    rows.truncate(count);
    for r in &rows {
        assert!(!r.outs.is_empty(), "{rel}: a row without a status word");
    }
    rows
}

/// `true`/`false` as the fixtures encode a `bool` return word.
fn flag(b: bool) -> U256 {
    if b {
        U256::from(1)
    } else {
        U256::ZERO
    }
}

/// Fails on the first of at most `limit` mismatches, listing them all.
fn assert_no_mismatches(mismatches: &[String], limit: usize) {
    if mismatches.is_empty() {
        return;
    }
    for m in mismatches.iter().take(limit) {
        eprintln!("{m}");
    }
    panic!("{} mismatches (first {} listed)", mismatches.len(), mismatches.len().min(limit));
}

// ------------------------------------------------------------------ the deployed library on a fork

const FIXTURE: &str = "lev_curve_fork_fixture.json.gz";
const MATH: &str = "0xc002d0731e6A2E6e80bE754779BCef6B01aFF0BB";

/// `testdata/lev_curve_fork_fixture.json.gz`: a `leverageQuote` / `deleverageQuote` /
/// `anchorAndBase` / `isStateSafe` grid plus a keccak-seeded sweep called on the deployed
/// `CollRebalancerMath` bytecode on Base at block 51317000 (`testdata/gen/LevForkFixture.t.sol`),
/// and the port's constants pinned to its `frozenParams()`.
#[test]
fn lev_curve_fork_fixture() {
    let doc: Value = serde_json::from_slice(&read_fixture(FIXTURE)).expect("fixture");
    assert!(
        doc["math"]
            .as_str()
            .expect("math")
            .eq_ignore_ascii_case(MATH),
        "{}",
        doc["math"]
    );

    // LevCurveParams: the 13-word curve, hZero, leverageRatioWad, targetSpreadCapPpm, then the four
    // caller-owned fields the library leaves zero.
    let params = words(&doc["frozenParams"]);
    assert_eq!(params.len(), 20);
    for (i, want) in frozen_params().iter().enumerate() {
        assert_eq!(params[i], *want, "frozenParams word {i}");
    }
    for word in &params[16..] {
        assert!(word.is_zero());
    }

    let stride = u64_of(&doc["stride"]) as usize;
    assert_eq!(stride, 9);
    let v = words(&doc["v"]);
    assert_eq!(v.len() % stride, 0);
    let n = v.len() / stride;
    let mut counts = [0usize; 3];
    let mut filled = [0usize; 2];
    for i in 0..n {
        let r = &v[i * stride..(i + 1) * stride];
        let msg = format!("row {i} {r:?}");
        let kind: u64 = r[0].try_into().expect("kind");
        counts[kind as usize] += 1;
        match kind {
            0 | 1 => {
                let q = if kind == 0 {
                    leverage_quote(r[1], r[2], r[3], LEVERAGE_RATIO_WAD, r[4], r[5])
                } else {
                    deleverage_quote(r[1], r[2], r[3], LEVERAGE_RATIO_WAD, r[4], r[5])
                };
                assert_eq!(r[6], q.out, "{msg}");
                assert_eq!(r[7], q.new_collateral, "{msg}");
                assert_eq!(r[8], q.new_debt, "{msg}");
                if !q.out.is_zero() {
                    filled[kind as usize] += 1;
                }
            }
            2 => {
                let (x_anchor, base_x) = anchor_and_base(r[1], r[2], r[3], LEVERAGE_RATIO_WAD);
                assert_eq!(r[6], x_anchor, "{msg}");
                assert_eq!(r[7], base_x, "{msg}");
                assert_eq!(
                    !r[8].is_zero(),
                    is_state_safe(r[1], r[2], r[3], x_anchor, LEVERAGE_RATIO_WAD),
                    "{msg}"
                );
            }
            _ => panic!("{msg}"),
        }
    }
    eprintln!(
        "block {}: {} leverage ({} filled), {} deleverage ({} filled), {} anchor rows",
        u64_of(&doc["block"]),
        counts[0],
        filled[0],
        counts[1],
        filled[1],
        counts[2]
    );
    assert!(filled[0] > 500);
    assert!(filled[1] > 500);
}

// ------------------------------------------------------------------ the parity tape

const TAPE_ARCHIVE: &str = "lev_curve_tape_v1.tar.gz";
const TAPE_MANIFEST_SHA256: &str =
    "9df07c65ea45563c0f343ed98290df1fb10fa1a3ea870dbc3c9bf51c26a058f6";
const TAPE_STEM: &str = "levcurve_c1_tape_v1";
const TAPE_STRIDE: usize = 13;

// Row layout.
const KIND: usize = 0;
const COLLATERAL: usize = 1;
const DEBT: usize = 2;
const PRICE: usize = 3;
const SPREAD: usize = 4;
const AMOUNT_IN: usize = 5;
const RUST_OUT: usize = 6;
const RUST_NEW_COLLATERAL: usize = 7;
const RUST_NEW_DEBT: usize = 8;
const CLASS: usize = 9;
const ANCHOR: usize = 10;
const OUT_GROSS: usize = 11;
const GROSS_CAPPED: usize = 12;

// Row classes.
const CLS_EXACT: u64 = 0;
const CLS_CROSSING: u64 = 1;
const CLS_DUST_GUARD: u64 = 2;
const CLS_NO_CAPPED_PORTION: u64 = 3;
const CLS_LIVENESS: u64 = 4;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct Classes {
    #[serde(rename = "EXACT")]
    exact: usize,
    #[serde(rename = "PRORATA_CROSSING")]
    crossing: usize,
    #[serde(rename = "PRORATA_DUST_GUARD")]
    dust_guard: usize,
    #[serde(rename = "PRORATA_NO_CAPPED_PORTION")]
    no_capped_portion: usize,
    #[serde(rename = "LIVENESS_EXCLUSION")]
    liveness: usize,
}

#[derive(Deserialize)]
struct ManifestBlock {
    block: String,
    file: String,
    rows: usize,
    sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    schema: String,
    stride: usize,
    rust_sha256: String,
    rows: usize,
    classes: Classes,
    blocks: Vec<ManifestBlock>,
}

#[derive(Deserialize)]
struct Block {
    block: String,
    rows: usize,
    classes: Classes,
    v: Vec<Value>,
}

fn read_tape() -> HashMap<String, Vec<u8>> {
    let raw = read_stored(TAPE_ARCHIVE);
    let gz = flate2::read::GzDecoder::new(raw.as_slice());
    let mut files = HashMap::new();
    for entry in tar::Archive::new(gz)
        .entries()
        .expect("tar")
    {
        let mut entry = entry.expect("tar entry");
        let name = entry
            .path()
            .expect("entry path")
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .expect("entry body");
        files.insert(name, body);
    }
    files
}

fn mul_div_floor(x: U256, y: U256, d: U256) -> U256 {
    mul_div_floor_raw(x, y, d).0
}

/// The Rust twin's `effective_deleverage_spread` cliff the library replaced.
fn cliff_spread(cv: U256, debt: U256, posted: U256) -> U256 {
    if U256::from(2) * debt >= cv && posted > TARGET_SPREAD_CAP_PPM {
        TARGET_SPREAD_CAP_PPM
    } else {
        posted
    }
}

fn marked(collateral: U256, price: U256) -> U256 {
    if price.is_zero() {
        U256::ZERO
    } else {
        mul_div_floor(collateral, price, WAD)
    }
}

fn three(x: U256) -> U256 {
    U256::from(3) * x
}

fn two(x: U256) -> U256 {
    U256::from(2) * x
}

/// The tape's own bookkeeping: `outGross` reproduces the twin's answer at the cliff rate, and the
/// twin's post-state is its inputs moved by its own fill.
fn assert_self_consistent(r: &[U256], msg: &str) {
    let (rust_out, out_gross) = (r[RUST_OUT], r[OUT_GROSS]);
    if !out_gross.is_zero() {
        let mut rate = r[SPREAD];
        if !r[KIND].is_zero() {
            rate = cliff_spread(marked(r[COLLATERAL], r[PRICE]), r[DEBT], rate);
        }
        assert_eq!(
            rust_out,
            mul_div_floor(out_gross, PPM - rate, PPM),
            "tape outGross does not reproduce the Rust answer: {msg}"
        );
        assert!(!rust_out.is_zero(), "{msg}");
    }
    if rust_out.is_zero() {
        assert_eq!(r[COLLATERAL], r[RUST_NEW_COLLATERAL], "{msg}");
        assert_eq!(r[DEBT], r[RUST_NEW_DEBT], "{msg}");
    } else if r[KIND].is_zero() {
        assert_eq!(r[COLLATERAL] + r[AMOUNT_IN], r[RUST_NEW_COLLATERAL], "{msg}");
        assert_eq!(r[DEBT] + rust_out, r[RUST_NEW_DEBT], "{msg}");
    } else {
        assert_eq!(r[COLLATERAL] - rust_out, r[RUST_NEW_COLLATERAL], "{msg}");
        assert_eq!(r[DEBT] - r[AMOUNT_IN], r[RUST_NEW_DEBT], "{msg}");
    }
}

/// What every class must satisfy regardless of its payout: leverage rows, rows at or below the cap
/// and rows off the strict branch are EXACT; the port never fills where the twin declined nor
/// releases more; and the two spellings of "at or below target" agree on the strict branch.
fn assert_structural(r: &[U256], out: U256, msg: &str) {
    let cls: u64 = r[CLASS].try_into().expect("class");
    if r[KIND].is_zero() {
        assert_eq!(cls, CLS_EXACT, "a leverage row diverged: {msg}");
    }
    if r[SPREAD] <= TARGET_SPREAD_CAP_PPM {
        assert_eq!(cls, CLS_EXACT, "a row at or below the spread cap diverged: {msg}");
    }
    if r[ANCHOR].is_zero() {
        assert_eq!(cls, CLS_EXACT, "a row off the strict-anchor branch diverged: {msg}");
    }
    if r[RUST_OUT].is_zero() {
        assert!(out.is_zero(), "filled where the Rust twin declined: {msg}");
    } else {
        assert!(out <= r[RUST_OUT], "released MORE than the Rust twin: {msg}");
    }
    if !r[KIND].is_zero() && !r[ANCHOR].is_zero() {
        let cv = marked(r[COLLATERAL], r[PRICE]);
        let debt = r[DEBT];
        assert_eq!(three(debt) >= r[ANCHOR], two(debt) >= cv, "{msg}");
    }
}

/// `_expectedOut`: the Solidity answer reconstructed per class from the tape's `outGross` /
/// `grossCapped`.
fn expected_out(r: &[U256]) -> U256 {
    let (spread, out_gross, gross_capped) = (r[SPREAD], r[OUT_GROSS], r[GROSS_CAPPED]);
    match r[CLASS].try_into().expect("class") {
        CLS_EXACT => r[RUST_OUT],
        CLS_CROSSING => {
            mul_div_floor(gross_capped, PPM - TARGET_SPREAD_CAP_PPM, PPM) +
                mul_div_floor(out_gross - gross_capped, PPM - spread, PPM)
        }
        CLS_DUST_GUARD | CLS_NO_CAPPED_PORTION => mul_div_floor(out_gross, PPM - spread, PPM),
        _ => U256::ZERO,
    }
}

/// `_assertRow`: the class-specific pin on the payout and the post-state.
fn assert_row(
    r: &[U256],
    out: U256,
    new_collateral: U256,
    new_debt: U256,
    expected: U256,
    msg: &str,
) {
    let cls: u64 = r[CLASS].try_into().expect("class");
    let (collateral, debt, spread) = (r[COLLATERAL], r[DEBT], r[SPREAD]);
    let (anchor, out_gross, gross_capped, rust_out) =
        (r[ANCHOR], r[OUT_GROSS], r[GROSS_CAPPED], r[RUST_OUT]);

    if cls == CLS_EXACT {
        assert_eq!(rust_out, out, "EXACT payout: {msg}");
        assert_eq!(r[RUST_NEW_COLLATERAL], new_collateral, "EXACT collateral: {msg}");
        assert_eq!(r[RUST_NEW_DEBT], new_debt, "EXACT debt: {msg}");
        return;
    }
    assert_eq!(r[KIND], U256::from(1), "{msg}");
    assert!(spread > TARGET_SPREAD_CAP_PPM, "{msg}");
    assert!(!anchor.is_zero(), "{msg}");
    assert!(three(debt) >= anchor, "{msg}");
    assert!(!out_gross.is_zero(), "{msg}");

    match cls {
        CLS_CROSSING => {
            assert!(anchor >= DUST_ANCHOR_FLOOR, "{msg}");
            assert!(!gross_capped.is_zero(), "{msg}");
            assert!(gross_capped < out_gross, "{msg}");
            assert_eq!(expected, out, "crossing payout is not the pro-rata blend: {msg}");
            assert!(out < rust_out, "crossing row reproduces the cliff: {msg}");
        }
        CLS_DUST_GUARD => {
            assert!(anchor < DUST_ANCHOR_FLOOR, "{msg}");
            assert!(gross_capped.is_zero(), "{msg}");
            assert_eq!(expected, out, "dust-guard payout: {msg}");
            assert!(out < rust_out, "{msg}");
            assert!(rust_out - out <= U256::from(2), "{msg}");
        }
        CLS_NO_CAPPED_PORTION => {
            assert!(anchor >= DUST_ANCHOR_FLOOR, "{msg}");
            assert!(gross_capped.is_zero(), "{msg}");
            assert_eq!(expected, out, "no-capped-portion payout: {msg}");
            assert!(out < rust_out, "{msg}");
        }
        CLS_LIVENESS => {
            assert_eq!(out_gross, U256::from(2), "{msg}");
            assert_eq!(gross_capped, U256::from(1), "{msg}");
            assert_eq!(rust_out, U256::from(1), "{msg}");
            assert!(out.is_zero(), "{msg}");
            assert_eq!(collateral, new_collateral, "{msg}");
            assert_eq!(debt, new_debt, "{msg}");
            return;
        }
        _ => panic!("{msg}: tape declares a class this gate has no sanction for"),
    }
    assert_eq!(collateral - out, new_collateral, "{msg}");
    assert_eq!(r[RUST_NEW_DEBT], new_debt, "{msg}");
}

fn replay_rows(v: &[U256], rows: usize) -> Classes {
    let mut tally = Classes::default();
    for i in 0..rows {
        let r = &v[i * TAPE_STRIDE..(i + 1) * TAPE_STRIDE];
        let q = if r[KIND].is_zero() {
            leverage_quote(
                r[COLLATERAL],
                r[DEBT],
                r[PRICE],
                LEVERAGE_RATIO_WAD,
                r[SPREAD],
                r[AMOUNT_IN],
            )
        } else {
            deleverage_quote(
                r[COLLATERAL],
                r[DEBT],
                r[PRICE],
                LEVERAGE_RATIO_WAD,
                r[SPREAD],
                r[AMOUNT_IN],
            )
        };
        let msg = format!("row {i} {r:?}");
        assert_self_consistent(r, &msg);
        assert_structural(r, q.out, &msg);
        assert_row(r, q.out, q.new_collateral, q.new_debt, expected_out(r), &msg);
        match r[CLASS].try_into().expect("class") {
            CLS_EXACT => tally.exact += 1,
            CLS_CROSSING => tally.crossing += 1,
            CLS_DUST_GUARD => tally.dust_guard += 1,
            CLS_NO_CAPPED_PORTION => tally.no_capped_portion += 1,
            CLS_LIVENESS => tally.liveness += 1,
            _ => panic!("{msg}: tape declares a class this gate has no sanction for"),
        }
    }
    tally
}

/// The Rust<->Solidity leverage-curve parity tape (`testdata/lev_curve_tape_v1.tar.gz`, the
/// unmodified c104 tape `test/flamm/lev/tape/levcurve_c1_tape_v1.*.json`), replayed against the
/// port with the assertions of `test/flamm/lev/CollRebalancerMathLevCurveParity.t.sol` @ c104
/// `80abd43`. The tape carries an earlier Rust twin's answer; the expected SOLIDITY answer is
/// reconstructed per row class exactly as the Solidity gate's `_expectedOut` / `_assertRow` do, so
/// every one of the 22,465 rows pins the deployed library's payout wei for wei. All five row
/// classes are replayed, none sampled.
#[test]
fn lev_curve_parity_tape() {
    let files = read_tape();
    let manifest_raw = &files[&format!("{TAPE_STEM}.manifest.json")];
    assert_eq!(
        sha256_hex(manifest_raw),
        TAPE_MANIFEST_SHA256,
        "tape manifest is not the pinned one"
    );

    let m: Manifest = serde_json::from_slice(manifest_raw).expect("manifest");
    assert_eq!(m.schema, "levcurve-c1-parity-v1");
    assert_eq!(m.stride, TAPE_STRIDE);
    assert_eq!(m.rust_sha256, "0x894a710140d9070849665cb63597847d2f2252a5d6962f27e0ad0f3a0a1624fd");
    assert_eq!(
        m.classes,
        Classes {
            exact: 18_876,
            crossing: 3_102,
            dust_guard: 194,
            no_capped_portion: 185,
            liveness: 108
        }
    );
    assert_eq!(m.rows, 22_465);
    assert_eq!(m.blocks.len(), 16);

    let mut total = 0;
    let mut tally = Classes::default();
    for (i, b) in m.blocks.iter().enumerate() {
        let raw = files
            .get(&b.file)
            .unwrap_or_else(|| panic!("block {i} {}: file missing from the archive", b.block));
        assert_eq!(
            format!("0x{}", sha256_hex(raw)),
            b.sha256,
            "block {i} {}: block file does not match the digest the manifest pins",
            b.block
        );
        let blk: Block = serde_json::from_slice(raw).expect("block");
        assert_eq!(b.block, blk.block);
        assert_eq!(blk.v.len(), blk.rows * TAPE_STRIDE);
        assert_eq!(b.rows, blk.rows);
        let v: Vec<U256> = blk.v.iter().map(word).collect();
        let got = replay_rows(&v, blk.rows);
        assert_eq!(blk.classes, got, "block {i} {}: class tally for this block moved", b.block);
        tally.exact += got.exact;
        tally.crossing += got.crossing;
        tally.dust_guard += got.dust_guard;
        tally.no_capped_portion += got.no_capped_portion;
        tally.liveness += got.liveness;
        total += b.rows;
    }
    assert_eq!(m.rows, total);
    assert_eq!(m.classes, tally);
}

// ------------------------------------------------------------------ the hook

const LOCAL_FIXTURE: &str = "lev_hook_local_fixture.json.gz";
const FORK_FIXTURE: &str = "lev_hook_fork_fixture.json.gz";
const BAND_FIXTURE: &str = "lev_hook_band_fixture.json.gz";

/// `LOAN_SCALE` of the cbBTC/USDC pool: L18 per USDC base unit.
const LOAN_SCALE: U256 = U256::from_limbs([1_000_000_000_000, 0, 0, 0]);

struct HookFixture {
    block: u64,
    scenarios: Vec<String>,
    rows: Vec<HashMap<String, U256>>,
}

fn load_hook_fixture(rel: &str) -> HookFixture {
    let doc: Value = serde_json::from_slice(&read_fixture(rel)).expect("fixture");
    let fields: Vec<String> = doc["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|f| f.as_str().expect("field").to_string())
        .collect();
    let stride = u64_of(&doc["stride"]) as usize;
    let n = u64_of(&doc["rows"]) as usize;
    assert_eq!(fields.len(), stride);
    let v = words(&doc["v"]);
    assert_eq!(v.len(), n * stride);
    let rows = (0..n)
        .map(|i| {
            fields
                .iter()
                .enumerate()
                .map(|(j, name)| (name.clone(), v[i * stride + j]))
                .collect()
        })
        .collect();
    let scenarios = doc["scenarios"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    s.as_str()
                        .expect("scenario")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    HookFixture { block: u64_of(&doc["block"]), scenarios, rows }
}

fn set(r: &HashMap<String, U256>, name: &str) -> bool {
    !r[name].is_zero()
}

fn context(r: &HashMap<String, U256>) -> (LeverContext, LevBook) {
    let ctx = LeverContext {
        pool: PoolContext {
            physical_pool_asset: r["ctx.physicalPoolAsset"],
            posted_pool_asset: r["ctx.postedPoolAsset"],
            liquid_loan_asset: r["ctx.liquidLoanAsset"],
            supplied_loan_asset: r["ctx.suppliedLoanAsset"],
            debt_loan_asset: r["ctx.debtLoanAsset"],
            share_supply: r["ctx.shareSupply"],
            price_wad: r["ctx.priceWad"],
            price_ts: r["ctx.priceTs"]
                .try_into()
                .expect("priceTs"),
            loan_count: r["ctx.loanCount"]
                .try_into()
                .expect("loanCount"),
        },
        up: set(r, "ctx.up"),
        spread_ppm: r["ctx.spreadPpm"],
        amount_in: r["ctx.amountIn"],
        max_out: r["ctx.maxOut"],
    };
    let book = LevBook {
        rs: r["book.rs"],
        is: r["book.is"],
        rv: r["book.rv"],
        iv: r["book.iv"],
        reservation_price_wad: r["reservationPriceWad"],
    };
    (ctx, book)
}

fn div_ceil(x: U256, y: U256) -> U256 {
    let (q, r) = x.div_rem(y);
    if r.is_zero() {
        q
    } else {
        q + U256::from(1)
    }
}

/// Replays every captured row and returns the number of (fills, reverts, uncaptured) rows. A row
/// the generator could not capture carries no context to compare, so it is skipped; the count is
/// returned because `fills + reverts` is the compared total and is smaller than `rows.len()`.
fn check_hook_rows(fx: &HookFixture) -> (usize, usize, usize) {
    let (mut fills, mut reverts, mut skipped) = (0, 0, 0);
    for (i, r) in fx.rows.iter().enumerate() {
        if !set(r, "captured") {
            skipped += 1;
            continue;
        }
        let scenario = fx
            .scenarios
            .get(usize::try_from(r["scenario"]).expect("scenario"))
            .cloned()
            .unwrap_or_default();
        let msg = format!(
            "row {i} scenario {scenario:?} up={} amount={} override={}",
            set(r, "up"),
            r["amount"],
            r["hookAmountOverride"]
        );
        let (ctx, book) = context(r);

        let f = frame(&ctx.pool, &book);
        if set(r, "frameOk") {
            let f = f.unwrap_or_else(|e| panic!("frame reverted {e}: {msg}"));
            assert_eq!(r["frame.cv"], f.cv, "frame.cv {msg}");
            assert_eq!(r["frame.d"], f.d.into_raw(), "frame.d {msg}");
            assert_eq!(r["frame.v"], f.v, "frame.v {msg}");
            assert_eq!(r["frame.s"], f.s, "frame.s {msg}");
            assert_eq!(r["frame.xAnchor"], f.x_anchor, "frame.xAnchor {msg}");
        } else {
            assert!(f.is_err(), "frame did not revert: {msg}");
        }

        let fill = quote(&ctx, &book);
        if set(r, "hookOk") {
            let fill = fill.unwrap_or_else(|e| panic!("quote reverted {e}: {msg}"));
            assert_eq!(r["hook.amountInUsed"], fill.amount_in_used, "amountInUsed {msg}");
            assert_eq!(r["hook.grossOut"], fill.gross_out, "grossOut {msg}");
            assert_eq!(r["hook.virtualLegL18"], fill.virtual_leg_l18, "virtualLegL18 {msg}");
            assert_eq!(r["hook.crAfterWad"], fill.cr_after_wad, "crAfterWad {msg}");
            fills += 1;
        } else {
            let want =
                recorded_revert(r["hook.revertLen"], r["hook.revertSelector"], r["hook.revertArg"]);
            assert_eq!(fill, Err(want), "revert {want}: {msg}");
            reverts += 1;
        }

        // Where core accepted the fill, its payout is the hook's fill netted on the loan grid: this
        // pins that the captured context is the one core priced (core's own gates are not
        // modelled here).
        if set(r, "poolOk") && set(r, "hookOk") && r["hookAmountOverride"].is_zero() {
            let fill = fill.expect("checked above");
            assert_eq!(r["pool.spreadPpm"], ctx.spread_ppm, "{msg}");
            assert_eq!(r["pool.crAfterWad"], fill.cr_after_wad, "{msg}");
            if ctx.up {
                assert_eq!(r["pool.amountInUsed"], fill.amount_in_used, "{msg}");
                let net = (fill.gross_out - fill.virtual_leg_l18) / LOAN_SCALE;
                assert_eq!(r["pool.amountOut"], net, "{msg}");
            } else {
                assert_eq!(r["pool.amountOut"], fill.gross_out, "{msg}");
                let pay = div_ceil(fill.amount_in_used - fill.virtual_leg_l18, LOAN_SCALE);
                assert_eq!(r["pool.amountInUsed"], pay.min(r["amount"]), "{msg}");
            }
        }
    }
    (fills, reverts, skipped)
}

#[test]
fn lev_hook_local_fixture() {
    let fx = load_hook_fixture(LOCAL_FIXTURE);
    let (fills, reverts, skipped) = check_hook_rows(&fx);
    eprintln!("{} rows: {fills} fills, {reverts} reverts, {skipped} uncaptured", fx.rows.len());
    assert_eq!(fills + reverts + skipped, fx.rows.len());
    // The local generator captures every row it emits.
    assert_eq!(skipped, 0);
    assert!(fills > 50);
    assert!(reverts > 10);
}

/// `test/flamm/lev/VenueGolden.t.sol`'s bit-exact sequence: rows 0-3 of the local fixture are the
/// pre-step states of leverUp 0.05 BTC, leverDown out1/2, leverUp 0.05 BTC, leverDown
/// (out1 - out1/2 + out3) / 4 and row 4 the post-sequence probe. The frame must land on every
/// pinned CV/D and the fill, netted the way core settles it, on every pinned payout.
#[test]
fn lev_venue_golden() {
    let fx = load_hook_fixture(LOCAL_FIXTURE);
    let u = |s: &str| U256::from_str_radix(s, 10).expect("decimal");
    let cv = [
        u("1999999999999999997898758"),
        u("2009999999999999997888251"),
        u("2007538233999999997890838"),
        u("2017538233999999997880332"),
        u("2015692939999999997882270"),
    ];
    let d = [
        u("999999999999999997898758"),
        u("1009937593495999997888251"),
        u("1007468796959999997890838"),
        u("1017388072002999997880332"),
        u("1015541054419999997882270"),
    ];
    let outs: [u64; 4] = [4_937_593_496, 1_230_883, 4_919_275_043, 922_647];

    let (mut out1, mut out3) = (0u64, 0u64);
    for k in 0..5 {
        let r = &fx.rows[k];
        assert!(r["scenario"].is_zero());
        let (ctx, book) = context(r);
        let f = frame(&ctx.pool, &book).expect("frame");
        assert_eq!(cv[k], f.cv, "cv{k}");
        assert_eq!(d[k], f.d.into_raw(), "d{k}");
        if k == 4 {
            break;
        }
        let amount: u64 = r["amount"].try_into().expect("amount");
        match k {
            0 | 2 => assert_eq!(5_000_000, amount),
            1 => assert_eq!(out1 / 2, amount),
            _ => assert_eq!((out1 - out1 / 2 + out3) / 4, amount),
        }
        let fill = quote(&ctx, &book).expect("fill");
        let out: u64 = if ctx.up {
            ((fill.gross_out - fill.virtual_leg_l18) / LOAN_SCALE)
                .try_into()
                .expect("out")
        } else {
            fill.gross_out.try_into().expect("out")
        };
        assert_eq!(outs[k], out, "out{}", k + 1);
        match k {
            0 => out1 = out,
            2 => out3 = out,
            _ => {}
        }
    }
}

/// Every generated lev fixture pinned to the digest recorded in `testdata/README.md`.
#[test]
fn lev_fixture_digests() {
    for (rel, want) in [
        (
            "lev_curve_fork_fixture.json.gz",
            "798735119ee4e322ec929a75aa48d8855e630f622fc20aa4d3a27a54c30d4e9f",
        ),
        (FORK_FIXTURE, "f5d027dc34dbc37289edbf91312d5adf67217bef1a94e02883bee49319580641"),
        (LOCAL_FIXTURE, "6e8b8d178b2f07c24aa0b4b94021b48a44f50455829786f11f07b92dbeabd59f"),
        (BAND_FIXTURE, "52e04fdf28c6224faa48e1a4cf581be3d8d070b5a0a53b65c47cf4651d2cb90d"),
        (TAPE_ARCHIVE, "18fe3e2aa02cce91f8b312f95730b2ef556363e271e82dbbc12d1d107ce29437"),
        (CURVE_FIXTURE, "12ac1f76ce0eb7b9d03d8440a738b23a18abb531b19866321db12eb01ca66cd4"),
    ] {
        assert_eq!(digest_of(rel), want, "{rel}");
        assert_eq!(sha256_hex(&read_stored(rel)), want, "{rel}");
    }
}

#[test]
fn lev_hook_fork_fixture() {
    let fx = load_hook_fixture(FORK_FIXTURE);
    assert_eq!(fx.block, 51_317_000);
    let (fills, reverts, skipped) = check_hook_rows(&fx);
    eprintln!("{} rows: {fills} fills, {reverts} reverts, {skipped} uncaptured", fx.rows.len());
    assert_eq!(fills + reverts + skipped, fx.rows.len());
    // 19 of the fork fixture's 402 rows are states the generator could not capture on the fork;
    // pinned so a regeneration that stops capturing rows is noticed rather than silently
    // shrinking the comparison.
    assert_eq!(skipped, 19);
    assert!(fills > 150);
    assert!(reverts > 100);
}

/// `_assertAnchorAndBand`, exposed by a harness over the compiled hook
/// (`testdata/gen/LevGoldenFixture.t.sol`), across dust and in-domain states with the pre-anchor
/// straddling the post-state's own anchor.
#[test]
fn lev_assert_anchor_and_band_fixture() {
    let doc: Value = serde_json::from_slice(&read_fixture(BAND_FIXTURE)).expect("fixture");
    let stride = u64_of(&doc["stride"]) as usize;
    assert_eq!(stride, 6);
    let v = words(&doc["v"]);
    let mut drops = 0;
    for i in 0..v.len() / stride {
        let r = &v[i * stride..(i + 1) * stride];
        let msg = format!("row {i} {r:?}");
        let got = assert_anchor_and_band(r[0], r[1], r[2]);
        if !r[3].is_zero() {
            assert_eq!(got, Ok(()), "{msg}");
            continue;
        }
        let want = recorded_revert(r[4], r[5], U256::ZERO);
        assert_eq!(got, Err(want), "revert {want}: {msg}");
        if want == FlammError::FillValueDrop {
            drops += 1;
        }
    }
    assert!(drops > 100);
}

// ------------------------------------------------------------------ curve edges

const CURVE_FIXTURE: &str = "edges/lev_curve_edges.json.gz";

/// Evaluates one curve op and returns its ABI words; `internal` reports whether the op is a private
/// library function (whose out-of-domain Solidity reverts are not reachable from any entrypoint).
fn curve_eval(op: &str, i: &[U256]) -> Option<(Vec<U256>, bool)> {
    let words = match op {
        "fp" => {
            let mut w = frozen_params().to_vec();
            w.extend([U256::ZERO; 4]);
            return Some((w, false));
        }
        "ab" => {
            let (x, b) = anchor_and_base(i[0], i[1], i[2], i[3]);
            return Some((vec![x, b], false));
        }
        "lq" => {
            let q = leverage_quote(i[0], i[1], i[2], i[3], i[4], i[5]);
            return Some((vec![q.out, q.new_collateral, q.new_debt], false));
        }
        "dq" => {
            let q = deleverage_quote(i[0], i[1], i[2], i[3], i[4], i[5]);
            return Some((vec![q.out, q.new_collateral, q.new_debt], false));
        }
        "ss" => return Some((vec![flag(is_state_safe(i[0], i[1], i[2], i[3], i[4]))], false)),
        "mv" => {
            let (ok, cv) = marked_value(i[0], i[1]);
            vec![flag(ok), cv]
        }
        "sa" => {
            let a = strict_anchor(i[0], i[1], i[2]);
            vec![flag(a.ok), a.anchor, a.h, flag(a.half_law)]
        }
        "hla" => vec![half_law_anchor(i[0], i[1])],
        "ric" => vec![flag(root_interval_contains(i[0], i[1], i[2]))],
        "cvr" => {
            let (ok, cv) = cv_required_on_anchor(i[0], i[1]);
            vec![flag(ok), cv]
        }
        "dcap" => {
            let (ok, d) = debt_cap_on_anchor(i[0], i[1]);
            vec![flag(ok), d]
        }
        "rs" => {
            let r = recovery_state(i[0], i[1], i[2]);
            if !r.ok {
                vec![flag(false)]
            } else {
                vec![flag(true), r.anchor, r.base_x, r.y, r.wall_cv, r.wall_debt, r.stable_to_wall]
            }
        }
        "rdy" => vec![recovery_debt_at_y(i[0], i[1])],
        "dsp" => vec![deleverage_spread(i[0], i[1], i[2])],
        "pr" => {
            let (o, s) = deleverage_pro_rata(i[0], i[1], i[2], i[3], i[4], i[5], i[6]);
            vec![o, s]
        }
        "rdl" => {
            let q = recovery_deleverage(i[0], i[1], i[2], i[3], i[4], i[5], i[6]);
            vec![q.out, q.new_collateral, q.new_debt]
        }
        "psa" => vec![flag(post_strict_anchor_accepted(i[0], i[1], i[2], i[3], i[4], i[5]))],
        "paa" => vec![flag(post_any_anchor_accepted(i[0], i[1], i[2], i[3], i[4], i[5]))],
        "abe" => vec![anchor_best_effort(i[0], i[1], i[2])],
        "phi" => vec![phi_wad(i[0])],
        "dn" => vec![debt_norm_wad(i[0])],
        "lerp" => vec![lerp_floor(i[0], i[1], i[2])],
        "b3" => vec![bezier3(i[0])],
        "b4" => vec![bezier4(i[0])],
        "m512" => {
            let (hi, lo) = mul512(i[0], i[1]);
            vec![hi, lo]
        }
        "pgt" => vec![flag(product_gt(i[0], i[1], i[2], i[3]))],
        _ => return None,
    };
    Some((words, true))
}

/// `edges/lev_curve_edges.json.gz` (`testdata/gen/LevCurveEdges.t.sol` on a Base fork at block
/// 51318000): a row is `[op, inputs, [status, words...]]`, status 0 carrying the ABI return words
/// verbatim, status 1 the revert as `[len, selector, first argument word]`. Public curve ops ran
/// on the deployed `CollRebalancerMath` and were asserted equal (in Solidity) to a verbatim
/// internal-visibility copy that produced the private-helper rows.
#[test]
fn lev_curve_edges() {
    let rows = load_edges(CURVE_FIXTURE);
    assert_eq!(rows.len(), 77_002);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut skipped = 0;
    let mut mismatches = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let (want, internal) = curve_eval(&r.op, &r.inputs)
            .unwrap_or_else(|| panic!("row {i}: unknown op {:?}", r.op));
        if !r.outs[0].is_zero() {
            // Solidity reverted: only allowed for private helpers driven out of their callers'
            // domain.
            assert!(internal, "row {i}: public op {} reverted in Solidity", r.op);
            skipped += 1;
            continue;
        }
        let mut got = &r.outs[1..];
        if r.op == "rs" && got[0].is_zero() {
            got = &got[..1];
        }
        if got.len() != want.len() {
            mismatches.push(format!(
                "row {i} {} in={:?}: word count sol={} rust={}",
                r.op,
                r.inputs,
                got.len(),
                want.len()
            ));
            continue;
        }
        if let Some(k) = (0..want.len()).find(|k| got[*k] != want[*k]) {
            mismatches.push(format!(
                "row {i} {} in={:?} word {k}: sol={} rust={}",
                r.op, r.inputs, got[k], want[k]
            ));
            continue;
        }
        *counts.entry(r.op.clone()).or_default() += 1;
    }
    eprintln!(
        "compared {} of {} rows ({counts:?}), {skipped} internal out-of-domain reverts skipped",
        rows.len() - skipped,
        rows.len()
    );
    assert_no_mismatches(&mismatches, 50);
}
