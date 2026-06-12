//! Shared fixtures, input generation, and the `BenchQuoter` abstraction for the
//! Uniswap V3 CLMM swap benchmarks.
//!
//! Design goals driven by the grill review:
//! - **Implementation-agnostic.** Every benchmark group and the replay-equality test are generic
//!   over [`BenchQuoter`], so a candidate swap engine plugs in by implementing the trait once and
//!   registering it.
//! - **Owned fixtures.** [`BenchQuoter::prepare`] builds an owned instance in the `iter_batched`
//!   setup closure (untimed); [`BenchQuoter::warm`] performs any per-block precompute there too
//!   (untimed in warm benches, the measured routine in the precompute bench, omitted in cold
//!   benches). The timed routine receives the owned, optionally-warmed fixture — so a lazy-cache
//!   candidate's build never lands inside the timed window.
//! - **Platform-independent inputs.** [`stratified_amounts`] uses only integer arithmetic over
//!   `BigUint`, so the same `(valid_limit, n, seed)` yields a bit-identical sequence on any
//!   platform/build — replay-equality safe.

// Shared by the `clmm_swap` bench and the `clmm_replay` test; each consumer uses
// a different subset of the API, so unused-item warnings here are expected.
#![allow(dead_code)]

use std::{fs, path::PathBuf, str::FromStr};

use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::Zero;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::Deserialize;
use tycho_common::{
    models::Chain,
    simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
    Bytes,
};
use tycho_simulation::{
    evm::protocol::{
        uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
        utils::uniswap::tick_list::TickInfo,
    },
    tycho_common::models::token::Token,
};

pub const SEED: u64 = 42;
pub const STRATA: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ZeroForOne,
    OneForZero,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::ZeroForOne => "0to1",
            Direction::OneForZero => "1to0",
        }
    }
}

/// One quote result, captured for both performance benches (output is
/// black-boxed) and replay-equality (outputs compared across implementations).
pub enum QuoteOutput {
    /// Full or partial swap. `new_state` is the post-swap pool state; partial
    /// fills (the `Ticks exceeded` path) also land here, carrying their partial
    /// result and resulting state.
    Filled { amount: BigUint, gas: BigUint, new_state: Box<dyn ProtocolSim>, partial: bool },
    /// An early bail-out that did almost no work (no-liquidity, fatal, overflow,
    /// price-limit). Carries a classification string for equality diffing.
    Bailed { class: String },
}

impl QuoteOutput {
    pub fn is_bail(&self) -> bool {
        matches!(self, QuoteOutput::Bailed { .. })
    }

    pub fn is_partial(&self) -> bool {
        matches!(self, QuoteOutput::Filled { partial: true, .. })
    }

    /// Bit-exact observable equality: identical fill/bail classification, and for
    /// fills identical amount, gas, partial flag, and resulting pool state
    /// (compared via [`ProtocolSim::eq`], which covers sqrt_price, tick,
    /// liquidity, and ticks).
    pub fn equivalent(&self, other: &QuoteOutput) -> bool {
        match (self, other) {
            (
                QuoteOutput::Filled { amount: a1, gas: g1, new_state: s1, partial: p1 },
                QuoteOutput::Filled { amount: a2, gas: g2, new_state: s2, partial: p2 },
            ) => a1 == a2 && g1 == g2 && p1 == p2 && s1.eq(s2.as_ref()),
            (QuoteOutput::Bailed { class: c1 }, QuoteOutput::Bailed { class: c2 }) => c1 == c2,
            _ => false,
        }
    }
}

