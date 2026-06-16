#[path = "../benches/common/mod.rs"]
mod common;

use num_bigint::BigUint;

#[test]
fn balancer_v2_2token_decode_and_simulate() {
    let pools = common::load_pools("balancer_v2_2token");
    assert_eq!(
        pools.len(),
        1,
        "Expected exactly one decoded pool from balancer_v2_2token fixture, got {}",
        pools.len()
    );

    let (t_in, t_out) = common::pool_tokens("balancer_v2_2token");

    let state = pools
        .values()
        .next()
        .expect("pool count asserted to be 1");

    let amount_in = BigUint::from(1_000_000_000_000_000u64);
    let result = state
        .get_amount_out(amount_in, &t_in, &t_out)
        .unwrap_or_else(|e| {
            panic!("get_amount_out failed for {} -> {}: {e:?}", t_in.symbol, t_out.symbol)
        });

    assert!(
        result.amount > BigUint::from(0u64),
        "Expected a positive amount_out for {} -> {}, got {}",
        t_in.symbol,
        t_out.symbol,
        result.amount
    );
}
