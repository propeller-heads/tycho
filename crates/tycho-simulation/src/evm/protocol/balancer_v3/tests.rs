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
    // The dataset predates the min-balance check, so none of its pools register one.
    let min_token_balances = entry["state"]["min_token_balances"]
        .as_array()
        .map(|_| uint_list(&entry["state"], "min_token_balances"))
        .unwrap_or_default();
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
        min_token_balances,
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
                "STABLE" => {
                    let max_in_scaled_18 = pool
                        .stable_max_swap_amount_in(index_in, index_out)
                        .expect("stable limit resolves");
                    assert!(
                        pool.stable_swap_keeps_balance_valid(
                            index_in,
                            index_out,
                            &max_in_scaled_18
                        )
                        .expect("predicate resolves"),
                        "stable pool {} limit itself violates the 10000x imbalance bound \
                         `StableMath.ensureBalancesWithinMaxImbalanceRange` enforces",
                        pool_id(entry)
                    );
                }
                _ => {}
            }
        }
    }
}

/// The stable bisection in [`BalancerV3State::stable_max_swap_amount_in`] must find the exact
/// boundary of `StableMath.ensureBalancesWithinMaxImbalanceRange`, not merely a safe amount inside
/// it: one wei more must already fail the same check.
#[test]
fn stable_limit_sits_at_the_imbalance_boundary() {
    let (timestamp, dataset) = load_dataset();
    let stable_pools: Vec<_> = dataset
        .iter()
        .filter(|entry| entry["state"]["pool_type"] == "STABLE")
        .collect();
    assert!(!stable_pools.is_empty(), "dataset carries no stable pools");

    for entry in stable_pools {
        let pool = build_state(entry, timestamp);
        let vault_headroom_cap = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
        for (index_in, index_out) in [(0usize, 1usize), (1, 0)] {
            let max_in_scaled_18 = pool
                .stable_max_swap_amount_in(index_in, index_out)
                .expect("stable limit resolves");
            if max_in_scaled_18 ==
                vault_headroom_cap.saturating_sub(pool.state_balances()[index_in])
            {
                // The pool is so far from its imbalance bound that Vault storage headroom, not
                // the imbalance check, is what capped the search; there is no boundary to probe.
                continue;
            }
            assert!(
                pool.stable_swap_keeps_balance_valid(index_in, index_out, &max_in_scaled_18)
                    .expect("predicate resolves"),
                "pool {} limit itself must satisfy the imbalance bound",
                pool_id(entry)
            );
            assert!(
                !pool
                    .stable_swap_keeps_balance_valid(
                        index_in,
                        index_out,
                        &(max_in_scaled_18 + U256::from(1u8))
                    )
                    .expect("predicate resolves"),
                "pool {} limit is not tight: one more wei still satisfies the imbalance bound",
                pool_id(entry)
            );
        }
    }
}

/// Builds a synthetic 3-token stable pool, standing in for one whose third token has drained to
/// zero balance without the pool itself being re-seeded.
fn stable_pool_with_balances(balances: Vec<U256>) -> BalancerV3State {
    let num_tokens = balances.len();
    let tokens: Vec<String> = (0..num_tokens)
        .map(|i| format!("0x{:040x}", i + 0xa))
        .collect();
    let base = BasePoolState {
        pool_address: "0x0000000000000000000000000000000000000f".to_string(),
        pool_type: "STABLE".to_string(),
        tokens: tokens.clone(),
        scaling_factors: vec![U256::from(1u8); num_tokens],
        token_rates: vec![uint_wad(); num_tokens],
        balances_live_scaled_18: balances,
        swap_fee: U256::ZERO,
        aggregate_swap_fee: U256::ZERO,
        total_supply: uint_wad() * U256::from(1_000u32),
        supports_unbalanced_liquidity: true,
        hook_type: None,
    };
    BalancerV3State::new(
        address("0x000000000000000000000000000000000000f0"),
        address(VAULT),
        PoolTypeAttribute { pool_type: BalancerPoolType::Stable, version: None },
        tokens
            .iter()
            .map(|t| address(t))
            .collect(),
        Vec::new(),
        0,
        PoolState::Stable(StableState {
            base,
            mutable: StableMutable { amp: U256::from(100_000u32) },
        }),
    )
}

