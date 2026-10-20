use std::{any::Any, collections::HashMap};

use alloy::primitives::{Sign, I256, U256};
use num_bigint::BigUint;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use tracing::{error, trace};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, Price, ProtocolSim,
            QueryPoolSwapParams, SwapConstraint,
        },
    },
    Bytes,
};

use super::{
    adaptive_fee::{calculate_volume_per_liquidity, FeeConfiguration},
    attributes,
    timepoints::{Timepoint, Timepoints},
};
use crate::evm::{
    protocol::{
        clmm::clmm_swap_to_price,
        safe_math::{safe_add_u256, safe_sub_u256},
        u256_num::u256_to_biguint,
        utils::{
            add_fee_markup,
            slipstreams::raw_target_price,
            uniswap::{
                liquidity_math,
                pool_tick::PoolTick,
                sqrt_price_math::{get_amount0_delta, get_amount1_delta, sqrt_price_q96_to_f64},
                swap_math::{self, SwapStepResult},
                tick_list::{TickInfo, TickList, TickListErrorKind},
                tick_math::{
                    get_sqrt_ratio_at_tick_cached, MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO,
                    MIN_TICK,
                },
                StepComputation, SwapResults, SwapState,
            },
        },
    },
    query_pool_swap::price_to_f64_with_decimals,
};

/// Algebra's `TickTable` indexes raw ticks: a row holds 256 consecutive ticks, which is
/// Uniswap V3's word with a tick spacing of one.
const TICK_TABLE_SPACING: u16 = 1;

// Gas constants from an Arbitrum One fork (Foundry, cold storage, blocks 512300663 for
// WETH/USDC and 512302761 for WETH/ARB), measuring `gasleft()` around `pool.swap` from the
// caller: the pool's reads and writes, its operator calls, the callback's input transfer, the
// output transfer and the community fee transfer to the vault. A swap that crosses no tick and
// writes no timepoint cost 101k–116k; the first swap at a new block timestamp cost 131k–144k
// more for the timepoint write and the fee recomputation; over swaps crossing 1–400 initialized
// ticks a least-squares fit gives 17.4k per crossing plus 5.9k per loop iteration, within ±1%
// except for the smallest swaps (−7%). Rounded up so that the estimate errs on the high side.
const SWAP_BASE_GAS: u64 = 120_000;
const TIMEPOINT_WRITE_GAS: u64 = 145_000;
// One loop iteration: a tick table row and the price movement math.
const GAS_PER_STEP: u64 = 6_000;
// Crossing an initialized tick rewrites its outer accumulators.
const GAS_PER_INITIALIZED_TICK_CROSS: u64 = 18_000;
// Conservative budget for one swap transaction: `get_limits` walks ticks until a swap would
// spend it, pricing every row and crossing as `swap` does.
const MAX_SWAP_GAS: u64 = 16_700_000;

/// What the first swap at the execution timestamp does before moving the price: it writes a
/// timepoint and recomputes both directional fees from the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingWrite {
    timepoint: Timepoint,
    fee_zto: u16,
    fee_otz: u16,
}

/// A Camelot V3 pool: Uniswap V3 concentrated liquidity with Algebra V1.9's adaptive,
/// directional fee.
///
/// Everything a swap reads is pool state the indexer emits as attributes; the fee a quote pays
/// is the one the chain would charge in the execution block, which depends on whether the pool
/// already wrote a timepoint at that block's timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamelotV3State {
    id: String,
    /// Timestamp of the block a quote against this state is expected to execute in, maintained
    /// by the stream decoder through [`ProtocolSim::apply_block`]. `AlgebraPool` keys its fee
    /// recomputation on it, not on the block number.
    execution_block_timestamp: u64,
    liquidity: u128,
    sqrt_price: U256,
    tick: i32,
    /// `globalState.feeZto` / `feeOtz`: the fees stored after the last recomputation.
    fee_zto: u16,
    fee_otz: u16,
    /// `globalState.timepointIndex`: the ring index of the last written timepoint.
    timepoint_index: u16,
    /// `volumePerLiquidityInBlock`: volume accumulated since the last timepoint, which the
    /// next timepoint records.
    volume_per_liquidity_in_block: u128,
    ticks: TickList,
    timepoints: Timepoints,
    fee_config_zto: FeeConfiguration,
    fee_config_otz: FeeConfiguration,
}

