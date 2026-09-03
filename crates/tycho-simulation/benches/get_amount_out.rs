use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use num_bigint::BigUint;
use tycho_common::simulation::protocol_sim::ProtocolSim;

#[path = "common/mod.rs"]
mod common;

const FIXTURES: [&str; 3] = ["balancer_v2_2token", "curve_3token", "curve_4token"];

fn single_pool(fixture: &str) -> Box<dyn ProtocolSim> {
    let pools = common::load_pools(fixture);
    assert_eq!(pools.len(), 1, "fixture {fixture} must contain exactly one pool");
    pools.into_values().next().unwrap()
}

fn bench_get_amount_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_amount_out");
    for fixture in FIXTURES {
        let state = single_pool(fixture);
        let (t_in, t_out) = common::pool_tokens(fixture);
        for (label, amount) in [
            ("small", BigUint::from(1_000_000_000_000_000u64)),
            ("large", BigUint::from(1_000_000_000_000_000_000u64)),
        ] {
            group.bench_with_input(BenchmarkId::new(fixture, label), &amount, |b, amt| {
                b.iter(|| {
                    let _ = state.get_amount_out(amt.clone(), &t_in, &t_out);
                });
            });
        }
    }
    group.finish();
}

fn bench_spot_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("spot_price");
    for fixture in FIXTURES {
        let state = single_pool(fixture);
        let (t_in, t_out) = common::pool_tokens(fixture);
        group.bench_function(fixture, |b| {
            b.iter(|| {
                let _ = state.spot_price(&t_in, &t_out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_get_amount_out, bench_spot_price);
criterion_main!(benches);
