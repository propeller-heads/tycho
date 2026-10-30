use std::{collections::HashMap, str::FromStr};

use num_bigint::BigUint;
use num_traits::Zero;
use serde::Deserialize;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{token::Token, Chain},
    simulation::{errors::SimulationError, protocol_sim::Balances},
};

use super::*;

const VOLATILE_MODULE: &str = "0x7949DE8d67E4F6b8509Bdbb43FF0503aF1247a7A";
const STABLE_MODULE: &str = "0x0195fC76Da986D66B2A468133a1be41E90893Da6";

fn token(address: &str, decimals: u32) -> Token {
    Token::new(
        &Bytes::from_str(address).unwrap(),
        "T",
        decimals,
        0,
        &[Some(10_000)],
        Chain::Base,
        100,
    )
}

fn token0() -> Token {
    token("0x0000000000000000000000000000000000000001", 18)
}

fn token1() -> Token {
    token("0x0000000000000000000000000000000000000002", 18)
}

fn fees(custom_fee: u32, stable: bool) -> AeroBasicFees {
    AeroBasicFees {
        custom_fee,
        default_fee: if stable { 5 } else { 30 },
        fee_module: Bytes::from_str(if stable { STABLE_MODULE } else { VOLATILE_MODULE }).unwrap(),
        paused: false,
    }
}

fn volatile(custom_fee: u32) -> AeroBasicState {
    let reserves = (U256::from(1_000_000_000u64), U256::from(2_000_000_000u64));
    AeroBasicState::new(reserves, false, fees(custom_fee, false), (18, 18)).unwrap()
}

fn amount_out(state: &AeroBasicState, amount_in: u64) -> Result<BigUint, SimulationError> {
    state
        .get_amount_out(BigUint::from(amount_in), &token0(), &token1())
        .map(|result| result.amount)
}

#[derive(Deserialize)]
struct Capture {
    chain: String,
    block: u64,
    pools: Vec<CapturedPool>,
}

#[derive(Deserialize)]
struct CapturedPool {
    pool: String,
    stable: bool,
    token0: String,
    token1: String,
    decimals0: u32,
    decimals1: u32,
    reserve0: String,
    reserve1: String,
    custom_fee: u32,
    default_fee: u32,
    fee_module: String,
    quotes: Vec<CapturedQuote>,
}

#[derive(Deserialize)]
struct CapturedQuote {
    token_in: String,
    amount_in: String,
    amount_out: String,
}

/// Every quote in the fixture is the pool's own `getAmountOut(amountIn, tokenIn)` read on-chain at
/// the capture block, for inputs from 1e-9 to 2x of the input reserve in both directions. The pools
/// cover volatile and stable curves, 18/18, 18/6 and 6/18 decimals, custom fees of 1, 30 and 100
/// bps and the 5/30 bps factory defaults, on Base and Arc.
#[test]
fn matches_onchain_get_amount_out_to_the_wei() {
    let captures: Vec<Capture> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/assets/aero_basic/onchain_quotes.json"
    )))
    .unwrap();

    let mut checked = 0;
    let mut zero_outputs = 0;
    for capture in &captures {
        for pool in &capture.pools {
            let token0 = token(&pool.token0, pool.decimals0);
            let token1 = token(&pool.token1, pool.decimals1);
            let state = AeroBasicState::new(
                (U256::from_str(&pool.reserve0).unwrap(), U256::from_str(&pool.reserve1).unwrap()),
                pool.stable,
                AeroBasicFees {
                    custom_fee: pool.custom_fee,
                    default_fee: pool.default_fee,
                    fee_module: Bytes::from_str(&pool.fee_module).unwrap(),
                    paused: false,
                },
                (pool.decimals0 as u8, pool.decimals1 as u8),
            )
            .unwrap();
            for quote in &pool.quotes {
                let (token_in, token_out) = if quote.token_in == pool.token0 {
                    (&token0, &token1)
                } else {
                    (&token1, &token0)
                };
                let amount_in = BigUint::from_str(&quote.amount_in).unwrap();
                let expected = BigUint::from_str(&quote.amount_out).unwrap();
                let context = format!(
                    "{} block {} pool {} amount_in {} from {}",
                    capture.chain, capture.block, pool.pool, quote.amount_in, quote.token_in
                );
                let result = state.get_amount_out(amount_in, token_in, token_out);
                if expected.is_zero() {
                    // A swap with no output reverts on-chain, so the quote is refused.
                    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{context}");
                    zero_outputs += 1;
                } else {
                    let result = result.unwrap_or_else(|err| panic!("{context}: {err}"));
                    assert_eq!(result.amount, expected, "{context}");
                }
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 154);
    assert!(zero_outputs < 10, "the fixture should mostly exercise non-zero outputs");
}

#[test]
fn custom_fee_resolution_follows_the_fee_module() {
    let no_fee = amount_out(&volatile(420), 1_000_000).unwrap();
    let default_fee = amount_out(&volatile(0), 1_000_000).unwrap();
    let custom_100 = amount_out(&volatile(100), 1_000_000).unwrap();

    assert_eq!(no_fee, BigUint::from(1_998_001u64));
    // 30 bps default: 1_000_000 - 3_000 in.
    assert_eq!(default_fee, BigUint::from(1_992_013u64));
    assert_eq!(custom_100, BigUint::from(1_978_041u64));
    // The factory ignores fee-module results above the 1000 bps cap and uses its default.
    assert_eq!(amount_out(&volatile(1_001), 1_000_000).unwrap(), default_fee);
}

#[test]
fn zero_fee_module_uses_the_default_fee() {
    let mut state = volatile(100);
    state.fee_module = Bytes::from([0u8; 20]);

    assert_eq!(
        amount_out(&state, 1_000_000).unwrap(),
        amount_out(&volatile(0), 1_000_000).unwrap()
    );
}

#[test]
fn unknown_fee_module_is_not_quoted() {
    let mut state = volatile(0);
    state.fee_module = Bytes::from([7u8; 20]);

    assert!(matches!(amount_out(&state, 1_000), Err(SimulationError::FatalError(_))));
}

#[test]
fn other_kinds_fee_module_is_not_quoted() {
    let mut state = volatile(0);
    state.fee_module = Bytes::from_str(STABLE_MODULE).unwrap();

    assert!(matches!(amount_out(&state, 1_000), Err(SimulationError::FatalError(_))));
}

#[test]
fn paused_pool_is_not_quoted() {
    let mut fees = fees(0, false);
    fees.paused = true;
    let state =
        AeroBasicState::new((U256::from(1_000u64), U256::from(1_000u64)), false, fees, (18, 18))
            .unwrap();

    assert!(matches!(amount_out(&state, 10), Err(SimulationError::RecoverableError(_))));
}

#[test]
fn input_too_small_for_any_output_is_rejected() {
    // 1 wei of token1 buys 0.5 wei of token0, which rounds to nothing.
    let result = volatile(0).get_amount_out(BigUint::from(1u64), &token1(), &token0());

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))));
}