impl CamelotV3State {
    /// Creates a state from the pool's indexed values.
    ///
    /// `ticks` may be in any order and may include ticks whose net liquidity is zero; they stay
    /// step boundaries as on chain. `timepoints` must hold the timepoint at `timepoint_index`
    /// and, unless that is the ring's first write, the one before it.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        execution_block_timestamp: u64,
        liquidity: u128,
        sqrt_price: U256,
        tick: i32,
        fee_zto: u16,
        fee_otz: u16,
        timepoint_index: u16,
        volume_per_liquidity_in_block: u128,
        mut ticks: Vec<TickInfo>,
        timepoints: Vec<Timepoint>,
        fee_config_zto: FeeConfiguration,
        fee_config_otz: FeeConfiguration,
    ) -> Result<Self, SimulationError> {
        ticks.sort_by_key(|tick| tick.index);
        let state = Self {
            id,
            execution_block_timestamp,
            liquidity,
            sqrt_price,
            tick,
            fee_zto,
            fee_otz,
            timepoint_index,
            volume_per_liquidity_in_block,
            ticks: TickList::from(TICK_TABLE_SPACING, ticks)?,
            timepoints: Timepoints::new(timepoints),
            fee_config_zto,
            fee_config_otz,
        };
        if state.is_initialized() {
            state
                .timepoints
                .check_last(timepoint_index)
                .map_err(|err| state.with_pool(err))?;
        }
        Ok(state)
    }

    /// Prefixes an error with the pool id, so that a log line names the pool it is about.
    fn with_pool(&self, err: SimulationError) -> SimulationError {
        match err {
            SimulationError::FatalError(message) => {
                SimulationError::FatalError(format!("pool {}: {message}", self.id))
            }
            SimulationError::RecoverableError(message) => {
                SimulationError::RecoverableError(format!("pool {}: {message}", self.id))
            }
            SimulationError::InvalidInput(message, partial) => {
                SimulationError::InvalidInput(format!("pool {}: {message}", self.id), partial)
            }
        }
    }

    /// Whether `initialize` has run. It sets the price and writes the first timepoint, so
    /// before it the pool has no ring to recompute fees from and nothing to quote.
    fn is_initialized(&self) -> bool {
        !self.sqrt_price.is_zero()
    }

    /// The timepoint write and fee recomputation the first swap at the execution timestamp
    /// performs, or `None` when the pool already wrote a timepoint at that timestamp or has
    /// not been initialized yet.
    fn pending_write(&self) -> Result<Option<PendingWrite>, SimulationError> {
        if !self.is_initialized() {
            return Ok(None);
        }
        // `_blockTimestamp()` truncates to uint32.
        let time = self.execution_block_timestamp as u32;
        let Some(timepoint) = self.timepoints.write(
            self.timepoint_index,
            time,
            self.tick,
            self.liquidity,
            self.volume_per_liquidity_in_block,
        )?
        else {
            return Ok(None);
        };
        let (volatility, volume_per_liquidity) = self.timepoints.averages(
            Some(&timepoint),
            time,
            self.tick,
            timepoint.index,
            self.liquidity,
        )?;
        // `getFees` passes the volatility average divided by 15.
        let volatility = U256::from(volatility / 15);
        Ok(Some(PendingWrite {
            timepoint,
            fee_zto: self
                .fee_config_zto
                .get_fee(volatility, volume_per_liquidity)?,
            fee_otz: self
                .fee_config_otz
                .get_fee(volatility, volume_per_liquidity)?,
        }))
    }

    /// The fees a swap in the execution block pays, zero-to-one and one-to-zero: the
    /// recomputed ones when the block's first swap recomputes them, otherwise the stored ones.
    /// The constructor stores `BASE_FEE` in both until the first swap after `initialize`.
    fn fees(&self) -> Result<(u16, u16), SimulationError> {
        Ok(match self.pending_write()? {
            Some(pending) => (pending.fee_zto, pending.fee_otz),
            None => (self.fee_zto, self.fee_otz),
        })
    }

    /// The fee a swap in direction `zero_for_one` pays: the one `pending` recomputes when the
    /// swap is the block's first, otherwise the stored one.
    fn direction_fee(&self, pending: Option<&PendingWrite>, zero_for_one: bool) -> u16 {
        match (pending, zero_for_one) {
            (Some(pending), true) => pending.fee_zto,
            (Some(pending), false) => pending.fee_otz,
            (None, true) => self.fee_zto,
            (None, false) => self.fee_otz,
        }
    }

    /// The swap that moves the spot price of `token_in` in `token_out` into the band
    /// `[target, target * (1 + tolerance)]`, computed in closed form, or `None` when the closed
    /// form cannot land there and the generic search has to.
    ///
    /// The spot price carries the fee of buying `token_in` back, which after the swap is the
    /// stored or recomputed fee of the other direction, so the raw price the swap stops at strips
    /// that fee. The swap itself pays the fee of its own direction. The output and the state
    /// after are those of an exact-input swap of the input found, the trade that executes.
    fn swap_to_target_price(
        &self,
        params: &QueryPoolSwapParams,
        target: &Price,
        tolerance: f64,
    ) -> Result<Option<PoolSwap>, SimulationError> {
        let (token_in, token_out) = (params.token_in(), params.token_out());
        let target_f64 = price_to_f64_with_decimals(target, token_in.decimals, token_out.decimals)?;
        if target_f64 == self.spot_price(token_in, token_out)? {
            return Ok(Some(PoolSwap::new(
                BigUint::ZERO,
                BigUint::ZERO,
                Box::new(self.clone()),
                None,
            )));
        }
        let zero_for_one = token_in < token_out;
        let pending = self.pending_write()?;
        let spot_fee = self.direction_fee(pending.as_ref(), !zero_for_one);
        let Some(limit) = raw_target_price(target, tolerance, u32::from(spot_fee)) else {
            return Ok(None);
        };
        let fee = self.direction_fee(pending.as_ref(), zero_for_one);
        // A target past the indexed ticks or outside the price range fails here; the generic
        // search then reports it the way it does for every other protocol.
        let Ok((amount_in, _, _)) = clmm_swap_to_price(
            self.sqrt_price,
            &token_in.address,
            &token_out.address,
            &limit,
            0,
            Sign::Positive,
            |zero_for_one, amount_specified, sqrt_price_limit| {
                self.swap(
                    zero_for_one,
                    amount_specified,
                    Some(sqrt_price_limit),
                    fee,
                    pending.as_ref(),
                )
            },
        ) else {
            return Ok(None);
        };
        if amount_in.is_zero() {
            return Ok(None);
        }
        // The closed form stops exactly at the raw price with its input rounded up. An
        // exact-input swap of that input turns the rounding into output, up to one input unit's
        // worth, which is what the chain pays for it.
        let Ok(executed) = self.get_amount_out(amount_in.clone(), token_in, token_out) else {
            return Ok(None);
        };
        let spot = executed
            .new_state
            .spot_price(token_in, token_out)?;
        if spot < target_f64 || spot > target_f64 * (1.0 + tolerance) {
            return Ok(None);
        }
        Ok(Some(PoolSwap::new(amount_in, executed.amount, executed.new_state, None)))
    }

    /// Records the timepoint and fees of `pending`, as `_calculateSwapAndLock` does before its
    /// loop: the ring index advances and the block's volume accumulator restarts.
    fn commit_write(&mut self, pending: &PendingWrite) {
        self.timepoints
            .upsert(pending.timepoint);
        self.timepoint_index = pending.timepoint.index;
        self.fee_zto = pending.fee_zto;
        self.fee_otz = pending.fee_otz;
        self.volume_per_liquidity_in_block = 0;
    }

    /// Applies the result of a swap to a copy of this state, including the pending write and
    /// the volume the swap adds to `volumePerLiquidityInBlock`.
    fn state_after(
        &self,
        pending: Option<&PendingWrite>,
        result: &SwapResults,
        zero_for_one: bool,
    ) -> Result<Self, SimulationError> {
        let mut new_state = self.clone();
        if let Some(pending) = pending {
            new_state.commit_write(pending);
        }
        new_state.liquidity = result.liquidity;
        new_state.tick = result.tick.value();
        new_state.sqrt_price = result.sqrt_price;
        let amount_in = result.amount_specified - result.amount_remaining;
        let (amount0, amount1) = if zero_for_one {
            (amount_in, result.amount_calculated)
        } else {
            (result.amount_calculated, amount_in)
        };
        new_state.volume_per_liquidity_in_block = new_state
            .volume_per_liquidity_in_block
            .wrapping_add(calculate_volume_per_liquidity(result.liquidity, amount0, amount1)?);
        Ok(new_state)
    }

    fn swap(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
        fee: u16,
        pending: Option<&PendingWrite>,
    ) -> Result<SwapResults, SimulationError> {
        // Nothing to quote; the tick lookup below also indexes into the list unguarded.
        if self.liquidity == 0 || self.ticks.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }
        let price_limit = if let Some(limit) = sqrt_price_limit {
            limit
        } else if zero_for_one {
            safe_add_u256(MIN_SQRT_RATIO, U256::from(1u64))?
        } else {
            safe_sub_u256(MAX_SQRT_RATIO, U256::from(1u64))?
        };

        let price_limit_valid = if zero_for_one {
            price_limit > MIN_SQRT_RATIO && price_limit < self.sqrt_price
        } else {
            price_limit < MAX_SQRT_RATIO && price_limit > self.sqrt_price
        };
        if !price_limit_valid {
            return Err(SimulationError::InvalidInput("Price limit out of range".into(), None));
        }

        let exact_input = amount_specified > I256::ZERO;
        let mut state = SwapState {
            amount_remaining: amount_specified,
            amount_calculated: I256::ZERO,
            sqrt_price: self.sqrt_price,
            tick: PoolTick::from(self.tick),
            liquidity: self.liquidity,
        };
        let mut gas_used =
            U256::from(SWAP_BASE_GAS + if pending.is_some() { TIMEPOINT_WRITE_GAS } else { 0 });

        while state.amount_remaining != I256::ZERO && state.sqrt_price != price_limit {
            let (mut next_tick, next_tick_info) = match self
                .ticks
                .next_initialized_tick_within_one_word(state.tick.value(), zero_for_one)
            {
                Ok(next) => next,
                Err(tick_err) => match tick_err.kind {
                    TickListErrorKind::TicksExeeded => {
                        // The partial result goes through the same bookkeeping as a full one,
                        // so a swap chained onto it sees the write and this fill's volume.
                        let partial = SwapResults {
                            amount_calculated: state.amount_calculated,
                            amount_specified,
                            amount_remaining: state.amount_remaining,
                            sqrt_price: state.sqrt_price,
                            liquidity: state.liquidity,
                            tick: state.tick.clone(),
                            gas_used,
                        };
                        let new_state = self.state_after(pending, &partial, zero_for_one)?;
                        return Err(SimulationError::InvalidInput(
                            "Ticks exceeded".into(),
                            Some(GetAmountOutResult::new(
                                u256_to_biguint(state.amount_calculated.abs().into_raw()),
                                u256_to_biguint(gas_used),
                                Box::new(new_state),
                            )),
                        ));
                    }
                    _ => return Err(SimulationError::FatalError("Unknown error".to_string())),
                },
            };
            next_tick = next_tick.clamp(MIN_TICK, MAX_TICK);

            let sqrt_price_start = state.sqrt_price;
            let sqrt_price_next = match next_tick_info {
                Some(info) => info.sqrt_price,
                None => get_sqrt_ratio_at_tick_cached(next_tick)?,
            };
            let target = if (sqrt_price_next < price_limit) == zero_for_one {
                price_limit
            } else {
                sqrt_price_next
            };
            let SwapStepResult { sqrt_price, amount_in_with_fee, amount_out } =
                swap_math::compute_swap_step(
                    state.sqrt_price,
                    target,
                    state.liquidity,
                    state.amount_remaining,
                    u32::from(fee),
                )?;
            state.sqrt_price = sqrt_price;
            let step = StepComputation {
                sqrt_price_start,
                tick_next: next_tick,
                sqrt_price_next,
                amount_in_with_fee,
                amount_out,
            };
            gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_STEP))?;

            if exact_input {
                state.amount_remaining -=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_in_with_fee)
                        .ok_or_else(|| SimulationError::FatalError("step input overflow".into()))?;
                state.amount_calculated -=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_out).ok_or_else(
                        || SimulationError::FatalError("step output overflow".into()),
                    )?;
            } else {
                state.amount_remaining +=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_out).ok_or_else(
                        || SimulationError::FatalError("step output overflow".into()),
                    )?;
                state.amount_calculated +=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_in_with_fee)
                        .ok_or_else(|| SimulationError::FatalError("step input overflow".into()))?;
            }
            if state.sqrt_price == step.sqrt_price_next {
                if let Some(info) = next_tick_info {
                    let liquidity_net =
                        if zero_for_one { -info.net_liquidity } else { info.net_liquidity };
                    state.liquidity =
                        liquidity_math::add_liquidity_delta(state.liquidity, liquidity_net)?;
                    gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_INITIALIZED_TICK_CROSS))?;
                }
                state.tick =
                    PoolTick::from(if zero_for_one { step.tick_next - 1 } else { step.tick_next });
            } else if state.sqrt_price != step.sqrt_price_start {
                state.tick = PoolTick::at_sqrt_price(state.sqrt_price)?;
            }
        }
        Ok(SwapResults {
            amount_calculated: state.amount_calculated,
            amount_specified,
            amount_remaining: state.amount_remaining,
            sqrt_price: state.sqrt_price,
            liquidity: state.liquidity,
            tick: state.tick,
            gas_used,
        })
    }

    fn apply_attribute(&mut self, key: &str, value: &Bytes) -> Result<(), String> {
        match key {
            attributes::LIQUIDITY => self.liquidity = attributes::u128_attr(key, value)?,
            attributes::SQRT_PRICE_X96 => self.sqrt_price = attributes::u160_attr(key, value)?,
            attributes::TICK => self.tick = attributes::i24_attr(key, value)?,
            attributes::FEE_ZTO => self.fee_zto = attributes::u16_attr(key, value)?,
            attributes::FEE_OTZ => self.fee_otz = attributes::u16_attr(key, value)?,
            attributes::TIMEPOINT_INDEX => self.timepoint_index = attributes::u16_attr(key, value)?,
            attributes::VOLUME_PER_LIQUIDITY_IN_BLOCK => {
                self.volume_per_liquidity_in_block = attributes::u128_attr(key, value)?
            }
            attributes::FEE_CONFIG_ZTO => {
                self.fee_config_zto =
                    FeeConfiguration::from_slot(value).map_err(|err| err.to_string())?
            }
            attributes::FEE_CONFIG_OTZ => {
                self.fee_config_otz =
                    FeeConfiguration::from_slot(value).map_err(|err| err.to_string())?
            }
            _ => {
                if let Some(tick) = attributes::tick_of_key(key) {
                    self.ticks
                        .set_tick(tick?, attributes::i128_attr(key, value)?)
                        .map_err(|err| err.to_string())?;
                } else if let Some(index) = attributes::timepoint_of_key(key) {
                    self.timepoints.upsert(
                        Timepoint::from_attribute(index?, value).map_err(|err| err.to_string())?,
                    );
                }
            }
        }
        Ok(())
    }

    fn delete_attribute(&mut self, key: &str) -> Result<(), String> {
        if let Some(tick) = attributes::tick_of_key(key) {
            self.ticks.remove_tick(tick?);
        } else if let Some(index) = attributes::timepoint_of_key(key) {
            self.timepoints.remove(index?);
        }
        Ok(())
    }
}

