use std::{any::Any, collections::HashMap};

use alloy::primitives::{Sign, I256, U256};
use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use tracing::{error, trace};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_sub_u256},
    u256_num::u256_to_biguint,
    utils::{
        add_fee_markup,
        slipstreams::{
            dynamic_fee_module::{get_dynamic_fee, DynamicFeeConfig},
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
    block_timestamp: u64,
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
}

impl AerodromeSlipstreamsState {
    /// Creates a new instance of `AerodromeSlipstreamsState`.
    ///
    /// # Arguments
    /// - `id`: The id of the protocol component.
    /// - `block_timestamp`: The timestamp of the block.
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
        block_timestamp: u64,
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
            block_timestamp,
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
        })
    }

    fn get_fee(&self) -> Result<u32, SimulationError> {
        get_dynamic_fee(
            &self.dfc,
            self.default_fee,
            self.tick,
            self.liquidity,
            self.observation_index,
            self.observation_cardinality,
            &self.observations,
            self.block_timestamp as u32,
        )
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
        let twap_overhead = if self.dfc.scaling_factor() != 0 { TWAP_FEE_OVERHEAD } else { 0 };
        let mut gas_used = U256::from((SWAP_BASE_GAS + twap_overhead) as u64);
        let mut n_loops = 0;

        let fee = self.get_fee()?;
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
            Ok(fee) => fee as f64 / 1_000_000.0,
            Err(err) => {
                error!(
                    pool = %self.id,
                    block_timestamp = self.block_timestamp,
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
        Ok(add_fee_markup(price, self.get_fee()? as f64 / 1_000_000.0))
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
        if let Some(block_timestamp) = delta
            .updated_attributes
            .get("block_timestamp")
        {
            self.block_timestamp = BigInt::from_signed_bytes_be(block_timestamp)
                .to_u64()
                .unwrap();
        }
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
        if let Some(dfc_base_fee) = delta
            .updated_attributes
            .get("dfc_baseFee")
        {
            self.dfc
                .update_base_fee(u32::from(dfc_base_fee.clone()));
        }
        if let Some(dfc_fee_cap) = delta
            .updated_attributes
            .get("dfc_feeCap")
        {
            self.dfc
                .update_fee_cap(u32::from(dfc_fee_cap.clone()));
        }
        if let Some(dfc_scaling_factor) = delta
            .updated_attributes
            .get("dfc_scalingFactor")
        {
            self.dfc
                .update_scaling_factor(u64::from(dfc_scaling_factor.clone()));
        }
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
    use alloy::primitives::{Sign, I256, U256};
    use tycho_common::simulation::errors::SimulationError;

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
            DynamicFeeConfig::new(3000, 10_000, 1),
        )
        .expect("Failed to create pool")
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
            DynamicFeeConfig::new(3000, 10_000, 1),
        )
        .expect("Failed to create pool");

        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000u64)).unwrap();
        let result = pool.swap(true, amount, None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }
}
