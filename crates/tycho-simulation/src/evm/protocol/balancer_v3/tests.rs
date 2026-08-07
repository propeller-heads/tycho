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
        reclammv2::reclammv2_data::{ReClammV2Immutable, ReClammV2Mutable, ReClammV2State},
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

use crate::evm::protocol::{
    balancer_v3::{
        state::BalancerV3State,
        vm::{BalancerPoolType, PoolTypeAttribute},
    },
    u256_num::u256_to_biguint,
};

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
fn pool_state(state: &Value, timestamp: u64) -> (BalancerPoolType, PoolState) {
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
        "RECLAMM" => (
            BalancerPoolType::Reclamm,
            PoolState::ReClammV2(ReClammV2State {
                immutable: ReClammV2Immutable {
                    pool_address: base.pool_address.clone(),
                    tokens: base.tokens.clone(),
                },
                base,
                mutable: ReClammV2Mutable {
                    last_virtual_balances: uint_list(state, "last_virtual_balances"),
                    daily_price_shift_base: uint(state, "daily_price_shift_base"),
                    last_timestamp: uint(state, "last_timestamp"),
                    current_timestamp: U256::from(timestamp),
                    centeredness_margin: uint(state, "centeredness_margin"),
                    start_fourth_root_price_ratio: uint(state, "start_fourth_root_price_ratio"),
                    end_fourth_root_price_ratio: uint(state, "end_fourth_root_price_ratio"),
                    price_ratio_update_start_time: uint(state, "price_ratio_update_start_time"),
                    price_ratio_update_end_time: uint(state, "price_ratio_update_end_time"),
                },
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

/// Loads the recorded entries together with the block timestamp they were taken at.
fn load_dataset() -> (u64, Vec<Value>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DATASET);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
    let root: Value = serde_json::from_str(&raw).expect("dataset is not valid JSON");
    let timestamp = root["block_timestamp"]
        .as_str()
        .expect("block_timestamp must be a decimal string")
        .parse()
        .expect("block_timestamp must be a u64");
    let pools = root["pools"]
        .as_array()
        .expect("dataset must carry a `pools` array")
        .clone();
    (timestamp, pools)
}

fn build_state(entry: &Value, timestamp: u64) -> BalancerV3State {
    let (pool_type, state) = pool_state(&entry["state"], timestamp);
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
        // The dataset records no factory generation, which is also what a pool indexed before
        // generations were labelled reports.
        PoolTypeAttribute { pool_type, version: None },
        tokens,
        timestamp,
        state,
    )
}

#[test]
fn pool_type_attribute_splits_family_from_generation() {
    assert_eq!(
        PoolTypeAttribute::parse("WeightedPoolFactory@v1"),
        Ok(PoolTypeAttribute {
            pool_type: BalancerPoolType::Weighted,
            version: Some("v1".to_string()),
        })
    );
    // Only the first separator delimits the family, so labels may contain it themselves.
    assert_eq!(
        PoolTypeAttribute::parse("ReClammPoolFactory@2025-01-01@rc2"),
        Ok(PoolTypeAttribute {
            pool_type: BalancerPoolType::Reclamm,
            version: Some("2025-01-01@rc2".to_string()),
        })
    );
}

#[test]
fn pool_type_attribute_without_a_generation_still_resolves() {
    assert_eq!(
        PoolTypeAttribute::parse("StablePoolFactory"),
        Ok(PoolTypeAttribute { pool_type: BalancerPoolType::Stable, version: None })
    );
}

#[test]
fn pool_type_attribute_rejects_unquotable_and_malformed_values() {
    for marker in ["GyroECLPPoolFactory@v1", "QuantAMMWeightedPoolFactory", "@v1", ""] {
        assert!(
            PoolTypeAttribute::parse(marker).is_err(),
            "`{marker}` must not resolve to a pool family"
        );
    }
    // A separator with nothing after it is a packaging slip, not a versionless pool.
    assert!(PoolTypeAttribute::parse("WeightedPoolFactory@")
        .expect_err("empty version must be rejected")
        .contains("empty factory version"));
}

#[test]
fn get_amount_out_matches_onchain_quotes() {
    let (timestamp, dataset) = load_dataset();
    assert!(!dataset.is_empty(), "dataset is empty");

    let mut compared = 0usize;
    let mut failures = Vec::new();

    for entry in &dataset {
        let pool = build_state(entry, timestamp);
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
    let (timestamp, dataset) = load_dataset();
    let entry = &dataset[0];
    let pool = build_state(entry, timestamp);
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
    let (timestamp, dataset) = load_dataset();
    for entry in dataset {
        let pool = build_state(&entry, timestamp);
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

/// The reported limits must stay inside what the Vault enforces for each pool family.
///
/// Weighted maths rejects inputs above 30% of the input reserve. Stable maths accepts any input
/// the Vault can still store, so only the output side bounds it. reCLAMM caps a swap's output at
/// 99% of the reserve. And no family can pay out a full reserve — a limit beyond these turns
/// into routes that revert on chain.
#[test]
fn limits_stay_inside_what_the_vault_enforces() {
    let (timestamp, dataset) = load_dataset();
    for entry in &dataset {
        let pool = build_state(entry, timestamp);
        let kind = entry["state"]["pool_type"]
            .as_str()
            .expect("pool_type");
        let tokens = pool.token_addresses().to_vec();
        let raw_reserves = pool.raw_balances();

        for (index_in, index_out) in [(0usize, 1usize), (1, 0)] {
            let (max_in, max_out) = pool
                .get_limits(tokens[index_in].clone(), tokens[index_out].clone())
                .expect("limits resolve");
            // Equality happens: a stable pool's asymptotic output rounds to the whole raw
            // reserve even though it stays below the scaled-18 balance.
            let reserve_out = u256_to_biguint(raw_reserves[index_out]);
            assert!(
                max_out <= reserve_out,
                "pool {} promises {max_out} of token {index_out}, above its reserve \
                 {reserve_out}",
                pool_id(entry)
            );
            match kind {
                "WEIGHTED" => {
                    let cap = u256_to_biguint(raw_reserves[index_in]) * BigUint::from(30u32) /
                        BigUint::from(100u32);
                    assert!(
                        max_in <= cap,
                        "weighted pool {} offers {max_in} of token {index_in}, above the {cap} \
                         its maths accepts",
                        pool_id(entry)
                    );
                }
                "RECLAMM" => {
                    // A unit of slack absorbs the scaled-18 -> raw conversions on either side.
                    let cap = u256_to_biguint(raw_reserves[index_out]) * BigUint::from(99u32) /
                        BigUint::from(100u32) +
                        BigUint::from(1u32);
                    assert!(
                        max_out <= cap,
                        "reCLAMM pool {} pays out {max_out} of token {index_out}, above the \
                         {cap} its maths accepts",
                        pool_id(entry)
                    );
                }
                // Stable pools: the input side is bounded by Vault storage, not the maths, and
                // `limits_bound_a_quotable_amount` proves the reported limit still quotes.
                _ => {}
            }
        }
    }
}

#[test]
fn spot_price_is_positive_in_both_directions() {
    let (timestamp, dataset) = load_dataset();
    for entry in dataset {
        let pool = build_state(&entry, timestamp);
        for (base, quote) in [(0, 1), (1, 0)] {
            let price = pool
                .spot_price(&token_at(&pool, base), &token_at(&pool, quote))
                .expect("spot price resolves");
            assert!(price.is_finite() && price > 0.0, "spot price must be finite and positive");
        }
    }
}

/// reCLAMM shifts its price range with time, so a quote must depend on the timestamp the state was
/// read at. This guards the plumbing: if the timestamp stopped reaching the maths, quotes would
/// silently freeze at whatever moment the pool was decoded.
#[test]
fn reclamm_quotes_move_with_the_block_timestamp() {
    let (timestamp, dataset) = load_dataset();
    let reclamm: Vec<_> = dataset
        .iter()
        .filter(|e| e["state"]["pool_type"] == "RECLAMM")
        .collect();
    assert!(!reclamm.is_empty(), "dataset carries no reCLAMM pools");

    let mut moved = 0usize;
    for entry in &reclamm {
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

        // A full day later the range has shifted, so at least one pool must quote differently.
        let now = build_state(entry, timestamp)
            .get_amount_out(amount_in.clone(), &token_in, &token_out)
            .expect("quote at the recorded timestamp");
        let later = build_state(entry, timestamp + 86_400)
            .get_amount_out(amount_in, &token_in, &token_out)
            .expect("quote a day later");
        if now.amount != later.amount {
            moved += 1;
        }
    }

    assert!(
        moved > 0,
        "no reCLAMM pool changed its quote when the timestamp advanced a day; the timestamp is \
         probably not reaching the maths"
    );
}

#[test]
fn unknown_tokens_are_rejected() {
    let (timestamp, dataset) = load_dataset();
    let pool = build_state(&dataset[0], timestamp);
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