#[typetag::serde]
impl ProtocolSim for CamelotV3State {
    /// The higher of the two directional fees, so that a single figure never understates what
    /// a swap in the execution block pays.
    fn fee(&self) -> f64 {
        match self.fees() {
            Ok((fee_zto, fee_otz)) => f64::from(fee_zto.max(fee_otz)) / 1_000_000.0,
            Err(err) => {
                error!(
                    pool = %self.id,
                    execution_block_timestamp = self.execution_block_timestamp,
                    %err,
                    "Error while calculating the adaptive fee"
                );
                f64::MAX / 1_000_000.0
            }
        }
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if !self.is_initialized() {
            return Err(SimulationError::RecoverableError("pool is not initialized".to_string()));
        }
        let (fee_zto, fee_otz) = self.fees()?;
        // Buying `base` sells `quote`, so the fee is the one of the direction quote -> base.
        let (price, fee) = if base < quote {
            (sqrt_price_q96_to_f64(self.sqrt_price, base.decimals, quote.decimals)?, fee_otz)
        } else {
            (
                1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, quote.decimals, base.decimals)?,
                fee_zto,
            )
        };
        Ok(add_fee_markup(price, f64::from(fee) / 1_000_000.0))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let zero_for_one = token_in < token_out;
        let amount_specified = I256::checked_from_sign_and_abs(
            Sign::Positive,
            U256::from_be_slice(&amount_in.to_bytes_be()),
        )
        .ok_or_else(|| {
            SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
        })?;

        let pending = self.pending_write()?;
        let fee = self.direction_fee(pending.as_ref(), zero_for_one);
        let result = self.swap(zero_for_one, amount_specified, None, fee, pending.as_ref())?;
        trace!(?amount_in, ?token_in, ?token_out, ?zero_for_one, ?result, "CAMELOT V3 SWAP");

        let new_state = self.state_after(pending.as_ref(), &result, zero_for_one)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(
                result
                    .amount_calculated
                    .abs()
                    .into_raw(),
            ),
            u256_to_biguint(result.gas_used),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if self.liquidity == 0 || self.ticks.is_empty() {
            return Ok((BigUint::zero(), BigUint::zero()));
        }

        let zero_for_one = token_in < token_out;
        let mut current_tick = self.tick;
        let mut current_sqrt_price = self.sqrt_price;
        let mut current_liquidity = self.liquidity;
        let mut total_amount_in = U256::ZERO;
        let mut total_amount_out = U256::ZERO;
        // The walk ends where a swap would exhaust `MAX_SWAP_GAS`, counting the block's
        // timepoint write and every row and crossing at the price `swap` charges.
        let mut gas_used = SWAP_BASE_GAS + TIMEPOINT_WRITE_GAS;