/// A stable pool's third token draining to zero balance must not crash a swap between the other
/// two: `stable_math::compute_invariant` divides by every balance directly (not through a checked
/// helper), and `get_limits` only screens the two tokens actually being swapped.
#[test]
fn stable_pool_zero_balance_on_untouched_token_does_not_divide_by_zero() {
    let balances =
        vec![uint_wad() * U256::from(1_000u32), uint_wad() * U256::from(1_000u32), U256::ZERO];
    let pool = stable_pool_with_balances(balances);
    let token0 = token_at(&pool, 0);
    let token1 = token_at(&pool, 1);

    let (max_in, max_out) = pool
        .get_limits(token0.address, token1.address)
        .expect("limits resolve, rather than panicking");
    assert_eq!(
        (max_in, max_out),
        (BigUint::ZERO, BigUint::ZERO),
        "a pool with a drained third token cannot be swapped through at all"
    );
}

/// Builds a synthetic 50/50 weighted pool for exercising the v2 `MinTokenBalanceLib` cap, which
/// the recorded parity dataset predates and so never carries.
fn weighted_pool_with_min_balances(
    balances: [U256; 2],
    min_token_balances: Vec<U256>,
) -> BalancerV3State {
    let base = BasePoolState {
        pool_address: "0x0000000000000000000000000000000000000f".to_string(),
        pool_type: "WEIGHTED".to_string(),
        tokens: vec![
            "0x0000000000000000000000000000000000000a".to_string(),
            "0x0000000000000000000000000000000000000b".to_string(),
        ],
        // Both tokens are already 18-decimal, so no decimal scaling is needed.
        scaling_factors: vec![U256::from(1u8); 2],
        token_rates: vec![uint_wad(), uint_wad()],
        balances_live_scaled_18: balances.to_vec(),
        swap_fee: U256::ZERO,
        aggregate_swap_fee: U256::ZERO,
        total_supply: uint_wad() * U256::from(1_000u32),
        supports_unbalanced_liquidity: true,
        hook_type: None,
    };
    let weights = vec![uint_wad() / U256::from(2u8), uint_wad() / U256::from(2u8)];
    let tokens = base
        .tokens
        .iter()
        .map(|token| address(token))
        .collect();
    BalancerV3State::new(
        address("0x000000000000000000000000000000000000f0"),
        address(VAULT),
        PoolTypeAttribute {
            pool_type: BalancerPoolType::Weighted,
            version: Some("v2".to_string()),
        },
        tokens,
        min_token_balances,
        0,
        PoolState::Weighted(WeightedState::new(base, weights)),
    )
}

/// `1e18`, matching the `WAD` fixed-point scale everything here is expressed in.
fn uint_wad() -> U256 {
    U256::from(1_000_000_000_000_000_000u128)
}

/// A v2 weighted pool's per-token minimum balance can bind tighter than `MAX_IN_RATIO`: buying
/// down to a high minimum on the output side cannot spend anywhere near 30% of the input reserve
/// in a deep, evenly weighted pool.
#[test]
fn weighted_v2_min_balance_caps_input_tighter_than_ratio() {
    let balances = [uint_wad() * U256::from(1_000u32), uint_wad() * U256::from(1_000u32)];
    // Token 1 may not drop below 900 of the 1000 it holds; token 0 has no floor of its own.
    let min_balances = vec![U256::ZERO, uint_wad() * U256::from(900u32)];
    let pool = weighted_pool_with_min_balances(balances, min_balances);
    let token0 = token("0x0000000000000000000000000000000000000a");
    let token1 = token("0x0000000000000000000000000000000000000b");

    let (max_in, max_out) = pool
        .get_limits(token0.address.clone(), token1.address.clone())
        .expect("limits resolve");

    let ratio_cap = u256_to_biguint(uint_wad() * U256::from(300u32)); // 30% of 1000
    assert!(
        max_in < ratio_cap,
        "min-balance cap should bind tighter than MAX_IN_RATIO: got {max_in}, ratio cap {ratio_cap}"
    );
    assert!(
        max_out <= u256_to_biguint(uint_wad() * U256::from(100u32)),
        "must not exceed headroom"
    );

    let quoted = pool
        .get_amount_out(max_in, &token0, &token1)
        .expect("the reported limit must be quotable");
    let new_balance_out = quoted
        .new_state
        .as_any()
        .downcast_ref::<BalancerV3State>()
        .expect("new state is a BalancerV3State")
        .state_balances()[1];
    assert!(
        new_balance_out >= uint_wad() * U256::from(900u32),
        "swap at the reported limit must not push token 1 below its registered minimum, got \
         {new_balance_out}"
    );
}