/// Classifies a `get_amount_out` result into a [`QuoteOutput`]. The
/// `InvalidInput("Ticks exceeded", Some(partial))` variant is a real partial
/// swap and counts as `Filled { partial: true }`; all other errors are early
/// bail-outs.
fn classify(
    result: Result<tycho_common::simulation::protocol_sim::GetAmountOutResult, SimulationError>,
) -> QuoteOutput {
    match result {
        Ok(out) => QuoteOutput::Filled {
            amount: out.amount,
            gas: out.gas,
            new_state: out.new_state,
            partial: false,
        },
        Err(SimulationError::InvalidInput(_, Some(partial))) => QuoteOutput::Filled {
            amount: partial.amount,
            gas: partial.gas,
            new_state: partial.new_state,
            partial: true,
        },
        Err(SimulationError::InvalidInput(msg, None)) => {
            QuoteOutput::Bailed { class: format!("invalid:{msg}") }
        }
        Err(SimulationError::RecoverableError(msg)) => {
            QuoteOutput::Bailed { class: format!("recoverable:{msg}") }
        }
        Err(SimulationError::FatalError(msg)) => {
            QuoteOutput::Bailed { class: format!("fatal:{msg}") }
        }
    }
}

/// The pluggable swap engine under test. Candidates implement this and register
/// alongside the reference in every benchmark group and the replay test.
pub trait BenchQuoter: Sized {
    fn name() -> &'static str;

    /// Build an owned instance from the snapshot. Runs in `iter_batched` setup
    /// (untimed). For the reference this is `UniswapV3State::new`.
    fn prepare(pool: &LoadedPool) -> Self;

    /// Per-block precompute / cache warm. Untimed in warm benches, the measured
    /// routine in `clmm_precompute`, deliberately skipped in cold benches. The
    /// reference has no precompute, so this is a no-op.
    fn warm(&mut self, direction: Direction);

    /// Quote one swap. Errors are classified into [`QuoteOutput`].
    fn quote(&self, amount: &BigUint, direction: Direction) -> QuoteOutput;
}

/// Reference implementation: wraps `UniswapV3State::get_amount_out` exactly as
/// production calls it (including the per-call state clone). `warm` is a no-op.
pub struct ReferenceQuoter {
    state: UniswapV3State,
    token0: Token,
    token1: Token,
}