        while let Ok((tick, next_tick_info)) = self
            .ticks
            .next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
            gas_used += GAS_PER_STEP +
                if next_tick_info.is_some() { GAS_PER_INITIALIZED_TICK_CROSS } else { 0 };
            if gas_used > MAX_SWAP_GAS {
                break;
            }
            let next_tick = tick.clamp(MIN_TICK, MAX_TICK);
            let sqrt_price_next = match next_tick_info {
                Some(info) => info.sqrt_price,
                None => get_sqrt_ratio_at_tick_cached(next_tick)?,
            };

            let (amount_in, amount_out) = if zero_for_one {
                (
                    get_amount0_delta(
                        sqrt_price_next,
                        current_sqrt_price,
                        current_liquidity,
                        true,
                    )?,
                    get_amount1_delta(
                        sqrt_price_next,
                        current_sqrt_price,
                        current_liquidity,
                        false,
                    )?,
                )
            } else {
                (
                    get_amount1_delta(
                        sqrt_price_next,
                        current_sqrt_price,
                        current_liquidity,
                        true,
                    )?,
                    get_amount0_delta(
                        sqrt_price_next,
                        current_sqrt_price,
                        current_liquidity,
                        false,
                    )?,
                )
            };
            total_amount_in = safe_add_u256(total_amount_in, amount_in)?;
            total_amount_out = safe_add_u256(total_amount_out, amount_out)?;

