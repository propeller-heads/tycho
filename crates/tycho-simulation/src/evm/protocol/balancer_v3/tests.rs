//! Parity check for [`BalancerV3State`] against recorded on-chain quotes.
//!
//! The dataset in `tests/assets/balancer_v3/native_parity_dataset.json` holds mainnet pool state as
//! the Vault and pool getters reported it at one block, together with the `amountOut` that
//! `BatchRouter.querySwapExactIn` returned for a spread of swap sizes at that same block. Replaying
//! it here exercises the whole native path — token indexing, raw-amount handling and the maths —
//! without needing an RPC or an indexed database, which also makes it a regression baseline for the
//! state mapping.
//!
//! Regenerate the dataset with `tests/assets/balancer_v3/fetch_native_parity_dataset.py`.
use std::{fs, path::PathBuf, str::FromStr};

use alloy::primitives::U256;
use balancer_maths_rust::{
    common::types::{BasePoolState, PoolState},
    pools::{
        stable::stable_data::{StableMutable, StableState},
        weighted::WeightedState,
    },
};
use num_bigint::BigUint;
use serde_json::Value;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use crate::evm::protocol::balancer_v3::{state::BalancerV3State, vm::BalancerPoolType};

const DATASET: &str = "tests/assets/balancer_v3/native_parity_dataset.json";
/// Placeholder Vault address: these tests never touch the VM, so the value is only carried around.
const VAULT: &str = "0xba1333333333a1ba1108e8412f11850a5c319ba9";

fn uint(value: &Value, field: &str) -> U256 {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("`{field}` must be a decimal string"))
        .parse()
        .unwrap_or_else(|e| panic!("`{field}` is not a valid U256: {e}"))
}

fn uint_list(value: &Value, field: &str) -> Vec<U256> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("`{field}` must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .expect("decimal string")
                .parse()
                .expect("valid U256")
        })
        .collect()
}

fn token_list(value: &Value) -> Vec<String> {
    value["tokens"]
        .as_array()
        .expect("tokens must be an array")
        .iter()
        .map(|item| {
            item.as_str()
                .expect("token address string")
                .to_string()
        })
        .collect()
}

/// Rebuilds the maths library's state from a recorded entry, standing in for what
/// `vm::read_pool_state` produces from live storage.
fn pool_state(state: &Value) -> (BalancerPoolType, PoolState) {
    let base = BasePoolState {
        pool_address: state["pool_address"]
            .as_str()
            .expect("pool_address")
            .to_string(),
        pool_type: state["pool_type"]
            .as_str()
            .expect("pool_type")
            .to_string(),
        tokens: token_list(state),
        scaling_factors: uint_list(state, "scaling_factors"),
        token_rates: uint_list(state, "token_rates"),
        balances_live_scaled_18: uint_list(state, "balances_live_scaled_18"),
        swap_fee: uint(state, "swap_fee"),
        aggregate_swap_fee: uint(state, "aggregate_swap_fee"),
        total_supply: uint(state, "total_supply"),
        supports_unbalanced_liquidity: state["supports_unbalanced_liquidity"]
            .as_bool()
            .expect("supports_unbalanced_liquidity"),
        hook_type: None,
    };
    match state["pool_type"]
        .as_str()
        .expect("pool_type")
    {
        "WEIGHTED" => (
            BalancerPoolType::Weighted,
            PoolState::Weighted(WeightedState::new(base, uint_list(state, "weights"))),
        ),
        "STABLE" => (
            BalancerPoolType::Stable,
            PoolState::Stable(StableState {
                base,
                mutable: StableMutable { amp: uint(state, "amp") },
            }),
        ),
        other => panic!("dataset carries pool type `{other}`, which the decoder cannot build"),
    }
}

fn address(raw: &str) -> Bytes {
    Bytes::from_str(raw).unwrap_or_else(|e| panic!("invalid address {raw}: {e}"))
}

/// Builds a token good enough for quoting: only the address is read by `BalancerV3State`, since the
/// maths works off the pool's own scaling factors rather than token decimals.
fn token(raw: &str) -> Token {
    Token::new(&address(raw), "TKN", 18, 0, &[Some(0)], Chain::Ethereum, 100)
}

fn load_dataset() -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DATASET);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
    serde_json::from_str::<Value>(&raw)
        .expect("dataset is not valid JSON")
        .as_array()
        .expect("dataset must be an array")
        .clone()
}

fn build_state(entry: &Value) -> BalancerV3State {
    let (pool_type, state) = pool_state(&entry["state"]);
    let tokens = state
        .base()
        .tokens
        .iter()
        .map(|token| address(token))
        .collect();
    BalancerV3State::new(
        address(
            entry["state"]["pool_address"]
                .as_str()
                .expect("pool_address"),
        ),
        address(VAULT),
        pool_type,
        tokens,
        state,
    )
}

