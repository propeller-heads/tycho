use std::{sync::Arc, thread};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use num_bigint::BigUint;

#[path = "common/mod.rs"]
mod common;

const CALLS_PER_THREAD: u64 = 50;

fn bench_threads(c: &mut Criterion) {
    let pools = common::load_pools("balancer_v2_2token");
    assert_eq!(pools.len(), 1, "fixture must contain exactly one pool");
    let state: Arc<Box<dyn tycho_common::simulation::protocol_sim::ProtocolSim>> =
        Arc::new(pools.into_values().next().unwrap());

    let (t_in, t_out) = common::pool_tokens("balancer_v2_2token");
    let t_in = Arc::new(t_in);
    let t_out = Arc::new(t_out);
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let mut group = c.benchmark_group("get_amount_out_contended");
    for threads in [1usize, 2, 4, 8] {
        group.throughput(Throughput::Elements(threads as u64 * CALLS_PER_THREAD));
        group.bench_with_input(BenchmarkId::from_parameter(threads), &threads, |b, &n| {
            b.iter(|| {
                let handles: Vec<_> = (0..n)
                    .map(|_| {
                        let (state, t_in, t_out, amount) = (
                            state.clone(),
                            t_in.clone(),
                            t_out.clone(),
                            amount.clone(),
                        );
                        thread::spawn(move || {
                            for _ in 0..CALLS_PER_THREAD {
                                let _ = state.get_amount_out(amount.clone(), &t_in, &t_out);
                            }
                        })
                    })
                    .collect();
                for h in handles {
                    h.join().unwrap();
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_threads);
criterion_main!(benches);
