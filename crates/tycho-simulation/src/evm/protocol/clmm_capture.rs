//! Differential output capture for the CLMM swap paths.
//!
//! Generates a deterministic corpus of pools and swap inputs for every protocol that uses
//! the shared Uniswap tick walk (uniswap_v3, uniswap_v4, velodrome_slipstreams,
//! aerodrome_slipstreams) and dumps the full observable result of `swap` and `get_limits`
//! as one line per case.
//!
//! Run it on two commits and diff the dumps to prove output equivalence:
//!
//! ```shell
//! CLMM_CAPTURE_PATH=/tmp/before.jsonl cargo test -p tycho-simulation --release \
//!     capture_clmm_outputs -- --ignored --nocapture
//! ```

use std::io::Write;

use alloy::primitives::{Sign, I256, U256};
use tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes};

use crate::evm::protocol::{
    aerodrome_slipstreams::state::AerodromeSlipstreamsState,
    uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
    uniswap_v4::state::{UniswapV4Fees, UniswapV4State},
    utils::{
        slipstreams::{dynamic_fee_module::DynamicFeeConfig, observations::Observation},
        uniswap::{
            tick_list::TickInfo,
            tick_math::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_TICK, MIN_TICK},
        },
    },
    velodrome_slipstreams::state::VelodromeSlipstreamsState,
};

/// xorshift64* — deterministic, dependency-free PRNG.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
        choices[self.below(choices.len() as u64) as usize]
    }
}

struct PoolBlueprint {
    ticks: Vec<TickInfo>,
    liquidity: u128,
    sqrt_price: U256,
    tick: i32,
}

fn generate_pool(rng: &mut Rng, spacing: u16) -> Option<PoolBlueprint> {
    let tick_count = 2 + rng.below(46) as usize;
    let mut indices: Vec<i32> = (0..tick_count)
        .map(|_| (rng.below(6001) as i32 - 3000) * spacing as i32)
        .collect();
    indices.sort_unstable();
    indices.dedup();
    if indices.len() < 2 {
        return None;
    }

    let ticks: Vec<TickInfo> = indices
        .iter()
        .map(|&index| {
            let magnitude = (rng.next() as u128) << rng.below(33);
            let net_liquidity =
                if rng.below(2) == 0 { magnitude as i128 } else { -(magnitude as i128) };
            TickInfo::new(index, net_liquidity).expect("indices are within tick range")
        })
        .collect();

    // 1 in 16 pools has zero liquidity to exercise the no-liquidity error path
    let liquidity = if rng.below(16) == 0 { 0u128 } else { (rng.next() as u128) << rng.below(43) };

    let first_index = ticks
        .first()
        .expect("at least two ticks")
        .index;
    let last_index = ticks
        .last()
        .expect("at least two ticks")
        .index;
    let span = (last_index - first_index) as u64 + 1;
    let current_tick = (first_index + rng.below(span) as i32).clamp(MIN_TICK + 1, MAX_TICK - 1);

    let base_sqrt_price =
        get_sqrt_ratio_at_tick(current_tick).expect("current tick is within range");
    let sqrt_price = base_sqrt_price + U256::from(rng.below(1_000_000));
    // Most pools report a tick consistent with the price; 1 in 8 carries a stale tick to
    // exercise the walk with inconsistent state.
    let tick = if rng.below(8) == 0 {
        current_tick
    } else {
        get_tick_at_sqrt_ratio(sqrt_price).expect("offset price stays within range")
    };

    Some(PoolBlueprint { ticks, liquidity, sqrt_price, tick })
}

fn swap_amounts(rng: &mut Rng) -> Vec<I256> {
    let mut amounts = Vec::new();
    for exponent in [0u32, 6, 12, 18, 24, 30] {
        let magnitude = U256::from(10u128.pow(exponent)) + U256::from(rng.below(1_000));
        for sign in [Sign::Positive, Sign::Negative] {
            amounts.push(
                I256::checked_from_sign_and_abs(sign, magnitude).expect("magnitude fits in I256"),
            );
        }
    }
    amounts
}

fn price_limits(rng: &mut Rng, blueprint: &PoolBlueprint) -> Vec<Option<U256>> {
    let offset_tick = |tick: i32, delta: i32| -> Option<U256> {
        get_sqrt_ratio_at_tick((tick + delta).clamp(MIN_TICK + 1, MAX_TICK - 1)).ok()
    };
    let near = 1 + rng.below(40) as i32;
    let far = 1000 + rng.below(100_000) as i32;
    vec![
        None,
        offset_tick(blueprint.tick, -near),
        offset_tick(blueprint.tick, near),
        offset_tick(blueprint.tick, -far),
        offset_tick(blueprint.tick, far),
        // invalid limit on purpose: equals the current price
        Some(blueprint.sqrt_price),
    ]
}

fn capture<S, G>(out: &mut impl Write, protocol: &str, seed: u64, swap_fn: S, limits_fn: G)
where
    S: Fn(bool, I256, Option<U256>) -> String,
    G: Fn(bool) -> String,
{
    let mut rng = Rng(seed);
    let blueprint_rng_state = rng.next();
    let mut case_rng = Rng(blueprint_rng_state);

    for direction in [true, false] {
        writeln!(out, "{protocol}|limits|z2o={direction}|{}", limits_fn(direction)).unwrap();
    }

    for amount in swap_amounts(&mut case_rng) {
        for direction in [true, false] {
            let line = swap_fn(direction, amount, None);
            writeln!(out, "{protocol}|swap|z2o={direction}|amt={amount}|limit=None|{line}")
                .unwrap();
        }
    }
}

