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
        protocol_sim::{Balances, BlockContext, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::{
    evm::protocol::{
        safe_math::{safe_add_u256, safe_sub_u256},
        u256_num::u256_to_biguint,
        utils::{
            add_fee_markup,
            slipstreams::{
                dynamic_fee_module::{get_dynamic_fee, DynamicFeeConfig, ResolvedFee},
                observations::{Observation, Observations},
            },
            uniswap::{
                i24_be_bytes_to_i32, liquidity_math,
                sqrt_price_math::{get_amount0_delta, get_amount1_delta, sqrt_price_q96_to_f64},
                swap_math,
                tick_list::{TickInfo, TickList, TickListErrorKind},
                tick_math::{
                    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK,
                    MIN_SQRT_RATIO, MIN_TICK,
                },
                StepComputation, SwapResults, SwapState,
            },
        },
    },
    protocol::models::BlockPositionAssumption,
};

// Cold-storage warmup on the first loop iteration:
// nextInitializedTickWithinOneWord first call (~3,000) vs warm (~1,060)
// calculateFees first call via cold getUnstakedFee STATICCALL (~19,050) vs warm (~6,055)
const FIRST_LOOP_OVERHEAD: i32 = 15_000;
// Steady-state per-loop: nextInitializedTickWithinOneWord (warm) + getSqrtRatioAtTick
// + computeSwapStep + calculateFees (warm) + toInt256x2 + EVM opcode overhead
const LOOP_GAS_COST: i32 = 12_500;
// cross(): updates tick fee growth and staked reward growth slots.
// Warm ticks (previously crossed, non-zero SSTORE slots) cost ~22k; cold ticks ~76k.
// We bias toward the cold end to prefer overestimation: 70k.
const TICK_CROSSING_GAS_COST: i32 = 70_000;
// When dfc.scaling_factor != 0, fee() does a TWAP binary search on the observation ring
// buffer (~77k–91k gas) instead of a simple slot read (~18k–27k gas). This extra cost is
// added once per swap on top of the base.
const TWAP_FEE_OVERHEAD: i32 = 65_000;
// Pre/post loop overhead: fee(), slot0 reads, end-of-swap writes.
const SWAP_BASE_GAS: i32 = 125_000;
// Conservative max gas for a single swap. Used to cap get_limits iteration.
const MAX_SWAP_GAS: u64 = 16_700_000;
// Maximum initialized ticks that can be crossed within MAX_SWAP_GAS.
const MAX_TICKS_CROSSED: u64 =
    (MAX_SWAP_GAS - SWAP_BASE_GAS as u64) / TICK_CROSSING_GAS_COST as u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AerodromeSlipstreamsState {
    id: String,
    /// Timestamp of the block a quote against this state is expected to execute in.
    ///
    /// Maintained by the stream decoder via [`ProtocolSim::apply_block`], not decoded from
    /// the pool: the fee module's initial-vs-dynamic branch keys on the *execution* block, which
    /// is the next block for a confirmed update and the still-open block for a flashblock
    /// update.
    execution_block_timestamp: u64,
    liquidity: u128,
    sqrt_price: U256,
    observation_index: u16,
    observation_cardinality: u16,
    default_fee: u32,
    tick_spacing: i32,
    tick: i32,
    ticks: TickList,
    observations: Observations,
    dfc: DynamicFeeConfig,
    /// What quotes may assume about the swap's position within its execution block; see
    /// [`BlockPositionAssumption`].
    position_assumption: BlockPositionAssumption,
}

impl AerodromeSlipstreamsState {
    /// Creates a new instance of `AerodromeSlipstreamsState`.
    ///
    /// # Arguments
    /// - `id`: The id of the protocol component.
    /// - `execution_block_timestamp`: Timestamp of the block a quote is expected to execute in.
    /// - `liquidity`: The initial liquidity of the pool.
    /// - `sqrt_price`: The square root of the current price.
    /// - `observation_index`: The index of the current observation.
    /// - `observation_cardinality`: The cardinality of the observation.
    /// - `default_fee`: The default fee for the pool.
    /// - `tick_spacing`: The tick spacing for the pool.
    /// - `tick`: The current tick of the pool.
    /// - `ticks`: A vector of `TickInfo` representing the tick information for the pool.
    /// - `observations`: A vector of `Observation` representing the observation information for the
    ///   pool.
    /// - `dfc`: The dynamic fee configuration for the pool.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        execution_block_timestamp: u64,
        liquidity: u128,
        sqrt_price: U256,
        observation_index: u16,
        observation_cardinality: u16,
        default_fee: u32,
        tick_spacing: i32,
        tick: i32,
        ticks: Vec<TickInfo>,
        observations: Vec<Observation>,
        dfc: DynamicFeeConfig,
    ) -> Result<Self, SimulationError> {
        let tick_list = TickList::from(tick_spacing as u16, ticks)?;
        Ok(AerodromeSlipstreamsState {
            id,
            execution_block_timestamp,
            liquidity,
            sqrt_price,
            observation_index,
            observation_cardinality,
            default_fee,
            tick_spacing,
            tick,
            ticks: tick_list,
            observations: Observations::new(observations),
            dfc,
            position_assumption: BlockPositionAssumption::default(),
        })
    }

    /// Sets what quotes assume about the swap's position within its execution block.
    ///
    /// A consumer-side preference, independent of the pool's on-chain state.
    pub fn with_position_assumption(mut self, assumption: BlockPositionAssumption) -> Self {
        self.position_assumption = assumption;
        self
    }

    fn get_fee(&self) -> Result<ResolvedFee, SimulationError> {
        get_dynamic_fee(
            &self.dfc,
            self.default_fee,
            self.tick,
            self.liquidity,
            self.observation_index,
            self.observation_cardinality,
            &self.observations,
            self.execution_block_timestamp as u32,
            self.position_assumption == BlockPositionAssumption::First,
        )
    }

    /// Records the observation the pool would write for a swap that moved the tick from
    /// `self.tick` to `post_swap_tick`, so that a second swap chained onto this state in the same
    /// block resolves the dynamic fee instead of the initial fee.
    ///
    /// Mirrors `CLPool.swap`, which writes only when the tick moved and passes the pre-swap tick
    /// and liquidity. Must be called before the caller overwrites `tick`/`liquidity`.
    fn record_observation(&mut self, post_swap_tick: i32) -> Result<(), SimulationError> {
        if post_swap_tick == self.tick {
            return Ok(());
        }
        self.observation_index = self.observations.write(
            self.observation_index,
            self.execution_block_timestamp as u32,
            self.tick,
            self.liquidity,
            self.observation_cardinality,
        )?;
        Ok(())
    }

    fn swap(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
    ) -> Result<SwapResults, SimulationError> {
        if self.liquidity == 0 {
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

        let exact_input = amount_specified > I256::from_raw(U256::from(0u64));

        let mut state = SwapState {
            amount_remaining: amount_specified,
            amount_calculated: I256::from_raw(U256::from(0u64)),
            sqrt_price: self.sqrt_price,
            tick: self.tick,
            liquidity: self.liquidity,
        };
        let resolved_fee = self.get_fee()?;
        let twap_overhead = if resolved_fee.observed_twap { TWAP_FEE_OVERHEAD } else { 0 };
        let mut gas_used = U256::from((SWAP_BASE_GAS + twap_overhead) as u64);
        let mut n_loops = 0;

        let fee = resolved_fee.fee;
        while state.amount_remaining != I256::from_raw(U256::from(0u64)) &&
            state.sqrt_price != price_limit
        {
            let (mut next_tick, initialized) = match self
                .ticks
                .next_initialized_tick_within_one_word(state.tick, zero_for_one)
            {
                Ok((tick, init)) => (tick, init),
                Err(tick_err) => match tick_err.kind {
                    TickListErrorKind::TicksExeeded => {
                        let mut new_state = self.clone();
                        // Best effort in an error path: a failed write only degrades the fee of
                        // a chained simulation on this partial result, and must not mask the
                        // more informative TicksExceeded error below.
                        if let Err(record_err) = new_state.record_observation(state.tick) {
                            trace!(%record_err, "skipping observation write on partial result");
                        }
                        new_state.liquidity = state.liquidity;
                        new_state.tick = state.tick;
                        new_state.sqrt_price = state.sqrt_price;
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
            let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;
            let (sqrt_price, amount_in, amount_out, fee_amount) = swap_math::compute_swap_step(
                state.sqrt_price,
                AerodromeSlipstreamsState::get_sqrt_ratio_target(
                    sqrt_price_next,
                    price_limit,
                    zero_for_one,
                ),
                state.liquidity,
                state.amount_remaining,
                fee,
            )?;
            state.sqrt_price = sqrt_price;

            let step = StepComputation {
                sqrt_price_start,
                tick_next: next_tick,
                initialized,
                sqrt_price_next,
                amount_in,
                amount_out,
                fee_amount,
            };
            if exact_input {
                state.amount_remaining -= I256::checked_from_sign_and_abs(
                    Sign::Positive,
                    safe_add_u256(step.amount_in, step.fee_amount)?,
                )
                .unwrap();
                state.amount_calculated -=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_out).unwrap();
            } else {
                state.amount_remaining +=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_out).unwrap();
                state.amount_calculated += I256::checked_from_sign_and_abs(
                    Sign::Positive,
                    safe_add_u256(step.amount_in, step.fee_amount)?,
                )
                .unwrap();
            }
            if state.sqrt_price == step.sqrt_price_next {
                if step.initialized {
                    let liquidity_raw = self
                        .ticks
                        .get_tick(step.tick_next)
                        .unwrap()
                        .net_liquidity;
                    let liquidity_net = if zero_for_one { -liquidity_raw } else { liquidity_raw };
                    state.liquidity =
                        liquidity_math::add_liquidity_delta(state.liquidity, liquidity_net)?;
                    gas_used = safe_add_u256(gas_used, U256::from(TICK_CROSSING_GAS_COST))?;
                }
                state.tick = if zero_for_one { step.tick_next - 1 } else { step.tick_next };
            } else if state.sqrt_price != step.sqrt_price_start {
                state.tick = get_tick_at_sqrt_ratio(state.sqrt_price)?;
            }
            gas_used = safe_add_u256(gas_used, U256::from(LOOP_GAS_COST))?;
            if n_loops == 0 {
                gas_used = safe_add_u256(gas_used, U256::from(FIRST_LOOP_OVERHEAD))?;
            }
            n_loops += 1;
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

    fn get_sqrt_ratio_target(
        sqrt_price_next: U256,
        sqrt_price_limit: U256,
        zero_for_one: bool,
    ) -> U256 {
        let cond1 = if zero_for_one {
            sqrt_price_next < sqrt_price_limit
        } else {
            sqrt_price_next > sqrt_price_limit
        };

        if cond1 {
            sqrt_price_limit
        } else {
            sqrt_price_next
        }
    }
}

#[typetag::serde]
impl ProtocolSim for AerodromeSlipstreamsState {
    fn fee(&self) -> f64 {
        match self.get_fee() {
            Ok(resolved) => resolved.fee as f64 / 1_000_000.0,
            Err(err) => {
                error!(
                    pool = %self.id,
                    execution_block_timestamp = self.execution_block_timestamp,
                    %err,
                    "Error while calculating dynamic fee"
                );
                f64::MAX / 1_000_000.0
            }
        }
    }

    fn spot_price(&self, a: &Token, b: &Token) -> Result<f64, SimulationError> {
        let price = if a < b {
            sqrt_price_q96_to_f64(self.sqrt_price, a.decimals, b.decimals)?
        } else {
            1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, b.decimals, a.decimals)?
        };
        Ok(add_fee_markup(price, self.get_fee()?.fee as f64 / 1_000_000.0))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_a: &Token,
        token_b: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let zero_for_one = token_a < token_b;
        let amount_specified = I256::checked_from_sign_and_abs(
            Sign::Positive,
            U256::from_be_slice(&amount_in.to_bytes_be()),
        )
        .ok_or_else(|| {
            SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
        })?;

        let result = self.swap(zero_for_one, amount_specified, None)?;

        trace!(?amount_in, ?token_a, ?token_b, ?zero_for_one, ?result, "SLIPSTREAMS SWAP");
        let mut new_state = self.clone();
        new_state.record_observation(result.tick)?;
        new_state.liquidity = result.liquidity;
        new_state.tick = result.tick;
        new_state.sqrt_price = result.sqrt_price;

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
        // If the pool has no liquidity, return zeros for both limits
        if self.liquidity == 0 {
            return Ok((BigUint::zero(), BigUint::zero()));
        }

        let zero_for_one = token_in < token_out;
        let mut current_tick = self.tick;
        let mut current_sqrt_price = self.sqrt_price;
        let mut current_liquidity = self.liquidity;
        let mut total_amount_in = U256::from(0u64);
        let mut total_amount_out = U256::from(0u64);

        // Iterate through all ticks in the direction of the swap
        // Continues until there is no more liquidity in the pool or no more ticks to process
        let mut ticks_crossed: u64 = 0;
        while let Ok((tick, initialized)) = self
            .ticks
            .next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
            if ticks_crossed >= MAX_TICKS_CROSSED {
                break;
            }
            ticks_crossed += 1;
            // Clamp the tick value to ensure it's within valid range
            let next_tick = tick.clamp(MIN_TICK, MAX_TICK);

            // Calculate the sqrt price at the next tick boundary
            let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;

            // Calculate the amount of tokens swapped when moving from current_sqrt_price to
            // sqrt_price_next. Direction determines which token is being swapped in vs out
            let (amount_in, amount_out) = if zero_for_one {
                let amount0 = get_amount0_delta(
                    sqrt_price_next,
                    current_sqrt_price,
                    current_liquidity,
                    true,
                )?;
                let amount1 = get_amount1_delta(
                    sqrt_price_next,
                    current_sqrt_price,
                    current_liquidity,
                    false,
                )?;
                (amount0, amount1)
            } else {
                let amount0 = get_amount0_delta(
                    sqrt_price_next,
                    current_sqrt_price,
                    current_liquidity,
                    false,
                )?;
                let amount1 = get_amount1_delta(
                    sqrt_price_next,
                    current_sqrt_price,
                    current_liquidity,
                    true,
                )?;
                (amount1, amount0)
            };

            // Accumulate total amounts for this tick range
            total_amount_in = safe_add_u256(total_amount_in, amount_in)?;
            total_amount_out = safe_add_u256(total_amount_out, amount_out)?;

            // If this tick is "initialized" (meaning its someone's position boundary), update the
            // liquidity when crossing it
            // For zero_for_one, liquidity is removed when crossing a tick
            // For one_for_zero, liquidity is added when crossing a tick
            if initialized {
                let liquidity_raw = self
                    .ticks
                    .get_tick(next_tick)
                    .unwrap()
                    .net_liquidity;
                let liquidity_delta = if zero_for_one { -liquidity_raw } else { liquidity_raw };
                current_liquidity =
                    liquidity_math::add_liquidity_delta(current_liquidity, liquidity_delta)?;
            }

            // Move to the next tick position
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
        // apply attribute changes
        if let Some(liquidity) = delta
            .updated_attributes
            .get("liquidity")
        {
            // This is a hotfix because if the liquidity has never been updated after creation, it's
            // currently encoded as H256::zero(), therefore, we can't decode this as u128.
            // We can remove this once it has been fixed on the tycho side.
            let liq_16_bytes = if liquidity.len() == 32 {
                // Make sure it only happens for 0 values, otherwise error.
                if liquidity == &Bytes::zero(32) {
                    Bytes::from([0; 16])
                } else {
                    return Err(TransitionError::DecodeError(format!(
                        "Liquidity bytes too long for {liquidity}, expected 16",
                    )));
                }
            } else {
                liquidity.clone()
            };

            self.liquidity = u128::from(liq_16_bytes);
        }
        if let Some(sqrt_price) = delta
            .updated_attributes
            .get("sqrt_price_x96")
        {
            self.sqrt_price = U256::from_be_slice(sqrt_price);
        }
        if let Some(observation_index) = delta
            .updated_attributes
            .get("observationIndex")
        {
            self.observation_index = u16::from(observation_index.clone());
        }
        if let Some(observation_cardinality) = delta
            .updated_attributes
            .get("observationCardinality")
        {
            self.observation_cardinality = u16::from(observation_cardinality.clone());
        }
        if let Some(default_fee) = delta
            .updated_attributes
            .get("default_fee")
        {
            self.default_fee = u32::from(default_fee.clone());
        }
        self.dfc
            .update_from_attributes(&delta.updated_attributes)
            .map_err(|err| {
                TransitionError::DecodeError(format!(
                    "Failed to update dynamic fee module config: {err}"
                ))
            })?;
        if let Some(tick) = delta.updated_attributes.get("tick") {
            // This is a hotfix because if the tick has never been updated after creation, it's
            // currently encoded as H256::zero(), therefore, we can't decode this as i32.
            // We can remove this once it has been fixed on the tycho side.
            let ticks_4_bytes = if tick.len() == 32 {
                // Make sure it only happens for 0 values, otherwise error.
                if tick == &Bytes::zero(32) {
                    Bytes::from([0; 4])
                } else {
                    return Err(TransitionError::DecodeError(format!(
                        "Tick bytes too long for {tick}, expected 4"
                    )));
                }
            } else {
                tick.clone()
            };
            self.tick = i24_be_bytes_to_i32(&ticks_4_bytes);
        }

        // apply tick & observations changes
        for (key, value) in delta.updated_attributes.iter() {
            // tick liquidity keys are in the format "ticks/{tick_index}/net_liquidity"
            if key.starts_with("ticks/") {
                let parts: Vec<&str> = key.split('/').collect();
                self.ticks
                    .set_tick_liquidity(
                        parts[1]
                            .parse::<i32>()
                            .map_err(|err| TransitionError::DecodeError(err.to_string()))?,
                        i128::from(value.clone()),
                    )
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
            }

            // observations keys are in the format "observations/{observation_index}"
            if let Some(idx_str) = key.strip_prefix("observations/") {
                if let Ok(idx) = idx_str.parse::<i32>() {
                    let _ = self
                        .observations
                        .upsert_observation(idx, value);
                }
            }
        }
        // delete ticks - ignores deletes for attributes other than tick liquidity
        for key in delta.deleted_attributes.iter() {
            // tick liquidity keys are in the format "ticks/{tick_index}/net_liquidity"
            if key.starts_with("ticks/") {
                let parts: Vec<&str> = key.split('/').collect();
                self.ticks
                    .set_tick_liquidity(
                        parts[1]
                            .parse::<i32>()
                            .map_err(|err| TransitionError::DecodeError(err.to_string()))?,
                        0,
                    )
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
            }

            // observations keys are in the format "observations/{observation_index}"
            if let Some(idx_str) = key.strip_prefix("observations/") {
                if let Ok(idx) = idx_str.parse::<i32>() {
                    let _ = self
                        .observations
                        .upsert_observation(idx, &[]);
                }
            }
        }
        Ok(())
    }

    /// Re-emits only when the resolved fee actually changed: idle pools whose initial-vs-dynamic
    /// branch stays put return `false` indefinitely, and same-block flashblocks short-circuit on
    /// the unchanged timestamp.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let timestamp = block.timestamp();
        if timestamp == self.execution_block_timestamp {
            return false;
        }
        let fee_before = self.get_fee().ok();
        self.execution_block_timestamp = timestamp;
        fee_before != self.get_fee().ok()
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
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<AerodromeSlipstreamsState>()
        {
            let self_fee = match self.get_fee() {
                Ok(fee) => fee,
                Err(_) => return false,
            };
            let other_fee = match other_state.get_fee() {
                Ok(fee) => fee,
                Err(_) => return false,
            };

            self.liquidity == other_state.liquidity &&
                self.sqrt_price == other_state.sqrt_price &&
                self_fee == other_fee &&
                self.tick == other_state.tick &&
                self.ticks == other_state.ticks
        } else {
            false
        }
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::{Sign, I256, U256};
    use tycho_common::{models::Chain, simulation::errors::SimulationError};

    use super::*;
    use crate::evm::protocol::utils::{
        slipstreams::{dynamic_fee_module::DynamicFeeConfig, observations::Observation},
        uniswap::{
            tick_list::TickInfo,
            tick_math::{
                get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MIN_SQRT_RATIO,
                MIN_TICK,
            },
        },
    };

    fn create_basic_test_pool() -> AerodromeSlipstreamsState {
        let sqrt_price = get_sqrt_ratio_at_tick(0).expect("Failed to calculate sqrt price");
        let ticks = vec![TickInfo::new(-120, 0).unwrap(), TickInfo::new(120, 0).unwrap()];
        AerodromeSlipstreamsState::new(
            "test-pool".to_string(),
            1_000_000,
            100_000_000_000_000_000_000u128,
            sqrt_price,
            0,
            1,
            3000,
            1,
            0,
            ticks,
            vec![Observation::default()],
            DynamicFeeConfig::new(3000, 10_000, 1, false, 0),
        )
        .expect("Failed to create pool")
    }

    fn dynamic_fee_delta(dynamic_fee_module: [u8; 20]) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: "test-pool".to_string(),
            updated_attributes: HashMap::from([
                ("dynamic_fee_module".to_string(), Bytes::from(dynamic_fee_module)),
                ("dfc_baseFee".to_string(), Bytes::from(500_u32.to_be_bytes())),
                ("dfc_scalingFactor".to_string(), Bytes::from(0_u64.to_be_bytes())),
                ("dfc_feeCap".to_string(), Bytes::from(700_u32.to_be_bytes())),
                ("dfc_initialFeeEnabled".to_string(), Bytes::from([0_u8])),
                ("dfc_initialFee".to_string(), Bytes::from(0_u32.to_be_bytes())),
            ]),
            ..Default::default()
        }
    }

    /// Pool whose last swap wrote an observation at `last_observation_ts`, with the initial fee
    /// enabled (750 pips) and a dynamic component on top of a 2700 pip base.
    ///
    /// Built with the first-in-block assumption on: most tests here exercise the optimistic
    /// path. The worst-case-default tests switch it back to `BlockPositionAssumption::WorstCase`.
    fn initial_fee_pool(last_observation_ts: u32) -> AerodromeSlipstreamsState {
        let mut pool = create_basic_test_pool();
        pool.dfc = DynamicFeeConfig::new(2700, 30_000, 0, true, 750);
        pool.position_assumption = BlockPositionAssumption::First;
        pool.observations = Observations::new(vec![Observation {
            block_timestamp: last_observation_ts,
            initialized: true,
            index: 0,
            ..Default::default()
        }]);
        pool
    }

    /// Replays Base block 50166683 on pool 0xdFe5F275020def30993f042174Fc2D335678b626
    /// (AERO/cbBTC), the pair of swaps from the original report:
    ///
    /// - tx 0x3b0a96e9bb376d74b4b99d651336c790b2b2b65a660491c28cae3df1a5d69def (index 67), the
    ///   block's first tick-moving swap, paid the 750 pip initial fee;
    /// - tx 0xe934500efe7f9ef56370daf4859c21c3a439d998a80b5bd2e5a117e3045021e1 (index 154) paid the
    ///   2700 pip dynamic fee.
    ///
    /// Pool state is reconstructed from archive RPC at the parent block 50166682 (slot0,
    /// liquidity, observations[213], DynamicSwapFeeModule config); swap amounts come from the
    /// on-chain Swap events. Both outputs must match wei-exact, and the end-of-block oracle
    /// index must match the chain (213 -> 214: exactly one observation written).
    #[test]
    fn replays_base_block_50166683_swap_pair_wei_exact() {
        let mut observations: Vec<Observation> = (0..213)
            .map(|index| Observation { index, ..Default::default() })
            .collect();
        observations.push(Observation {
            block_timestamp: 1_787_122_711, // == parent block ts: the pool traded in that block
            tick_cumulative: -18_995_710_863_218,
            seconds_per_liquidity_cumulative_x128: U256::from_str(
                "42501948193164408449462610706599523891176959",
            )
            .unwrap(),
            initialized: true,
            index: 213,
        });

        let mut pool = AerodromeSlipstreamsState::new(
            "0xdFe5F275020def30993f042174Fc2D335678b626".to_string(),
            1_787_122_711, // seed: decoded at the parent block
            1_128_781_556_759_264_064u128,
            U256::from_str("1979649713595747421731").unwrap(),
            213,
            360,
            2700, // tickSpacingToFee(200)
            200,
            -350_116,
            // No initialized tick is crossed (liquidity is unchanged across both swaps);
            // zero-net bounds outside the traversed range stand in for the full tick map.
            vec![TickInfo::new(-351_000, 0).unwrap(), TickInfo::new(-349_000, 0).unwrap()],
            observations,
            DynamicFeeConfig::new(2700, 0, 0, true, 750),
        )
        .expect("state should build")
        // The replayed swap was in fact the block's first: the optimistic mode reproduces it.
        .with_position_assumption(BlockPositionAssumption::First);

        // The quotes execute in block 50166683 (ts 1_787_122_713).
        assert!(pool.apply_block(&BlockContext::new(50_166_683, 1_787_122_713)));

        let aero = Token::new(
            &Bytes::from_str("0x940181a94A35A4569E4529A3CDfB74e38FD98631").unwrap(),
            "AERO",
            18,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        );
        let cbbtc = Token::new(
            &Bytes::from_str("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf").unwrap(),
            "cbBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        );

        assert_eq!(pool.fee(), 750.0 / 1_000_000.0);
        let first = pool
            .get_amount_out(BigUint::from(1_688_626u32), &cbbtc, &aero)
            .expect("first swap should succeed");
        assert_eq!(first.amount, BigUint::from(2_702_489_253_591_513_843_346u128));

        assert_eq!(first.new_state.fee(), 2700.0 / 1_000_000.0);
        let second = first
            .new_state
            .get_amount_out(BigUint::from(450_733u32), &cbbtc, &aero)
            .expect("second swap should succeed");
        assert_eq!(second.amount, BigUint::from(719_894_300_964_297_656_776u128));

        let replayed = first
            .new_state
            .as_any()
            .downcast_ref::<AerodromeSlipstreamsState>()
            .expect("state type");
        assert_eq!(replayed.observation_index, 214, "chain slot0 shows 214 after the block");
        assert_eq!(
            replayed
                .observations
                .timestamp_at(214, 360)
                .unwrap(),
            1_787_122_713
        );
    }

    #[test]
    fn ticks_exceeded_partial_result_still_records_the_observation() {
        // The partial result carried inside the TicksExceeded error must price a chained swap
        // with the dynamic fee, exactly like a successful swap's new_state.
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(101, 1_002));
        let token_a =
            Token::new(&Bytes::from([0x11; 20]), "A", 18, 0, &[Some(10_000)], Chain::Base, 100);
        let token_b =
            Token::new(&Bytes::from([0x22; 20]), "B", 18, 0, &[Some(10_000)], Chain::Base, 100);

        let err = pool
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000_000_000u128),
                &token_a,
                &token_b,
            )
            .expect_err("swap must exhaust the tick list");
        let SimulationError::InvalidInput(_, Some(partial)) = err else {
            panic!("expected a partial result, got {err:?}");
        };

        assert_eq!(partial.new_state.fee(), 2700.0 / 1_000_000.0);
    }

    #[test]
    fn default_quotes_the_worse_fee_when_position_is_unknown() {
        // Without the first-in-block assumption the quote must never over-state the output:
        // before the pool is touched in the execution block, the worse of the two branches
        // (here the 2700 dynamic fee) applies — which is also the pre-fix behavior.
        let mut pool = initial_fee_pool(1_000);
        pool.position_assumption = BlockPositionAssumption::WorstCase;
        pool.apply_block(&BlockContext::new(101, 1_002));

        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            2700
        );
    }

    #[test]
    fn worst_case_picks_the_initial_fee_when_it_is_the_higher_one() {
        // Nothing stops a pool from configuring initialFee above its dynamic fee, so the worst
        // case is max(initial, dynamic).
        let mut pool = initial_fee_pool(1_000);
        pool.dfc = DynamicFeeConfig::new(500, 30_000, 0, true, 4_000);
        pool.position_assumption = BlockPositionAssumption::WorstCase;
        pool.apply_block(&BlockContext::new(101, 1_002));

        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            4_000
        );
    }

    #[test]
    fn worst_case_keeps_a_flat_fee_pool_quiet_across_blocks() {
        // With scaling 0 the worst-case fee is constant, so apply_block must never request a
        // re-emission: the default mode adds no per-block load for such pools.
        let mut pool = initial_fee_pool(1_000);
        pool.position_assumption = BlockPositionAssumption::WorstCase;
        pool.apply_block(&BlockContext::new(100, 1_000));

        assert!(!pool.apply_block(&BlockContext::new(101, 1_002)));
        assert!(!pool.apply_block(&BlockContext::new(102, 1_004)));
    }

    #[test]
    fn apply_block_reports_a_fee_flip_and_is_idempotent() {
        // Pool traded in block 100 (ts 1_000): decoded with execution block == that block, so the
        // dynamic fee applies. Crossing to the next block flips the branch to the initial fee.
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(100, 1_000));

        assert!(pool.apply_block(&BlockContext::new(101, 1_002)), "branch flip must re-emit");
        assert!(!pool.apply_block(&BlockContext::new(101, 1_002)), "repeat block is a no-op");
    }

    #[test]
    fn apply_block_stays_quiet_while_the_fee_does_not_move() {
        // Idle pool: the initial fee already applies and keeps applying as blocks pass, so
        // consumers must not be told anything changed.
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(101, 1_002));

        assert!(!pool.apply_block(&BlockContext::new(102, 1_004)));
        assert!(!pool.apply_block(&BlockContext::new(103, 1_006)));
    }

    #[test]
    fn quotes_initial_fee_for_the_next_block_after_the_pool_traded() {
        // The pool wrote its observation in the block we decoded. A quote lands in the *next*
        // block, where no observation exists yet — under the first-in-block assumption it pays
        // the initial fee.
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(101, 1_002));

        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            750
        );
    }

    #[test]
    fn quotes_dynamic_fee_when_targeting_a_block_the_pool_already_traded_in() {
        // Flashblock consumer: the block is still open and the pool traded in an earlier
        // flashblock, so a quote landing later in the same block pays the dynamic fee.
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(100, 1_000));

        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            2700
        );
    }

    #[test]
    fn chained_swap_in_the_same_block_pays_the_dynamic_fee() {
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(101, 1_002));
        let token_a =
            Token::new(&Bytes::from([0x11; 20]), "A", 18, 0, &[Some(10_000)], Chain::Base, 100);
        let token_b =
            Token::new(&Bytes::from([0x22; 20]), "B", 18, 0, &[Some(10_000)], Chain::Base, 100);

        assert_eq!(pool.fee(), 750.0 / 1_000_000.0);

        let result = pool
            .get_amount_out(BigUint::from(100_000_000_000_000_000u128), &token_a, &token_b)
            .expect("first swap should succeed");

        // The first swap moved the tick, so it wrote an observation at the execution timestamp;
        // the pool state it hands back prices the next swap in that block as a follow-up.
        assert_eq!(result.new_state.fee(), 2700.0 / 1_000_000.0);
    }

    #[test]
    fn swap_that_does_not_move_the_tick_leaves_the_initial_fee_available() {
        // `CLPool.swap` only writes an observation when the tick changed, so a swap that stays
        // inside the tick leaves the next swap in the block on the initial fee.
        let mut pool = initial_fee_pool(1_000);
        pool.apply_block(&BlockContext::new(101, 1_002));

        pool.record_observation(pool.tick)
            .expect("no-op write should succeed");

        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            750
        );
    }

    #[test]
    fn initial_fee_branch_does_not_charge_the_twap_gas_overhead() {
        let mut pool = initial_fee_pool(1_000);
        pool.dfc = DynamicFeeConfig::new(2700, 30_000, 6_000_000, true, 750);
        pool.apply_block(&BlockContext::new(101, 1_002));

        let resolved = pool
            .get_fee()
            .expect("fee should be computable");

        assert_eq!(resolved, ResolvedFee { fee: 750, observed_twap: false });
    }

    #[test]
    fn dynamic_fee_update_applies_for_supported_module() {
        let mut pool = create_basic_test_pool();
        pool.dfc = DynamicFeeConfig::new(4500, 10_000, 1, false, 0);
        let delta =
            dynamic_fee_delta(hex_literal::hex!("090b2A6bb475c00e2256e2095A60887cD710803b"));

        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .expect("dynamic fee update should be valid");

        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            500
        );
    }

    #[test]
    fn dynamic_fee_update_falls_back_to_default_for_unsupported_module() {
        // An unsupported-module delta resets to default rather than erroring; pool keeps
        // default_fee.
        let mut pool = create_basic_test_pool();
        pool.dfc = DynamicFeeConfig::new(4500, 10_000, 1, false, 0);
        let delta =
            dynamic_fee_delta(hex_literal::hex!("DB45818A6db280ecfeB33cbeBd445423d0216b5D"));

        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .expect("unsupported module delta should decode to the default config");

        assert_eq!(pool.dfc, DynamicFeeConfig::default());
        assert_eq!(
            pool.get_fee()
                .expect("fee should be computable")
                .fee,
            3000
        );
    }

    #[test]
    fn applies_partial_dynamic_fee_updates_after_module_initialization() {
        let mut pool = create_basic_test_pool();
        pool.dfc = DynamicFeeConfig::new(4500, 10_000, 1, false, 0);
        let delta = ProtocolStateDelta {
            component_id: "test-pool".to_string(),
            updated_attributes: HashMap::from([(
                "dfc_baseFee".to_string(),
                Bytes::from(500_u32.to_be_bytes()),
            )]),
            ..Default::default()
        };

        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .expect("partial dynamic fee update should be valid");

        assert_eq!(pool.dfc, DynamicFeeConfig::new(500, 10_000, 1, false, 0));
    }

    #[test]
    fn test_partial_step_updates_tick_when_price_moves_without_crossing_initialized_tick() {
        let pool = create_basic_test_pool();
        let amount =
            I256::checked_from_sign_and_abs(Sign::Positive, U256::from(100_000_000_000_000_000u64))
                .unwrap();

        let result = pool
            .swap(true, amount, None)
            .expect("swap should stay within the current liquidity range");
        let expected_tick =
            get_tick_at_sqrt_ratio(result.sqrt_price).expect("new sqrt price should map to a tick");

        assert_ne!(result.sqrt_price, pool.sqrt_price);
        assert_ne!(result.sqrt_price, get_sqrt_ratio_at_tick(-120).unwrap());
        assert_ne!(expected_tick, pool.tick);
        assert_eq!(result.tick, expected_tick);
    }

    #[test]
    fn test_swap_keeps_boundary_tick_when_price_does_not_move() {
        let mut pool = create_basic_test_pool();
        pool.tick = -1;
        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1u64)).unwrap();

        let result = pool
            .swap(true, amount, None)
            .expect("swap should consume the input as fee without moving price");

        assert_eq!(result.sqrt_price, pool.sqrt_price);
        assert_eq!(get_tick_at_sqrt_ratio(result.sqrt_price).unwrap(), 0);
        assert_eq!(result.tick, pool.tick);
    }

    #[test]
    fn test_swap_price_limit_out_of_range_returns_error() {
        let pool = create_basic_test_pool();
        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000u64)).unwrap();

        let result = pool.swap(true, amount, Some(pool.sqrt_price));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        let result = pool.swap(true, amount, Some(MIN_SQRT_RATIO));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        let result = pool.swap(false, amount, Some(pool.sqrt_price));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        let result = pool.swap(false, amount, Some(MAX_SQRT_RATIO));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn test_swap_at_extreme_price_returns_error() {
        let sqrt_price = MIN_SQRT_RATIO + U256::from(1u64);
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");
        let ticks =
            vec![TickInfo::new(MIN_TICK, 0).unwrap(), TickInfo::new(MIN_TICK + 1, 0).unwrap()];
        let pool = AerodromeSlipstreamsState::new(
            "test-pool".to_string(),
            1_000_000,
            100_000_000_000_000_000_000u128,
            sqrt_price,
            0,
            1,
            3000,
            1,
            tick,
            ticks,
            vec![Observation::default()],
            DynamicFeeConfig::new(3000, 10_000, 1, false, 0),
        )
        .expect("Failed to create pool");

        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000u64)).unwrap();
        let result = pool.swap(true, amount, None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }
}
