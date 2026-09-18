// Copyright (c) 2026 Everlong Labs Limited

//! Fixture support shared by the parity replays: the Solidity-generated fixtures of the Go port
//! (`testdata/`, provenance and generators in `testdata/README.md`), read relative to the crate,
//! gunzipped by suffix and checked against their pinned sha256 before a single row is replayed.
//! Every row of every fixture is reproduced to the wei and by revert class, with no sampling and
//! no tolerance.

use std::{
    fmt::Debug,
    fs,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
};

use alloy::primitives::U256;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::FlammError;

/// The module's `testdata/` directory.
pub fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/evm/protocol/flamm/testdata")
}

/// The sha256 of every stored fixture (`testdata/README.md`): a fixture that is not the pinned one
/// fails before any row is replayed.
pub const DIGESTS: &[(&str, &str)] = &[
    ("alm_curve_grid.json.gz", "04b679fb9a16062be73e3af140d65628b88a7b01e13c963189ff79af12c16373"),
    (
        "core_e2e_grid_51302915.jsonl.gz",
        "ae588fcaf6e08b24b5d7e7b3f12c491b89c73f9e155ceb8641090c5a6004ddd5",
    ),
    (
        "core_e2e_grid_51313000.jsonl.gz",
        "4e21a865486e59b4fc0eae66290ed509fc1afe88254c558b67cad6b7e62258ee",
    ),
    (
        "core_e2e_grid_51324800.jsonl.gz",
        "2d053f484f935e3e715c8f3e8655ef6a32f08c4255a6d8d8d2622f0d303130b1",
    ),
    (
        "core_e2e_seq_51302915.jsonl.gz",
        "9ae2f16137c0a744dd730758ad97537bb8533ce6a0868367d1113ff7c35c1116",
    ),
    (
        "core_e2e_seq_51313000.jsonl.gz",
        "e2d4569b540b7853b49e04507e7ba485439aae3ebb26621c28352b411a92536f",
    ),
    (
        "core_e2e_seq_51324800.jsonl.gz",
        "6529b264e751e5026344539a039b45bb90542d56b5f501d5ed3f3f5c05f64d45",
    ),
    ("fee_fill_grid.json.gz", "2d704853e4f3c88784646aa8fac663dabb4b1f793ee32f3386b9a4b1046fc5e3"),
    ("gate_int_edges.json.gz", "2cf610563f5c38e742ca243b77f9d7c254e5949cad35ae068e33b4d7c0ec123b"),
    ("gate_math.json.gz", "3d2c978672c08d67df2310e34fb9eb936f09562882cfdb366f1d70ca5f14fe3c"),
    ("hook_fill_grid.json.gz", "ecd74e6003afd5cabf5b2a1c73df9e014f7cc5b91dc1c0cfb984767cbbe2b05b"),
    ("hook_live_swap.json.gz", "93124624cfc488cd3fb0a14508405a637258b103f7fd3318dcaa4fac9985dc2f"),
    (
        "lev_curve_fork_fixture.json.gz",
        "798735119ee4e322ec929a75aa48d8855e630f622fc20aa4d3a27a54c30d4e9f",
    ),
    (
        "lev_curve_tape_v1.tar.gz",
        "18fe3e2aa02cce91f8b312f95730b2ef556363e271e82dbbc12d1d107ce29437",
    ),
    (
        "lev_hook_band_fixture.json.gz",
        "52e04fdf28c6224faa48e1a4cf581be3d8d070b5a0a53b65c47cf4651d2cb90d",
    ),
    (
        "lev_hook_fork_fixture.json.gz",
        "f5d027dc34dbc37289edbf91312d5adf67217bef1a94e02883bee49319580641",
    ),
    (
        "lev_hook_local_fixture.json.gz",
        "6e8b8d178b2f07c24aa0b4b94021b48a44f50455829786f11f07b92dbeabd59f",
    ),
    ("mm_irm_grid.json.gz", "968fdfad25549694a4376a665633cec0f1e9b3cfd6f9301f21c66da98a1ec9c1"),
    ("mm_live_settle.json.gz", "870b1a9fc3b09ab84025292874d9f304ded40ba3c87626d2ec5b703ada3f7f9c"),
    ("mm_live_views.json.gz", "14478ba371bc55b0df5ac5d4427c8e35d66e487796825a7679811ad81c107488"),
    ("mm_muldiv_edges.json.gz", "29f59dd8c4989eaedbc63a4fc5817c9de8e79b548e270b1811f67f22ca050c58"),
    ("mm_multi_venue.json.gz", "46b96f01244f9a8380918554adc9e662c00f8ca7d7520e8d75b216a32da4c42b"),
    ("mm_real_sell.json.gz", "d6d6e3ff5e9c84eaab525f570ae31a1c6aff46d9b331e96b93731bfd33330b64"),
    (
        "edges/alm_curve_edges.json.gz",
        "3636618f5913e6aaf430b392736ac18f0e09c056b81b2b95e780530ee098a12b",
    ),
    (
        "edges/core_edge_grid_51302915.jsonl.gz",
        "946d384674b004378198eb5fd023d28fea944ebcf6d626c34994561149d6eba2",
    ),
    (
        "edges/core_edge_grid_51324800.jsonl.gz",
        "8c085a8183d9de9a6edbe7fdab5d08e276141a598739bd4a524baf814fe8f570",
    ),
    (
        "edges/core_edge_grid_51326000.jsonl.gz",
        "463868de0f612bdb55be5cad602a5911b6a9d08b3f2904cb578c42f9edc7d3b4",
    ),
    (
        "edges/core_edge_seq_51302915.jsonl.gz",
        "e6e14db48bcfe28a3f87ff072a5cc2ac70e92e4a27d669c39d3dec9338fca99c",
    ),
    (
        "edges/core_edge_seq_51324800.jsonl.gz",
        "53414398f523e1507ed8fdd2c2e4f91642f02dba22a195068af4a5ac948b553d",
    ),
    (
        "edges/core_edge_seq_51326000.jsonl.gz",
        "1906c0dfc12c37b6f3d7dd5b52ffee245cf1107b6aa5da43ea29dc0773ab341e",
    ),
    ("edges/fee_edges.json.gz", "7959030d1ad231297c9690481882c6129d988454340bbeca27a0454c81d84253"),
    (
        "edges/gate_edges.json.gz",
        "08b7042989376e9a143f9561bd4bf718ae5306351b1d4e0eabc866ececa3407b",
    ),
    (
        "edges/hook_fill_edges.json.gz",
        "408011fc26d01bd6665dcdbe557419034dbcdfdbac59ad6a890a6279e4220b3b",
    ),
    (
        "edges/hook_fill_loan_scale_edges.json.gz",
        "8850d02792690121ecebb898088727fdc6d514dc122d98028e182d151add38ac",
    ),
    (
        "edges/hook_live_tx_edges.json.gz",
        "70562e9bb36f73fd388129f8a938e2189bda2fd205003486c94a61653809a271",
    ),
    (
        "edges/lev_curve_edges.json.gz",
        "12ac1f76ce0eb7b9d03d8440a738b23a18abb531b19866321db12eb01ca66cd4",
    ),
    (
        "edges/mm_account_edges.json.gz",
        "36f62065833adc4ac9010cd47b67201ee08cb90bfc42fd9afde6ee2a01a5e945",
    ),
    (
        "edges/mm_accrual_grid.jsonl.gz",
        "25b2bc16424dd45f1165bec5ba5b6cf668d55b9674ecdb6d11fa251ebc771590",
    ),
    (
        "edges/mm_irm_edges.json.gz",
        "f89edd49854ef05173d17b0e8e3db076cb2da2565fe058b5b73221b3fd7ad5bc",
    ),
    (
        "edges/mm_market_edges.json.gz",
        "ae283f592a151ace394b26b10b969d732dfa6697be55953b21d8f4e6ab2fdb0d",
    ),
    (
        "edges/mm_morpho_edges.json.gz",
        "e44750b12fb7f2e56374a8d3f5afb511df2aaa8c68b3ee2d2fd7468925222847",
    ),
    (
        "edges/mm_router_edges.json.gz",
        "a75ca51f72abd6742e623e741a12379faa8dfec2894b297ae3859939a3efef74",
    ),
    (
        "edges/mm_settle_edges.json.gz",
        "bf1aa976d3ab281f6cc64c1f264539bcde480a02a974c3eab70c2fc7fb07b2b1",
    ),
    (
        "edges/router_sequence_a.jsonl.gz",
        "4bb4ffac4e4b3cbad94c5114c9f1c0c65b971892501155bcab38ad1cf277b55d",
    ),
    (
        "edges/router_sequence_b.jsonl.gz",
        "3adc3e50d26f41a498307bfec81b4735d70c2238ce7575d6e2f737acbce0b646",
    ),
    (
        "edges/router_sequence_liquidation.jsonl.gz",
        "2399691adf0b8116dc8adbb4b9caf06490f37577ff96a8d26b085835ec625a54",
    ),
    (
        "edges/router_settlement_edges_one_loan.json.gz",
        "565816ed7ce9c910d51381025c15e583228e032f8325ceadd87794491b963069",
    ),
    (
        "edges/router_settlement_edges_two_loans.json.gz",
        "aad4bfb10e2cd5b5cf6e754dd918328d795d62813ef38cb016d5b5ea1412f266",
    ),
    (
        "edges/swap_settlement_sequence_a.jsonl.gz",
        "8b1d5d0c0a144097078502c5d74f4b7cd07e96b6e21ff1567c62b0b346826cd8",
    ),
    (
        "edges/swap_settlement_sequence_b.jsonl.gz",
        "1e1353a7a804097e97ec648a5eec328b12386418a96552f1d560bae7124491ed",
    ),
    (
        "edges/swap_settlement_sequence_c.jsonl.gz",
        "6f4fe3b95b44bd336080f380dc7b7eee45bcb7e87e08a3ea89dc64483daa1740",
    ),
    (
        "edges/swap_settlement_sequence_d.jsonl.gz",
        "04eff241349eb5f1da08d98af84e8172450db92969ae4f56a88458b646a8d0c7",
    ),
    (
        "edges/swap_settlement_sequence_e.jsonl.gz",
        "efaeb37faac834f3a422b1d12252e9a3fd8c84983a6672214c9b3c21bef094e2",
    ),
    // The deployed pool's component snapshots and preview grids (README section 5).
    (
        "snapshots/51302915.json.gz",
        "04b6aaf95785fcbe31b8dd2d90826c45253142a72a0bc26b65becb9e5f295cd5",
    ),
    (
        "snapshots/51313000.json.gz",
        "e001dea4effe6372872d7896e2b0cf522af8cf67f990d29c43e23514f757a970",
    ),
    (
        "snapshots/51409000.json.gz",
        "2e3d7e26a6a0385cc17ff2bf2d56ecb62777f2ff634fa72825a1c3fbb2a7cbd5",
    ),
    (
        "snapshots/swap_51302916_delta.json.gz",
        "f667cbfa9e24a8e48015b30e3969c767dabde9968972d54e6c45789d3ba91c98",
    ),
    // The substreams' stream and the chain's answers along it (README section 6).
    (
        "snapshots/e2e_stream.json.gz",
        "06b482655e2b24d600494d617999cf84c50ec2682e4661d7ee97ea8bfe2f43e0",
    ),
    (
        "snapshots/e2e_grids.json.gz",
        "e6636e338627c8569d606b29da6a3b901c1b104057a5b790e87b66aa6c61d0c3",
    ),
];

