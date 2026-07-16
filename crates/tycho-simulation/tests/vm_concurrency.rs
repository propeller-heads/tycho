#[path = "../benches/common/mod.rs"]
mod common;

use std::{sync::Arc, thread};

use num_bigint::BigUint;

/// Proves that calling `get_amount_out` concurrently on the globally-shared `SHARED_TYCHO_DB`
/// produces results identical to a single-threaded oracle.
///
/// The oracle is computed before spawning threads, so each thread independently re-derives the
/// full sequence and byte-for-byte compares its output to the reference. This acts as a
/// regression guard for future interior-mutability caching optimisations on `spot_price`.
#[test]
fn concurrent_get_amount_out_matches_single_threaded_oracle() {
    let pools = common::load_pools("balancer_v2_2token");
    assert_eq!(pools.len(), 1, "expected exactly one pool in balancer_v2_2token fixture");

    let state = pools
        .into_values()
        .next()
        .expect("pool count asserted to be 1");
    let (t_in, t_out) = common::pool_tokens("balancer_v2_2token");

    let amounts: Vec<BigUint> = (1..=20u64)
        .map(|i| BigUint::from(i) * BigUint::from(1_000_000_000_000_000u64))
        .collect();

    // Oracle: single-threaded reference, computed before any threads are spawned.
    let oracle: Vec<Option<BigUint>> = amounts
        .iter()
        .map(|a| {
            state
                .get_amount_out(a.clone(), &t_in, &t_out)
                .ok()
                .map(|r| r.amount)
        })
        .collect();

    // ProtocolSim is Send + Sync, so Arc<Box<dyn ProtocolSim>> is safe to share.
    let state = Arc::new(state);
    let amounts = Arc::new(amounts);
    let oracle = Arc::new(oracle);
    let t_in = Arc::new(t_in);
    let t_out = Arc::new(t_out);

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (state, amounts, oracle, t_in, t_out) = (
                Arc::clone(&state),
                Arc::clone(&amounts),
                Arc::clone(&oracle),
                Arc::clone(&t_in),
                Arc::clone(&t_out),
            );
            thread::spawn(move || {
                for (i, a) in amounts.iter().enumerate() {
                    let got = state
                        .get_amount_out(a.clone(), &t_in, &t_out)
                        .ok()
                        .map(|r| r.amount);
                    assert_eq!(got, oracle[i], "thread result diverged at index {i}");
                }
            })
        })
        .collect();

    for h in handles {
        h.join()
            .expect("worker thread panicked");
    }
}

/// Proves that racing the lazy `spot_price` compute-on-miss cache from many threads against a
/// genuinely COLD cache still produces results identical to a single-threaded oracle.
///
/// A pool decoded by `load_pools` is decoder-warmed, so its spot-price cache is already populated.
/// To exercise the compute-on-miss path under contention we instead take the `new_state` returned
/// by `get_amount_out`: for a normal (non-override) pool that swap clears the derived caches, so
/// the returned state has an EMPTY spot-price cache. The read-through insert on miss is idempotent
/// (same inputs always compute the same value), so even if multiple threads race the initial miss
/// and each recompute the value, every thread must still agree with an oracle computed on a
/// separate, single-threaded cold state.
#[test]
fn concurrent_spot_price_matches_single_threaded_oracle() {
    let (t_in, t_out) = common::pool_tokens("balancer_v2_2token");
    let amount = BigUint::from(1_000_000_000_000_000_000u64); // 1 unit, below the sell limit

    // Build a cold-cache state through the public API: get_amount_out clears the derived caches on
    // the returned new_state for normal pools, so its spot-price cache starts empty.
    let make_cold = || {
        let pools = common::load_pools("balancer_v2_2token");
        assert_eq!(pools.len(), 1, "expected exactly one pool in balancer_v2_2token fixture");
        pools
            .into_values()
            .next()
            .expect("pool count asserted to be 1")
            .get_amount_out(amount.clone(), &t_in, &t_out)
            .expect("get_amount_out")
            .new_state
    };

    // Oracle: post-swap spot price on a cold state, computed single-threaded.
    let oracle = make_cold()
        .spot_price(&t_in, &t_out)
        .expect("oracle spot price");

    // Race a SECOND cold state from 8 threads: the first reader computes-on-miss and inserts, the
    // rest race that miss; all must agree with the oracle.
    let state = Arc::new(make_cold());
    let t_in = Arc::new(t_in);
    let t_out = Arc::new(t_out);

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (state, t_in, t_out) = (Arc::clone(&state), Arc::clone(&t_in), Arc::clone(&t_out));
            thread::spawn(move || {
                for _ in 0..50 {
                    let got = state
                        .spot_price(&t_in, &t_out)
                        .expect("spot price");
                    assert_eq!(got, oracle, "concurrent spot price diverged from oracle");
                }
            })
        })
        .collect();

    for h in handles {
        h.join()
            .expect("worker thread panicked");
    }
}
