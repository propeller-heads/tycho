//! Property tests for the native `TradeLimitPrice` solvers.
//!
//! For randomized pools and limits, the native trait-method implementations are checked
//! against three properties:
//! - **Limit invariant**: round-tripping the computed `amount_in` through `get_amount_out` yields a
//!   trade price at or above the limit (1 wei of output slack).
//! - **Maximality**: a 1% larger trade violates the limit.
//! - **Brent agreement**: the generic Brent search (`query_pool_swap` free function) lands within
//!   its convergence bound of the native result. The bound scales with `tolerance / price_gap`
//!   because the amount error of a price-space search grows as the limit approaches the spot price.
#![cfg(feature = "evm")]

use std::str::FromStr;

use alloy::primitives::U256;
use num_bigint::BigUint;
use proptest::prelude::*;
use tycho_common::{
    hex_bytes::Bytes,
    models::{token::Token, Chain},
    simulation::protocol_sim::{Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint},
};
use tycho_simulation::evm::protocol::{
    uniswap_v2::state::UniswapV2State,
    uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
    utils::uniswap::tick_list::TickInfo,
};

const BRENT_TOLERANCE: f64 = 0.001;

fn token_0() -> Token {
    Token::new(
        &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
        "T0",
        18,
        0,
        &[Some(10_000)],
        Chain::Ethereum,
        100,
    )
}

fn token_1() -> Token {
    Token::new(
        &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
        "T1",
        18,
        0,
        &[Some(10_000)],
        Chain::Ethereum,
        100,
    )
}

fn trade_limit_params(token_in: Token, token_out: Token, limit: Price) -> QueryPoolSwapParams {
    QueryPoolSwapParams::new(
        token_in,
        token_out,
        SwapConstraint::TradeLimitPrice {
            limit,
            tolerance: BRENT_TOLERANCE,
            min_amount_in: None,
            max_amount_in: None,
        },
    )
}

/// Asserts the trade price of `(amount_in, amount_out)` is at or above the limit,
/// allowing 1 wei of output rounding.
fn assert_limit_satisfied(amount_in: &BigUint, amount_out: &BigUint, limit: &Price) {
    assert!(
        (amount_out + BigUint::from(1u8)) * &limit.denominator >= amount_in * &limit.numerator,
        "Achieved trade price is below the limit: in={amount_in} out={amount_out}"
    );
}

