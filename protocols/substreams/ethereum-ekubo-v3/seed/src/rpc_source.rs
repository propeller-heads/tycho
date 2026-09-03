//! The archive-node source: `PoolInitialized` logs name the pools, full storage dumps of the core
//! and the timed extensions hold their state.
//!
//! All pools live in one core contract, and the timed extensions (TWAMM, BoostedFees) keep their
//! pool state in their own contracts, all addressed arithmetically from the pool id, so every slot
//! of the four dumps can be attributed to a pool without walking the bitmaps.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

use alloy::{
    primitives::{address, aliases::U96, Address, B256, U256},
    sol,
    sol_types::SolEvent,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use ekubo_sdk::chain::evm::{float_sqrt_ratio_to_fixed, EvmPoolConfig, EvmPoolTypeConfig};
use num_bigint::BigInt;
use prost::Message as _;
use tracing::{info, warn};
use tycho_seed::{
    cli::Package,
    file,
    format::Header,
    rpc::{Dump, Rpc},
};

use crate::{
    layout::{
        self, bit_is_set, classify_core_slot, classify_timed_slot, tick_to_bitmap_word_and_index,
        time_to_bitmap_word_and_index, CoreSlot, PoolIds, TimedSlot, MAX_TICK, MIN_TICK,
    },
    packing::{
        parse_pool_balance_update, parse_pool_state, parse_tick_info, parse_time_info,
        parse_timed_pool_state, real_last_time, PoolState, TickInfo,
    },
    pb::ekubo::{
        block_transaction_events::transaction_events::pool_log::{
            PoolSnapshot, RateDeltaSnapshot, TickSnapshot, TimedSnapshot,
        },
        PoolSeed, Seed,
    },
    reserves,
    time::is_time_valid,
};

pub const CORE: Address = address!("0x00000000000014aA86C5d3c41765bb24e11bd701");
pub const TWAMM_V1: Address = address!("0xd4F1060cB9c1A13e1d2d20379b8aa2cF7541eD9b");
pub const TWAMM_V2: Address = address!("0xd47f1B1eDCfEaBb08F6eBd8FC337c27E636C75BA");
pub const BOOSTED_FEES: Address = address!("0xd4b54d0ca6979da05f25895e6e269e678ba00f9e");

/// Block of the first pool initialization on mainnet, the stock manifest's `initialBlock`.
pub const FIRST_BLOCK: u64 = 24134506;

sol! {
    struct PoolKey {
        address token0;
        address token1;
        bytes32 config;
    }

    event PoolInitialized(bytes32 poolId, PoolKey poolKey, int32 tick, uint96 sqrtRatio);
}

#[derive(ClapArgs)]
pub struct Args {
    /// Block whose post-state the seed describes.
    #[arg(long)]
    block: u64,
    /// Path of the seed file to write.
    #[arg(long)]
    out: PathBuf,
    /// JSON-RPC URL of an archive node that supports `debug_storageRangeAt`.
    #[arg(long, env = "RPC_URL", hide_env_values = true)]
    rpc_url: String,
    /// Log consistency violations between logs, storage and bitmaps instead of failing.
    #[arg(long)]
    lenient: bool,
}

pub async fn run(package: &Package, args: Args) -> Result<()> {
    let rpc = Rpc::connect(&args.rpc_url)?;

    info!(block = args.block, "collecting pool initializations");
    let inits = pool_initializations(&rpc, FIRST_BLOCK, args.block).await?;
    info!(pools = inits.len(), "dumping core and extension storage");
    let dump = rpc
        .dump_storage(args.block, &[CORE, TWAMM_V1, TWAMM_V2, BOOSTED_FEES])
        .await?;

    let seed = build_seed(&dump, &inits, Checks { lenient: args.lenient, violations: 0 })?;
    let header = Header {
        package: package.name.to_owned(),
        block_number: dump.block_number,
        block_hash: dump.block_hash.0,
    };
    let bytes = file::write(&args.out, &header, &seed.encode_to_vec())?;
    info!(path = %args.out.display(), bytes, "wrote seed");
    println!("{}", (package.describe)(&seed.encode_to_vec())?);

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolInit {
    pool_id: B256,
    token0: Address,
    token1: Address,
    config: B256,
}

/// Every pool the core initialized in `[from_block, to_block]`, in chain order.
async fn pool_initializations(rpc: &Rpc, from_block: u64, to_block: u64) -> Result<Vec<PoolInit>> {
    rpc.logs(CORE, PoolInitialized::SIGNATURE_HASH, from_block, to_block)
        .await?
        .iter()
        .map(|log| {
            let event = PoolInitialized::decode_log_data(&log.inner.data)
                .context("decoding PoolInitialized")?;
            Ok(PoolInit {
                pool_id: event.poolId,
                token0: event.poolKey.token0,
                token1: event.poolKey.token1,
                config: event.poolKey.config,
            })
        })
        .collect()
}

/// Collects consistency violations between the pool logs, the storage dumps and the bitmaps. Each
/// one is a bug in this tool or an unexpected contract state, so by default the first one aborts.
struct Checks {
    lenient: bool,
    violations: usize,
}

impl Checks {
    fn violation(&mut self, message: String) -> Result<()> {
        self.violations += 1;
        if self.lenient {
            warn!("{message}");
            Ok(())
        } else {
            bail!("{message} (pass --lenient to continue anyway)")
        }
    }
}

/// A pool's slots in the core dump.
#[derive(Default)]
struct CorePool {
    state: Option<B256>,
    ticks: BTreeMap<i32, TickInfo>,
    bitmaps: BTreeMap<u64, U256>,
}

/// A pool's slots in one timed extension's dump.
#[derive(Default)]
struct TimedPool {
    state: Option<B256>,
    time_infos: BTreeMap<u64, B256>,
    time_bitmaps: BTreeMap<u64, U256>,
}

fn build_seed(dump: &Dump, inits: &[PoolInit], mut checks: Checks) -> Result<Seed> {
    let distinct: HashSet<_> = inits
        .iter()
        .map(|init| init.pool_id)
        .collect();
    if distinct.len() != inits.len() {
        bail!("{} pool initializations but only {} distinct pool ids", inits.len(), distinct.len());
    }
    let pool_ids = PoolIds::new(distinct);

    let core = group_core(&pool_ids, storage(dump, CORE)?)?;
    let timed: HashMap<Address, HashMap<B256, TimedPool>> = [TWAMM_V1, TWAMM_V2, BOOSTED_FEES]
        .into_iter()
        .map(|extension| Ok((extension, group_timed(&pool_ids, storage(dump, extension)?)?)))
        .collect::<Result<_>>()?;

    let mut pools = Vec::with_capacity(inits.len());
    for init in inits {
        let config = EvmPoolConfig::try_from(init.config)
            .map_err(|e| anyhow!("pool {}: invalid config: {e}", init.pool_id))?;
        let core_pool = core.get(&init.pool_id);
        let Some(state) = core_pool.and_then(|pool| pool.state) else {
            checks.violation(format!("pool {} has no state slot in the core", init.pool_id))?;
            continue;
        };
        let state = parse_pool_state(state);

        let ticks = match config.pool_type_config {
            EvmPoolTypeConfig::Concentrated(tick_spacing) => concentrated_ticks(
                init.pool_id,
                core_pool.unwrap(),
                tick_spacing.0,
                &state,
                &mut checks,
            )?,
            EvmPoolTypeConfig::FullRange(_) | EvmPoolTypeConfig::Stableswap(_) => {
                if let Some(pool) = core_pool.filter(|pool| !pool.ticks.is_empty()) {
                    checks.violation(format!(
                        "non-concentrated pool {} has {} tick slots",
                        init.pool_id,
                        pool.ticks.len()
                    ))?;
                }
                fixed_range_ticks(&config.pool_type_config, state.liquidity)
            }
        };

        let sqrt_ratio = float_sqrt_ratio_to_fixed(U96::from_be_bytes(state.sqrt_ratio_float));
        let (balance0, balance1) = reserves::reserves(sqrt_ratio, &ticks)
            .map_err(|e| anyhow!("pool {}: computing reserves: {e}", init.pool_id))?;

        let timed_snapshot = if [TWAMM_V1, TWAMM_V2, BOOSTED_FEES].contains(&config.extension) {
            Some(timed_snapshot(
                init.pool_id,
                config.extension,
                timed[&config.extension].get(&init.pool_id),
                dump.timestamp,
                &mut checks,
            )?)
        } else {
            None
        };

        pools.push(PoolSeed {
            pool_id: init.pool_id.to_vec(),
            sqrt_ratio_float: state.sqrt_ratio_float.to_vec(),
            snapshot: Some(PoolSnapshot {
                token0: init.token0.to_vec(),
                token1: init.token1.to_vec(),
                config: init.config.to_vec(),
                tick: state.tick,
                sqrt_ratio: vec![],
                liquidity: signed_be(state.liquidity),
                ticks: ticks
                    .into_iter()
                    .map(|(index, delta)| TickSnapshot { index, liquidity_delta: signed_be(delta) })
                    .collect(),
                balance0: signed_be(balance0),
                balance1: signed_be(balance1),
                timed: timed_snapshot,
            }),
        });
    }

    if checks.violations > 0 {
        warn!(violations = checks.violations, "seed built despite consistency violations");
    }

    Ok(Seed { pools })
}

fn storage(dump: &Dump, address: Address) -> Result<&HashMap<B256, B256>> {
    dump.storage
        .get(&address)
        .ok_or_else(|| anyhow!("no storage dump for {address}"))
}

fn group_core(pool_ids: &PoolIds, slots: &HashMap<B256, B256>) -> Result<HashMap<B256, CorePool>> {
    let mut pools: HashMap<B256, CorePool> = HashMap::new();
    let mut other = 0usize;
    for (&slot, &value) in slots {
        match classify_core_slot(pool_ids, slot)? {
            CoreSlot::PoolState { pool } => pools.entry(pool).or_default().state = Some(value),
            CoreSlot::Tick { pool, tick } => {
                pools
                    .entry(pool)
                    .or_default()
                    .ticks
                    .insert(tick, parse_tick_info(value));
            }
            CoreSlot::Bitmap { pool, word } => {
                pools
                    .entry(pool)
                    .or_default()
                    .bitmaps
                    .insert(word, value.into());
            }
            CoreSlot::FeesPerLiquidity { .. } |
            CoreSlot::FeesPerLiquidityOutside0 { .. } |
            CoreSlot::FeesPerLiquidityOutside1 { .. } => {}
            CoreSlot::Other => other += 1,
        }
    }
    info!(slots = slots.len(), pools = pools.len(), other, "grouped core storage");

    Ok(pools)
}

fn group_timed(
    pool_ids: &PoolIds,
    slots: &HashMap<B256, B256>,
) -> Result<HashMap<B256, TimedPool>> {
    let mut pools: HashMap<B256, TimedPool> = HashMap::new();
    for (&slot, &value) in slots {
        match classify_timed_slot(pool_ids, slot)? {
            TimedSlot::PoolState { pool } => pools.entry(pool).or_default().state = Some(value),
            TimedSlot::TimeInfo { pool, time } => {
                pools
                    .entry(pool)
                    .or_default()
                    .time_infos
                    .insert(time, value);
            }
            TimedSlot::TimeBitmap { pool, word } => {
                pools
                    .entry(pool)
                    .or_default()
                    .time_bitmaps
                    .insert(word, value.into());
            }
            TimedSlot::RewardRates { .. } |
            TimedSlot::RewardRatesBefore { .. } |
            TimedSlot::Other => {}
        }
    }

    Ok(pools)
}

/// The initialized ticks of a concentrated pool with a nonzero net liquidity delta, checked
/// against the tick spacing, the bitmaps and the pool's active liquidity.
fn concentrated_ticks(
    pool_id: B256,
    pool: &CorePool,
    tick_spacing: u32,
    state: &PoolState,
    checks: &mut Checks,
) -> Result<Vec<(i32, i128)>> {
    let mut ticks = Vec::new();
    let mut sum = 0i128;
    let mut active = 0i128;

    for (&tick, info) in &pool.ticks {
        if tick % tick_spacing as i32 != 0 {
            checks.violation(format!(
                "pool {pool_id}: tick {tick} is not a multiple of tick spacing {tick_spacing}"
            ))?;
        }

        let (word, index) = tick_to_bitmap_word_and_index(tick, tick_spacing);
        let flagged = pool
            .bitmaps
            .get(&word)
            .is_some_and(|word| bit_is_set(*word, index));
        if flagged != (info.liquidity_net != 0) {
            checks.violation(format!(
                "pool {pool_id}: tick {tick} has liquidity_net {} but its bitmap bit is {flagged}",
                info.liquidity_net
            ))?;
        }

        if info.liquidity_delta != 0 {
            ticks.push((tick, info.liquidity_delta));
            sum += info.liquidity_delta;
            if tick <= state.tick {
                active += info.liquidity_delta;
            }
        }
    }

    for (&word, bits) in &pool.bitmaps {
        for index in 0..=u8::MAX {
            if !bit_is_set(*bits, index) {
                continue;
            }
            let tick = layout::bitmap_word_and_index_to_tick(word, index, tick_spacing);
            if !pool
                .ticks
                .get(&tick)
                .is_some_and(|info| info.liquidity_net != 0)
            {
                checks.violation(format!(
                    "pool {pool_id}: bitmap flags tick {tick} but it has no liquidity"
                ))?;
            }
        }
    }

    if sum != 0 {
        checks.violation(format!("pool {pool_id}: tick liquidity deltas sum to {sum}"))?;
    }
    if active != state.liquidity as i128 {
        checks.violation(format!(
            "pool {pool_id}: ticks up to {} sum to liquidity {active} but the pool state says {}",
            state.tick, state.liquidity
        ))?;
    }

    Ok(ticks)
}

/// Full-range and stableswap pools hold all liquidity in one position whose bounds are fixed by the
/// config, so the stock package sees exactly one tick pair for them.
fn fixed_range_ticks(config: &EvmPoolTypeConfig, liquidity: u128) -> Vec<(i32, i128)> {
    if liquidity == 0 {
        return vec![];
    }

    let (lower, upper) = match config {
        EvmPoolTypeConfig::FullRange(_) => (MIN_TICK, MAX_TICK),
        EvmPoolTypeConfig::Stableswap(stableswap) => {
            let width = (MAX_TICK as u32 >> stableswap.amplification_factor) as i32;
            (
                (stableswap.center_tick - width).max(MIN_TICK),
                (stableswap.center_tick + width).min(MAX_TICK),
            )
        }
        EvmPoolTypeConfig::Concentrated(_) => unreachable!("concentrated pools read their ticks"),
    };

    vec![(lower, liquidity as i128), (upper, -(liquidity as i128))]
}

fn timed_snapshot(
    pool_id: B256,
    extension: Address,
    pool: Option<&TimedPool>,
    block_timestamp: u64,
    checks: &mut Checks,
) -> Result<TimedSnapshot> {
    let Some(state) = pool.and_then(|pool| pool.state) else {
        checks.violation(format!("timed pool {pool_id} has no state slot in {extension}"))?;
        return Ok(TimedSnapshot {
            last_time: block_timestamp,
            rate0: signed_be(0u128),
            rate1: signed_be(0u128),
            rate_deltas: vec![],
        });
    };
    let pool = pool.unwrap();
    let state = parse_timed_pool_state(state);
    let last_time = real_last_time(block_timestamp, state.last_time_u32);

    let mut rate_deltas = Vec::new();
    for (&time, &word) in &pool.time_infos {
        let (delta0, delta1) = if extension == BOOSTED_FEES {
            let update = parse_pool_balance_update(word);
            (update.delta0, update.delta1)
        } else {
            let info = parse_time_info(word);
            (info.delta0, info.delta1)
        };
        let nonzero = delta0 != 0 || delta1 != 0;

        let (bitmap_word, index) = time_to_bitmap_word_and_index(time);
        let flagged = pool
            .time_bitmaps
            .get(&bitmap_word)
            .is_some_and(|bits| bit_is_set(*bits, index));
        if flagged != nonzero {
            checks.violation(format!(
                "pool {pool_id}: time {time} has deltas ({delta0}, {delta1}) but its bitmap bit is \
                 {flagged}"
            ))?;
        }
        if !nonzero {
            continue;
        }
        if time <= last_time || !is_time_valid(last_time, time) {
            checks.violation(format!(
                "pool {pool_id}: time {time} is not a valid time after the last execution at \
                 {last_time}"
            ))?;
        }

        rate_deltas.push(RateDeltaSnapshot {
            time,
            delta0: if delta0 != 0 { signed_be(delta0) } else { vec![] },
            delta1: if delta1 != 0 { signed_be(delta1) } else { vec![] },
        });
    }

    Ok(TimedSnapshot {
        last_time,
        rate0: signed_be(state.rate0),
        rate1: signed_be(state.rate1),
        rate_deltas,
    })
}

/// Two's-complement big-endian bytes, the encoding the package's stores parse.
fn signed_be(value: impl Into<BigInt>) -> Vec<u8> {
    value.into().to_signed_bytes_be()
}

#[cfg(test)]
mod tests {
    use ekubo_sdk::quoting::pools::{
        full_range::FullRangePoolTypeConfig, stableswap::StableswapPoolTypeConfig,
    };

    use super::*;

    #[test]
    fn full_range_and_stableswap_pools_hold_one_position() {
        assert_eq!(
            fixed_range_ticks(&EvmPoolTypeConfig::FullRange(FullRangePoolTypeConfig), 5),
            vec![(MIN_TICK, 5), (MAX_TICK, -5)]
        );
        assert_eq!(
            fixed_range_ticks(
                &EvmPoolTypeConfig::Stableswap(StableswapPoolTypeConfig {
                    center_tick: 1600,
                    amplification_factor: 10,
                }),
                7
            ),
            vec![(1600 - (MAX_TICK >> 10), 7), (1600 + (MAX_TICK >> 10), -7)]
        );
        assert!(
            fixed_range_ticks(&EvmPoolTypeConfig::FullRange(FullRangePoolTypeConfig), 0).is_empty()
        );
    }

    #[test]
    fn signed_be_matches_store_encoding() {
        assert_eq!(signed_be(0u128), vec![0]);
        assert_eq!(signed_be(255u128), vec![0, 255]);
        assert_eq!(signed_be(-1i128), vec![255]);
    }
}