            if let Some(info) = next_tick_info {
                let liquidity_delta =
                    if zero_for_one { -info.net_liquidity } else { info.net_liquidity };
                match liquidity_math::add_liquidity_delta(current_liquidity, liquidity_delta) {
                    Ok(new_liquidity) => current_liquidity = new_liquidity,
                    // Inconsistent tick data: the liquidity walked so far is all that is known.
                    Err(_) => break,
                }
            }
            current_tick = if zero_for_one { next_tick - 1 } else { next_tick };
            current_sqrt_price = sqrt_price_next;
        }

        Ok((u256_to_biguint(total_amount_in), u256_to_biguint(total_amount_out)))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        for (key, value) in delta.updated_attributes.iter() {
            self.apply_attribute(key, value)
                .map_err(TransitionError::DecodeError)?;
        }
        for key in delta.deleted_attributes.iter() {
            self.delete_attribute(key)
                .map_err(TransitionError::DecodeError)?;
        }
        Ok(())
    }

    /// Re-emits only when the fee a swap would pay changed: a pool whose last timepoint is
    /// older than the window and whose volume is settled keeps the same recomputed fee from
    /// block to block, and same-timestamp blocks short-circuit.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let timestamp = block.timestamp();
        if timestamp == self.execution_block_timestamp {
            return false;
        }
        let fees_before = self.fees().ok();
        self.execution_block_timestamp = timestamp;
        fees_before != self.fees().ok()
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<CamelotV3State>()
            .is_some_and(|other| self == other)
    }

    /// A target price is reached in closed form when it can be, otherwise by the generic search.
    ///
    /// A trade limit is not supported, as for Uniswap V3: it bounds the average price of the
    /// trade, which no closed form gives, and the generic search starts from `get_limits`,
    /// which on a pool with full-range positions is so large that its bisection never reaches
    /// a trade that small. It would answer zero where a trade qualifies.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "Camelot V3 does not support TradeLimitPrice constraint in query_pool_swap"
                    .to_string(),
                None,
            )),
            SwapConstraint::PoolTargetPrice { target, tolerance, .. } => {
                if !tolerance.is_finite() || *tolerance < 0.0 {
                    return Err(SimulationError::InvalidInput(
                        format!("target price tolerance {tolerance} is not a non-negative number"),
                        None,
                    ));
                }
                match self.swap_to_target_price(params, target, *tolerance)? {
                    Some(swap) => Ok(swap),
                    None => crate::evm::query_pool_swap::query_pool_swap(self, params),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, path::Path, str::FromStr};

    use num_bigint::BigUint;
    use serde::Deserialize;
    use tycho_common::{
        models::Chain,
        simulation::protocol_sim::{QueryPoolSwapParams, SwapConstraint},
    };

    use super::*;
    use crate::evm::{
        protocol::utils::uniswap::tick_math::get_sqrt_ratio_at_tick,
        query_pool_swap::test_helpers::{target_price_params, to_price},
    };

    /// One real Arbitrum swap, captured by `dump_fixture.py` in the PR: the pool state at the
    /// parent block, every initialized tick around the swap and the oracle timepoints the fee
    /// recomputation reads, plus the on-chain results to reproduce.
    #[derive(Deserialize)]
    struct Fixture {
        pool: String,
        tx: String,
        block: u64,
        timestamp: u64,
        token0: FixtureToken,
        token1: FixtureToken,
        pre: PreState,
        swap: SwapEvent,
        post: PostState,
    }

    #[derive(Deserialize)]
    struct FixtureToken {
        address: String,
        decimals: u32,
    }

    #[derive(Deserialize)]
    struct PreState {
        liquidity: String,
        sqrt_price: String,
        tick: i32,
        fee_zto: u16,
        fee_otz: u16,
        timepoint_index: u16,
        volume_per_liquidity_in_block: String,
        fee_config_zto: String,
        fee_config_otz: String,
        /// `[tick, liquidityTotal, liquidityDelta]` for every initialized tick fetched.
        ticks: Vec<(i32, String, String)>,
        /// Zero-net ticks placed at the fetched boundary when the pool has no initialized tick
        /// there; the swap never reaches them.
        #[serde(default)]
        stand_in_ticks: Vec<(i32, String, String)>,
        /// `[index, two storage words]`.
        timepoints: Vec<(u16, String)>,
    }

    #[derive(Deserialize)]
    struct SwapEvent {
        amount0: String,
        amount1: String,
        sqrt_price_after: String,
        liquidity_after: String,
        tick_after: i32,
        fee_event: Option<(u16, u16)>,
    }

    #[derive(Deserialize)]
    struct PostState {
        volume_per_liquidity_in_block: String,
        timepoint_index: u16,
        fee_zto: u16,
        fee_otz: u16,
        new_timepoint: Option<String>,
    }

    fn hex_bytes(value: &str) -> Vec<u8> {
        hex::decode(value.trim_start_matches("0x")).expect("hex fixture value")
    }

    fn token(fixture: &FixtureToken) -> Token {
        Token::new(
            &Bytes::from_str(&fixture.address).unwrap(),
            "T",
            fixture.decimals,
            0,
            &[Some(10_000)],
            Chain::Arbitrum,
            100,
        )
    }

    fn load_fixtures() -> Vec<Fixture> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assets/camelot_v3");
        let mut fixtures: Vec<Fixture> = fs::read_dir(&dir)
            .expect("fixture directory")
            .map(|entry| entry.expect("fixture entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext == "json")
            })
            .map(|path| {
                serde_json::from_str(&fs::read_to_string(&path).expect("fixture file"))
                    .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
            })
            .collect();
        fixtures.sort_by_key(|f| f.block);
        assert!(!fixtures.is_empty(), "no fixtures in {}", dir.display());
        fixtures
    }

    fn fixture(pool_prefix: &str) -> Fixture {
        load_fixtures()
            .into_iter()
            .find(|f| f.pool.starts_with(pool_prefix))
            .unwrap_or_else(|| panic!("no fixture for pool {pool_prefix}"))
    }

    fn fixture_timepoints(pre: &PreState) -> Vec<Timepoint> {
        pre.timepoints
            .iter()
            .map(|(index, words)| Timepoint::from_attribute(*index, &hex_bytes(words)).unwrap())
            .collect()
    }

    fn fixture_config(slot: &str) -> FeeConfiguration {
        FeeConfiguration::from_slot(&hex_bytes(slot)).unwrap()
    }

    fn state_from(fixture: &Fixture) -> CamelotV3State {
        let pre = &fixture.pre;
        let ticks = pre
            .ticks
            .iter()
            .chain(pre.stand_in_ticks.iter())
            .map(|(tick, _, delta)| TickInfo::new(*tick, delta.parse().unwrap()).unwrap())
            .collect();
        CamelotV3State::new(
            fixture.pool.clone(),
            // Decoded at the parent block; `apply_block` moves it to the swap's block below.
            fixture.timestamp - 1,
            pre.liquidity.parse().unwrap(),
            U256::from_str(&pre.sqrt_price).unwrap(),
            pre.tick,
            pre.fee_zto,
            pre.fee_otz,
            pre.timepoint_index,
            pre.volume_per_liquidity_in_block
                .parse()
                .unwrap(),
            ticks,
            fixture_timepoints(pre),
            fixture_config(&pre.fee_config_zto),
            fixture_config(&pre.fee_config_otz),
        )
        .unwrap()
    }

    /// The fixture's state at its swap block, as the stream decoder hands it out.
    fn state_at_swap_block(fixture: &Fixture) -> CamelotV3State {
        let mut state = state_from(fixture);
        state.apply_block(&BlockContext::new(fixture.block, fixture.timestamp));
        state
    }

    /// The direction and input amount of the fixture's swap.
    fn fixture_swap(fixture: &Fixture) -> (bool, BigUint) {
        let amount0 = I256::from_dec_str(&fixture.swap.amount0).unwrap();
        let amount1 = I256::from_dec_str(&fixture.swap.amount1).unwrap();
        let zero_for_one = amount0 > I256::ZERO;
        let amount_in = if zero_for_one { amount0 } else { amount1 };
        (zero_for_one, BigUint::from_str(&amount_in.to_string()).unwrap())
    }

    fn whole_token(token: &Token) -> BigUint {
        BigUint::from(10u32).pow(token.decimals)
    }

    fn quote(
        state: &CamelotV3State,
        amount_in: &BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> (BigUint, CamelotV3State) {
        let result = state
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .unwrap_or_else(|err| panic!("{}: {err}", state.id));
        let after = result
            .new_state
            .as_any()
            .downcast_ref::<CamelotV3State>()
            .unwrap()
            .clone();
        (result.amount, after)
    }

    fn signed(amount: &BigUint, sign: Sign) -> I256 {
        I256::checked_from_sign_and_abs(sign, U256::from_be_slice(&amount.to_bytes_be())).unwrap()
    }

    /// Replays every captured swap wei-exact: the output amount, the post-swap price, tick and
    /// liquidity, the `Fee` event (or its absence), the written timepoint and the pool's
    /// `volumePerLiquidityInBlock` must all match the chain.
    #[test]
    fn replays_arbitrum_swaps_wei_exact() {
        for fixture in load_fixtures() {
            let label = format!("{} block {} tx {}", fixture.pool, fixture.block, fixture.tx);
            let mut state = state_from(&fixture);
            state.apply_block(&BlockContext::new(fixture.block, fixture.timestamp));

            let amount0 = I256::from_dec_str(&fixture.swap.amount0).unwrap();
            let amount1 = I256::from_dec_str(&fixture.swap.amount1).unwrap();
            let zero_for_one = amount0 > I256::ZERO;
            let (token_in, token_out, amount_in, expected_out) = if zero_for_one {
                (token(&fixture.token0), token(&fixture.token1), amount0, -amount1)
            } else {
                (token(&fixture.token1), token(&fixture.token0), amount1, -amount0)
            };

            let result = state
                .get_amount_out(
                    BigUint::from_str(&amount_in.to_string()).unwrap(),
                    &token_in,
                    &token_out,
                )
                .unwrap_or_else(|err| panic!("{label}: {err}"));
            assert_eq!(
                result.amount,
                BigUint::from_str(&expected_out.to_string()).unwrap(),
                "{label}: amount out"
            );

            let after = result
                .new_state
                .as_any()
                .downcast_ref::<CamelotV3State>()
                .unwrap();
            assert_eq!(
                after.sqrt_price,
                U256::from_str(&fixture.swap.sqrt_price_after).unwrap(),
                "{label}: price"
            );
            assert_eq!(after.tick, fixture.swap.tick_after, "{label}: tick");
            assert_eq!(
                after.liquidity,
                fixture
                    .swap
                    .liquidity_after
                    .parse::<u128>()
                    .unwrap(),
                "{label}: liquidity"
            );
            assert_eq!(
                (after.fee_zto, after.fee_otz),
                (fixture.post.fee_zto, fixture.post.fee_otz),
                "{label}: fees"
            );
            assert_eq!(
                after.timepoint_index, fixture.post.timepoint_index,
                "{label}: timepoint index"
            );
            assert_eq!(
                after.volume_per_liquidity_in_block,
                fixture
                    .post
                    .volume_per_liquidity_in_block
                    .parse::<u128>()
                    .unwrap(),
                "{label}: volumePerLiquidityInBlock"
            );
            match (&fixture.swap.fee_event, &fixture.post.new_timepoint) {
                (Some(fee), Some(words)) => {
                    assert_eq!((after.fee_zto, after.fee_otz), *fee, "{label}: Fee event");
                    let written = after
                        .timepoints
                        .get(fixture.post.timepoint_index)
                        .unwrap_or_else(|| panic!("{label}: written timepoint missing"));
                    let expected =
                        Timepoint::from_attribute(fixture.post.timepoint_index, &hex_bytes(words))
                            .unwrap();
                    assert_eq!(*written, expected, "{label}: written timepoint");
                }
                (None, None) => {
                    assert_eq!(
                        after.timepoint_index, fixture.pre.timepoint_index,
                        "{label}: no timepoint written"
                    );
                }
                other => panic!("{label}: inconsistent fixture {other:?}"),
            }
        }
    }

    /// A second swap in the block finds the timepoint the first one wrote: it writes nothing,
    /// pays the committed fees and adds its own volume to the block's accumulator.
    #[test]
    fn a_second_swap_in_the_same_block_writes_nothing_and_adds_its_volume() {
        for fixture in load_fixtures() {
            let label = format!("{} block {}", fixture.pool, fixture.block);
            let (zero_for_one, amount_in) = fixture_swap(&fixture);
            let (token_in, token_out) = if zero_for_one {
                (token(&fixture.token0), token(&fixture.token1))
            } else {
                (token(&fixture.token1), token(&fixture.token0))
            };
            let (_, after) =
                quote(&state_at_swap_block(&fixture), &amount_in, &token_in, &token_out);
            assert!(after.pending_write().unwrap().is_none(), "{label}: the block wrote");

            let (amount_out, after2) = quote(&after, &amount_in, &token_in, &token_out);

            assert_eq!(after2.timepoint_index, after.timepoint_index, "{label}: index");
            assert_eq!(after2.timepoints, after.timepoints, "{label}: ring");
            assert_eq!(
                (after2.fee_zto, after2.fee_otz),
                (after.fee_zto, after.fee_otz),
                "{label}: fees"
            );
            let (amount0, amount1) = if zero_for_one {
                (signed(&amount_in, Sign::Positive), signed(&amount_out, Sign::Negative))
            } else {
                (signed(&amount_out, Sign::Negative), signed(&amount_in, Sign::Positive))
            };
            assert_eq!(
                after2.volume_per_liquidity_in_block,
                after
                    .volume_per_liquidity_in_block
                    .wrapping_add(
                        calculate_volume_per_liquidity(after2.liquidity, amount0, amount1).unwrap()
                    ),
                "{label}: the volume accumulates"
            );
        }
    }

    /// Once the block's timepoint is written, a swap pays the stored fee of its own direction.
    #[test]
    fn stored_fees_apply_per_direction_once_the_block_wrote() {
        let fixture = fixture("0x299c");
        let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
        let (zero_for_one, amount_in) = fixture_swap(&fixture);
        let (token_in, token_out) =
            if zero_for_one { (&token0, &token1) } else { (&token1, &token0) };
        let (_, written) = quote(&state_at_swap_block(&fixture), &amount_in, token_in, token_out);
        assert!(written
            .pending_write()
            .unwrap()
            .is_none());
        let with_fees = |fee_zto: u16, fee_otz: u16| {
            let mut state = written.clone();
            state.fee_zto = fee_zto;
            state.fee_otz = fee_otz;
            state
        };
        let asymmetric = with_fees(1_000, 9_000);
        let (sell0, sell1) = (whole_token(&token0), whole_token(&token1));

        assert_eq!(
            quote(&asymmetric, &sell0, &token0, &token1).0,
            quote(&with_fees(1_000, 1_000), &sell0, &token0, &token1).0,
            "zero-to-one pays fee_zto"
        );
        assert_eq!(
            quote(&asymmetric, &sell1, &token1, &token0).0,
            quote(&with_fees(9_000, 9_000), &sell1, &token1, &token0).0,
            "one-to-zero pays fee_otz"
        );
        assert!(
            quote(&with_fees(1_000, 1_000), &sell0, &token0, &token1).0 >
                quote(&with_fees(9_000, 9_000), &sell0, &token0, &token1).0,
            "the fee changes the output"
        );
    }

    /// The first swap of a block recomputes both fees; each direction pays the fee of its own
    /// configuration, as does the spot price of buying into that direction.
    #[test]
    fn recomputed_fees_follow_their_directions_configuration() {
        let fixture = fixture("0x299c");
        let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
        let symmetric = state_at_swap_block(&fixture);
        let mut asymmetric = symmetric.clone();
        asymmetric.fee_config_otz.base_fee += 1_000;
        assert!(
            asymmetric
                .pending_write()
                .unwrap()
                .is_some(),
            "the swap recomputes the fee"
        );
        let (sell0, sell1) = (whole_token(&token0), whole_token(&token1));

        assert_eq!(
            quote(&asymmetric, &sell0, &token0, &token1).0,
            quote(&symmetric, &sell0, &token0, &token1).0,
            "zero-to-one ignores the one-to-zero configuration"
        );
        assert!(
            quote(&asymmetric, &sell1, &token1, &token0).0 <
                quote(&symmetric, &sell1, &token1, &token0).0,
            "one-to-zero pays the higher fee"
        );
        // Buying token0 sells token1, so the one-to-zero fee marks that price up.
        assert!(
            asymmetric
                .spot_price(&token0, &token1)
                .unwrap() >
                symmetric
                    .spot_price(&token0, &token1)
                    .unwrap()
        );
        assert_eq!(
            asymmetric
                .spot_price(&token1, &token0)
                .unwrap(),
            symmetric
                .spot_price(&token1, &token0)
                .unwrap()
        );
        assert!(asymmetric.fee() > symmetric.fee());
    }

    /// The spot price of `base` in `quote` is what a small sale of `quote` pays per unit of
    /// `base` received, fee included. Small is relative to the pool: a hundredth of the
    /// captured swap, sold in its direction and, with what it bought, back the other way.
    #[test]
    fn spot_price_is_the_marginal_price_of_a_small_swap() {
        for fixture in load_fixtures() {
            let state = state_at_swap_block(&fixture);
            let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
            let (zero_for_one, amount_in) = fixture_swap(&fixture);
            let amount_out = u256_to_biguint(
                I256::from_dec_str(if zero_for_one {
                    &fixture.swap.amount1
                } else {
                    &fixture.swap.amount0
                })
                .unwrap()
                .unsigned_abs(),
            );
            let probes = if zero_for_one {
                [(&token1, &token0, amount_in), (&token0, &token1, amount_out)]
            } else {
                [(&token0, &token1, amount_in), (&token1, &token0, amount_out)]
            };
            for (base, quote_token, captured) in probes {
                let sold = captured / 100u32;
                let (bought, _) = quote(&state, &sold, quote_token, base);
                let as_f64 = |amount: &BigUint| {
                    amount
                        .to_string()
                        .parse::<f64>()
                        .unwrap()
                };
                let paid_per_base = as_f64(&sold) / as_f64(&bought) *
                    10f64.powi(base.decimals as i32 - quote_token.decimals as i32);
                let spot = state
                    .spot_price(base, quote_token)
                    .unwrap();
                assert!(
                    ((spot - paid_per_base) / paid_per_base).abs() < 2e-3,
                    "{} buying {}: spot {spot} vs marginal {paid_per_base}",
                    fixture.pool,
                    base.address
                );
            }
        }
    }

    /// A target-price query takes the closed-form path: the pool lands in the tolerance band,
    /// an exact-input quote of the returned input pays the returned output, and the post-swap
    /// state carries the block's timepoint write when there is one, as the swap would on chain.
    #[test]
    fn query_pool_swap_reaches_a_target_price_in_closed_form() {
        for fixture in load_fixtures() {
            let label = format!("{} block {}", fixture.pool, fixture.block);
            let state = state_at_swap_block(&fixture);
            // The fixtures at a timestamp the ring already holds write nothing.
            let written_index = state
                .pending_write()
                .unwrap()
                .map_or(state.timepoint_index, |pending| pending.timepoint.index);
            let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
            for (token_in, token_out) in [(&token0, &token1), (&token1, &token0)] {
                let price = to_price(
                    state
                        .spot_price(token_in, token_out)
                        .unwrap() *
                        0.99,
                    token_in,
                    token_out,
                );
                // The band is around the price passed, which `to_price` rounds to atomic units.
                let target =
                    price_to_f64_with_decimals(&price, token_in.decimals, token_out.decimals)
                        .unwrap();
                let tolerance = 1e-4;
                let params = target_price_params(token_in, token_out, price, tolerance);

                let swap = state
                    .query_pool_swap(&params)
                    .unwrap_or_else(|err| panic!("{label}: {err}"));

                assert!(swap.price_points().is_none(), "{label}: the closed form takes no steps");
                let after = swap
                    .new_state()
                    .as_any()
                    .downcast_ref::<CamelotV3State>()
                    .unwrap();
                let spot = after
                    .spot_price(token_in, token_out)
                    .unwrap();
                assert!(
                    spot >= target && spot <= target * (1.0 + tolerance),
                    "{label}: spot {spot}, target {target}"
                );
                assert_eq!(after.timepoint_index, written_index, "{label}: the write");
                let (amount_out, _) = quote(&state, swap.amount_in(), token_in, token_out);
                assert_eq!(&amount_out, swap.amount_out(), "{label}: amount out");
            }
        }
    }

    /// A trade-limit query is refused with an error a caller can act on, rather than answered
    /// with the zero the generic search returns on a pool with full-range positions.
    #[test]
    fn query_pool_swap_refuses_a_trade_limit_price() {
        let fixture = fixture("0x299c");
        let state = state_at_swap_block(&fixture);
        let (token_in, token_out) = (token(&fixture.token1), token(&fixture.token0));
        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: to_price(1.0, &token_in, &token_out),
                tolerance: 1e-3,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let err = state
            .query_pool_swap(&params)
            .unwrap_err();

        assert!(matches!(err, SimulationError::InvalidInput(_, None)), "{err:?}");
    }

    /// A tolerance that is not a non-negative number cannot define a band.
    #[test]
    fn query_pool_swap_rejects_an_invalid_tolerance() {
        let fixture = fixture("0x299c");
        let state = state_at_swap_block(&fixture);
        let (token_in, token_out) = (token(&fixture.token0), token(&fixture.token1));
        let target = state
            .spot_price(&token_in, &token_out)
            .unwrap() *
            0.99;
        for tolerance in [-1e-4, f64::NAN, f64::INFINITY] {
            let params = target_price_params(
                &token_in,
                &token_out,
                to_price(target, &token_in, &token_out),
                tolerance,
            );
            assert!(
                matches!(state.query_pool_swap(&params), Err(SimulationError::InvalidInput(..))),
                "tolerance {tolerance}"
            );
        }
    }

    /// A pool whose positions span the whole tick range absorbs everything up to the range end
    /// or, when that is further than one swap's gas allows, up to the row the gas budget reaches:
    /// thousands of tick-table rows either way.
    #[test]
    fn get_limits_reach_the_range_end_or_the_gas_budget() {
        // Rows a swap can afford after its base cost and the block's timepoint write, and the
        // rows' worth of gas the crossing at the range end costs.
        let affordable_rows = (MAX_SWAP_GAS - SWAP_BASE_GAS - TIMEPOINT_WRITE_GAS) / GAS_PER_STEP;
        let crossing_rows = GAS_PER_INITIALIZED_TICK_CROSS / GAS_PER_STEP;
        let full_range: Vec<_> = load_fixtures()
            .into_iter()
            .filter(|f| f.pre.ticks.len() == 2)
            .collect();
        assert!(!full_range.is_empty());
        for fixture in full_range {
            let label = format!("{} block {}", fixture.pool, fixture.block);
            let state = state_at_swap_block(&fixture);
            let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
            let (low, high) = (fixture.pre.ticks[0].0, fixture.pre.ticks[1].0);
            for (token_in, token_out, end, zero_for_one) in
                [(&token0, &token1, low, true), (&token1, &token0, high, false)]
            {
                let (limit_in, limit_out) = state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .unwrap();
                // The amounts of moving the whole liquidity from the current price to `tick`.
                let amounts_to = |tick: i32| {
                    let sqrt_end = get_sqrt_ratio_at_tick(tick).unwrap();
                    let (amount_in, amount_out) = if zero_for_one {
                        (
                            get_amount0_delta(sqrt_end, state.sqrt_price, state.liquidity, true),
                            get_amount1_delta(sqrt_end, state.sqrt_price, state.liquidity, false),
                        )
                    } else {
                        (
                            get_amount1_delta(sqrt_end, state.sqrt_price, state.liquidity, true),
                            get_amount0_delta(sqrt_end, state.sqrt_price, state.liquidity, false),
                        )
                    };
                    (u256_to_biguint(amount_in.unwrap()), u256_to_biguint(amount_out.unwrap()))
                };
                let rows_to_end = u64::from((end - state.tick).unsigned_abs() / 256 + 1);
                let (end_in, end_out) = amounts_to(end);
                if rows_to_end + crossing_rows < affordable_rows {
                    // Every row rounds its amounts once.
                    let rows = BigUint::from(rows_to_end + 1);
                    assert!(
                        limit_in >= end_in && limit_in <= &end_in + &rows,
                        "{label}: limit in {limit_in}, the range end needs {end_in}"
                    );
                    assert!(
                        limit_out <= end_out && &limit_out + &rows >= end_out,
                        "{label}: limit out {limit_out}, the range end yields {end_out}"
                    );
                } else {
                    // The gas budget binds first: the walk gets within two rows of what a swap
                    // can afford and stops short of the range end.
                    let affordable_ticks = 256 * (affordable_rows as i32 - 2);
                    let budget_tick = if zero_for_one {
                        state.tick - affordable_ticks
                    } else {
                        state.tick + affordable_ticks
                    };
                    let (budget_in, budget_out) = amounts_to(budget_tick);
                    let rows = BigUint::from(affordable_rows + 1);
                    assert!(
                        limit_in >= budget_in && limit_in < end_in,
                        "{label}: limit in {limit_in}, the budget reaches {budget_in}"
                    );
                    assert!(
                        &limit_out + &rows >= budget_out && limit_out <= end_out,
                        "{label}: limit out {limit_out}, the budget yields {budget_out}"
                    );
                }
                let (amount_out, _) = quote(&state, &limit_in, token_in, token_out);
                assert!(
                    amount_out <= &limit_out + 4096u32 && &amount_out * 100u32 > &limit_out * 99u32,
                    "{label}: swapping the limit yields {amount_out} of {limit_out}"
                );
            }
        }
    }

    /// A swap that runs past the last indexed tick still commits the timepoint write and adds
    /// the volume of the part it filled, like a full swap does.
    #[test]
    fn a_partial_fill_commits_the_write_and_its_volume() {
        let fixture = fixture("0x299c");
        let pre = &fixture.pre;
        let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
        let liquidity = 1_000_000_000_000_000_000u128;
        // Positions end at tick 100 with half the liquidity still in range, so a large sale of
        // token1 walks off the indexed ticks while the pool still has liquidity.
        let ticks = vec![
            TickInfo::new(-100, liquidity as i128).unwrap(),
            TickInfo::new(100, -(liquidity as i128) / 2).unwrap(),
        ];
        let mut state = CamelotV3State::new(
            fixture.pool.clone(),
            fixture.timestamp - 1,
            liquidity,
            get_sqrt_ratio_at_tick(0).unwrap(),
            0,
            pre.fee_zto,
            pre.fee_otz,
            pre.timepoint_index,
            0,
            ticks,
            fixture_timepoints(pre),
            fixture_config(&pre.fee_config_zto),
            fixture_config(&pre.fee_config_otz),
        )
        .unwrap();
        state.apply_block(&BlockContext::new(fixture.block, fixture.timestamp));
        let pending = state
            .pending_write()
            .unwrap()
            .expect("the first swap of the block writes");

        let err = state
            .get_amount_out(BigUint::from(10u32).pow(30), &token1, &token0)
            .unwrap_err();
        let SimulationError::InvalidInput(message, Some(partial)) = err else {
            panic!("expected a partial fill, got {err:?}")
        };
        assert_eq!(message, "Ticks exceeded");
        assert!(partial.amount > BigUint::zero());
        let after = partial
            .new_state
            .as_any()
            .downcast_ref::<CamelotV3State>()
            .unwrap();
        assert_eq!(after.timepoint_index, pending.timepoint.index);
        assert_eq!(
            after
                .timepoints
                .get(pending.timepoint.index),
            Some(&pending.timepoint)
        );
        assert_eq!((after.fee_zto, after.fee_otz), (pending.fee_zto, pending.fee_otz));
        assert_eq!(after.liquidity, liquidity / 2);
        assert_eq!(after.tick, 101);
        assert!(after.volume_per_liquidity_in_block > 0, "the filled part's volume is recorded");
    }

    /// A block's delta reaches every attribute family: the scalars, a tick whose net liquidity
    /// drops to zero but stays a step boundary, a removed tick, a new timepoint and a fee
    /// configuration.
    #[test]
    fn delta_transition_applies_every_attribute_family() {
        let fixture = fixture("0x299c");
        let pre = &fixture.pre;
        let mut state = state_from(&fixture);
        state.apply_block(&BlockContext::new(fixture.block + 1, fixture.timestamp + 20));
        let fees_before = state.fees().unwrap();
        let (kept, removed) = (pre.ticks[1].0, pre.ticks[2].0);
        assert_ne!(
            state
                .ticks
                .get_tick(kept)
                .unwrap()
                .net_liquidity,
            0
        );
        let last = *state
            .timepoints
            .get(pre.timepoint_index)
            .unwrap();
        let written = Timepoint {
            index: pre.timepoint_index + 1,
            block_timestamp: fixture.timestamp as u32 + 10,
            tick_cumulative: last.tick_cumulative + i64::from(last.average_tick) * 10,
            ..last
        };
        let base_fee = fixture_config(&pre.fee_config_zto).base_fee;
        let mut config = hex_bytes(&pre.fee_config_zto);
        config[8..10].copy_from_slice(&(base_fee + 1_000).to_be_bytes());
        let sqrt_price = get_sqrt_ratio_at_tick(-310_000).unwrap();
        let delta = ProtocolStateDelta {
            component_id: fixture.pool.clone(),
            updated_attributes: HashMap::from([
                ("liquidity".to_string(), Bytes::from(7u128.to_be_bytes())),
                (
                    "sqrt_price_x96".to_string(),
                    Bytes::from(sqrt_price.to_be_bytes::<32>()[12..].to_vec()),
                ),
                ("tick".to_string(), Bytes::from((-310_000i32).to_be_bytes()[1..].to_vec())),
                ("timepoint_index".to_string(), Bytes::from(written.index.to_be_bytes())),
                (
                    format!("timepoints/{}", written.index),
                    Bytes::from(written.to_attribute().to_vec()),
                ),
                ("fee_config_zto".to_string(), Bytes::from(config)),
                (format!("ticks/{kept}"), Bytes::from(0i128.to_be_bytes())),
            ]),
            deleted_attributes: HashSet::from([format!("ticks/{removed}")]),
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(state.liquidity, 7);
        assert_eq!(state.sqrt_price, sqrt_price);
        assert_eq!(state.tick, -310_000);
        assert_eq!(state.timepoint_index, written.index);
        assert_eq!(state.timepoints.get(written.index), Some(&written));
        assert_eq!(state.fee_config_zto.base_fee, base_fee + 1_000);
        assert_eq!(
            state
                .ticks
                .get_tick(kept)
                .unwrap()
                .net_liquidity,
            0
        );
        let (next_tick, next_tick_info) = state
            .ticks
            .next_initialized_tick_within_one_word(kept - 1, false)
            .unwrap();
        assert_eq!(next_tick, kept);
        assert!(next_tick_info.is_some(), "a zero-net tick stays a step boundary");
        assert!(state.ticks.get_tick(removed).is_none());
        let fees_after = state.fees().unwrap();
        assert_ne!(fees_after, fees_before);
        assert!(fees_after.0 >= base_fee + 1_000);
    }

    /// Liquidity without any indexed tick is inconsistent data: the pool quotes nothing, with
    /// an error a consumer may retry rather than one that drops the pool for good.
    #[test]
    fn a_pool_without_ticks_errors_recoverably() {
        let fixture = fixture("0x299c");
        let pre = &fixture.pre;
        let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
        let state = CamelotV3State::new(
            fixture.pool.clone(),
            fixture.timestamp,
            pre.liquidity.parse().unwrap(),
            U256::from_str(&pre.sqrt_price).unwrap(),
            pre.tick,
            pre.fee_zto,
            pre.fee_otz,
            pre.timepoint_index,
            0,
            Vec::new(),
            fixture_timepoints(pre),
            fixture_config(&pre.fee_config_zto),
            fixture_config(&pre.fee_config_otz),
        )
        .unwrap();

        assert!(matches!(
            state.get_amount_out(whole_token(&token0), &token0, &token1),
            Err(SimulationError::RecoverableError(_))
        ));
        assert_eq!(
            state
                .get_limits(token0.address.clone(), token1.address.clone())
                .unwrap(),
            (BigUint::zero(), BigUint::zero())
        );
    }

    /// The ring must hold the timepoint before the last written one: the first swap of a block
    /// derives the previous tick from it, and without it the fee would silently diverge.
    #[test]
    fn a_ring_without_the_timepoint_before_the_last_is_rejected() {
        let fixture = fixture("0x299c");
        let pre = &fixture.pre;
        let before_last = pre.timepoint_index - 1;
        let timepoints: Vec<_> = fixture_timepoints(pre)
            .into_iter()
            .filter(|t| t.index != before_last)
            .collect();
        let err = CamelotV3State::new(
            fixture.pool.clone(),
            fixture.timestamp,
            pre.liquidity.parse().unwrap(),
            U256::from_str(&pre.sqrt_price).unwrap(),
            pre.tick,
            pre.fee_zto,
            pre.fee_otz,
            pre.timepoint_index,
            0,
            Vec::new(),
            timepoints,
            fixture_config(&pre.fee_config_zto),
            fixture_config(&pre.fee_config_otz),
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&fixture.pool), "{message}");
        assert!(message.contains(&format!("timepoint {before_last}")), "{message}");
    }

    /// Before `initialize` a pool has no price and no ring: it reports the fees its constructor
    /// stored and refuses to quote with an error a consumer may retry once it is initialized.
    #[test]
    fn an_uninitialized_pool_errors_recoverably_with_its_stored_fees() {
        let fixture = fixture("0x299c");
        let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
        let config = fixture_config(&fixture.pre.fee_config_zto);
        let state = CamelotV3State::new(
            "pool".to_string(),
            fixture.timestamp,
            0,
            U256::ZERO,
            0,
            100,
            100,
            0,
            0,
            Vec::new(),
            Vec::new(),
            config,
            config,
        )
        .unwrap();

        assert!(state.pending_write().unwrap().is_none());
        assert_eq!(state.fees().unwrap(), (100, 100));
        assert!(matches!(
            state.get_amount_out(whole_token(&token0), &token0, &token1),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            state.spot_price(&token0, &token1),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    /// A pool parked at its price boundary with no liquidity (SUSHI/WETH `0x1Ec0979B…` on chain
    /// holds donated tokens in that state) prices from its stored state, reports empty limits and
    /// refuses to quote with an error a consumer may retry, so it never takes a stream down.
    #[test]
    fn a_pool_at_its_price_boundary_without_liquidity_stays_quotable() {
        let fixture = fixture("0x299c");
        let pre = &fixture.pre;
        let (token0, token1) = (token(&fixture.token0), token(&fixture.token1));
        let mut state = CamelotV3State::new(
            fixture.pool.clone(),
            fixture.timestamp - 1,
            0,
            safe_add_u256(MIN_SQRT_RATIO, U256::from(1u64)).unwrap(),
            MIN_TICK,
            pre.fee_zto,
            pre.fee_otz,
            pre.timepoint_index,
            0,
            Vec::new(),
            fixture_timepoints(pre),
            fixture_config(&pre.fee_config_zto),
            fixture_config(&pre.fee_config_otz),
        )
        .unwrap();
        state.apply_block(&BlockContext::new(fixture.block, fixture.timestamp));

        for (base, quote_token) in [(&token0, &token1), (&token1, &token0)] {
            let spot = state
                .spot_price(base, quote_token)
                .unwrap();
            assert!(spot.is_finite() && spot > 0.0, "spot price {spot}");
        }
        assert!(state.fee().is_finite());
        assert_eq!(
            state
                .get_limits(token0.address.clone(), token1.address.clone())
                .unwrap(),
            (BigUint::zero(), BigUint::zero())
        );
        assert!(matches!(
            state.get_amount_out(whole_token(&token0), &token0, &token1),
            Err(SimulationError::RecoverableError(_))
        ));
    }
}