/// A registered minimum of exactly zero (one token has no floor while its counterpart does) must
/// not be treated as "sell it down to zero balance": that is a singular point on the weighted
/// curve, and inverting `computeOutGivenExactIn` against it divides by zero.
#[test]
fn weighted_v2_zero_min_balance_on_output_token_does_not_divide_by_zero() {
    let balances = [uint_wad() * U256::from(1_000u32), uint_wad() * U256::from(1_000u32)];
    // Token 0 has no registered floor; token 1's floor is irrelevant to this direction.
    let min_balances = vec![U256::ZERO, uint_wad() * U256::from(900u32)];
    let pool = weighted_pool_with_min_balances(balances, min_balances);
    let token0 = token("0x0000000000000000000000000000000000000a");
    let token1 = token("0x0000000000000000000000000000000000000b");

    // Selling token1 for token0: index_out is token0, whose own minimum is zero.
    let (max_in, max_out) = pool
        .get_limits(token1.address, token0.address)
        .expect("limits resolve");

    let ratio_cap = u256_to_biguint(uint_wad() * U256::from(300u32)); // 30% of 1000
    assert_eq!(
        max_in, ratio_cap,
        "a zero minimum on the output token registers no constraint, so MAX_IN_RATIO alone caps \
         it"
    );
    assert!(max_out > BigUint::ZERO);
}

/// A weighted v2 pool already sitting at (or a whisker above) a registered minimum has no
/// quotable headroom left, so `get_limits` must report `(0, 0)` rather than a dust amount.
#[test]
fn weighted_v2_min_balance_dust_pool_returns_zero_limits() {
    let balances = [uint_wad() * U256::from(1_000u32), uint_wad() * U256::from(1_000u32)];
    // Token 1's balance already equals its own minimum: there is no headroom to sell into.
    let min_balances = vec![U256::ZERO, uint_wad() * U256::from(1_000u32)];
    let pool = weighted_pool_with_min_balances(balances, min_balances);
    let token0 = token("0x0000000000000000000000000000000000000a");
    let token1 = token("0x0000000000000000000000000000000000000b");

    let (max_in, max_out) = pool
        .get_limits(token0.address, token1.address)
        .expect("limits resolve");
    assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
}

/// A pool so thin that even its own largest allowed swap trades below the Vault's
/// `MINIMUM_TRADE_AMOUNT` must report `(0, 0)`, not surface `TradeAmountTooSmall` as an error.
#[test]
fn dust_pool_below_minimum_trade_amount_returns_zero_limits() {
    // 30% of a 1e6-scaled18 balance is 3e5, below the Vault's 1e6 floor.
    let balances =
        [uint_wad() / U256::from(1_000_000_000_000u64), uint_wad() * U256::from(1_000u32)];
    let pool = weighted_pool_with_min_balances(balances, Vec::new());
    let token0 = token("0x0000000000000000000000000000000000000a");
    let token1 = token("0x0000000000000000000000000000000000000b");

    let (max_in, max_out) = pool
        .get_limits(token0.address, token1.address)
        .expect("a dust pool must resolve limits, not error");
    assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
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
