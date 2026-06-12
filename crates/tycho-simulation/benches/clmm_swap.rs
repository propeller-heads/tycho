//! Criterion benchmarks for the Uniswap V3 CLMM swap simulation, driven by a
//! frozen corpus of realistic tick data from the top mainnet V3 pools by volume
//! (see `benches/data/`, fetched by `fetch_pools.py`).
//!
//! Every group is generic over [`common::BenchQuoter`] so a candidate swap
//! engine plugs in by implementing the trait and adding one registration line.
//! All amounts are integer-only stratified draws over each pool/direction's
//! window-bounded `valid_limit`, pre-generated and validated (no early
//! bail-outs, non-degenerate strata) before any timing.
//!
//! Groups (see the hardened plan):
//! - `clmm_swap_1k` (primary, warm): 1000 quotes/pool/direction; setup clones state + warms +
//!   clones amounts (untimed); `Throughput::Elements(1000)`.
//! - `clmm_swap_1k_cold`: same routine, setup does NOT warm — cold-first cost.
//! - `clmm_precompute`: routine = `warm()` once (return state so drop is untimed); measures the
//!   once-per-block build. No throughput.
//! - `clmm_swap_per_stratum`: 100 same-stratum quotes × 10 strata, so each tick-crossing regime is
//!   measured separately (the regression-asymmetry lens). Representative subset only.
//! - `clmm_overhead`: 1000× state clone+box+drop — the per-call alloc/drop floor to subtract from
//!   `clmm_swap_1k` for math-only attribution.
//! - `clmm_swap_size_grid` (secondary): single quote across a size grid. Representative subset
//!   only.
//!
//! `clmm_swap_1k`, `clmm_swap_1k_cold`, `clmm_precompute`, and `clmm_overhead`
//! run over all pools; the per-stratum and size-grid groups run over a small
//! fee-tier-diverse subset (the expensive groups would otherwise blow up total
//! wall time at ~100 pools).

mod common;

use std::{hint::black_box, time::Duration};

use common::{
    load_pools, representative_subset, stratified_amounts, validate_pool_direction, BenchQuoter,
    Direction, LoadedPool, ReferenceQuoter, SEED, STRATA,
};
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BatchSize, BenchmarkGroup, BenchmarkId,
    Criterion, Throughput,
};
use num_bigint::BigUint;

const N_CALLS: u64 = 1000;
const PER_STRATUM_CALLS: u64 = 100;
const SUBSET_SIZE: usize = 6;
const DIRECTIONS: [Direction; 2] = [Direction::ZeroForOne, Direction::OneForZero];

/// Amounts for one pool/direction, generated once and validated. Returns `None`
/// when the direction has no usable depth (zero valid limit), so callers skip it.
fn validated_amounts(pool: &LoadedPool, direction: Direction, n: usize) -> Option<Vec<BigUint>> {
    let limit = pool.valid_limit(direction);
    if *limit == BigUint::ZERO {
        return None;
    }
    let amounts = stratified_amounts(limit, n, SEED);
    validate_pool_direction(pool, direction, &amounts);
    Some(amounts)
}

fn bench_id(pool: &LoadedPool, direction: Direction) -> String {
    format!("{}/{}/ticks={}", pool.label, direction.label(), pool.tick_count)
}

/// Runs `amounts.len()` quotes through `quoter`, black-boxing inputs and outputs.
fn run_quotes<Q: BenchQuoter>(quoter: &Q, amounts: &[BigUint], direction: Direction) {
    for amount in amounts {
        black_box(quoter.quote(black_box(amount), direction));
    }
}

