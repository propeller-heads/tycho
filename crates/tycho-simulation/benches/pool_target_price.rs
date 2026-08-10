//! Compares the closed-form `PoolTargetPrice` solver (`clmm_swap_to_price`, ~1 swap simulation)
//! against the generic Brent's-method search it replaced (up to 30 simulations) for the
//! slipstreams protocols. Pool fixture is the real WBTC/WETH pool used by the slipstreams
//! agreement tests (tick spacing 10, fee 500).

use std::str::FromStr;

use alloy::primitives::U256;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::{Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint},
    Bytes,
};
use tycho_simulation::evm::protocol::{
    aerodrome_slipstreams::state::AerodromeSlipstreamsState,
    utils::{
        slipstreams::{dynamic_fee_module::DynamicFeeConfig, observations::Observation},
        uniswap::tick_list::TickInfo,
    },
    velodrome_slipstreams::state::VelodromeSlipstreamsState,
};

fn wbtc() -> Token {
    Token::new(
        &Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
        "WBTC",
        8,
        0,
        &[Some(10_000)],
        Chain::Ethereum,
        100,
    )
}

fn weth() -> Token {
    Token::new(
        &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        "WETH",
        18,
        0,
        &[Some(10_000)],
        Chain::Ethereum,
        100,
    )
}

// Real WBTC/WETH pool state, shared with the state.rs agreement tests.
fn multi_tick_pool_fixture() -> (U256, Vec<TickInfo>) {
    let sqrt_price = U256::from_str("28437325270877025820973479874632004").unwrap();
    let ticks = vec![
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
    ];
    (sqrt_price, ticks)
}

fn aerodrome_pool() -> AerodromeSlipstreamsState {
    let (sqrt_price, ticks) = multi_tick_pool_fixture();
    AerodromeSlipstreamsState::new(
        "wbtc-weth-bench-pool".to_string(),
        1_000_000,
        377_952_820_878_029_838u128,
        sqrt_price,
        0,
        1,
        500,
        10,
        255830,
        ticks,
        vec![Observation::default()],
        DynamicFeeConfig::new(500, 10_000, 1, false, 0),
    )
    .expect("failed to build aerodrome bench pool")
}

fn velodrome_pool() -> VelodromeSlipstreamsState {
    let (sqrt_price, ticks) = multi_tick_pool_fixture();
    VelodromeSlipstreamsState::new(
        377_952_820_878_029_838u128,
        sqrt_price,
        500,
        0,
        10,
        255830,
        ticks,
    )
    .expect("failed to build velodrome bench pool")
}

/// Converts an f64 price (token_out/token_in) into the `Price` fraction `query_pool_swap`
/// expects, matching `crate::evm::query_pool_swap`'s own decimal-adjustment convention.
fn to_price(price_f64: f64, token_in: &Token, token_out: &Token) -> Price {
    let decimal_adj = 10_f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
    let price_no_decimals = price_f64 / decimal_adj;
    Price::new(BigUint::from((price_no_decimals * 1e18) as u128), BigUint::from(10u128.pow(18)))
}

/// Target 50bps below spot with a 1bp tolerance: a realistic caller request that crosses a few
/// ticks, so the generic search needs several simulations to converge.
fn target_price_params(
    pool: &dyn ProtocolSim,
    token_in: &Token,
    token_out: &Token,
) -> QueryPoolSwapParams {
    let spot = pool
        .spot_price(token_in, token_out)
        .expect("spot price should be computable");
    let target = to_price(spot * 0.995, token_in, token_out);
    QueryPoolSwapParams::new(
        token_in.clone(),
        token_out.clone(),
        SwapConstraint::PoolTargetPrice {
            target,
            tolerance: 0.0001,
            min_amount_in: None,
            max_amount_in: None,
        },
    )
}

fn bench_pool_target_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_target_price");
    let (token_in, token_out) = (wbtc(), weth());

    let aerodrome = aerodrome_pool();
    let aerodrome_params = target_price_params(&aerodrome, &token_in, &token_out);
    group.bench_function(BenchmarkId::new("aerodrome_slipstreams", "closed_form"), |b| {
        b.iter(|| {
            aerodrome
                .query_pool_swap(&aerodrome_params)
                .expect("closed form should succeed")
        });
    });
    group.bench_function(BenchmarkId::new("aerodrome_slipstreams", "generic_search"), |b| {
        b.iter(|| {
            tycho_simulation::evm::query_pool_swap::query_pool_swap(&aerodrome, &aerodrome_params)
                .expect("generic search should succeed")
        });
    });

    let velodrome = velodrome_pool();
    let velodrome_params = target_price_params(&velodrome, &token_in, &token_out);
    group.bench_function(BenchmarkId::new("velodrome_slipstreams", "closed_form"), |b| {
        b.iter(|| {
            velodrome
                .query_pool_swap(&velodrome_params)
                .expect("closed form should succeed")
        });
    });
    group.bench_function(BenchmarkId::new("velodrome_slipstreams", "generic_search"), |b| {
        b.iter(|| {
            tycho_simulation::evm::query_pool_swap::query_pool_swap(&velodrome, &velodrome_params)
                .expect("generic search should succeed")
        });
    });

    group.finish();
}

criterion_group!(benches, bench_pool_target_price);
criterion_main!(benches);
