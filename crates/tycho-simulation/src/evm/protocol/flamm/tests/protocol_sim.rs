// Copyright (c) 2026 Everlong Labs Limited

//! The `ProtocolSim` over the deployed pool's own snapshots (`testdata/snapshots/`, provenance in
//! `testdata/README.md` section 5): the component snapshots of the swap and lever-up venues at
//! three pinned Base blocks, decoded through `TryFromWithBlock` and quoted against the pool's
//! `previewSwap` / `previewLever` answers recorded by `eth_call` at the same blocks, the largest
//! fully consumed size per direction against the chain's own bisection (and the sizes the
//! protocol test harness derives from it), the spot price in the trait's definition against the
//! hook's recorded spot, the real swap of block 51302916 against its receipt and its storage
//! diffs, the required words, and the clock, delta, clone and equality semantics of the state.

use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{Address, U256};
use num_bigint::BigUint;
use serde_json::Value;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{
    dto::{ProtocolComponent, ProtocolStateDelta, ResponseProtocolState},
    models::{token::Token, Chain},
    simulation::{
        errors::SimulationError,
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, Price, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use super::{
    core::common::{build_state, decode_reads, revert_class},
    fixtures::{fixture_lines, load, s},
};
use crate::{
    evm::protocol::{
        flamm::{
            decoder::{lever_up_id, swap_id, DecodeError, Statics},
            error::FlammError,
            flamm_filter,
            sim::{GAS_LEVER_UP, GAS_SWAP_BUY, GAS_SWAP_SELL},
            FlammPoolState, VenueKind, PROTOCOL_SYSTEM,
        },
        u256_num::u256_to_biguint,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

/// The three recorded snapshots (`testdata/README.md` section 5): the parent of the first swap,
/// a block with the pool's first debt, and a later one with supply as well.
const SNAPSHOT_BLOCKS: &[u64] = &[51_302_915, 51_313_000, 51_409_000];
const POOL: &str = "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572";
const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";
const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

/// A `0x` hex value as bytes; an odd digit count (the snapshot's balances, `0x3591f`) is
/// left-padded with a zero nibble.
fn bytes(hex: &str) -> Bytes {
    let digits = hex.strip_prefix("0x").unwrap_or(hex);
    let padded = if digits.len() % 2 == 1 { format!("0x0{digits}") } else { format!("0x{digits}") };
    Bytes::from_str(&padded).unwrap_or_else(|e| panic!("{hex}: {e}"))
}

fn token(addr: &str, symbol: &str, decimals: u32) -> Token {
    Token::new(&bytes(addr), symbol, decimals, 0, &[Some(100_000)], Chain::Base, 100)
}

fn cbbtc() -> Token {
    token(CBBTC, "cbBTC", 8)
}

fn usdc() -> Token {
    token(USDC, "USDC", 6)
}

/// A recorded snapshot: the two components, the block and the grids.
struct Snapshot {
    raw: Value,
}

impl Snapshot {
    fn load(block: u64) -> Self {
        Self { raw: load(&format!("snapshots/{block}.json.gz")) }
    }

    fn number(&self) -> u64 {
        self.raw["block"]["number"]
            .as_u64()
            .unwrap()
    }

    fn timestamp(&self) -> u64 {
        self.raw["block"]["timestamp"]
            .as_u64()
            .unwrap()
    }

    fn header(&self) -> BlockHeader {
        BlockHeader {
            hash: bytes(s(&self.raw["block"], "hash")),
            number: self.number(),
            parent_hash: Bytes::zero(32),
            revert: false,
            timestamp: self.timestamp(),
            partial_block_index: None,
        }
    }

    /// The `ComponentWithState` of component `i` (0 swap, 1 lever-up), as the stream delivers
    /// it.
    fn component(&self, i: usize) -> ComponentWithState {
        let c = &self.raw["components"][i];
        let map = |v: &Value| -> HashMap<String, Bytes> {
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), bytes(v.as_str().unwrap())))
                .collect()
        };
        let comp = &c["component"];
        ComponentWithState {
            state: ResponseProtocolState {
                component_id: s(&c["state"], "component_id").to_owned(),
                attributes: map(&c["state"]["attributes"]),
                balances: c["state"]["balances"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (bytes(k), bytes(v.as_str().unwrap())))
                    .collect(),
            }
            .into(),
            component: ProtocolComponent {
                id: s(comp, "id").to_owned(),
                protocol_system: s(comp, "protocol_system").to_owned(),
                protocol_type_name: s(comp, "protocol_type_name").to_owned(),
                chain: Chain::Base.into(),
                tokens: comp["tokens"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| bytes(t.as_str().unwrap()))
                    .collect(),
                contract_ids: Vec::new(),
                static_attributes: map(&comp["static_attributes"]),
                creation_tx: bytes(s(comp, "creation_tx")),
                ..Default::default()
            }
            .into(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn grids(&self) -> &Value {
        &self.raw["grids"]
    }
}

fn decode(snap: &Snapshot, i: usize) -> Result<FlammPoolState, InvalidSnapshotError> {
    tokio_test::block_on(FlammPoolState::try_from_with_header(
        snap.component(i),
        snap.header(),
        &HashMap::new(),
        &HashMap::new(),
        &DecoderContext::new(),
    ))
}

fn decoded(snap: &Snapshot, i: usize) -> FlammPoolState {
    decode(snap, i).unwrap_or_else(|e| panic!("block {}: component {i}: {e:?}", snap.number()))
}

/// The recorded outcome of one `previewSwap` / `previewLever` row: the return words, or the
/// revert class.
fn recorded(row: &Value) -> Result<Vec<U256>, Option<FlammError>> {
    if row["ok"].as_bool().unwrap() {
        let h = s(row, "ret");
        let b = hex::decode(h.trim_start_matches("0x")).unwrap();
        Ok(b.chunks(32)
            .map(U256::from_be_slice)
            .collect())
    } else {
        Err(revert_class(s(row, "revert")))
    }
}

fn amount(row: &Value) -> U256 {
    U256::from_str_radix(s(row, "amount_in"), 10).unwrap()
}

/// The venue's limit in a direction, zero when it fills no size.
pub(super) fn limit_of(state: &FlammPoolState, tin: &Token, tout: &Token) -> U256 {
    let (a, _) = state
        .get_limits(tin.address.clone(), tout.address.clone())
        .unwrap();
    U256::from_str_radix(&a.to_string(), 10).unwrap()
}

/// The quote of a size the pool refuses, under `get_amount_out`'s contract: at or below the
/// venue's limit it is dust, the empty trade (nothing out, the venue's base gas, the state
/// unchanged); above it the typed refusal, whose message names `want` when given. Returns
/// whether the size was dust.
pub(super) fn refused_quote(
    quote: Result<GetAmountOutResult, SimulationError>,
    state: &FlammPoolState,
    a: U256,
    limit: U256,
    want: Option<&str>,
    ctx: &str,
) -> bool {
    if a <= limit {
        let q =
            quote.unwrap_or_else(|e| panic!("{ctx}: dust below the limit {limit} refused: {e}"));
        assert_eq!(q.amount, BigUint::ZERO, "{ctx}: dust paid something");
        assert!(q.gas > BigUint::ZERO, "{ctx}");
        assert!(q.new_state.eq(state), "{ctx}: dust moved the state");
        true
    } else {
        let e = quote
            .err()
            .unwrap_or_else(|| panic!("{ctx}: a size above the limit {limit} was quoted"));
        if let Some(want) = want {
            assert!(e.to_string().contains(want), "{ctx}: {e} is not {want}");
        }
        false
    }
}

#[test]
fn snapshots_decode_into_both_venues() {
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let swap = decoded(&snap, 0);
        let lever = decoded(&snap, 1);
        assert_eq!(swap.venue(), VenueKind::Swap);
        assert_eq!(lever.venue(), VenueKind::LeverUp);
        assert_eq!(swap.id(), swap_id(Address::from_str(POOL).unwrap()));
        assert_eq!(lever.id(), lever_up_id(Address::from_str(POOL).unwrap()));
        assert_eq!(swap.block(), block);
        assert_eq!(swap.clock(), snap.timestamp());
        assert!(swap.core().is_ok(), "{block}: {:?}", swap.core().err());
        // Both venues decode the same pool.
        assert_eq!(swap.flamm(), lever.flamm());
        assert_eq!(
            snap.component(0)
                .component
                .protocol_system,
            PROTOCOL_SYSTEM
        );
    }
}

/// The decoder reads the same state the Solidity-generated dump of the same block records
/// (`core_e2e_grid_51302915`, the Go tracker's reads decoded by the fixture harness), field for
/// field, the Morpho oracle at the block's timestamp included.
#[test]
fn decoded_state_is_the_fixture_state() {
    let snap = Snapshot::load(51_302_915);
    let state = decoded(&snap, 0);
    let line = fixture_lines("core_e2e_grid_51302915.jsonl.gz")
        .into_iter()
        .next()
        .unwrap();
    let row: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(row["k"], "state");
    let want = build_state(&decode_reads(&row["s"]));
    let got = state.flamm().unwrap();
    assert_eq!(got.pool, want.pool);
    assert_eq!(got.hooks, want.hooks);
    assert_eq!(got.feed, want.feed);
    assert_eq!(got.router, want.router);
    assert_eq!(got, &want);
}

/// Every recorded `previewSwap` row at every pinned block: the port's preview answers the same
/// words or refuses with the same class; a size the pool fills in full is quoted with the
/// recorded output and the venue's gas, a size it clips or refuses is not quoted.
#[test]
fn previews_reproduce_the_chain_at_every_pinned_block() {
    let (cb, us) = (cbbtc(), usdc());
    let mut rows = 0usize;
    let mut full = 0usize;
    let mut dust = 0usize;
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let state = decoded(&snap, 0);
        let flamm = state.flamm().unwrap();
        let now = snap.timestamp();
        for (dir, key) in [(true, "swap_sell"), (false, "swap_buy")] {
            let (tin, tout) = if dir { (&cb, &us) } else { (&us, &cb) };
            let limit = limit_of(&state, tin, tout);
            for row in snap.grids()[key].as_array().unwrap() {
                rows += 1;
                let a = amount(row);
                let chain = recorded(row);
                let ctx = format!("block {block} {key} {a}");
                let got = flamm.preview_swap(dir, a, now);
                match (&chain, &got) {
                    (Ok(words), Ok(p)) => {
                        assert_eq!(words[0], p.used_native, "{ctx}: amountInUsed");
                        assert_eq!(words[1], p.net_native, "{ctx}: amountOut");
                        assert_eq!(words[2], p.fee_wad, "{ctx}: feeWad");
                    }
                    (Err(Some(want)), Err(e)) => assert_eq!(want, e, "{ctx}: revert class"),
                    (Err(None), _) => panic!("{ctx}: unmapped revert data {}", s(row, "revert")),
                    _ => panic!("{ctx}: chain {chain:?}, port {got:?}"),
                }
                let quote = state.get_amount_out(u256_to_biguint(a), tin, tout);
                match &chain {
                    Ok(words) if words[0] == a => {
                        let q = quote.unwrap_or_else(|e| panic!("{ctx}: full fill refused: {e}"));
                        assert_eq!(q.amount, u256_to_biguint(words[1]), "{ctx}: amount out");
                        assert!(
                            q.gas >= BigUint::from(if dir { GAS_SWAP_SELL } else { GAS_SWAP_BUY }),
                            "{ctx}: gas {}",
                            q.gas
                        );
                        let next = q
                            .new_state
                            .as_any()
                            .downcast_ref::<FlammPoolState>()
                            .unwrap();
                        assert_ne!(next.flamm(), state.flamm(), "{ctx}: the fill moved the pool");
                        assert_eq!(next.clock(), state.clock());
                        full += 1;
                    }
                    Ok(_) => {
                        let e = quote
                            .err()
                            .unwrap_or_else(|| panic!("{ctx}: a clipped fill was quoted"));
                        assert!(e.to_string().contains("partial fill"), "{ctx}: {e}");
                    }
                    Err(want) => {
                        let want = want.as_ref().map(|w| w.to_string());
                        if refused_quote(quote, &state, a, limit, want.as_deref(), &ctx) {
                            assert!(!dir, "{ctx}: a sell is never dust");
                            dust += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(
        rows > 150 * SNAPSHOT_BLOCKS.len() &&
            full > 80 * SNAPSHOT_BLOCKS.len() &&
            dust > 20 * SNAPSHOT_BLOCKS.len(),
        "{rows} rows, {full} full fills, {dust} dust"
    );
}

/// `get_limits` locates the size the chain's own bisection found: the largest that fills in
/// full per direction, with the recorded output at it, and one more unit is refused. The
/// contract below the limit, as the doc of `get_limits` states it: the sizes the protocol test
/// harness derives from the limit (0.1%, 1% and 10%) fill in full; every sell fills from one
/// base unit; a buy's dust, which the pool refuses below a few thousand loan units, not as an
/// interval, is the empty trade, and the largest refused buy the chain recorded is below a
/// hundredth of a percent of the limit.
#[test]
fn limits_are_the_chains_largest_full_fills() {
    let (cb, us) = (cbbtc(), usdc());
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let state = decoded(&snap, 0);
        for (key, edge, tin, tout) in
            [("swap_sell", "sell_edge", &cb, &us), ("swap_buy", "buy_edge", &us, &cb)]
        {
            let rows = snap.grids()[key].as_array().unwrap();
            let want = U256::from_str_radix(s(snap.grids(), edge), 10).unwrap();
            let want_out = rows
                .iter()
                .find(|r| amount(r) == want)
                .map(|r| recorded(r).unwrap()[1])
                .unwrap();
            let (a, out) = state
                .get_limits(tin.address.clone(), tout.address.clone())
                .unwrap();
            assert_eq!(a, u256_to_biguint(want), "block {block} {edge}");
            assert_eq!(out, u256_to_biguint(want_out), "block {block} {edge} out");
            assert!(state
                .get_amount_out(a.clone() + BigUint::from(1u8), tin, tout)
                .is_err());
            assert!(state
                .get_amount_out(a.clone(), tin, tout)
                .is_ok());
            // The harness's sizes (`protocols/testing/src/test_runner.rs`, `run_simulation`).
            for per_mille in [1u32, 10, 100] {
                let size = &a * BigUint::from(per_mille) / BigUint::from(1000u32);
                let q = state
                    .get_amount_out(size.clone(), tin, tout)
                    .unwrap_or_else(|e| panic!("block {block} {key}: {per_mille}/1000: {e}"));
                assert!(q.amount > BigUint::ZERO, "block {block} {key}: {size} pays nothing");
            }
            // The dust the chain recorded below the limit: the largest refused size, if any, and
            // the smallest filled one.
            let refused_below = rows
                .iter()
                .filter(|r| amount(r) < want && recorded(r).is_err())
                .map(amount)
                .max();
            let smallest_filled = rows
                .iter()
                .filter(|r| recorded(r).is_ok())
                .map(amount)
                .min()
                .unwrap();
            if key == "swap_sell" {
                assert_eq!(refused_below, None, "block {block}: a sell refused below the limit");
                assert_eq!(smallest_filled, U256::from(1u8));
            } else {
                let largest_refused = refused_below.unwrap();
                assert!(largest_refused < want / U256::from(10_000u32));
                assert!(largest_refused <= U256::from(5000u32), "{largest_refused}");
                assert_eq!(smallest_filled, U256::from(4096u32));
                // Not an interval: 51409000 fills 4096 units and refuses 5000.
                assert_eq!(
                    block == 51_409_000,
                    largest_refused > smallest_filled,
                    "block {block}: refused {largest_refused}, filled {smallest_filled}"
                );
                let q = state
                    .get_amount_out(u256_to_biguint(largest_refused), tin, tout)
                    .unwrap();
                assert_eq!(q.amount, BigUint::ZERO);
                assert!(q.new_state.eq(&state));
            }
        }
    }
}

/// `fee` is the sell direction's `feeWad` of every recorded `previewSwap(true, .)`, and
/// `spot_price(base, quote)` is the trait's price: the hook's recorded `spot` in the token
/// frame, as `quote` per `base`, divided by `1 - fee` of the direction that buys `base`. A
/// small quote in that direction pays that price at the margin.
#[test]
fn fee_and_spot_follow_the_recorded_hook() {
    let (cb, us) = (cbbtc(), usdc());
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let state = decoded(&snap, 0);
        let flamm = state.flamm().unwrap();
        let now = snap.timestamp();
        let fee_of = |key: &str| -> U256 {
            let fees: Vec<U256> = snap.grids()[key]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|r| recorded(r).ok().map(|w| w[2]))
                .collect();
            assert!(!fees.is_empty());
            assert!(fees.iter().all(|f| *f == fees[0]), "the fee does not depend on the size");
            fees[0]
        };
        let sell_fee = fee_of("swap_sell");
        let buy_fee = fee_of("swap_buy");
        assert_eq!(flamm.swap_fee_wad(true, now), Ok(sell_fee), "block {block}");
        assert_eq!(flamm.swap_fee_wad(false, now), Ok(buy_fee), "block {block}");
        let wad = |w: U256| w.to_string().parse::<f64>().unwrap() / 1e18;
        let (sell_fee_f, buy_fee_f) = (wad(sell_fee), wad(buy_fee));
        assert!((state.fee() - sell_fee_f).abs() < 1e-15, "block {block}: fee {}", state.fee());
        let spot_word = recorded(&snap.grids()["spot"]).unwrap()[0];
        assert_eq!(flamm.hooks.swap.port().unwrap().spot(), Ok(spot_word));
        // N18 per sat times 1e8 sats, over 1e18: USDC per cbBTC.
        let pool_in_loan = wad(spot_word) * 1e8;
        let rel = |a: f64, b: f64| ((a - b) / b).abs();
        // Buying cbBTC with USDC is the buy direction; buying USDC with cbBTC the sell direction.
        let usdc_per_cbbtc = state.spot_price(&cb, &us).unwrap();
        let cbbtc_per_usdc = state.spot_price(&us, &cb).unwrap();
        assert!(rel(usdc_per_cbbtc, pool_in_loan / (1.0 - buy_fee_f)) < 1e-12, "{usdc_per_cbbtc}");
        assert!(
            rel(cbbtc_per_usdc, (1.0 / pool_in_loan) / (1.0 - sell_fee_f)) < 1e-12,
            "{cbbtc_per_usdc}"
        );
        assert!(usdc_per_cbbtc > 50_000.0 && usdc_per_cbbtc < 200_000.0, "{usdc_per_cbbtc}");
        // The price is the margin of a small trade in the buying direction, up to the payout's
        // rounding to a base unit: one USDC buys about 1280 sats at `usdc_per_cbbtc` (a unit of
        // output is 8e-4 of it), a hundred sats buy about 75 USDC at `cbbtc_per_usdc`. The two
        // orderings multiply to the two fees' markup, not to one.
        let paid = |amount_in: u64, tin: &Token, tout: &Token| -> f64 {
            let q = state
                .get_amount_out(BigUint::from(amount_in), tin, tout)
                .unwrap();
            let whole_in = amount_in as f64 / 10f64.powi(tin.decimals as i32);
            let whole_out = q
                .amount
                .to_string()
                .parse::<f64>()
                .unwrap() /
                10f64.powi(tout.decimals as i32);
            whole_in / whole_out
        };
        let buy_margin = paid(1_000_000, &us, &cb);
        let sell_margin = paid(100, &cb, &us);
        assert!(
            rel(buy_margin, usdc_per_cbbtc) < 1e-3,
            "block {block}: {buy_margin} vs {usdc_per_cbbtc}"
        );
        assert!(
            rel(sell_margin, cbbtc_per_usdc) < 1e-4,
            "block {block}: {sell_margin} vs {cbbtc_per_usdc}"
        );
        let product = usdc_per_cbbtc * cbbtc_per_usdc;
        assert!(rel(product, 1.0 / ((1.0 - buy_fee_f) * (1.0 - sell_fee_f))) < 1e-12, "{product}");
    }
}

/// `query_pool_swap` over the generic search, both directions of the swap venue: a trade limit
/// between the max-size execution price and the zero-size one is met by a fill inside the
/// limit, and a limit above the zero-size execution price (below the spot, which is gross of
/// the other direction's fee) is the zero swap in both directions. The search bisects toward
/// zero for such a limit and passes through a buy's dust, which answers as the empty trade
/// rather than ending the search with the pool's revert.
#[test]
fn query_pool_swap_answers_both_directions() {
    let (cb, us) = (cbbtc(), usdc());
    let whole = |amount: &BigUint, t: &Token| -> f64 {
        amount
            .to_string()
            .parse::<f64>()
            .unwrap() /
            10f64.powi(t.decimals as i32)
    };
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let state = decoded(&snap, 0);
        for (tin, tout) in [(&cb, &us), (&us, &cb)] {
            let ctx = format!("block {block} {} -> {}", tin.symbol, tout.symbol);
            let spot = state.spot_price(tin, tout).unwrap();
            let (max_in, max_out) = state
                .get_limits(tin.address.clone(), tout.address.clone())
                .unwrap();
            let max_price = whole(&max_out, tout) / whole(&max_in, tin);
            let small = &max_in / BigUint::from(10_000u32);
            let small_out = state
                .get_amount_out(small.clone(), tin, tout)
                .unwrap()
                .amount;
            let small_price = whole(&small_out, tout) / whole(&small, tin);
            assert!(
                max_price < small_price && small_price < spot,
                "{ctx}: {max_price} {small_price} {spot}"
            );
            let price = |p: f64| {
                let scale = 1e12;
                Price::new(
                    BigUint::from((p * scale * 10f64.powi(tout.decimals as i32)) as u128),
                    BigUint::from((scale * 10f64.powi(tin.decimals as i32)) as u128),
                )
            };
            let query = |limit: f64| {
                state.query_pool_swap(&QueryPoolSwapParams::new(
                    tin.clone(),
                    tout.clone(),
                    SwapConstraint::TradeLimitPrice {
                        limit: price(limit),
                        tolerance: 1e-4,
                        min_amount_in: None,
                        max_amount_in: None,
                    },
                ))
            };
            let mid = (max_price + small_price) / 2.0;
            let swap = query(mid).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert!(*swap.amount_in() > small && *swap.amount_in() < max_in, "{ctx}: {swap:?}");
            let got = whole(swap.amount_out(), tout) / whole(swap.amount_in(), tin);
            assert!(got >= mid && got <= mid * (1.0 + 1e-4), "{ctx}: {got} vs {mid}");
            let reached = state
                .get_amount_out(swap.amount_in().clone(), tin, tout)
                .unwrap();
            assert_eq!(&reached.amount, swap.amount_out(), "{ctx}");
            // Above the zero-size execution price: no fill holds it, the zero swap.
            let none = query(spot * (1.0 - 1e-6)).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert_eq!(*none.amount_in(), BigUint::ZERO, "{ctx}: {none:?}");
            assert_eq!(*none.amount_out(), BigUint::ZERO, "{ctx}");
        }
    }
}

/// The lever-up venue at the pinned blocks: `levPaused` on chain, so every `previewLever`
/// reverts and the venue quotes nothing; the reverse direction is not a venue at all.
#[test]
fn lever_up_venue_refuses_as_the_chain_does() {
    let (cb, us) = (cbbtc(), usdc());
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let state = decoded(&snap, 1);
        let flamm = state.flamm().unwrap();
        let now = snap.timestamp();
        assert!(flamm.lev_paused);
        for (up, key) in [(true, "lever_up"), (false, "lever_down")] {
            for row in snap.grids()[key].as_array().unwrap() {
                let a = amount(row);
                let want = recorded(row).unwrap_err().unwrap();
                assert_eq!(flamm.preview_lever(up, a, now), Err(want), "block {block} {key} {a}");
                assert_eq!(
                    flamm
                        .execute_lever(up, a, U256::ZERO, now, now)
                        .err(),
                    Some(want)
                );
            }
        }
        let e = state
            .get_amount_out(BigUint::from(15_000u32), &cb, &us)
            .unwrap_err();
        assert!(e.to_string().contains("LevPaused"), "{e}");
        let e = state
            .get_amount_out(BigUint::from(1_000_000u32), &us, &cb)
            .unwrap_err();
        assert!(e.to_string().contains("lever-down"), "{e}");
        assert_eq!(
            state
                .get_limits(cb.address.clone(), us.address.clone())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
        assert_eq!(
            state
                .get_limits(us.address.clone(), cb.address.clone())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
        assert!(state.spot_price(&us, &cb).is_err());
        assert!(state.spot_price(&cb, &us).is_err());
        // The keeper's spread post (17500 ppm at 1789099323, maxSpreadAge 3600) has lapsed at
        // every pinned block: the venue has no fee to quote. Re-posted at the clock, it does.
        assert_eq!(flamm.hooks.spread.spread_ppm(now), Ok((false, U256::ZERO)));
        assert_eq!(state.fee(), 1.0);
        let mut reposted = state.clone();
        let mut word: U256 = U256::from(17_500u32) | (U256::from(3600u32) << 72);
        word |= U256::from(now) << 104;
        let delta = ProtocolStateDelta {
            component_id: lever_up_id(Address::from_str(POOL).unwrap()),
            updated_attributes: HashMap::from([(
                "spread:0x0000000000000000000000000000000000000000000000000000000000000000"
                    .to_owned(),
                Bytes::from(word.to_be_bytes::<32>().to_vec()),
            )]),
            deleted_attributes: Default::default(),
        };
        reposted
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();
        assert_eq!(
            reposted
                .flamm()
                .unwrap()
                .hooks
                .spread
                .spread_ppm(now),
            Ok((true, U256::from(17_500u32)))
        );
        assert!((reposted.fee() - 0.0175).abs() < 1e-12, "{}", reposted.fee());
        // `fee` is the spread the pool fills at, not the raw post: `FLAMMLeverLib._spread`
        // floors a live answer at `LEV_SPREAD_FLOOR_PPM` (2500) and caps it at the band in ppm
        // (`swapPriceBandWad / 1e12`, itself at most `LEV_SPREAD_CEILING_PPM`).
        let posted = |ppm: u32| -> FlammPoolState {
            let mut st = reposted.clone();
            let word: U256 =
                U256::from(ppm) | (U256::from(3600u32) << 72) | (U256::from(now) << 104);
            let delta = ProtocolStateDelta {
                component_id: lever_up_id(Address::from_str(POOL).unwrap()),
                updated_attributes: HashMap::from([(
                    "spread:0x0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
                    Bytes::from(word.to_be_bytes::<32>().to_vec()),
                )]),
                deleted_attributes: Default::default(),
            };
            st.delta_transition(delta, &HashMap::new(), &Balances::default())
                .unwrap();
            st
        };
        let band_ppm = reposted.flamm().unwrap().pool.loans[0].swap_price_band_wad /
            U256::from(1_000_000_000_000u64);
        let band_ppm: u32 = band_ppm.to_string().parse().unwrap();
        assert!(band_ppm > 2500 && band_ppm <= 100_000, "{band_ppm}");
        let floored = posted(1_000);
        assert!((floored.fee() - 0.0025).abs() < 1e-12, "{}", floored.fee());
        let capped = posted(band_ppm + 10_000);
        assert!((capped.fee() - band_ppm as f64 / 1e6).abs() < 1e-12, "{}", capped.fee());
        let zero = state
            .get_amount_out(BigUint::ZERO, &cb, &us)
            .unwrap();
        assert_eq!(zero.amount, BigUint::ZERO);
        assert_eq!(zero.gas, BigUint::from(GAS_LEVER_UP));
    }
}

/// The lever-up venue over the `armed` scenario of the pool-core grid at 51302915 (the curator
/// unpaused the venue and the keeper posted a 17500 ppm spread at the block's timestamp): every
/// recorded `previewLever(true, .)` row is a quote of exactly the recorded output when the venue
/// fills the size in full, and a typed refusal otherwise; lever-down is never a quote.
#[test]
fn lever_up_quotes_the_armed_fixture() {
    let (cb, us) = (cbbtc(), usdc());
    let snap = Snapshot::load(51_302_915);
    let lines = fixture_lines("core_e2e_grid_51302915.jsonl.gz");
    let mut state = None;
    let mut limit = U256::ZERO;
    let (mut rows, mut full, mut refused, mut dust) = (0usize, 0usize, 0usize, 0usize);
    for line in &lines {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["tag"] != "armed" {
            continue;
        }
        if row["k"] == "state" {
            let armed = build_state(&decode_reads(&row["s"]));
            assert!(!armed.lev_paused);
            let st = decoded(&snap, 1).with_flamm(armed);
            assert_eq!(st.clock(), snap.timestamp());
            // The spread the pool fills at: the hook's 17500 ppm post, inside the venue's
            // clamp (`FLAMMLeverLib.sol:171-177`), so equal to it here.
            assert!((st.fee() - 0.0175).abs() < 1e-12, "{}", st.fee());
            limit = limit_of(&st, &cb, &us);
            state = Some(st);
            continue;
        }
        if row["k"] != "lv" {
            continue;
        }
        let st = state
            .as_ref()
            .expect("the state row comes first");
        let a = U256::from_str_radix(s(&row, "a"), 10).unwrap();
        let up = row["d"].as_i64() == Some(1);
        if a.is_zero() {
            // The pool reverts `InvalidAmount()`; the `ProtocolSim` answers the empty trade.
            continue;
        }
        let (tin, tout) = if up { (&cb, &us) } else { (&us, &cb) };
        let quote = st.get_amount_out(u256_to_biguint(a), tin, tout);
        rows += 1;
        match (up, row.get("r")) {
            (true, Some(r)) => {
                let words: Vec<U256> = r
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|w| U256::from_str_radix(w.as_str().unwrap(), 10).unwrap())
                    .collect();
                if words[0] == a {
                    let q = quote.unwrap_or_else(|e| panic!("armed lever-up {a}: {e}"));
                    assert_eq!(q.amount, u256_to_biguint(words[1]), "armed lever-up {a}");
                    assert_eq!(q.gas, BigUint::from(GAS_LEVER_UP));
                    full += 1;
                } else {
                    assert!(quote.is_err(), "armed lever-up {a}: a partial fill was quoted");
                    refused += 1;
                }
            }
            (true, None) => {
                let want = revert_class(s(&row, "e"))
                    .unwrap()
                    .to_string();
                let ctx = format!("armed lever-up {a}");
                if refused_quote(quote, st, a, limit, Some(&want), &ctx) {
                    dust += 1;
                } else {
                    refused += 1;
                }
            }
            (false, _) => {
                let e = quote.unwrap_err();
                assert!(e.to_string().contains("lever-down"), "{e}");
            }
        }
    }
    assert!(
        full > 500 && refused > 1000,
        "{rows} rows, {full} full, {refused} refused, {dust} dust"
    );
    let st = state.unwrap();
    let (max_in, max_out) = st
        .get_limits(cb.address.clone(), us.address.clone())
        .unwrap();
    assert!(max_in > BigUint::ZERO);
    assert_eq!(max_in, BigUint::from_str(&limit.to_string()).unwrap());
    assert!(st
        .get_amount_out(max_in.clone() + BigUint::from(1u8), &cb, &us)
        .is_err());
    assert_eq!(
        st.get_amount_out(max_in.clone(), &cb, &us)
            .unwrap()
            .amount,
        max_out
    );
    // The venue's one rate, USDC paid per cbBTC, read off the fill of 64 times its smallest full
    // fill (one base unit here), answers both orderings: `spot_price(USDC, cbBTC)` in the trait's
    // ordering (cbBTC paid per USDC bought, gross of the spread since the payout is net of it)
    // and `spot_price(cbBTC, USDC)` as the price of the direction the venue trades, which
    // `query_pool_swap` and the protocol test harness read for a cbBTC -> USDC trade.
    let rate_at = |sats: u64| -> f64 {
        let out = st
            .get_amount_out(BigUint::from(sats), &cb, &us)
            .unwrap()
            .amount
            .to_string()
            .parse::<f64>()
            .unwrap();
        (out / 1e6) / (sats as f64 / 1e8)
    };
    let rel = |a: f64, b: f64| ((a - b) / b).abs();
    let usdc_per_cbbtc = st.spot_price(&cb, &us).unwrap();
    let cbbtc_per_usdc = st.spot_price(&us, &cb).unwrap();
    assert!(usdc_per_cbbtc > 50_000.0 && usdc_per_cbbtc < 200_000.0, "{usdc_per_cbbtc}");
    assert!(rel(usdc_per_cbbtc, rate_at(64)) < 1e-12, "{usdc_per_cbbtc} vs {}", rate_at(64));
    assert!(rel(cbbtc_per_usdc * usdc_per_cbbtc, 1.0) < 1e-12);
    // The read's residual: the one-unit fill's rate is quantized by its ~741-unit payout (up
    // to 1.35e-3 of it), the 64-unit read by a 64th of that, and the curve moves the rate by
    // less than 3% over the venue's whole range (`max_in` units), so by far less over 64.
    assert!(rel(rate_at(1), usdc_per_cbbtc) < 1.5e-3, "{} vs {usdc_per_cbbtc}", rate_at(1));
    let max_rate = (max_out
        .to_string()
        .parse::<f64>()
        .unwrap() /
        1e6) /
        (max_in
            .to_string()
            .parse::<f64>()
            .unwrap() /
            1e8);
    assert!(max_rate < usdc_per_cbbtc && rel(max_rate, usdc_per_cbbtc) < 0.03, "{max_rate}");
    // The price is a bound on every fill's execution price and is net of the spread: the pool
    // pays at most `price * (1 - spread)` per cbBTC at the checked feed (`FLAMMLeverLib.sol:108`).
    let flamm = st.flamm().unwrap();
    let (feed_wad, _) = flamm.price(st.clock()).unwrap();
    let feed = feed_wad
        .to_string()
        .parse::<f64>()
        .unwrap() /
        1e18 *
        1e8;
    assert!(usdc_per_cbbtc <= feed * (1.0 - st.fee()) * (1.0 + 1e-9), "{usdc_per_cbbtc} vs {feed}");
    // `query_pool_swap` on the venue's one direction: the largest fill whose execution price
    // holds a limit between the max-size rate and the spot; a limit a hair below the spot is
    // reached, if at all, only by a fill the payout's rounding favours, far inside the range.
    let price = |usdc_per_cbbtc: f64| {
        Price::new(
            BigUint::from((usdc_per_cbbtc * 1e6 * 1e6) as u128),
            BigUint::from(1e8 as u128 * 1_000_000u128),
        )
    };
    let mid = (max_rate + usdc_per_cbbtc) / 2.0;
    let swap = st
        .query_pool_swap(&QueryPoolSwapParams::new(
            cb.clone(),
            us.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: price(mid),
                tolerance: 1e-4,
                min_amount_in: None,
                max_amount_in: None,
            },
        ))
        .unwrap();
    assert!(*swap.amount_in() > BigUint::ZERO && *swap.amount_in() < max_in, "{swap:?}");
    let got = (swap
        .amount_out()
        .to_string()
        .parse::<f64>()
        .unwrap() /
        1e6) /
        (swap
            .amount_in()
            .to_string()
            .parse::<f64>()
            .unwrap() /
            1e8);
    assert!(got >= mid && got <= mid * (1.0 + 1e-4), "{got} vs {mid}");
    let reached = st
        .get_amount_out(swap.amount_in().clone(), &cb, &us)
        .unwrap();
    assert_eq!(&reached.amount, swap.amount_out());
    let none = st
        .query_pool_swap(&QueryPoolSwapParams::new(
            cb.clone(),
            us.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: price(usdc_per_cbbtc * (1.0 - 1e-6)),
                tolerance: 1e-4,
                min_amount_in: None,
                max_amount_in: None,
            },
        ))
        .unwrap();
    assert!(none.amount_in() * BigUint::from(10u8) < max_in, "{none:?}");
    if *none.amount_in() > BigUint::ZERO {
        let got = (none
            .amount_out()
            .to_string()
            .parse::<f64>()
            .unwrap() /
            1e6) /
            (none
                .amount_in()
                .to_string()
                .parse::<f64>()
                .unwrap() /
                1e8);
        assert!(got >= usdc_per_cbbtc * (1.0 - 1e-6), "{got}");
    }
}

/// The swap venue over the `capped` scenario of the pool-core grid at 51302915 (a notional cap
/// the pool clips sells to): every recorded `previewSwap` row the pool fills in full is quoted
/// with the recorded output, every row it clips (`amountInUsed < amountIn`) is refused as a
/// partial fill, every reverting row above the limit is refused with its class, and every
/// reverting row at or below it (a buy's dust) is the empty trade.
#[test]
fn clipped_fills_are_refused() {
    let (cb, us) = (cbbtc(), usdc());
    let snap = Snapshot::load(51_302_915);
    let lines = fixture_lines("core_e2e_grid_51302915.jsonl.gz");
    let mut state = None;
    let mut limits = (U256::ZERO, U256::ZERO);
    let (mut full, mut partial, mut reverted, mut dust) = (0usize, 0usize, 0usize, 0usize);
    for line in &lines {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["tag"] != "capped" {
            continue;
        }
        if row["k"] == "state" {
            let capped = build_state(&decode_reads(&row["s"]));
            assert!(!capped.pool.loans[0]
                .max_swap_notional
                .is_zero());
            let st = decoded(&snap, 0).with_flamm(capped);
            limits = (limit_of(&st, &cb, &us), limit_of(&st, &us, &cb));
            state = Some(st);
            continue;
        }
        if row["k"] != "sw" {
            continue;
        }
        let st = state
            .as_ref()
            .expect("the state row comes first");
        let a = U256::from_str_radix(s(&row, "a"), 10).unwrap();
        if a.is_zero() {
            continue;
        }
        let sell = row["d"].as_i64() == Some(1);
        let (tin, tout) = if sell { (&cb, &us) } else { (&us, &cb) };
        let limit = if sell { limits.0 } else { limits.1 };
        let quote = st.get_amount_out(u256_to_biguint(a), tin, tout);
        match row.get("r") {
            Some(r) => {
                let words: Vec<U256> = r
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|w| U256::from_str_radix(w.as_str().unwrap(), 10).unwrap())
                    .collect();
                if words[0] == a {
                    let q = quote.unwrap_or_else(|e| panic!("capped d={sell} {a}: {e}"));
                    assert_eq!(q.amount, u256_to_biguint(words[1]), "capped d={sell} {a}");
                    full += 1;
                } else {
                    let e = quote.unwrap_err();
                    assert!(e.to_string().contains("partial fill"), "capped d={sell} {a}: {e}");
                    assert!(
                        e.to_string()
                            .contains(&words[0].to_string()),
                        "{e}"
                    );
                    partial += 1;
                }
            }
            None => {
                let want = revert_class(s(&row, "e"))
                    .unwrap()
                    .to_string();
                let ctx = format!("capped d={sell} {a}");
                if refused_quote(quote, st, a, limit, Some(&want), &ctx) {
                    assert!(!sell, "{ctx}: a sell is never dust");
                    dust += 1;
                } else {
                    reverted += 1;
                }
            }
        }
    }
    // The reverting buys split at the cap: dust below it (a payout of no or few sats), the
    // notional cap's own refusal above it.
    assert!(
        full > 900 && partial > 60 && reverted > 600 && dust > 2000,
        "{full} full, {partial} partial, {reverted} reverted, {dust} dust"
    );
    // The limit sits at the cap: one unit more is clipped.
    let st = state.unwrap();
    let (max_in, _) = st
        .get_limits(cb.address.clone(), us.address.clone())
        .unwrap();
    let e = st
        .get_amount_out(max_in + BigUint::from(1u8), &cb, &us)
        .unwrap_err();
    assert!(e.to_string().contains("partial fill"), "{e}");
}

/// The delta fixture of the swap block: the 12 words it changed and its header.
fn swap_delta() -> (Value, ProtocolStateDelta) {
    let d = load("snapshots/swap_51302916_delta.json.gz");
    let mut updated: HashMap<String, Bytes> = d["updated_attributes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), bytes(v.as_str().unwrap())))
        .collect();
    let number =
        u64::from_str_radix(s(&d["header"], "number").trim_start_matches("0x"), 16).unwrap();
    updated.insert("block_number".into(), Bytes::from(number));
    updated.insert("block_timestamp".into(), Bytes::from(swap_block_timestamp(&d)));
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: updated,
        deleted_attributes: Default::default(),
    };
    (d, delta)
}

fn swap_block_timestamp(d: &Value) -> u64 {
    u64::from_str_radix(s(&d["header"], "timestamp").trim_start_matches("0x"), 16).unwrap()
}

/// The pool's first settled swap, quoted at its execution block from the parent block's
/// snapshot: 15000 sats pay exactly 11301759 USDC, and the post-state of the quote is the
/// state the chain's storage diffs of that block decode into.
#[test]
fn the_real_swap_reproduces_from_the_parent_snapshot() {
    let (cb, us) = (cbbtc(), usdc());
    let snap = Snapshot::load(51_302_915);
    let mut state = decoded(&snap, 0);
    let (d, delta) = swap_delta();
    let ts = swap_block_timestamp(&d);
    assert_eq!(ts, snap.timestamp() + 2);
    // The pool has no shares in the venue yet and crosses no deadline: the advance is quiet,
    // and the clock still moves.
    assert!(!state.apply_block(&BlockContext::new(51_302_916, ts)));
    assert_eq!(state.clock(), ts);
    let q = state
        .get_amount_out(BigUint::from(15_000u32), &cb, &us)
        .unwrap();
    assert_eq!(q.amount, BigUint::from(11_301_759u32));
    assert_eq!(q.gas, BigUint::from(GAS_SWAP_SELL));
    let simulated = q
        .new_state
        .as_any()
        .downcast_ref::<FlammPoolState>()
        .unwrap()
        .flamm()
        .unwrap()
        .clone();

    let mut observed = decoded(&snap, 0);
    observed.apply_block(&BlockContext::new(51_302_916, ts));
    observed
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    assert_eq!(observed.block(), 51_302_916);
    assert_eq!(observed.clock(), ts);
    let mut chain = observed.flamm().unwrap().clone();
    // The ledger the block wrote: physical, the hook's committed book, the Router's managed
    // collateral, the accrued market with the pool's borrow, the position and the adapted rate.
    assert_eq!(simulated.pool.physical, U256::from(0x32d4au32));
    assert_eq!(simulated.pool, chain.pool);
    assert_eq!(simulated.hooks, chain.hooks);
    assert_eq!(simulated.router.venues[0].managed_collateral, U256::from(0x666du32));
    let (sim_m, chain_m) = (&simulated.router.venues[0].morpho, &chain.router.venues[0].morpho);
    assert_eq!(sim_m.market.total_borrow_assets, chain_m.market.total_borrow_assets);
    assert_eq!(sim_m.market.total_borrow_shares, chain_m.market.total_borrow_shares);
    assert_eq!(sim_m.market.last_update, chain_m.market.last_update);
    assert_eq!(sim_m.position, chain_m.position);
    assert_eq!(sim_m.rate_at_target, U256::from(0x57fee5c5u32));
    // The block's storage diff is the whole block's: another Morpho user withdrew from the
    // market in it (the supply totals end 90,140,614 assets below what the pool's own
    // transaction leaves, the borrow totals untouched), which no quote of the pool produces.
    let withdrawn = sim_m.market.total_supply_assets - chain_m.market.total_supply_assets;
    assert_eq!(withdrawn, U256::from(90_140_614u32));
    assert!(chain_m.market.total_supply_shares < sim_m.market.total_supply_shares);
    chain.router.venues[0]
        .morpho
        .market
        .total_supply_assets = sim_m.market.total_supply_assets;
    chain.router.venues[0]
        .morpho
        .market
        .total_supply_shares = sim_m.market.total_supply_shares;
    assert_eq!(simulated.router, chain.router);
    assert_eq!(simulated.feed, chain.feed);
    // The quote's post-state keeps the block it was decoded from; the delta's carries the
    // block that wrote it.
    assert_eq!(simulated.block, 51_302_915);
    chain.block = simulated.block;
    assert_eq!(simulated, chain);
    // The same quote from the block's own state is refused: the pool is where the fill left it,
    // and the next 15000 sats price differently.
    let again = observed
        .get_amount_out(BigUint::from(15_000u32), &cb, &us)
        .unwrap();
    assert_ne!(again.amount, BigUint::from(11_301_759u32));
}

/// `apply_block` is idempotent for a repeated timestamp, quiet while nothing a quote reads at
/// the clock moves, reports every deadline a quote crosses and every advance of a positioned
/// pool, and the quote at a clock is a function of the clock alone.
#[test]
fn apply_block_moves_the_clock_deterministically() {
    let (cb, us) = (cbbtc(), usdc());
    let quote = |st: &FlammPoolState| {
        st.get_amount_out(BigUint::from(20_000u32), &cb, &us)
            .unwrap()
            .amount
    };
    // 51302915: the pool holds no shares in the venue (its first swap is the next block), the
    // rate ceiling admits every borrow and no deadline is near, so a block advance changes
    // nothing a quote reads: the clock moves, the flag does not.
    let snap = Snapshot::load(51_302_915);
    let t0 = snap.timestamp();
    let mut state = decoded(&snap, 0);
    let position = &state.flamm().unwrap().router.venues[0]
        .morpho
        .position;
    assert!(position.borrow_shares.is_zero() && position.supply_shares.is_zero());
    assert!(!state.apply_block(&BlockContext::new(51_302_915, t0)));
    let at_t0 = quote(&state);
    assert!(!state.apply_block(&BlockContext::new(51_302_916, t0 + 2)));
    assert_eq!(state.clock(), t0 + 2);
    assert!(!state.apply_block(&BlockContext::new(51_302_916, t0 + 2)));
    assert!(!state.apply_block(&BlockContext::new(51_302_917, t0 + 4)));
    assert_eq!(quote(&state), at_t0);
    let mut fresh = decoded(&snap, 0);
    fresh.apply_block(&BlockContext::new(51_302_917, t0 + 4));
    assert_eq!(state, fresh);
    // The Morpho oracle's reveal is a deadline: two seconds before the snapshot the latest
    // primary round is still inside its cutoff and the previous one answers.
    assert!(state.apply_block(&BlockContext::new(51_302_914, 1_789_395_175)));
    assert!(state.apply_block(&BlockContext::new(51_302_915, t0)));
    assert_eq!(quote(&state), at_t0);
    // The pool asset feed's heartbeat (3600 s after its round) is a deadline: at the heartbeat
    // the round still reads and nothing else moved in the hour, one second past it every quote
    // refuses `StalePrice`; the sequencer, the spread and the buy side refuse too.
    let updated_at = state
        .flamm()
        .unwrap()
        .feed
        .asset
        .round
        .updated_at
        .to::<u64>();
    assert!(!state.apply_block(&BlockContext::new(51_399_999, updated_at + 3600)));
    assert_eq!(quote(&state), at_t0);
    assert!(state.apply_block(&BlockContext::new(51_400_000, updated_at + 3601)));
    let e = state
        .get_amount_out(BigUint::from(20_000u32), &cb, &us)
        .unwrap_err();
    assert!(e.to_string().contains("StalePrice"), "{e}");
    assert!(state
        .get_amount_out(BigUint::from(1_000_000u32), &us, &cb)
        .is_err());
    assert_eq!(state.fee(), 1.0);
    assert!(state.spot_price(&cb, &us).is_err());
    assert!(!state.apply_block(&BlockContext::new(51_400_000, updated_at + 3601)));
    assert!(!state.apply_block(&BlockContext::new(51_400_001, updated_at + 3603)));
    assert!(state.apply_block(&BlockContext::new(51_399_999, updated_at + 3600)));
    assert!(state
        .get_amount_out(BigUint::from(20_000u32), &cb, &us)
        .is_ok());
    // The lever-up venue reads the same pool: quiet between deadlines too.
    let mut lever = decoded(&snap, 1);
    assert!(!lever.apply_block(&BlockContext::new(51_302_916, t0 + 2)));
    assert!(lever.apply_block(&BlockContext::new(51_400_000, updated_at + 3601)));

    // 51313000: the pool owes the venue (its first swap borrowed), so its debt is valued anew
    // at every clock and every advance is a change; a repeated timestamp still is not.
    let snap = Snapshot::load(51_313_000);
    let t0 = snap.timestamp();
    let mut state = decoded(&snap, 0);
    assert!(!state.flamm().unwrap().router.venues[0]
        .morpho
        .position
        .borrow_shares
        .is_zero());
    assert!(!state.apply_block(&BlockContext::new(51_313_000, t0)));
    let at_t0 = quote(&state);
    assert!(state.apply_block(&BlockContext::new(51_313_001, t0 + 2)));
    assert!(!state.apply_block(&BlockContext::new(51_313_001, t0 + 2)));
    assert!(state.apply_block(&BlockContext::new(51_313_002, t0 + 4)));
    let at_t1 = quote(&state);
    let mut fresh = decoded(&snap, 0);
    fresh.apply_block(&BlockContext::new(51_313_002, t0 + 4));
    assert_eq!(quote(&fresh), at_t1);
    assert_eq!(state, fresh);
    // Back to t0: the same quote as before (the clock is not state).
    assert!(state.apply_block(&BlockContext::new(51_313_000, t0)));
    assert_eq!(quote(&state), at_t0);
    // The same pool with its shares in the venue returned to zero is quiet again.
    let mut flat = decoded(&snap, 0);
    let mut flamm = flat.flamm().unwrap().clone();
    flamm.router.venues[0]
        .morpho
        .position
        .borrow_shares = U256::ZERO;
    flamm.router.venues[0]
        .morpho
        .position
        .supply_shares = U256::ZERO;
    flat = flat.with_flamm(flamm);
    assert!(!flat.apply_block(&BlockContext::new(51_313_001, t0 + 2)));
    assert!(!flat.apply_block(&BlockContext::new(51_313_002, t0 + 4)));
}

/// The rate ceiling's verdict in `apply_block`: a ceiling every borrow up to the market's cash
/// passes, or none does, leaves the flag quiet on an unpositioned pool; a ceiling inside the
/// borrowable range moves with the IRM's adaptation every second and reports every advance.
#[test]
fn rate_ceiling_verdict_moves_the_flag() {
    let snap = Snapshot::load(51_302_915);
    let t0 = snap.timestamp();
    let base = decoded(&snap, 0);
    let venue = &base.flamm().unwrap().router.venues[0];
    assert!(venue.borrow_enabled && !venue.retired);
    let rate = |delta: U256| {
        venue
            .morpho
            .borrow_rate_after(delta, U256::ZERO, t0)
            .unwrap()
    };
    let (ok_low, at_zero) = rate(U256::ZERO);
    let (ok_high, at_cash) = rate(venue.morpho.free_liquidity());
    assert!(ok_low && ok_high && at_zero < at_cash);
    // The deployed ceiling admits the whole cash: quiet (the case of the block advance above).
    assert!(venue.max_borrow_rate_wad > at_cash);
    let with_cap = |cap: U256| {
        let mut flamm = base.flamm().unwrap().clone();
        flamm.router.venues[0].max_borrow_rate_wad = cap;
        base.clone().with_flamm(flamm)
    };
    for (cap, changes, what) in [
        (U256::ZERO, false, "no ceiling"),
        (at_cash << 1, false, "a ceiling well above the cash's rate"),
        (at_zero >> 1, false, "a ceiling well below the current rate"),
        ((at_zero + at_cash) >> 1, true, "a ceiling inside the borrowable range"),
    ] {
        let mut state = with_cap(cap);
        assert!(!state.apply_block(&BlockContext::new(51_302_915, t0)), "{what}: repeated");
        assert_eq!(state.apply_block(&BlockContext::new(51_302_916, t0 + 2)), changes, "{what}");
        assert_eq!(state.apply_block(&BlockContext::new(51_302_917, t0 + 4)), changes, "{what}");
        assert!(!state.apply_block(&BlockContext::new(51_302_917, t0 + 4)), "{what}: repeated");
    }
    // A ceiling the cash's rate just clears at `t0` no longer clears it once the IRM has adapted
    // upward for half an hour at full utilization (`AdaptiveCurveIrm._newRateAtTarget`): the
    // verdict flips to binding and the flag reports it, while the same advance without a
    // ceiling is quiet (the pool asset feed's round, 534 s old at `t0`, is inside its heartbeat
    // at both clocks).
    let mut capped = with_cap(at_cash + U256::from(1u8));
    let mut uncapped = with_cap(U256::ZERO);
    assert!(!uncapped.apply_block(&BlockContext::new(51_303_815, t0 + 1800)));
    assert!(capped.apply_block(&BlockContext::new(51_303_815, t0 + 1800)));
    assert!(capped
        .get_amount_out(BigUint::from(20_000u32), &cbbtc(), &usdc())
        .is_ok());
}

/// The Morpho oracle follows the SVR reveal with the clock: at the snapshot's timestamp the
/// latest primary round is revealed; a clock inside the cutoff of the latest round answers the
/// previous one, and a clock the carried ring cannot answer for refuses the venue.
#[test]
fn oracle_reveal_moves_with_the_clock() {
    let snap = Snapshot::load(51_302_915);
    let mut state = decoded(&snap, 0);
    let scale = U256::from(10u8).pow(U256::from(26u8));
    let price = |st: &FlammPoolState| {
        st.flamm().unwrap().router.venues[0]
            .morpho
            .oracle_price
    };
    assert_eq!(price(&state), scale * U256::from(7_837_541_366_038u64));
    // 3583 was recorded at 1789395165: at 1789395175 it is withheld, 3582 answers.
    state.apply_block(&BlockContext::new(51_302_914, 1_789_395_175));
    assert_eq!(price(&state), scale * U256::from(7_845_698_248_071u64));
    // Inside the secondary round's (3581) cutoff, the secondary answers.
    state.apply_block(&BlockContext::new(51_302_900, 1_789_395_050));
    assert_eq!(price(&state), scale * U256::from(7_837_829_036_529u64));
    // A ring round the reveal visits at the clock that the attributes do not carry: the venue
    // is refused, not guessed; a clock that does not visit it quotes.
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: HashMap::new(),
        deleted_attributes: ["feed:mo0:tx:3582".to_owned()].into(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    assert!(state.apply_block(&BlockContext::new(51_302_910, 1_789_395_100)));
    let e = state
        .get_amount_out(BigUint::from(20_000u32), &cbbtc(), &usdc())
        .unwrap_err();
    assert!(e.to_string().contains("Morpho oracle"), "{e}");
    assert!(state.apply_block(&BlockContext::new(51_302_915, snap.timestamp())));
    assert_eq!(price(&state), scale * U256::from(7_837_541_366_038u64));
    assert!(state
        .get_amount_out(BigUint::from(20_000u32), &cbbtc(), &usdc())
        .is_ok());
}

/// A delta that deletes a feed's rounds (a proxy rotation) leaves the component alive and
/// refusing; the rounds' return restores it; a malformed value is a transition error; unknown
/// attributes are carried without effect.
#[test]
fn deltas_rotate_refuse_and_recover() {
    let (cb, us) = (cbbtc(), usdc());
    let snap = Snapshot::load(51_313_000);
    let mut state = decoded(&snap, 0);
    let before = state.clone();
    let quote = |st: &FlammPoolState| st.get_amount_out(BigUint::from(20_000u32), &cb, &us);
    let out = quote(&state).unwrap().amount;
    let rounds = ["round", "answer", "started_at", "updated_at"].map(|n| format!("feed:asset:{n}"));
    let saved: HashMap<String, Bytes> = rounds
        .iter()
        .map(|n| (n.clone(), state.attributes()[n].clone()))
        .collect();
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: HashMap::from([(
            "feed:asset:aggregator".to_owned(),
            Bytes::from(vec![0x11u8; 20]),
        )]),
        deleted_attributes: rounds.iter().cloned().collect(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    let e = quote(&state).unwrap_err();
    assert!(matches!(e, SimulationError::RecoverableError(_)), "{e}");
    assert!(
        e.to_string()
            .contains("feed:asset:round"),
        "{e}"
    );
    assert!(matches!(state.core(), Err(DecodeError::Missing(_))));
    assert_eq!(state.fee(), 1.0);
    assert_ne!(state, before);
    // The new aggregator's first round restores the quote.
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: saved
            .into_iter()
            .chain([(
                "feed:asset:aggregator".to_owned(),
                before.attributes()["feed:asset:aggregator"].clone(),
            )])
            .collect(),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    assert_eq!(quote(&state).unwrap().amount, out);
    assert_eq!(state, before);
    // A word of the wrong width is a transition error.
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: HashMap::from([(
            "mm:0:market:0".to_owned(),
            Bytes::from(vec![0u8; 33]),
        )]),
        deleted_attributes: Default::default(),
    };
    assert!(state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .is_err());
    // A pool word the layout does not read, and an unknown name, change nothing.
    let mut state = before.clone();
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: HashMap::from([
            (
                "pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4520"
                    .to_owned(),
                Bytes::zero(32),
            ),
            ("something:else".to_owned(), Bytes::from(vec![1u8])),
        ]),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    assert_eq!(quote(&state).unwrap().amount, out);
    assert_eq!(state.flamm(), before.flamm());
}

/// A pending implementation upgrade makes the pool refuse once its `executableAt` is reached,
/// and a beacon upgrade that executed refuses on the implementation pin.
#[test]
fn scheduled_changes_and_upgrades_refuse() {
    let (cb, us) = (cbbtc(), usdc());
    let snap = Snapshot::load(51_313_000);
    let t0 = snap.timestamp();
    let mut state = decoded(&snap, 0);
    // FLAMMFactory slot 1: pendingImplementation (20 bytes) | executableAt (uint48 at byte 20).
    let mut word = [0u8; 32];
    word[12..32].copy_from_slice(&[0x22u8; 20]);
    let at = (t0 + 100).to_be_bytes();
    word[6..12].copy_from_slice(&at[2..]);
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: HashMap::from([(
            "factory:0x0000000000000000000000000000000000000000000000000000000000000001".to_owned(),
            Bytes::from(word.to_vec()),
        )]),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    assert_eq!(state.core().unwrap().scheduled_at, t0 + 100);
    assert!(state
        .get_amount_out(BigUint::from(20_000u32), &cb, &us)
        .is_ok());
    assert!(state.apply_block(&BlockContext::new(51_313_050, t0 + 100)));
    let e = state
        .get_amount_out(BigUint::from(20_000u32), &cb, &us)
        .unwrap_err();
    assert!(e.to_string().contains("scheduled"), "{e}");
    // The upgrade executes: the beacon's implementation word moves off the pinned one.
    let mut state = decoded(&snap, 0);
    let mut word = [0u8; 32];
    word[12..32].copy_from_slice(&[0x22u8; 20]);
    let delta = ProtocolStateDelta {
        component_id: POOL.to_owned(),
        updated_attributes: HashMap::from([(
            "factory:0x0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            Bytes::from(word.to_vec()),
        )]),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    assert!(matches!(state.core(), Err(DecodeError::Drift(_))), "{:?}", state.core());
    assert!(state
        .get_amount_out(BigUint::from(20_000u32), &cb, &us)
        .is_err());
}

/// The snapshot decoder fails closed, with a typed error, on a missing attribute, a codehash
/// the port does not model, a component whose kind disagrees with its id, wiring drift and a
/// venue set the statics do not describe.
#[test]
fn snapshot_decoding_fails_closed() {
    let snap = Snapshot::load(51_302_915);
    let base = snap.component(0);
    // The inclusion filter admits exactly the snapshots the decoder accepts.
    let with =
        |f: &dyn Fn(&mut ComponentWithState)| -> Result<FlammPoolState, InvalidSnapshotError> {
            let mut c = base.clone();
            f(&mut c);
            let admitted = flamm_filter(&c);
            let decoded = tokio_test::block_on(FlammPoolState::try_from_with_header(
                c,
                snap.header(),
                &HashMap::new(),
                &HashMap::new(),
                &DecoderContext::new(),
            ));
            assert_eq!(admitted, decoded.is_ok(), "{decoded:?}");
            decoded
        };
    assert!(with(&|_| {}).is_ok());
    assert!(flamm_filter(&snap.component(1)));
    let missing_static = with(&|c| {
        c.component
            .static_attributes
            .remove("hook_codehash");
    });
    assert!(
        matches!(missing_static, Err(InvalidSnapshotError::MissingAttribute(a)) if a == "hook_codehash")
    );
    let bad_pin = with(&|c| {
        c.component
            .static_attributes
            .insert("hook_codehash".into(), Bytes::zero(32));
    });
    assert!(
        matches!(&bad_pin, Err(InvalidSnapshotError::ValueError(m)) if m.contains("unregistered")),
        "{bad_pin:?}"
    );
    let wrong_kind = with(&|c| {
        c.component.static_attributes.insert(
            "component_kind".into(),
            Bytes::from(
                U256::from(1u8)
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        );
    });
    assert!(
        matches!(&wrong_kind, Err(InvalidSnapshotError::ValueError(m)) if m.contains("component_kind")),
        "{wrong_kind:?}"
    );
    let missing_word = with(&|c| {
        c.state
            .attributes
            .remove("mm:0:market:1");
    });
    assert!(
        matches!(missing_word, Err(InvalidSnapshotError::MissingAttribute(a)) if a == "mm:0:market:1")
    );
    let missing_kind = with(&|c| {
        c.state
            .attributes
            .remove("feed:mo0:kind");
    });
    assert!(matches!(missing_kind, Err(InvalidSnapshotError::MissingAttribute(_))));
    let drift = with(&|c| {
        c.state.attributes.insert(
            "pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4501".into(),
            Bytes::from(vec![0x33u8; 32]),
        );
    });
    assert!(
        matches!(&drift, Err(InvalidSnapshotError::ValueError(m)) if m.contains("drift")),
        "{drift:?}"
    );
    let two_venues = with(&|c| {
        c.state.attributes.insert(
            "router:0x627459f28fd627023883d9310c65240762faa343d3f2429d1746640d8d8a0577".into(),
            Bytes::from(
                U256::from(2u8)
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        );
    });
    assert!(
        matches!(&two_venues, Err(InvalidSnapshotError::ValueError(m)) if m.contains("venues.length")),
        "{two_venues:?}"
    );
    let wrong_tokens = with(&|c| c.component.tokens.reverse());
    assert!(
        matches!(&wrong_tokens, Err(InvalidSnapshotError::ValueError(m)) if m.contains("tokens")),
        "{wrong_tokens:?}"
    );
    let malformed = with(&|c| {
        c.state
            .attributes
            .insert("feed:asset:aggregator".into(), Bytes::from(vec![1u8; 7]));
    });
    assert!(
        matches!(&malformed, Err(InvalidSnapshotError::ValueError(m)) if m.contains("malformed")),
        "{malformed:?}"
    );
    let bad_id = with(&|c| {
        c.component.id = "0x1234".into();
        c.state.component_id = "0x1234".into();
    });
    assert!(matches!(&bad_id, Err(InvalidSnapshotError::ValueError(_))), "{bad_id:?}");
    // The statics alone.
    let st = Statics::parse(&base.component.id, &base.component.static_attributes).unwrap();
    assert_eq!(st.kind, VenueKind::Swap);
    assert_eq!(st.pool, Address::from_str(POOL).unwrap());
    assert_eq!(st.feed_mo0_max_sync_iterations, 20);
}

/// The words the pinned code writes non-zero when it constructs the contract are required, not
/// zero when absent: without one the snapshot is refused with `MissingAttribute` naming it, and
/// a delta that deletes one leaves the component alive and refusing every quote with the same
/// name (never a fill at a zero fee, cap, reservation price, dial, band or supply), recovering
/// when the word returns. A word the code may never write (a pending ceremony) still reads as
/// zero when absent.
#[test]
fn required_words_fail_closed() {
    const REQUIRED: &[(&str, &str)] = &[
        ("hook:0x0000000000000000000000000000000000000000000000000000000000000004", "fee params"),
        ("hook:0x0000000000000000000000000000000000000000000000000000000000000005", "vol params"),
        (
            "hook:0x0000000000000000000000000000000000000000000000000000000000000006",
            "inventory surcharge / half-lives",
        ),
        (
            "hook:0x000000000000000000000000000000000000000000000000000000000000000f",
            "reservationPriceWad",
        ),
        ("pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f450d", "phi / ltv"),
        (
            "pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f450f",
            "roomEpsilon / feeFloor",
        ),
        ("pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4510", "feeCapWad"),
        (
            "pool:0xe27b86aa3e64fe0cf7c9294fb8b6fb20a28e5f01ba99e4bca9e76b647cc44f25",
            "loans[0].swapPriceBandWad / feeFloorWad",
        ),
        ("pool:0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace02", "totalSupply"),
    ];
    let (cb, us) = (cbbtc(), usdc());
    for &block in SNAPSHOT_BLOCKS {
        let snap = Snapshot::load(block);
        let base = snap.component(0);
        let before = decoded(&snap, 0);
        let out = before
            .get_amount_out(BigUint::from(20_000u32), &cb, &us)
            .unwrap()
            .amount;
        for (name, what) in REQUIRED {
            let ctx = format!("block {block} {what} ({name})");
            assert!(before.attributes().contains_key(*name), "{ctx}: not in the snapshot");
            let mut c = base.clone();
            c.state.attributes.remove(*name);
            let refused = tokio_test::block_on(FlammPoolState::try_from_with_header(
                c,
                snap.header(),
                &HashMap::new(),
                &HashMap::new(),
                &DecoderContext::new(),
            ));
            assert!(
                matches!(&refused, Err(InvalidSnapshotError::MissingAttribute(a)) if a == name),
                "{ctx}: {refused:?}"
            );
            let mut state = before.clone();
            let delete = ProtocolStateDelta {
                component_id: POOL.to_owned(),
                updated_attributes: HashMap::new(),
                deleted_attributes: [(*name).to_owned()].into(),
            };
            state
                .delta_transition(delete, &HashMap::new(), &Balances::default())
                .unwrap();
            assert!(
                matches!(state.core(), Err(DecodeError::Missing(a)) if a == name),
                "{ctx}: {:?}",
                state.core()
            );
            let e = state
                .get_amount_out(BigUint::from(20_000u32), &cb, &us)
                .unwrap_err();
            assert!(
                matches!(&e, SimulationError::RecoverableError(m) if m.contains(name)),
                "{ctx}: {e}"
            );
            assert!(state
                .get_amount_out(BigUint::from(1_000_000u32), &us, &cb)
                .is_err());
            assert!(state
                .get_limits(cb.address.clone(), us.address.clone())
                .is_err());
            assert!(state.spot_price(&cb, &us).is_err());
            assert_eq!(state.fee(), 1.0);
            let restore = ProtocolStateDelta {
                component_id: POOL.to_owned(),
                updated_attributes: HashMap::from([(
                    (*name).to_owned(),
                    before.attributes()[*name].clone(),
                )]),
                deleted_attributes: Default::default(),
            };
            state
                .delta_transition(restore, &HashMap::new(), &Balances::default())
                .unwrap();
            assert_eq!(state, before, "{ctx}: not restored");
            assert_eq!(
                state
                    .get_amount_out(BigUint::from(20_000u32), &cb, &us)
                    .unwrap()
                    .amount,
                out
            );
        }
        // `pendingInvariantHook` (FLAMM_NS+19) is written only by a hook-set ceremony: absent,
        // it is the zero it has always been.
        let ceremony = "pool:0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4513";
        let mut state = before.clone();
        let delete = ProtocolStateDelta {
            component_id: POOL.to_owned(),
            updated_attributes: HashMap::new(),
            deleted_attributes: [ceremony.to_owned()].into(),
        };
        state
            .delta_transition(delete, &HashMap::new(), &Balances::default())
            .unwrap();
        assert_eq!(state.flamm(), before.flamm(), "block {block}");
        assert_eq!(
            state
                .get_amount_out(BigUint::from(20_000u32), &cb, &us)
                .unwrap()
                .amount,
            out
        );
    }
}

/// Clones are equal, a quote's post-state is not its pre-state, states of the two venues differ,
/// and the state survives a `typetag` round trip.
#[test]
fn clone_equality_and_serde() {
    let (cb, us) = (cbbtc(), usdc());
    let snap = Snapshot::load(51_313_000);
    let swap = decoded(&snap, 0);
    let lever = decoded(&snap, 1);
    let boxed: Box<dyn ProtocolSim> = swap.clone_box();
    assert!(ProtocolSim::eq(&swap, boxed.as_ref()));
    assert!(!ProtocolSim::eq(&swap, &lever));
    assert_ne!(swap, lever);
    let q = swap
        .get_amount_out(BigUint::from(20_000u32), &cb, &us)
        .unwrap();
    assert!(!ProtocolSim::eq(&swap, q.new_state.as_ref()));
    let json = serde_json::to_string(&boxed).unwrap();
    assert!(json.contains("FlammPoolState"));
    let back: Box<dyn ProtocolSim> = serde_json::from_str(&json).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), json);
    assert!(ProtocolSim::eq(&swap, back.as_ref()));
    // The stateless leverage hook survives the round trip as a present port (a unit struct
    // would read back as `None`).
    let b = back
        .as_any()
        .downcast_ref::<FlammPoolState>()
        .unwrap();
    assert!(b
        .flamm()
        .unwrap()
        .hooks
        .leverage
        .port()
        .is_ok());
    assert_eq!(
        back.get_amount_out(BigUint::from(20_000u32), &cb, &us)
            .unwrap()
            .amount,
        q.amount
    );
    // A zero input is the empty trade with the venue's base gas.
    let zero = swap
        .get_amount_out(BigUint::ZERO, &us, &cb)
        .unwrap();
    assert_eq!(zero.amount, BigUint::ZERO);
    assert_eq!(zero.gas, BigUint::from(GAS_SWAP_BUY));
    assert!(ProtocolSim::eq(&swap, zero.new_state.as_ref()));
    // A token outside the pair is refused whatever the size.
    let other = token("0x4200000000000000000000000000000000000006", "WETH", 18);
    assert!(swap
        .get_amount_out(BigUint::from(1u8), &other, &us)
        .is_err());
    assert!(swap
        .get_limits(other.address.clone(), us.address.clone())
        .is_err());
    assert!(swap.spot_price(&other, &us).is_err());
}