/// Primary warm-path group: setup builds + warms the fixture (untimed); the
/// timed routine runs 1000 quotes on the warm, owned fixture.
fn bench_swap_1k<Q: BenchQuoter>(group: &mut BenchmarkGroup<'_, WallTime>, pools: &[LoadedPool]) {
    for pool in pools {
        for direction in DIRECTIONS {
            let Some(amounts) = validated_amounts(pool, direction, N_CALLS as usize) else {
                continue;
            };
            group.bench_with_input(
                BenchmarkId::new(Q::name(), bench_id(pool, direction)),
                &amounts,
                |b, amounts| {
                    b.iter_batched(
                        || {
                            let mut quoter = Q::prepare(pool);
                            quoter.warm(direction);
                            (quoter, amounts.clone())
                        },
                        |(quoter, amounts)| run_quotes(&quoter, &amounts, direction),
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }
}

/// Cold-first group: identical routine, but setup does NOT warm. For the
/// reference (no precompute) this equals the warm group; for a lazy-cache
/// candidate it captures the one cold build folded into the first quote.
fn bench_swap_1k_cold<Q: BenchQuoter>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    pools: &[LoadedPool],
) {
    for pool in pools {
        for direction in DIRECTIONS {
            let Some(amounts) = validated_amounts(pool, direction, N_CALLS as usize) else {
                continue;
            };
            group.bench_with_input(
                BenchmarkId::new(Q::name(), bench_id(pool, direction)),
                &amounts,
                |b, amounts| {
                    b.iter_batched(
                        || (Q::prepare(pool), amounts.clone()),
                        |(quoter, amounts)| run_quotes(&quoter, &amounts, direction),
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }
}

/// Precompute group: the timed routine is a single `warm()` call. The fixture is
/// returned from the routine so its drop happens outside the timed window.
fn bench_precompute<Q: BenchQuoter>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    pools: &[LoadedPool],
) {
    for pool in pools {
        for direction in DIRECTIONS {
            if *pool.valid_limit(direction) == BigUint::ZERO {
                continue;
            }
            group.bench_function(BenchmarkId::new(Q::name(), bench_id(pool, direction)), |b| {
                b.iter_batched(
                    || Q::prepare(pool),
                    |mut quoter| {
                        quoter.warm(direction);
                        quoter
                    },
                    BatchSize::LargeInput,
                );
            });
        }
    }
}

/// Per-stratum group: 100 same-stratum quotes so each tick-crossing regime is
/// timed separately. Stratum `s` is the amounts at indices `s, s+STRATA, ...`
/// from the full 1000-amount sequence (same generator, so identical to the
/// primary group's draws). Representative subset only.
fn bench_per_stratum<Q: BenchQuoter>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    pools: &[LoadedPool],
    subset: &[usize],
) {
    for &idx in subset {
        let pool = &pools[idx];
        for direction in DIRECTIONS {
            let Some(all) = validated_amounts(pool, direction, N_CALLS as usize) else {
                continue;
            };
            for stratum in 0..STRATA {
                let amounts: Vec<BigUint> = all
                    .iter()
                    .skip(stratum)
                    .step_by(STRATA)
                    .take(PER_STRATUM_CALLS as usize)
                    .cloned()
                    .collect();
                if amounts.is_empty() {
                    continue;
                }
                let id = format!("{}/s{stratum}", bench_id(pool, direction));
                group.bench_with_input(BenchmarkId::new(Q::name(), id), &amounts, |b, amounts| {
                    b.iter_batched(
                        || {
                            let mut quoter = Q::prepare(pool);
                            quoter.warm(direction);
                            (quoter, amounts.clone())
                        },
                        |(quoter, amounts)| run_quotes(&quoter, &amounts, direction),
                        BatchSize::LargeInput,
                    );
                });
            }
        }
    }
}

/// Overhead floor: 1000× clone the reference fixture, box it, drop it. Subtract
/// from the primary group to attribute time to swap math vs per-call alloc/drop.
fn bench_overhead(group: &mut BenchmarkGroup<'_, WallTime>, pools: &[LoadedPool]) {
    for pool in pools {
        let id = format!("{}/ticks={}", pool.label, pool.tick_count);
        group.bench_function(BenchmarkId::new("clone_box_drop", id), |b| {
            b.iter(|| {
                for _ in 0..N_CALLS {
                    let boxed: Box<ReferenceQuoter> =
                        Box::new(black_box(ReferenceQuoter::prepare(pool)));
                    black_box(&boxed);
                    drop(boxed);
                }
            });
        });
    }
}

/// Size-grid group: a single quote at fixed fractions of the window valid limit,
/// pure-BigUint scaled (no float truncation). A skipped (zero) size is logged via
/// `eprintln` so the gradient table never silently drops a row.
fn bench_size_grid<Q: BenchQuoter>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    pools: &[LoadedPool],
    subset: &[usize],
) {
    // (numerator, denominator, label): fractions of valid_limit, exact integer math.
    let fractions: [(u64, u64, &str); 5] = [
        (1, 1_000_000, "0.0001pct"),
        (1, 10_000, "0.01pct"),
        (1, 1_000, "0.1pct"),
        (1, 100, "1pct"),
        (1, 10, "10pct"),
    ];

    for &idx in subset {
        let pool = &pools[idx];
        for direction in DIRECTIONS {
            let limit = pool.valid_limit(direction);
            if *limit == BigUint::ZERO {
                continue;
            }
            for (num, den, label) in fractions {
                let amount = (limit * BigUint::from(num)) / BigUint::from(den);
                if amount == BigUint::ZERO {
                    eprintln!(
                        "size_grid: {} {} {label} rounds to zero (valid_limit={limit}); skipped",
                        pool.label,
                        direction.label(),
                    );
                    continue;
                }
                let id = format!("{}/{}", bench_id(pool, direction), label);
                group.bench_with_input(BenchmarkId::new(Q::name(), id), &amount, |b, amount| {
                    b.iter_batched(
                        || {
                            let mut quoter = Q::prepare(pool);
                            quoter.warm(direction);
                            (quoter, amount.clone())
                        },
                        |(quoter, amount)| {
                            black_box(quoter.quote(black_box(&amount), direction));
                        },
                        BatchSize::LargeInput,
                    );
                });
            }
        }
    }
}

// Timing budget is sized so a full `cargo bench` run stays under ~15 min on the
// frozen corpus. The primary group runs every pool/direction; the secondary
// groups run a representative subset only (see `benchmark-notes.md` budget table).
fn primary(c: &mut Criterion) {
    let pools = load_pools();
    let mut group = c.benchmark_group("clmm_swap_1k");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_millis(1500));
    group.throughput(Throughput::Elements(N_CALLS));
    bench_swap_1k::<ReferenceQuoter>(&mut group, &pools);
    group.finish();
}

// Cold-first is identical to warm for the cacheless reference, so it runs only on
// the subset here; candidates with a lazy cache should widen it via a filter.
fn cold(c: &mut Criterion) {
    let pools = load_pools();
    let subset = representative_subset(&pools, SUBSET_SIZE);
    let subset_pools: Vec<LoadedPool> = subset
        .iter()
        .map(|&i| pools[i].clone())
        .collect();
    let mut group = c.benchmark_group("clmm_swap_1k_cold");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_millis(1500));
    group.throughput(Throughput::Elements(N_CALLS));
    bench_swap_1k_cold::<ReferenceQuoter>(&mut group, &subset_pools);
    group.finish();
}

fn precompute(c: &mut Criterion) {
    let pools = load_pools();
    let subset = representative_subset(&pools, SUBSET_SIZE);
    let subset_pools: Vec<LoadedPool> = subset
        .iter()
        .map(|&i| pools[i].clone())
        .collect();
    let mut group = c.benchmark_group("clmm_precompute");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_millis(1000));
    bench_precompute::<ReferenceQuoter>(&mut group, &subset_pools);
    group.finish();
}

fn per_stratum(c: &mut Criterion) {
    let pools = load_pools();
    let subset = representative_subset(&pools, SUBSET_SIZE);
    let mut group = c.benchmark_group("clmm_swap_per_stratum");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_millis(1000));
    group.throughput(Throughput::Elements(PER_STRATUM_CALLS));
    bench_per_stratum::<ReferenceQuoter>(&mut group, &pools, &subset);
    group.finish();
}

fn overhead(c: &mut Criterion) {
    let pools = load_pools();
    let subset = representative_subset(&pools, SUBSET_SIZE);
    let subset_pools: Vec<LoadedPool> = subset
        .iter()
        .map(|&i| pools[i].clone())
        .collect();
    let mut group = c.benchmark_group("clmm_overhead");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_millis(1000));
    group.throughput(Throughput::Elements(N_CALLS));
    bench_overhead(&mut group, &subset_pools);
    group.finish();
}

fn size_grid(c: &mut Criterion) {
    let pools = load_pools();
    let subset = representative_subset(&pools, SUBSET_SIZE);
    let mut group = c.benchmark_group("clmm_swap_size_grid");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_millis(1000));
    bench_size_grid::<ReferenceQuoter>(&mut group, &pools, &subset);
    group.finish();
}

criterion_group!(benches, primary, cold, precompute, per_stratum, overhead, size_grid);
criterion_main!(benches);