impl BenchQuoter for ReferenceQuoter {
    fn name() -> &'static str {
        "reference"
    }

    fn prepare(pool: &LoadedPool) -> Self {
        ReferenceQuoter {
            state: pool.state.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
        }
    }

    fn warm(&mut self, _direction: Direction) {}

    fn quote(&self, amount: &BigUint, direction: Direction) -> QuoteOutput {
        let (token_in, token_out) = match direction {
            Direction::ZeroForOne => (&self.token0, &self.token1),
            Direction::OneForZero => (&self.token1, &self.token0),
        };
        classify(
            self.state
                .get_amount_out(amount.clone(), token_in, token_out),
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenMeta {
    pub address: String,
    pub symbol: String,
    pub decimals: u32,
}

#[derive(Debug, Deserialize)]
pub struct TickSnapshot {
    pub index: i32,
    pub net_liquidity: String,
}

/// Subset of the persisted snapshot needed to rebuild a pool. Provenance-only
/// keys present in the JSON (`tick_spacing`, `truncated`, etc.) are ignored;
/// serde tolerates the extra fields.
#[derive(Debug, Deserialize)]
pub struct PoolSnapshot {
    pub pool_address: String,
    pub fee_tier: u32,
    pub token0: TokenMeta,
    pub token1: TokenMeta,
    pub liquidity: String,
    pub sqrt_price_x96: String,
    pub current_tick: i32,
    pub ticks: Vec<TickSnapshot>,
}

/// A pool snapshot resolved into a ready-to-simulate `UniswapV3State` plus the
/// two tokens and the per-direction window-bounded valid swap limits.
#[derive(Clone)]
pub struct LoadedPool {
    pub label: String,
    pub state: UniswapV3State,
    pub token0: Token,
    pub token1: Token,
    pub tick_count: usize,
    pub fee_tier: u32,
    /// 90% of `get_limits().0` for ZeroForOne — the largest amount that stays
    /// strictly inside the fetched tick window.
    pub valid_limit_0to1: BigUint,
    pub valid_limit_1to0: BigUint,
}

impl LoadedPool {
    pub fn valid_limit(&self, direction: Direction) -> &BigUint {
        match direction {
            Direction::ZeroForOne => &self.valid_limit_0to1,
            Direction::OneForZero => &self.valid_limit_1to0,
        }
    }
}

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data")
}

fn token_from_meta(meta: &TokenMeta) -> Token {
    let address = Bytes::from_str(&meta.address)
        .unwrap_or_else(|_| panic!("invalid token address {}", meta.address));
    Token::new(&address, &meta.symbol, meta.decimals, 0, &[Some(10_000)], Chain::Ethereum, 100)
}

fn fee_amount_from_tier(fee_tier: u32) -> FeeAmount {
    match fee_tier {
        100 => FeeAmount::Lowest,
        200 => FeeAmount::Lowest2,
        300 => FeeAmount::Lowest3,
        400 => FeeAmount::Lowest4,
        500 => FeeAmount::Low,
        2500 => FeeAmount::MediumLow,
        3000 => FeeAmount::Medium,
        5000 => FeeAmount::MediumHigh,
        10_000 => FeeAmount::High,
        other => panic!("unsupported fee tier {other}"),
    }
}

/// Window-bounded valid limit for one direction: 90% of the simulation's own
/// `get_limits` max amount_in. `get_limits` walks only the loaded ticks and
/// stops at the window edge, so this is exactly "consumable strictly inside the
/// fetched window". Computed in Rust (not the fetcher) to reuse the real swap
/// engine and stay deterministic across loads.
fn window_valid_limit(state: &UniswapV3State, token_in: &Token, token_out: &Token) -> BigUint {
    let limit_in = state
        .get_limits(token_in.address.clone(), token_out.address.clone())
        .map(|(limit_in, _)| limit_in)
        .unwrap_or_default();
    (limit_in * BigUint::from(9u32)) / BigUint::from(10u32)
}

fn pool_from_snapshot(snapshot: PoolSnapshot) -> LoadedPool {
    let token0 = token_from_meta(&snapshot.token0);
    let token1 = token_from_meta(&snapshot.token1);

    let ticks = snapshot
        .ticks
        .iter()
        .map(|tick| {
            let net_liquidity = i128::from_str(&tick.net_liquidity)
                .unwrap_or_else(|_| panic!("invalid net_liquidity {}", tick.net_liquidity));
            TickInfo::new(tick.index, net_liquidity)
                .unwrap_or_else(|err| panic!("invalid tick {}: {err:?}", tick.index))
        })
        .collect();

    let liquidity = u128::from_str(&snapshot.liquidity)
        .unwrap_or_else(|_| panic!("invalid liquidity {}", snapshot.liquidity));
    let sqrt_price = U256::from_str(&snapshot.sqrt_price_x96)
        .unwrap_or_else(|_| panic!("invalid sqrt_price {}", snapshot.sqrt_price_x96));

    let state = UniswapV3State::new(
        liquidity,
        sqrt_price,
        fee_amount_from_tier(snapshot.fee_tier),
        snapshot.current_tick,
        ticks,
    )
    .unwrap_or_else(|err| panic!("failed to build pool {}: {err:?}", snapshot.pool_address));

    let valid_limit_0to1 = window_valid_limit(&state, &token0, &token1);
    let valid_limit_1to0 = window_valid_limit(&state, &token1, &token0);

    let tick_count = snapshot.ticks.len();
    let label = format!(
        "{}-{}-{}bp",
        snapshot.token0.symbol,
        snapshot.token1.symbol,
        snapshot.fee_tier / 100
    );

    LoadedPool {
        label,
        state,
        token0,
        token1,
        tick_count,
        fee_tier: snapshot.fee_tier,
        valid_limit_0to1,
        valid_limit_1to0,
    }
}

#[derive(Debug, Deserialize)]
struct IndexEntry {
    file: String,
}

#[derive(Debug, Deserialize)]
struct PoolIndex {
    pools: Vec<IndexEntry>,
}

/// Loads every pool listed in `benches/data/index.json`, sorted by ascending
/// initialized-tick count so benchmark output reads thin-to-thick.
pub fn load_pools() -> Vec<LoadedPool> {
    let dir = data_dir();
    let index_raw = fs::read_to_string(dir.join("index.json"))
        .expect("benches/data/index.json missing — run data/fetch_pools.py");
    let index: PoolIndex = serde_json::from_str(&index_raw).expect("malformed index.json");

    let mut pools: Vec<LoadedPool> = index
        .pools
        .iter()
        .map(|entry| {
            let raw = fs::read_to_string(dir.join(&entry.file))
                .unwrap_or_else(|_| panic!("missing snapshot {}", entry.file));
            let snapshot: PoolSnapshot = serde_json::from_str(&raw)
                .unwrap_or_else(|err| panic!("malformed snapshot {}: {err}", entry.file));
            pool_from_snapshot(snapshot)
        })
        .collect();

    pools.sort_by_key(|pool| pool.tick_count);
    pools
}

/// A small, fee-tier-diverse subset of the corpus for the expensive per-stratum
/// and size-grid groups (the primary 1k group covers all pools). Picks the
/// thinnest and thickest pool in each fee tier present, capped at `max`.
pub fn representative_subset(pools: &[LoadedPool], max: usize) -> Vec<usize> {
    let mut tiers: Vec<u32> = pools
        .iter()
        .map(|p| p.fee_tier)
        .collect();
    tiers.sort_unstable();
    tiers.dedup();

    let mut chosen: Vec<usize> = Vec::new();
    for tier in tiers {
        let in_tier: Vec<usize> = pools
            .iter()
            .enumerate()
            .filter(|(_, p)| p.fee_tier == tier)
            .map(|(i, _)| i)
            .collect();
        if let Some(&thin) = in_tier.first() {
            chosen.push(thin);
        }
        if let Some(&thick) = in_tier.last() {
            if !chosen.contains(&thick) {
                chosen.push(thick);
            }
        }
    }
    chosen.truncate(max);
    chosen
}

/// Draws `n` stratified amounts from `(0, valid_limit]` using only integer
/// arithmetic, so the sequence is bit-identical across platforms/builds.
///
/// The range `1..=valid_limit` is split into [`STRATA`] log2-spaced strata by
/// bit length. Strata whose bit-length spans collide (small limits) are merged
/// so no stratum is empty. Amounts are drawn uniformly *within* each stratum's
/// integer range from a seeded `ChaCha8Rng`, round-robin across strata. Swap
/// cost grows with the log of the amount, so log2 strata give even coverage of
/// the tick-crossing cost curve.
pub fn stratified_amounts(valid_limit: &BigUint, n: usize, seed: u64) -> Vec<BigUint> {
    if valid_limit.is_zero() || n == 0 {
        return Vec::new();
    }

    let max_bits = valid_limit.bits();
    let boundaries = stratum_boundaries(max_bits, valid_limit);
    let stratum_count = boundaries.len();

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut amounts = Vec::with_capacity(n);
    for i in 0..n {
        let (lo, hi) = &boundaries[i % stratum_count];
        amounts.push(uniform_biguint(&mut rng, lo, hi));
    }
    amounts
}

/// Builds `STRATA` `(lo, hi)` integer ranges spanning `1..=valid_limit` by bit
/// length, merging ranges that would collapse for small limits so every
/// returned range is non-empty (`lo <= hi`).
fn stratum_boundaries(max_bits: u64, valid_limit: &BigUint) -> Vec<(BigUint, BigUint)> {
    let one = BigUint::from(1u32);
    let mut ranges: Vec<(BigUint, BigUint)> = Vec::with_capacity(STRATA);

    // Lower edge of the lowest stratum is 1; each stratum spans a contiguous
    // block of bit lengths so the union is exactly [1, valid_limit].
    let bits_per_stratum = max_bits.div_ceil(STRATA as u64).max(1);
    let mut lo = one.clone();
    let mut start_bit = 0u64;
    while start_bit < max_bits {
        let end_bit = (start_bit + bits_per_stratum).min(max_bits);
        // hi = min(2^end_bit - 1, valid_limit)
        let hi_pow = (&one << end_bit) - &one;
        let hi = if &hi_pow > valid_limit { valid_limit.clone() } else { hi_pow };
        if lo <= hi {
            ranges.push((lo.clone(), hi.clone()));
            lo = &hi + &one;
        }
        start_bit = end_bit;
    }

    if ranges.is_empty() {
        ranges.push((one, valid_limit.clone()));
    }
    ranges
}

/// Uniform integer in `[lo, hi]` from a `BigUint`-range rejection sampler.
fn uniform_biguint(rng: &mut ChaCha8Rng, lo: &BigUint, hi: &BigUint) -> BigUint {
    if lo >= hi {
        return lo.clone();
    }
    let span = (hi - lo) + BigUint::from(1u32);
    let bits = span.bits();
    let byte_len = bits.div_ceil(8) as usize;
    loop {
        let mut bytes = vec![0u8; byte_len];
        rng.fill(bytes.as_mut_slice());
        // Mask the top byte down to the span's bit length to keep rejection rare.
        let excess_bits = (byte_len as u64 * 8) - bits;
        if excess_bits > 0 {
            bytes[0] >>= excess_bits;
        }
        let candidate = BigUint::from_bytes_be(&bytes);
        if candidate < span {
            return lo + candidate;
        }
    }
}

/// Per pool×direction validation result, used to assert the timed loop never
/// hides early bail-outs and to report the partial-fill mix.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub label: String,
    pub direction: Direction,
    pub partial_fills: usize,
    pub full_fills: usize,
}

