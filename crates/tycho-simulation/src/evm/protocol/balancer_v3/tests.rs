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

use alloy::primitives::{I256, U256};
use balancer_maths_rust::{
    common::{
        maths::mul_down_fixed,
        types::{BasePoolState, PoolState},
    },
    pools::{
        quantamm::quantamm_data::{QuantAmmImmutable, QuantAmmMutable, QuantAmmState},
        reclammv2::reclammv2_data::{ReClammV2Immutable, ReClammV2Mutable, ReClammV2State},
        stable::stable_data::{StableMutable, StableState},
        weighted::{WeightedState, MAX_IN_RATIO},
    },
};
use num_bigint::BigUint;
use serde_json::Value;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
    Bytes,
};

use crate::evm::protocol::{
    balancer_v3::{
        state::BalancerV3State,
        vm::{parse_pool_type, BalancerPoolType},
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

/// Reads a list of signed decimal strings, as QuantAMM's weight multipliers can be negative.
fn int_list(value: &Value, field: &str) -> Vec<I256> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("`{field}` must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .expect("decimal string")
                .parse()
                .expect("valid I256")
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
fn pool_state(state: &Value, timestamp: u64) -> PoolState {
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
        "WEIGHTED" => PoolState::Weighted(WeightedState::new(base, uint_list(state, "weights"))),
        "STABLE" => PoolState::Stable(StableState {
            base,
            mutable: StableMutable { amp: uint(state, "amp") },
        }),
        "QUANT_AMM_WEIGHTED" => PoolState::QuantAmm(QuantAmmState {
            base,
            mutable: QuantAmmMutable {
                first_four_weights_and_multipliers: int_list(
                    state,
                    "first_four_weights_and_multipliers",
                ),
                second_four_weights_and_multipliers: int_list(
                    state,
                    "second_four_weights_and_multipliers",
                ),
                last_update_time: uint(state, "last_update_time"),
                last_interop_time: uint(state, "last_interop_time"),
                current_timestamp: U256::from(timestamp),
            },
            immutable: QuantAmmImmutable {
                max_trade_size_ratio: uint(state, "max_trade_size_ratio"),
            },
        }),
        "RECLAMM" => PoolState::ReClammV2(ReClammV2State {
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
    let state = pool_state(&entry["state"], timestamp);
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
        tokens,
        min_token_balances,
        timestamp,
        state,
    )
}

#[test]
fn pool_type_attribute_names_the_family_whose_maths_applies() {
    assert_eq!(parse_pool_type("WeightedPoolFactory"), Ok(BalancerPoolType::Weighted));
    assert_eq!(parse_pool_type("StablePoolFactory"), Ok(BalancerPoolType::Stable));
    assert_eq!(parse_pool_type("ReClammPoolFactory"), Ok(BalancerPoolType::Reclamm));
    assert_eq!(parse_pool_type("QuantAMMWeightedPoolFactory"), Ok(BalancerPoolType::QuantAmm));
}

#[test]
fn pool_type_attribute_rejects_unquotable_and_malformed_values() {
    // `WeightedPoolFactory@v1` is what an earlier package wrote. Such components predate the
    // `vault` attribute too, so they are rejected either way — but rejecting the marker keeps a
    // stale package from being quoted on the strength of a family name alone.
    for marker in ["GyroECLPPoolFactory", "LBPoolFactory", "WeightedPoolFactory@v1", ""] {
        assert!(parse_pool_type(marker).is_err(), "`{marker}` must not resolve to a pool family");
    }
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
                "QUANT_AMM_WEIGHTED" => {
                    // `onSwap` caps both sides of the trade at the pool's own ratio, so neither
                    // the reported input nor the output it buys may cross it.
                    let ratio = uint(&entry["state"], "max_trade_size_ratio");
                    let cap = |reserve: U256| {
                        u256_to_biguint(reserve) * u256_to_biguint(ratio) /
                            u256_to_biguint(uint_wad())
                    };
                    assert!(
                        max_in <= cap(raw_reserves[index_in]),
                        "QuantAMM pool {} offers {max_in} of token {index_in}, above the share of \
                         the reserve its maxTradeSizeRatio allows",
                        pool_id(entry)
                    );
                    assert!(
                        max_out <= cap(raw_reserves[index_out]),
                        "QuantAMM pool {} pays out {max_out} of token {index_out}, above the \
                         share of the reserve its maxTradeSizeRatio allows",
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

/// Builds a synthetic weighted pool for exercising the v2 `MinTokenBalanceLib` cap, which the
/// recorded parity dataset predates and so never carries.
fn weighted_pool_with_min_balances(
    balances: [U256; 2],
    weights: [U256; 2],
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
    let weights = weights.to_vec();
    let tokens = base
        .tokens
        .iter()
        .map(|token| address(token))
        .collect();
    BalancerV3State::new(
        address("0x000000000000000000000000000000000000f0"),
        address(VAULT),
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

/// The even split the min-balance tests use unless the weight ratio is what they are probing.
fn even_weights() -> [U256; 2] {
    [uint_wad() / U256::from(2u8), uint_wad() / U256::from(2u8)]
}

/// Inverting the minimum-balance bound raises `amountOut`'s share of the output reserve to
/// `weight_out / weight_in`, an exponent of 99 on a 99/1 pool. Draining that reserve down to a
/// floor of `1e12` would take more input than a `U256` holds, so the inversion overflows — which
/// says the minimum cannot bind before `MAX_IN_RATIO` does, not that the pool is unquotable.
///
/// The numbers are `0x48995dbdca50fa5346b0771d40a5ae7664262f7e` as it stood at block 25746755,
/// one of four mainnet pools that reported a fatal `MathOverflow` here during a live sweep.
#[test]
fn weighted_v2_unreachable_min_balance_falls_back_to_the_ratio_cap() {
    let balances = [
        U256::from_str("2995456711788000000000000").expect("balance parses"),
        U256::from_str("1968911072000000000000").expect("balance parses"),
    ];
    let weights =
        [uint_wad() * U256::from(99u8) / U256::from(100u8), uint_wad() / U256::from(100u8)];
    let floor = U256::from(1_000_000_000_000u64);
    let pool = weighted_pool_with_min_balances(balances, weights, vec![floor, floor]);
    let token0 = token("0x0000000000000000000000000000000000000a");
    let token1 = token("0x0000000000000000000000000000000000000b");

    // Selling the 1%-weight token for the 99%-weight one: the exponent is 0.99 / 0.01.
    let (max_in, max_out) = pool
        .get_limits(token1.address.clone(), token0.address.clone())
        .expect("an unreachable minimum must not fail the limit");

    assert_eq!(
        max_in,
        u256_to_biguint(mul_down_fixed(&balances[1], &MAX_IN_RATIO).expect("ratio cap")),
        "MAX_IN_RATIO alone should cap an input whose minimum-balance bound is unreachable"
    );
    assert!(max_out > BigUint::ZERO, "the pool still quotes at that limit");
    pool.get_amount_out(max_in, &token1, &token0)
        .expect("the reported limit must be quotable");
}

/// A v2 weighted pool's per-token minimum balance can bind tighter than `MAX_IN_RATIO`: buying
/// down to a high minimum on the output side cannot spend anywhere near 30% of the input reserve
/// in a deep, evenly weighted pool.
#[test]
fn weighted_v2_min_balance_caps_input_tighter_than_ratio() {
    let balances = [uint_wad() * U256::from(1_000u32), uint_wad() * U256::from(1_000u32)];
    // Token 1 may not drop below 900 of the 1000 it holds; token 0 has no floor of its own.
    let min_balances = vec![U256::ZERO, uint_wad() * U256::from(900u32)];
    let pool = weighted_pool_with_min_balances(balances, even_weights(), min_balances);
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
    let pool = weighted_pool_with_min_balances(balances, even_weights(), min_balances);
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
    let pool = weighted_pool_with_min_balances(balances, even_weights(), min_balances);
    let token0 = token("0x0000000000000000000000000000000000000a");
    let token1 = token("0x0000000000000000000000000000000000000b");

    let (max_in, max_out) = pool
        .get_limits(token0.address, token1.address)
        .expect("limits resolve");
    assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
}

/// The protocol's share of the swap fee leaves the pool, so a quote's reported state must have it
/// deducted in the same units the balances are held in. Only a token whose raw amount differs from
/// its scaled-18 one can tell the two apart, so this uses a 6-decimal input token — with 18-decimal
/// tokens at a rate of one the two are identical and any mix-up stays hidden.
///
/// The expectation follows the Vault: the fee is truncated to whole input-token units before being
/// charged, so it is `toScaled18(toRaw(totalFee) * aggregatePercentage)`.
#[test]
fn protocol_fee_leaves_the_pool_in_the_balances_own_units() {
    let scaling_factor = U256::from(1_000_000_000_000u64); // a 6-decimal input token
    let base = BasePoolState {
        pool_address: "0x00000000000000000000000000000000000000f0".to_string(),
        pool_type: "WEIGHTED".to_string(),
        tokens: vec![
            "0x000000000000000000000000000000000000000a".to_string(),
            "0x000000000000000000000000000000000000000b".to_string(),
        ],
        scaling_factors: vec![scaling_factor, U256::from(1u8)],
        token_rates: vec![uint_wad(), uint_wad()],
        balances_live_scaled_18: vec![uint_wad() * U256::from(1_000u32); 2],
        swap_fee: uint_wad() / U256::from(100u8), // 1%
        aggregate_swap_fee: uint_wad() / U256::from(4u8), // a quarter of it goes to the protocol
        total_supply: uint_wad() * U256::from(1_000u32),
        supports_unbalanced_liquidity: true,
        hook_type: None,
    };
    let weights = vec![uint_wad() / U256::from(2u8); 2];
    let pool = BalancerV3State::new(
        address("0x00000000000000000000000000000000000000f0"),
        address(VAULT),
        base.tokens
            .iter()
            .map(|t| address(t))
            .collect(),
        Vec::new(),
        0,
        PoolState::Weighted(WeightedState::new(base, weights)),
    );

    let amount_in_raw = U256::from(100_000_000u64); // 100 whole units of the 6-decimal token
    let before = pool.state_balances()[0];
    let quoted = pool
        .get_amount_out(u256_to_biguint(amount_in_raw), &token_at(&pool, 0), &token_at(&pool, 1))
        .expect("quote succeeds");
    let after = quoted
        .new_state
        .as_any()
        .downcast_ref::<BalancerV3State>()
        .expect("new state is a BalancerV3State")
        .state_balances()[0];

    let amount_in_scaled = amount_in_raw * scaling_factor;
    let total_fee_scaled = amount_in_scaled / U256::from(100u8);
    let protocol_fee_raw = (total_fee_scaled / scaling_factor) / U256::from(4u8);
    let protocol_fee_scaled = protocol_fee_raw * scaling_factor;
    assert_eq!(
        after - before,
        amount_in_scaled - protocol_fee_scaled,
        "the protocol fee must be deducted in scaled-18 units, not the input token's raw ones"
    );
}

/// A pool so thin that even its own largest allowed swap trades below the Vault's
/// `MINIMUM_TRADE_AMOUNT` must report `(0, 0)`, not surface `TradeAmountTooSmall` as an error.
#[test]
fn dust_pool_below_minimum_trade_amount_returns_zero_limits() {
    // 30% of a 1e6-scaled18 balance is 3e5, below the Vault's 1e6 floor.
    let balances =
        [uint_wad() / U256::from(1_000_000_000_000u64), uint_wad() * U256::from(1_000u32)];
    let pool = weighted_pool_with_min_balances(balances, even_weights(), Vec::new());
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

/// A reCLAMM pool small enough that its invariant rounds to zero has no price range to divide by.
/// `balancer-maths-rust` used to panic on one; since 0.5.0 it reports `ZeroInvariant`, which has to
/// reach the caller as an ordinary recoverable error rather than unwinding the thread quoting it.
///
/// The state is the reproduction that upstream took for its own regression test: balances far
/// enough off centre to trigger the price-range update, and small enough that
/// `(balance + virtual)` products fall below one WAD.
#[test]
fn reclamm_pool_with_a_zero_invariant_is_reported_not_panicked() {
    let dust = U256::from(100_000_000u64);
    let base = BasePoolState {
        pool_address: "0x00000000000000000000000000000000000000f0".to_string(),
        pool_type: "RECLAMM_V2".to_string(),
        tokens: vec![
            "0x000000000000000000000000000000000000000a".to_string(),
            "0x000000000000000000000000000000000000000b".to_string(),
        ],
        scaling_factors: vec![U256::from(1u8); 2],
        token_rates: vec![uint_wad(), uint_wad()],
        balances_live_scaled_18: vec![dust, U256::from(1u8)],
        swap_fee: U256::ZERO,
        aggregate_swap_fee: U256::ZERO,
        total_supply: dust,
        supports_unbalanced_liquidity: true,
        hook_type: None,
    };
    let pool = BalancerV3State::new(
        address("0x00000000000000000000000000000000000000f0"),
        address(VAULT),
        base.tokens
            .iter()
            .map(|t| address(t))
            .collect(),
        Vec::new(),
        200,
        PoolState::ReClammV2(ReClammV2State {
            immutable: ReClammV2Immutable {
                pool_address: base.pool_address.clone(),
                tokens: base.tokens.clone(),
            },
            base,
            mutable: ReClammV2Mutable {
                last_virtual_balances: vec![dust, dust],
                daily_price_shift_base: uint_wad(),
                last_timestamp: U256::from(100u64),
                current_timestamp: U256::from(200u64),
                // Above the pool's centeredness, so the price-range update runs.
                centeredness_margin: uint_wad() / U256::from(2u8),
                start_fourth_root_price_ratio: uint_wad(),
                end_fourth_root_price_ratio: uint_wad(),
                price_ratio_update_start_time: U256::ZERO,
                price_ratio_update_end_time: U256::ZERO,
            },
        }),
    );

    let error = pool
        .get_limits(pool.token_addresses()[0].clone(), pool.token_addresses()[1].clone())
        .expect_err("a pool with no price range cannot report a limit");
    match error {
        // Recoverable rather than fatal: a re-seeded pool quotes again.
        SimulationError::RecoverableError(reason) => assert!(
            reason.contains("no usable price range"),
            "expected the price-range report, got: {reason}"
        ),
        other => panic!("expected a recoverable error, got {other:?}"),
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
