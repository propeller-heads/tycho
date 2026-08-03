//! Parity check between `balancer-maths-rust` and on-chain Balancer V3 quotes.
//!
//! `vm:balancer_v3` currently simulates through the generic VM adapter, which executes the Vault
//! in REVM for every quote (measured mean 33.4 ms over 1600 integration-test samples). Curve moved
//! its hot path to native Rust math while keeping VM-based state extraction at decode time; this
//! test establishes whether the same move is viable for Balancer V3.
//!
//! The dataset is recorded from mainnet: each entry carries the pool state as the Vault and pool
//! getters report it at a fixed block, plus the `amountOut` that
//! `BatchRouter.querySwapExactIn` returns for a set of swaps at that same block. The test replays
//! those swaps through `balancer-maths-rust` and requires every result to match to the wei, so it
//! needs no RPC access and doubles as a regression baseline while the migration proceeds.
//!
//! Regenerate the dataset with
//! `tests/assets/balancer_v3/fetch_native_parity_dataset.py` (needs `cast` and an archive RPC).

use std::{collections::BTreeMap, fs, path::PathBuf};

use alloy::primitives::U256;
use balancer_maths_rust::{
    common::types::{BasePoolState, PoolState, PoolStateOrBuffer, SwapInput, SwapKind},
    pools::{
        stable::stable_data::{StableMutable, StableState},
        weighted::WeightedState,
    },
    vault::Vault,
};
use serde_json::Value;

const DATASET: &str = "tests/assets/balancer_v3/native_parity_dataset.json";

/// Parses a decimal string field into a `U256`.
///
/// The dataset stores every integer as a decimal string so it stays readable in review and does
/// not depend on how `alloy` happens to serialize `U256`.
fn uint(value: &Value, field: &str) -> U256 {
    let raw = value[field]
        .as_str()
        .unwrap_or_else(|| panic!("`{field}` must be a decimal string, got {}", value[field]));
    raw.parse()
        .unwrap_or_else(|e| panic!("`{field}` is not a valid U256: {raw} ({e})"))
}

fn uint_list(value: &Value, field: &str) -> Vec<U256> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("`{field}` must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .expect("array entries must be decimal strings")
                .parse()
                .expect("array entries must be valid U256")
        })
        .collect()
}

fn string_list(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("`{field}` must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .expect("array entries must be strings")
                .to_string()
        })
        .collect()
}

fn base_state(state: &Value) -> BasePoolState {
    BasePoolState {
        pool_address: state["pool_address"]
            .as_str()
            .expect("pool_address")
            .to_string(),
        pool_type: state["pool_type"]
            .as_str()
            .expect("pool_type")
            .to_string(),
        tokens: string_list(state, "tokens"),
        scaling_factors: uint_list(state, "scaling_factors"),
        token_rates: uint_list(state, "token_rates"),
        balances_live_scaled_18: uint_list(state, "balances_live_scaled_18"),
        swap_fee: uint(state, "swap_fee"),
        aggregate_swap_fee: uint(state, "aggregate_swap_fee"),
        total_supply: uint(state, "total_supply"),
        supports_unbalanced_liquidity: state["supports_unbalanced_liquidity"]
            .as_bool()
            .expect("supports_unbalanced_liquidity"),
        hook_type: state["hook_type"]
            .as_str()
            .map(str::to_string),
    }
}

/// Builds the pool state the maths library expects from a recorded dataset entry.
///
/// Panics on an unknown `pool_type`: the dataset is committed alongside this test, so an unhandled
/// type means the two drifted apart and silently skipping it would weaken the check.
fn pool_state(state: &Value) -> PoolState {
    let base = base_state(state);
    match state["pool_type"]
        .as_str()
        .expect("pool_type")
    {
        "WEIGHTED" => PoolState::Weighted(WeightedState::new(base, uint_list(state, "weights"))),
        "STABLE" => PoolState::Stable(StableState {
            base,
            mutable: StableMutable { amp: uint(state, "amp") },
        }),
        other => panic!("dataset carries pool type `{other}`, which this test cannot build"),
    }
}

#[test]
fn native_maths_match_onchain_quotes() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DATASET);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
    let dataset: Value = serde_json::from_str(&raw).expect("dataset is not valid JSON");
    let entries = dataset
        .as_array()
        .expect("dataset must be an array of pool entries");
    assert!(!entries.is_empty(), "dataset is empty");

    let vault = Vault::new();
    let mut compared = 0usize;
    let mut per_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures = Vec::new();

    for entry in entries {
        let state = &entry["state"];
        let pool = state["pool_address"]
            .as_str()
            .expect("pool_address");
        let kind = state["pool_type"]
            .as_str()
            .expect("pool_type");
        let built = pool_state(state);

        let swaps = entry["swaps"]
            .as_array()
            .expect("swaps must be an array");
        assert!(!swaps.is_empty(), "pool {pool} has no recorded swaps");

        for swap in swaps {
            compared += 1;
            *per_type
                .entry(kind.to_string())
                .or_default() += 1;

            let input = SwapInput {
                amount_raw: uint(swap, "amount"),
                swap_kind: SwapKind::GivenIn,
                token_in: swap["token_in"]
                    .as_str()
                    .expect("token_in")
                    .to_string(),
                token_out: swap["token_out"]
                    .as_str()
                    .expect("token_out")
                    .to_string(),
            };
            let expected = uint(swap, "chain");

            let state_or_buffer = PoolStateOrBuffer::Pool(Box::new(built.clone()));
            match vault.swap(&input, &state_or_buffer, None) {
                Ok(actual) if actual == expected => {}
                Ok(actual) => failures.push(format!(
                    "{kind} {pool} {} -> {}: amount_in {} gave {actual}, chain returned {expected}",
                    input.token_in, input.token_out, input.amount_raw
                )),
                Err(e) => failures.push(format!(
                    "{kind} {pool} {} -> {}: amount_in {} failed: {e:?}",
                    input.token_in, input.token_out, input.amount_raw
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
    println!("{compared} swaps matched on-chain quotes to the wei ({per_type:?})");
}
