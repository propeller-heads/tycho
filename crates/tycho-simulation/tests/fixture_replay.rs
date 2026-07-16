#[path = "../benches/common/mod.rs"]
mod common;

use num_bigint::BigUint;
use rstest::rstest;

#[rstest]
#[case::balancer_v2_2token("balancer_v2_2token")]
#[case::curve_3token("curve_3token")]
#[case::curve_4token("curve_4token")]
fn decode_and_simulate(#[case] fixture: &str) {
    let pools = common::load_pools(fixture);
    assert_eq!(
        pools.len(),
        1,
        "Expected exactly one decoded pool from {fixture} fixture, got {}",
        pools.len()
    );

    let (t_in, t_out) = common::pool_tokens(fixture);

    let state = pools
        .values()
        .next()
        .expect("pool count asserted to be 1");

    let amount_in = BigUint::from(1_000_000_000_000_000u64);
    let result = state
        .get_amount_out(amount_in, &t_in, &t_out)
        .unwrap_or_else(|e| {
            panic!(
                "get_amount_out failed for {fixture}: {} -> {}: {e:?}",
                t_in.symbol, t_out.symbol
            )
        });

    assert!(
        result.amount > BigUint::from(0u64),
        "Expected a positive amount_out for {fixture}: {} -> {}, got {}",
        t_in.symbol,
        t_out.symbol,
        result.amount
    );
}