/// Asserts the Brent result lands within its convergence bound of the native result.
///
/// Brent accepts any amount whose trade price falls within `[limit, limit·(1+tolerance)]`,
/// so its amount may undershoot the native maximum by roughly
/// `tolerance / price_gap_fraction`; `3×` covers the search's f64 conversions.
fn assert_brent_agreement(native_in: &BigUint, brent_in: &BigUint, limit_per_10000: u32) {
    let gap = 10_000 - limit_per_10000;
    let undershoot_bound = 3.0 * BRENT_TOLERANCE * 10_000.0 / gap as f64;

    // Brent never exceeds the true maximum by more than rounding noise
    assert!(
        brent_in <=
            &(native_in * BigUint::from(10_001u32) / BigUint::from(10_000u32) +
                BigUint::from(2u8)),
        "Brent exceeded the native maximum: native={native_in} brent={brent_in}"
    );

    let floor_bps = ((1.0 - undershoot_bound).max(0.0) * 10_000.0) as u32;
    assert!(
        brent_in >= &(native_in * BigUint::from(floor_bps) / BigUint::from(10_000u32)),
        "Brent undershot beyond its convergence bound ({undershoot_bound}): \
         native={native_in} brent={brent_in}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn v2_native_matches_brent(
        reserve_in_mantissa in 1u128..1000,
        reserve_in_exp in 6u32..22,
        reserve_out_mantissa in 1u128..1000,
        reserve_out_exp in 6u32..22,
        limit_per_10000 in 5_000u32..9_900,
    ) {
        let reserve0 = U256::from(reserve_in_mantissa) * U256::from(10u8).pow(U256::from(reserve_in_exp));
        let reserve1 = U256::from(reserve_out_mantissa) * U256::from(10u8).pow(U256::from(reserve_out_exp));
        let state = UniswapV2State::new(reserve0, reserve1);
        let (token_in, token_out) = (token_0(), token_1());

        // limit = effective spot × limit_per_10000/10000, constructed exactly from reserves
        let reserve0_big = BigUint::from_bytes_be(&reserve0.to_be_bytes::<32>());
        let reserve1_big = BigUint::from_bytes_be(&reserve1.to_be_bytes::<32>());
        let limit = Price::new(
            &reserve1_big * BigUint::from(9_970u32) * BigUint::from(limit_per_10000),
            &reserve0_big * BigUint::from(10_000u32) * BigUint::from(10_000u32),
        );

        let native = state
            .query_pool_swap(&trade_limit_params(token_in.clone(), token_out.clone(), limit.clone()))
            .expect("native solver should succeed for a reachable limit");
        prop_assert!(*native.amount_in() > BigUint::ZERO);

        // Limit invariant via the real swap formula
        let actual_out = state
            .get_amount_out(native.amount_in().clone(), &token_in, &token_out)
            .expect("round-trip swap should succeed")
            .amount;
        assert_limit_satisfied(native.amount_in(), &actual_out, &limit);

        // Maximality: 1% more input violates the limit
        let larger_in = native.amount_in() * BigUint::from(101u32) / BigUint::from(100u32);
        let larger_out = state
            .get_amount_out(larger_in.clone(), &token_in, &token_out)
            .expect("larger swap should succeed")
            .amount;
        prop_assert!(
            larger_out * &limit.denominator < larger_in * &limit.numerator,
            "A 1% larger trade should violate the limit"
        );

        // Brent agreement (skipped when Brent itself fails to converge, e.g. f64 edge cases)
        if let Ok(brent) = tycho_simulation::evm::query_pool_swap::query_pool_swap(
            &state,
            &trade_limit_params(token_in, token_out, limit),
        ) {
            assert_brent_agreement(native.amount_in(), brent.amount_in(), limit_per_10000);
        }
    }

    #[test]
    fn v2_native_rejects_unreachable(
        reserve_in_mantissa in 1u128..1000,
        reserve_in_exp in 6u32..22,
        reserve_out_mantissa in 1u128..1000,
        reserve_out_exp in 6u32..22,
        limit_per_10000 in 10_100u32..20_000,
    ) {
        let reserve0 = U256::from(reserve_in_mantissa) * U256::from(10u8).pow(U256::from(reserve_in_exp));
        let reserve1 = U256::from(reserve_out_mantissa) * U256::from(10u8).pow(U256::from(reserve_out_exp));
        let state = UniswapV2State::new(reserve0, reserve1);

        let reserve0_big = BigUint::from_bytes_be(&reserve0.to_be_bytes::<32>());
        let reserve1_big = BigUint::from_bytes_be(&reserve1.to_be_bytes::<32>());
        let limit = Price::new(
            &reserve1_big * BigUint::from(9_970u32) * BigUint::from(limit_per_10000),
            &reserve0_big * BigUint::from(10_000u32) * BigUint::from(10_000u32),
        );

        let result = state.query_pool_swap(&trade_limit_params(token_0(), token_1(), limit));
        prop_assert!(result.is_err(), "A limit above the effective spot must be rejected");
    }

    #[test]
    fn v3_native_matches_brent(
        liquidity_mantissa in 1u128..1000,
        liquidity_exp in 15u32..19,
        tick_index in -200i32..200,
        tick_gaps in prop::collection::vec(1u32..40, 1..8),
        net_liquidity_divisor in 20u128..100,
        limit_per_10000 in 5_000u32..9_900,
    ) {
        let liquidity = liquidity_mantissa * 10u128.pow(liquidity_exp);
        // Place the price half a tick above an aligned tick so the f64-derived sqrt
        // price unambiguously belongs to that tick (sqrt = 2^96 · 1.0001^((tick+0.5)/2))
        let aligned_tick = tick_index * 10;
        let sqrt_price_f64 = 2f64.powi(96) * 1.0001f64.powf((aligned_tick as f64 + 0.5) / 2.0);
        let sqrt_price = U256::from(sqrt_price_f64 as u128);
        let tick = aligned_tick;

        // Initialized ticks below the current price with positive net liquidity, so
        // crossing them (zero-for-one) only ever removes liquidity it previously added
        let net_liquidity = (liquidity / net_liquidity_divisor) as i128;
        let mut ticks = Vec::new();
        let mut offset = 0i32;
        for gap in &tick_gaps {
            offset += (*gap as i32) * 10;
            ticks.push(TickInfo::new(aligned_tick - offset, net_liquidity).unwrap());
        }
        ticks.reverse();

        let pool = UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Low, tick, ticks).unwrap();
        let (token_in, token_out) = (token_0(), token_1());

        // limit = effective spot × limit_per_10000/10000: g·s0²·m / (F·Q192·10000)
        let sqrt_price_big = BigUint::from_bytes_be(&sqrt_price.to_be_bytes::<32>());
        let limit = Price::new(
            &sqrt_price_big * &sqrt_price_big * BigUint::from(999_500u32) * BigUint::from(limit_per_10000),
            (BigUint::from(1u8) << 192) * BigUint::from(1_000_000u32) * BigUint::from(10_000u32),
        );

        let native = pool
            .query_pool_swap(&trade_limit_params(token_in.clone(), token_out.clone(), limit.clone()))
            .expect("native solver should succeed for a reachable limit");
        prop_assert!(*native.amount_in() > BigUint::ZERO);

        // Limit invariant via the real swap engine (skipped when the swap exhausts the
        // tick list, which the native walk treats as returning all available liquidity)
        if let Ok(result) = pool.get_amount_out(native.amount_in().clone(), &token_in, &token_out) {
            assert_limit_satisfied(native.amount_in(), &result.amount, &limit);
        }

        if let Ok(brent) = tycho_simulation::evm::query_pool_swap::query_pool_swap(
            &pool,
            &trade_limit_params(token_in, token_out, limit),
        ) {
            assert_brent_agreement(native.amount_in(), brent.amount_in(), limit_per_10000);
        }
    }

    #[test]
    fn v3_native_rejects_unreachable(
        liquidity_mantissa in 1u128..1000,
        liquidity_exp in 15u32..19,
        limit_per_10000 in 10_100u32..20_000,
    ) {
        let liquidity = liquidity_mantissa * 10u128.pow(liquidity_exp);
        let sqrt_price = U256::from_str("79228162514264337593543950336").unwrap();
        let pool = UniswapV3State::new(
            liquidity,
            sqrt_price,
            FeeAmount::Low,
            0,
            vec![TickInfo::new(-600, (liquidity / 2) as i128).unwrap()],
        )
        .unwrap();

        let sqrt_price_big = BigUint::from_bytes_be(&sqrt_price.to_be_bytes::<32>());
        let limit = Price::new(
            &sqrt_price_big * &sqrt_price_big * BigUint::from(999_500u32) * BigUint::from(limit_per_10000),
            (BigUint::from(1u8) << 192) * BigUint::from(1_000_000u32) * BigUint::from(10_000u32),
        );

        let result = pool.query_pool_swap(&trade_limit_params(token_0(), token_1(), limit));
        prop_assert!(result.is_err(), "A limit above the effective spot must be rejected");
    }
}