#[test]
fn new_state_excludes_the_fee_from_the_input_reserve() {
    let state = volatile(100);

    let result = state
        .get_amount_out(BigUint::from(1_000_000u64), &token0(), &token1())
        .unwrap();
    let new_state = result
        .new_state
        .as_any()
        .downcast_ref::<AeroBasicState>()
        .unwrap();

    // 1% of the input goes to the pool's fee contract, not to its reserves.
    assert_eq!(new_state.reserve0, U256::from(1_000_000_000u64 + 990_000));
    assert_eq!(new_state.reserve1, U256::from(2_000_000_000u64 - 1_978_041));
}

#[test]
fn stable_math_does_not_overflow_on_large_reserves() {
    // With 1e30 reserves, the curve's `a * b` products exceed 256 bits, which the pool's 512-bit
    // `mulDiv` absorbs. Checked math overflows above ~4.9e28 and `mulDiv` above ~4.9e32.
    let reserve = U256::from(10u64).pow(U256::from(30u64));
    let state = AeroBasicState::new((reserve, reserve), true, fees(0, true), (18, 18)).unwrap();
    let checked = StablePool {
        reserve0: reserve,
        reserve1: reserve,
        decimals0: 18,
        decimals1: 18,
        math: StableMath::Checked,
    };

    let out = amount_out(&state, 1_000_000_000_000).unwrap();

    assert!(checked
        .get_amount_out(U256::from(1_000_000_000_000u64), true, 5)
        .is_err());
    // A balanced stable pool trades close to 1:1, minus the 5 bps fee.
    assert!(out > BigUint::from(999_000_000_000u64) && out < BigUint::from(1_000_000_000_000u64));
}

#[test]
fn stable_spot_price_matches_a_small_trade() {
    let state = AeroBasicState::new(
        (
            U256::from(10u64).pow(U256::from(24u64)),
            U256::from(3u64) * U256::from(10u64).pow(U256::from(23u64)),
        ),
        true,
        fees(420, true),
        (18, 18),
    )
    .unwrap();
    let amount_in = 10u64.pow(15);

    let spot = state
        .spot_price(&token0(), &token1())
        .unwrap();
    let executed = amount_out(&state, amount_in)
        .unwrap()
        .to_string()
        .parse::<f64>()
        .unwrap() /
        amount_in as f64;
    let reverse = state
        .spot_price(&token1(), &token0())
        .unwrap();

    assert!((spot - executed).abs() / spot < 1e-6, "spot {spot} vs executed {executed}");
    assert!((spot * reverse - 1.0).abs() < 1e-12);
}

#[test]
fn delta_transition_updates_fees_and_pause() {
    let mut state = volatile(0);
    let delta = ProtocolStateDelta {
        updated_attributes: HashMap::from([
            ("reserve0".to_string(), Bytes::from(vec![0x0f, 0x42, 0x40])),
            ("custom_fee".to_string(), Bytes::from(vec![100])),
            ("default_fee".to_string(), Bytes::from(vec![25])),
            ("paused".to_string(), Bytes::from(vec![1])),
        ]),
        ..Default::default()
    };

    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();

    assert_eq!(state.reserve0, U256::from(1_000_000u64));
    assert_eq!(state.custom_fee, 100);
    assert_eq!(state.default_fee, 25);
    assert!(state.paused);
}

#[test]
fn delta_transition_rejects_an_impossible_default_fee() {
    let mut state = volatile(0);
    let delta = ProtocolStateDelta {
        updated_attributes: HashMap::from([(
            "default_fee".to_string(),
            Bytes::from(10_000u32.to_be_bytes().to_vec()),
        )]),
        ..Default::default()
    };

    assert!(state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .is_err());
}