#[test]
fn get_amount_out_matches_onchain_quotes() {
    let dataset = load_dataset();
    assert!(!dataset.is_empty(), "dataset is empty");

    let mut compared = 0usize;
    let mut failures = Vec::new();

    for entry in &dataset {
        let pool = build_state(entry);
        let kind = entry["state"]["pool_type"]
            .as_str()
            .expect("pool_type");
        let swaps = entry["swaps"]
            .as_array()
            .expect("swaps must be an array");
        assert!(!swaps.is_empty(), "a pool entry carries no swaps");

        for swap in swaps {
            compared += 1;
            let token_in = token(
                swap["token_in"]
                    .as_str()
                    .expect("token_in"),
            );
            let token_out = token(
                swap["token_out"]
                    .as_str()
                    .expect("token_out"),
            );
            let amount_in = BigUint::from_str(swap["amount"].as_str().expect("amount"))
                .expect("amount is a decimal string");
            let expected = BigUint::from_str(swap["chain"].as_str().expect("chain"))
                .expect("chain amount is a decimal string");

            match pool.get_amount_out(amount_in.clone(), &token_in, &token_out) {
                Ok(result) if result.amount == expected => {}
                Ok(result) => failures.push(format!(
                    "{kind} {} {} -> {}: amount_in {amount_in} gave {}, chain returned {expected}",
                    pool_id(entry),
                    token_in.address,
                    token_out.address,
                    result.amount
                )),
                Err(e) => failures.push(format!(
                    "{kind} {} {} -> {}: amount_in {amount_in} failed: {e:?}",
                    pool_id(entry),
                    token_in.address,
                    token_out.address
                )),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {compared} swaps diverged from the on-chain quote:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn swapping_moves_balances_in_the_right_direction() {
    let dataset = load_dataset();
    let entry = &dataset[0];
    let pool = build_state(entry);
    let swap = &entry["swaps"][0];
    let token_in = token(
        swap["token_in"]
            .as_str()
            .expect("token_in"),
    );
    let token_out = token(
        swap["token_out"]
            .as_str()
            .expect("token_out"),
    );
    let amount_in =
        BigUint::from_str(swap["amount"].as_str().expect("amount")).expect("decimal amount");

    let before = pool.state_balances().to_vec();
    let result = pool
        .get_amount_out(amount_in, &token_in, &token_out)
        .expect("quote succeeds");
    let after = result
        .new_state
        .as_any()
        .downcast_ref::<BalancerV3State>()
        .expect("new state is a BalancerV3State")
        .state_balances()
        .to_vec();

    let index_in = pool
        .token_index(&token_in.address)
        .expect("token_in is registered");
    let index_out = pool
        .token_index(&token_out.address)
        .expect("token_out is registered");
    assert!(after[index_in] > before[index_in], "input balance must grow");
    assert!(after[index_out] < before[index_out], "output balance must shrink");
}

#[test]
fn limits_bound_a_quotable_amount() {
    for entry in load_dataset() {
        let pool = build_state(&entry);
        let tokens = pool.token_addresses().to_vec();
        let (max_in, max_out) = pool
            .get_limits(tokens[0].clone(), tokens[1].clone())
            .expect("limits resolve");
        assert!(max_in > BigUint::ZERO, "limit must allow a non-zero input");
        assert!(max_out > BigUint::ZERO, "limit must allow a non-zero output");

        let quoted = pool
            .get_amount_out(max_in.clone(), &token_at(&pool, 0), &token_at(&pool, 1))
            .expect("the reported limit must be quotable");
        assert_eq!(quoted.amount, max_out, "limit output must match a quote at the limit");
    }
}

#[test]
fn spot_price_is_positive_in_both_directions() {
    for entry in load_dataset() {
        let pool = build_state(&entry);
        for (base, quote) in [(0, 1), (1, 0)] {
            let price = pool
                .spot_price(&token_at(&pool, base), &token_at(&pool, quote))
                .expect("spot price resolves");
            assert!(price.is_finite() && price > 0.0, "spot price must be finite and positive");
        }
    }
}

#[test]
fn unknown_tokens_are_rejected() {
    let dataset = load_dataset();
    let pool = build_state(&dataset[0]);
    let stranger = token("0x0000000000000000000000000000000000000001");
    let known = token_at(&pool, 0);

    assert!(pool
        .get_amount_out(BigUint::from(1u8), &stranger, &known)
        .is_err());
    assert!(pool
        .spot_price(&stranger, &known)
        .is_err());
    assert!(pool
        .get_limits(stranger.address.clone(), known.address)
        .is_err());
}

fn pool_id(entry: &Value) -> &str {
    entry["state"]["pool_address"]
        .as_str()
        .expect("pool_address")
}

fn token_at(pool: &BalancerV3State, index: usize) -> Token {
    Token::new(&pool.token_addresses()[index], "TKN", 18, 0, &[Some(0)], Chain::Ethereum, 100)
}
