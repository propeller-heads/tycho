#[path = "../benches/common/mod.rs"]
mod common;

use num_bigint::BigUint;

#[test]
fn balancer_v2_2token_decode_and_simulate() {
    let pools = common::load_pools("balancer_v2_2token");
    assert!(!pools.is_empty(), "Expected at least one decoded pool from balancer_v2_2token fixture");

    let (t_in, t_out) = common::pool_tokens("balancer_v2_2token");
    println!("token_in:  {} ({})", t_in.symbol, t_in.address);
    println!("token_out: {} ({})", t_out.symbol, t_out.address);

    let state = pools
        .values()
        .next()
        .expect("pools map is non-empty but iterator returned None");

    let amount_in = BigUint::from(1_000_000_000_000_000u64);
    let result = state
        .get_amount_out(amount_in, &t_in, &t_out)
        .expect("get_amount_out should succeed for a valid Balancer V2 pool");

    println!("amount_out: {}", result.amount);
    assert!(
        result.amount > BigUint::from(0u64),
        "Expected a positive amount_out, got {}",
        result.amount
    );
}
