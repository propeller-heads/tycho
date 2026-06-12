//! Compares the native `TradeLimitPrice` solvers against the generic Brent search.
//!
//! Run with `cargo bench -p tycho-simulation --bench trade_limit_price`.

use std::str::FromStr;

use alloy::primitives::U256;
use criterion::{criterion_group, criterion_main, Criterion};
use num_bigint::BigUint;
use tycho_common::{
    hex_bytes::Bytes,
    models::{token::Token, Chain},
    simulation::protocol_sim::{Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint},
};
use tycho_simulation::evm::{
    protocol::{
        uniswap_v2::state::UniswapV2State,
        uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
        utils::uniswap::tick_list::TickInfo,
    },
    query_pool_swap::query_pool_swap,
};

fn token(address: &str, symbol: &str, decimals: u32) -> Token {
    Token::new(
        &Bytes::from_str(address).unwrap(),
        symbol,
        decimals,
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
            tolerance: 0.001,
            min_amount_in: None,
            max_amount_in: None,
        },
    )
}

fn bench_v2(c: &mut Criterion) {
    let state = UniswapV2State::new(
        U256::from_str("6770398782322527849696614").unwrap(),
        U256::from_str("5124813135806900540214").unwrap(),
    );
    let token_in = token("0x0000000000000000000000000000000000000000", "T0", 18);
    let token_out = token("0x0000000000000000000000000000000000000001", "T1", 18);

    // Limit 1% below the effective spot price
    let reserve0_big = BigUint::from_bytes_be(&state.reserve0.to_be_bytes::<32>());
    let reserve1_big = BigUint::from_bytes_be(&state.reserve1.to_be_bytes::<32>());
    let limit = Price::new(
        reserve1_big * BigUint::from(9_970u32) * BigUint::from(99u32),
        reserve0_big * BigUint::from(10_000u32) * BigUint::from(100u32),
    );
    let params = trade_limit_params(token_in, token_out, limit);

    let mut group = c.benchmark_group("uniswap_v2_trade_limit_price");
    group.bench_function("native", |b| b.iter(|| state.query_pool_swap(&params).unwrap()));
    group.bench_function("brent", |b| b.iter(|| query_pool_swap(&state, &params).unwrap()));
    group.finish();
}

fn bench_v3(c: &mut Criterion) {
    // Real WBTC/WETH 0.05% pool data
    let sqrt_price = U256::from_str("28437325270877025820973479874632004").unwrap();
    let pool = UniswapV3State::new(
        377_952_820_878_029_838u128,
        sqrt_price,
        FeeAmount::Low,
        255830,
        vec![
            TickInfo::new(255760, 1_759_015_528_199_933).unwrap(),
            TickInfo::new(255770, 6_393_138_051_835_308).unwrap(),
            TickInfo::new(255780, 228_206_673_808_681).unwrap(),
            TickInfo::new(255820, 1_319_490_609_195_820).unwrap(),
            TickInfo::new(255830, 678_916_926_147_901).unwrap(),
            TickInfo::new(255840, 12_208_947_683_433_103).unwrap(),
            TickInfo::new(255850, 1_177_970_713_095_301).unwrap(),
            TickInfo::new(255860, 8_752_304_680_520_407).unwrap(),
            TickInfo::new(255880, 1_486_478_248_067_104).unwrap(),
            TickInfo::new(255890, 1_878_744_276_123_248).unwrap(),
            TickInfo::new(255900, 77_340_284_046_725_227).unwrap(),
        ],
    )
    .unwrap();
    let token_in = token("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599", "WBTC", 8);
    let token_out = token("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", "WETH", 18);

    // Limit 0.2% below the effective spot price g·s0²/(F·Q192)
    let sqrt_price_big = BigUint::from_bytes_be(&sqrt_price.to_be_bytes::<32>());
    let limit = Price::new(
        &sqrt_price_big * &sqrt_price_big * BigUint::from(999_500u32) * BigUint::from(998u32),
        (BigUint::from(1u8) << 192) * BigUint::from(1_000_000u32) * BigUint::from(1_000u32),
    );
    let params = trade_limit_params(token_in, token_out, limit);

    let mut group = c.benchmark_group("uniswap_v3_trade_limit_price");
    group.bench_function("native", |b| b.iter(|| pool.query_pool_swap(&params).unwrap()));
    group.bench_function("brent", |b| b.iter(|| query_pool_swap(&pool, &params).unwrap()));
    group.finish();
}

criterion_group!(benches, bench_v2, bench_v3);
criterion_main!(benches);