#[test]
#[ignore = "differential capture tool, run explicitly with CLMM_CAPTURE_PATH set"]
fn capture_clmm_outputs() {
    let path = std::env::var("CLMM_CAPTURE_PATH")
        .unwrap_or_else(|_| "/tmp/clmm_capture.jsonl".to_string());
    let file = std::fs::File::create(&path).expect("create capture file");
    let mut out = std::io::BufWriter::new(file);

    let token_low = Bytes::from(vec![0x01u8; 20]);
    let token_high = Bytes::from(vec![0x02u8; 20]);

    let mut master = Rng(0x5EED_C0FFEE_u64);
    let mut pools_captured = 0u32;

    for pool_index in 0..400u32 {
        let spacing = master.pick(&[1u16, 10, 60, 200]);
        let pool_seed = master.next();
        let Some(blueprint) = generate_pool(&mut Rng(pool_seed), spacing) else { continue };
        let limits = price_limits(&mut master, &blueprint);

        // --- uniswap_v3 ---
        let fee = match spacing {
            1 => FeeAmount::Lowest,
            10 => FeeAmount::Low,
            60 => FeeAmount::Medium,
            _ => FeeAmount::High,
        };
        if let Ok(v3) = UniswapV3State::new(
            blueprint.liquidity,
            blueprint.sqrt_price,
            fee,
            blueprint.tick,
            blueprint.ticks.clone(),
        ) {
            for limit in &limits {
                let tag = format!("v3#{pool_index}|limit={limit:?}");
                capture(
                    &mut out,
                    &tag,
                    pool_seed,
                    |z2o, amount, _| format!("{:?}", v3.swap(z2o, amount, *limit)),
                    |z2o| {
                        let (a, b) =
                            if z2o { (&token_low, &token_high) } else { (&token_high, &token_low) };
                        format!("{:?}", v3.get_limits(a.clone(), b.clone()))
                    },
                );
            }
        }

        // --- uniswap_v4 ---
        let fees = UniswapV4Fees::new(
            (master.below(10_000)) as u32,
            (master.below(10_000)) as u32,
            (master.below(100_000)) as u32,
        );
        let lp_fee_override = if master.below(2) == 0 { None } else { Some(500u32) };
        if let Ok(v4) = UniswapV4State::new(
            blueprint.liquidity,
            blueprint.sqrt_price,
            fees,
            blueprint.tick,
            spacing as i32,
            blueprint.ticks.clone(),
        ) {
            for limit in &limits {
                let tag = format!("v4#{pool_index}|limit={limit:?}|ovr={lp_fee_override:?}");
                capture(
                    &mut out,
                    &tag,
                    pool_seed,
                    |z2o, amount, _| format!("{:?}", v4.swap(z2o, amount, *limit, lp_fee_override)),
                    |z2o| {
                        let (a, b) =
                            if z2o { (&token_low, &token_high) } else { (&token_high, &token_low) };
                        format!("{:?}", v4.get_limits(a.clone(), b.clone()))
                    },
                );
            }
        }

        // --- velodrome_slipstreams ---
        let default_fee = master.pick(&[100u32, 500, 3000, 10000]);
        let custom_fee = if master.below(2) == 0 { 0 } else { master.pick(&[400u32, 2500]) };
        if let Ok(velodrome) = VelodromeSlipstreamsState::new(
            blueprint.liquidity,
            blueprint.sqrt_price,
            default_fee,
            custom_fee,
            spacing as i32,
            blueprint.tick,
            blueprint.ticks.clone(),
        ) {
            for limit in &limits {
                let tag =
                    format!("velo#{pool_index}|limit={limit:?}|fee={default_fee}/{custom_fee}");
                capture(
                    &mut out,
                    &tag,
                    pool_seed,
                    |z2o, amount, _| format!("{:?}", velodrome.swap(z2o, amount, *limit)),
                    |z2o| {
                        let (a, b) =
                            if z2o { (&token_low, &token_high) } else { (&token_high, &token_low) };
                        format!("{:?}", velodrome.get_limits(a.clone(), b.clone()))
                    },
                );
            }
        }

        // --- aerodrome_slipstreams ---
        if let Ok(aerodrome) = AerodromeSlipstreamsState::new(
            format!("capture-{pool_index}"),
            1_000_000,
            blueprint.liquidity,
            blueprint.sqrt_price,
            0,
            1,
            default_fee,
            spacing as i32,
            blueprint.tick,
            blueprint.ticks.clone(),
            vec![Observation::default()],
            DynamicFeeConfig::new(3000, 10_000, 1),
        ) {
            for limit in &limits {
                let tag = format!("aero#{pool_index}|limit={limit:?}|fee={default_fee}");
                capture(
                    &mut out,
                    &tag,
                    pool_seed,
                    |z2o, amount, _| format!("{:?}", aerodrome.swap(z2o, amount, *limit)),
                    |z2o| {
                        let (a, b) =
                            if z2o { (&token_low, &token_high) } else { (&token_high, &token_low) };
                        format!("{:?}", aerodrome.get_limits(a.clone(), b.clone()))
                    },
                );
            }
        }

        pools_captured += 1;
    }

    out.flush().unwrap();
    println!("captured {pools_captured} pool blueprints to {path}");
    assert!(pools_captured > 350, "corpus generation degraded: {pools_captured} pools");
}