/// The pinned digest of a fixture.
pub fn digest_of(name: &str) -> &'static str {
    DIGESTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| *d)
        .unwrap_or_else(|| panic!("{name}: no pinned digest"))
}

/// The stored bytes of a fixture, checked against the pinned digest.
pub fn read_stored(name: &str) -> Vec<u8> {
    let path = testdata().join(name);
    let raw = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let got = hex::encode(Sha256::digest(&raw));
    assert_eq!(digest_of(name), got, "{name} is not the pinned fixture");
    raw
}

/// Reads a fixture, gunzipping `*.gz`; a JSON fixture has its wide numbers quoted
/// ([`quote_wide_numbers`]).
pub fn read_fixture(name: &str) -> Vec<u8> {
    let raw = read_stored(name);
    let text = if name.ends_with(".gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .unwrap_or_else(|e| panic!("{name}: gunzip: {e}"));
        out
    } else {
        raw
    };
    if name.contains(".json") {
        quote_wide_numbers(&text)
    } else {
        text
    }
}

/// Wraps every bare JSON number of sixteen or more digits in quotes, so a 256-bit word the edge
/// generators wrote as a number survives `serde_json` (which narrows a wide number to `f64`)
/// and reads back as the decimal string the other fixtures use; a number of fifteen digits or
/// fewer is exact as an integer and is left as it is, so typed integer fields (block numbers,
/// timestamps, indices, counts) still decode as numbers. Strings, escapes included, are left
/// untouched.
pub fn quote_wide_numbers(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + text.len() / 16);
    let mut i = 0;
    let mut in_string = false;
    while i < text.len() {
        let c = text[i];
        if in_string {
            out.push(c);
            if c == b'\\' {
                if let Some(&n) = text.get(i + 1) {
                    out.push(n);
                    i += 1;
                }
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'-' || c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < text.len() &&
                (text[i].is_ascii_digit() || matches!(text[i], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                i += 1;
            }
            let token = &text[start..i];
            let digits = token
                .iter()
                .filter(|b| b.is_ascii_digit())
                .count();
            if digits >= 16 {
                out.push(b'"');
                out.extend_from_slice(token);
                out.push(b'"');
            } else {
                out.extend_from_slice(token);
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// A JSON fixture as a `serde_json::Value`.
pub fn load(name: &str) -> Value {
    let text = read_fixture(name);
    serde_json::from_slice(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The non-empty lines of a JSONL fixture.
pub fn fixture_lines(name: &str) -> Vec<String> {
    let bytes = read_fixture(name);
    BufReader::new(&bytes[..])
        .lines()
        .map(|l| l.expect("line"))
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Parses a decimal fixture field.
pub fn u(v: &Value) -> U256 {
    let s = v
        .as_str()
        .unwrap_or_else(|| panic!("not a string: {v}"));
    U256::from_str_radix(s, 10).unwrap_or_else(|e| panic!("{s}: {e}"))
}

/// A decimal field of an object.
pub fn f(row: &Value, key: &str) -> U256 {
    u(row
        .get(key)
        .unwrap_or_else(|| panic!("missing {key} in {row}")))
}

/// A string field of an object.
pub fn s<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing {key} in {row}"))
}

/// A boolean field of an object.
pub fn b(row: &Value, key: &str) -> bool {
    row.get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("missing {key} in {row}"))
}

/// An array of decimal fields.
pub fn arr(v: &Value) -> Vec<U256> {
    v.as_array()
        .unwrap_or_else(|| panic!("not an array: {v}"))
        .iter()
        .map(u)
        .collect()
}

/// Decimal rendering of a word, as the fixtures store them.
pub fn dec(v: U256) -> String {
    v.to_string()
}

/// Maps recorded revert data onto the port's error: empty data is the bare `Math.mulDiv` revert, a
/// `Panic(uint256)` word one of the panics, a 4-byte word a custom-error selector.
pub fn want_err(err_hex: &str) -> FlammError {
    let data =
        hex::decode(err_hex.trim_start_matches("0x")).unwrap_or_else(|e| panic!("{err_hex}: {e}"));
    FlammError::from_revert_data(&data).unwrap_or_else(|| panic!("unmapped revert data {err_hex}"))
}

/// Asserts the port refused exactly where the contract reverted, or answered where it answered;
/// returns the answer in the latter case.
pub fn expect<T: Debug>(
    ok: bool,
    err_hex: &str,
    got: Result<T, FlammError>,
    ctx: &dyn Debug,
) -> Option<T> {
    if ok {
        match got {
            Ok(v) => Some(v),
            Err(e) => panic!("solidity ok, port refused {e}: {ctx:?}"),
        }
    } else {
        let want = want_err(err_hex);
        match got {
            Err(e) if e == want => None,
            other => {
                panic!("solidity reverted {want} ({err_hex}), port returned {other:?}: {ctx:?}")
            }
        }
    }
}

#[test]
fn wide_numbers_are_quoted_and_narrow_ones_kept() {
    let text = br#"{"a": 123456789012345678901234567890, "b": -12345678901234567890, "c": 51302915,
      "d": "1234567890123456789 \" -", "e": [1e+38, 999999999999999, 9999999999999999]}"#;
    let got = String::from_utf8(quote_wide_numbers(text)).unwrap();
    assert_eq!(
        got,
        r#"{"a": "123456789012345678901234567890", "b": "-12345678901234567890", "c": 51302915,
      "d": "1234567890123456789 \" -", "e": [1e+38, 999999999999999, "9999999999999999"]}"#
    );
    let v: Value = serde_json::from_str(&got).unwrap();
    assert_eq!(
        u(&v["a"]),
        "123456789012345678901234567890"
            .parse::<U256>()
            .unwrap()
    );
    assert_eq!(v["c"].as_u64(), Some(51_302_915));
}

/// Every committed fixture is present, gzipped and the pinned one.
#[test]
fn every_fixture_is_pinned_and_present() {
    let mut on_disk = Vec::new();
    for sub in ["", "edges", "snapshots"] {
        let dir = testdata().join(sub);
        for e in fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                continue;
            }
            let name = e.file_name().into_string().unwrap();
            if name == "README.md" {
                continue;
            }
            assert!(name.ends_with(".gz"), "{name}: fixtures are stored gzipped");
            on_disk.push(if sub.is_empty() { name } else { format!("{sub}/{name}") });
        }
    }
    on_disk.sort();
    let mut pinned: Vec<String> = DIGESTS
        .iter()
        .map(|(n, _)| n.to_string())
        .collect();
    pinned.sort();
    assert_eq!(on_disk, pinned);
    for (name, _) in DIGESTS {
        read_stored(name);
    }
}