/// Runs `amounts` once through the reference, panicking on any early bail-out
/// (no-liquidity / fatal / overflow / price-limit) and on stratum degeneracy
/// (>50% identical amounts within any single stratum). Guarantees the timed
/// loop's black-boxed `.ok()` never silently swallows a no-work bail-out.
///
/// Returns the partial/full fill counts for reporting.
pub fn validate_pool_direction(
    pool: &LoadedPool,
    direction: Direction,
    amounts: &[BigUint],
) -> ValidationReport {
    assert_stratum_non_degenerate(&pool.label, direction, amounts);

    let quoter = ReferenceQuoter::prepare(pool);
    let mut partial_fills = 0usize;
    let mut full_fills = 0usize;
    for amount in amounts {
        match quoter.quote(amount, direction) {
            QuoteOutput::Filled { partial: true, .. } => partial_fills += 1,
            QuoteOutput::Filled { partial: false, .. } => full_fills += 1,
            QuoteOutput::Bailed { class } => {
                panic!(
                    "validation: {} {} produced an early bail-out ({class}) for amount {amount}; \
                     the timed loop would hide this. Tighten valid_limit or drop the pool.",
                    pool.label,
                    direction.label(),
                );
            }
        }
    }

    ValidationReport { label: pool.label.clone(), direction, partial_fills, full_fills }
}

fn assert_stratum_non_degenerate(label: &str, direction: Direction, amounts: &[BigUint]) {
    let stratum_count = STRATA.min(amounts.len().max(1));
    for stratum in 0..stratum_count {
        let in_stratum: Vec<&BigUint> = amounts
            .iter()
            .enumerate()
            .filter(|(i, _)| i % stratum_count == stratum)
            .map(|(_, a)| a)
            .collect();
        if in_stratum.len() < 2 {
            continue;
        }
        let mut counts = std::collections::HashMap::new();
        for amount in &in_stratum {
            *counts
                .entry((*amount).clone())
                .or_insert(0usize) += 1;
        }
        let max_identical = counts
            .values()
            .copied()
            .max()
            .unwrap_or(0);
        let ratio = max_identical as f64 / in_stratum.len() as f64;
        assert!(
            ratio <= 0.5,
            "validation: {label} {} stratum {stratum} is degenerate \
             ({max_identical}/{} identical amounts > 50%); valid_limit too small for log2 strata",
            direction.label(),
            in_stratum.len(),
        );
    }
}
