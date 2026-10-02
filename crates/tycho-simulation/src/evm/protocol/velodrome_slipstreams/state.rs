use std::{any::Any, collections::HashMap, sync::Arc};

use alloy::primitives::{Sign, I256, U256};
use num_bigint::BigUint;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use tracing::trace;
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
    utils::uniswap::{
        liquidity_math,
        sqrt_price_math::{get_amount0_delta, get_amount1_delta, sqrt_price_q96_to_f64},
        swap_math,
        swap_step_cache::{CachedStep, SwapStepCache},
        tick_list::{TickInfo, TickList, TickListErrorKind},
        tick_math::{
            get_sqrt_ratio_at_tick_cached, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK,
            MIN_SQRT_RATIO, MIN_TICK,
        },
        StepComputation, SwapResults, SwapState,
    },
};

// The names of the constants reflect the exact method from the tenderly log.
const GAS_PER_TICK: u64 = 25_000;
// nextInitializedTickWithinOneWord +  computeSwapStep + calculateFees
const GAS_PER_LOOP: u64 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VelodromeSlipstreamsState {
    liquidity: u128,
    sqrt_price: U256,
    default_fee: u32,
    custom_fee: u32,
    tick_spacing: i32,
    tick: i32,
    /// Shared by clones of this state and the states its swaps return, so a quote does not copy
    /// the list. A change goes through `Arc::make_mut`, which copies the list while it is shared.
    ticks: Arc<TickList>,
    /// Swap steps already taken from this state, shared by its clones. Correct only for this
    /// state's price, tick, liquidity, fee and ticks: `delta_transition` and `after_swap` start an
    /// empty cache, and any other code that changes those fields must too.
    #[serde(skip)]
    step_cache: SwapStepCache,
}

impl VelodromeSlipstreamsState {
    /// Creates a new instance of `AerodromeSlipstreamsState`.
    ///
    /// # Arguments
    /// - `liquidity`: The initial liquidity of the pool.
    /// - `sqrt_price`: The square root of the current price.
    /// - `default_fee`: The default fee for the pool.
    /// - `custom_fee`: The custom fee for the pool.
    /// - `tick_spacing`: The tick spacing for the pool.
    /// - `tick`: The current tick of the pool.
    /// - `ticks`: A vector of `TickInfo` representing the tick information for the pool.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        liquidity: u128,
        sqrt_price: U256,
        default_fee: u32,
        custom_fee: u32,
        tick_spacing: i32,
        tick: i32,
        ticks: Vec<TickInfo>,
    ) -> Result<Self, SimulationError> {
        let tick_list = TickList::from(tick_spacing as u16, ticks)?;
        Ok(VelodromeSlipstreamsState {
            liquidity,
            sqrt_price,
            default_fee,
            custom_fee,
            tick_spacing,
            tick,
            ticks: Arc::new(tick_list),
            step_cache: SwapStepCache::default(),
        })
    }

    fn get_fee(&self) -> u32 {
        if self.custom_fee > 0 {
            self.custom_fee
        } else {
            self.default_fee
        }
    }

    /// A clone of this state at the price, tick and liquidity a swap ended at, with an empty step
    /// cache.
    fn after_swap(&self, sqrt_price: U256, tick: i32, liquidity: u128) -> Self {
        let mut state = self.clone();
        state.sqrt_price = sqrt_price;
        state.tick = tick;
        state.liquidity = liquidity;
        state.step_cache = SwapStepCache::default();
        state
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
        let mut gas_used = U256::from(130_000);

        let fee = self.get_fee();
        let mut recorder = if exact_input && sqrt_price_limit.is_none() {
            let origin = CachedStep::origin(self.sqrt_price, self.tick, self.liquidity, gas_used);
            self.step_cache
                .begin(zero_for_one, origin, amount_specified.into_raw(), fee)
        } else {
            None
        };
        if let Some(recorder) = &recorder {
            gas_used = recorder
                .start()
                .resume(&mut state, amount_specified);
        }
        while state.amount_remaining != I256::from_raw(U256::from(0u64)) &&
            state.sqrt_price != price_limit
        {
            let (mut next_tick, initialized_sqrt_price) = match self
                .ticks
                .next_initialized_tick_within_one_word(state.tick, zero_for_one)
            {
                Ok((tick, sqrt_price)) => (tick, sqrt_price),
                Err(tick_err) => match tick_err.kind {
                    TickListErrorKind::TicksExeeded => {
                        let new_state =
                            self.after_swap(state.sqrt_price, state.tick, state.liquidity);
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
            let initialized = initialized_sqrt_price.is_some();
            let sqrt_price_next = match initialized_sqrt_price {
                Some(sqrt_price) => sqrt_price,
                None => get_sqrt_ratio_at_tick_cached(next_tick)?,
            };
            let sqrt_ratio_target = VelodromeSlipstreamsState::get_sqrt_ratio_target(
                sqrt_price_next,
                price_limit,
                zero_for_one,
            );
            let (sqrt_price, amount_in, amount_out, fee_amount) = swap_math::compute_swap_step(
                state.sqrt_price,
                sqrt_ratio_target,
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
                    gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_TICK))?;
                }
                state.tick = if zero_for_one { step.tick_next - 1 } else { step.tick_next };
            } else if state.sqrt_price != step.sqrt_price_start {
                state.tick = get_tick_at_sqrt_ratio(state.sqrt_price)?;
            }
            gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_LOOP))?;
            let reached_target = state.sqrt_price == sqrt_ratio_target;
            recorder = recorder
                .and_then(|recorder| recorder.after_step(&state, &step, reached_target, gas_used));
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
impl ProtocolSim for VelodromeSlipstreamsState {
    fn fee(&self) -> f64 {
        self.get_fee() as f64 / 1_000_000.0
    }

    fn spot_price(&self, a: &Token, b: &Token) -> Result<f64, SimulationError> {
        if a < b {
            sqrt_price_q96_to_f64(self.sqrt_price, a.decimals, b.decimals)
        } else {
            sqrt_price_q96_to_f64(self.sqrt_price, b.decimals, a.decimals)
                .map(|price| 1.0f64 / price)
        }
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
        let new_state = self.after_swap(result.sqrt_price, result.tick, result.liquidity);

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
        while let Ok((tick, initialized_sqrt_price)) = self
            .ticks
            .next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
            let initialized = initialized_sqrt_price.is_some();
            // Clamp the tick value to ensure it's within valid range
            let next_tick = tick.clamp(MIN_TICK, MAX_TICK);

            let sqrt_price_next = match initialized_sqrt_price {
                Some(sqrt_price) => sqrt_price,
                None => get_sqrt_ratio_at_tick_cached(next_tick)?,
            };

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
        // Swap steps taken from the old state do not hold for the new one.
        self.step_cache = SwapStepCache::default();
        // apply attribute changes
        if let Some(liquidity) = delta
            .updated_attributes
            .get("liquidity")
        {
            self.liquidity = u128::from(liquidity.clone());
        }
        if let Some(sqrt_price) = delta
            .updated_attributes
            .get("sqrt_price_x96")
        {
            self.sqrt_price = U256::from_be_slice(sqrt_price);
        }
        if let Some(default_fee) = delta
            .updated_attributes
            .get("default_fee")
        {
            self.default_fee = u32::from(default_fee.clone());
        }
        if let Some(custom_fee) = delta
            .updated_attributes
            .get("custom_fee")
        {
            self.custom_fee = u32::from(custom_fee.clone());
        }
        if let Some(tick) = delta.updated_attributes.get("tick") {
            self.tick = i32::from(tick.clone());
        }

        // apply tick & observations changes
        for (key, value) in delta.updated_attributes.iter() {
            // tick liquidity keys are in the format "ticks/{tick_index}/net_liquidity"
            if key.starts_with("ticks/") {
                let parts: Vec<&str> = key.split('/').collect();
                Arc::make_mut(&mut self.ticks)
                    .set_tick_liquidity(
                        parts[1]
                            .parse::<i32>()
                            .map_err(|err| TransitionError::DecodeError(err.to_string()))?,
                        i128::from(value.clone()),
                    )
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
            }
        }
        // delete ticks - ignores deletes for attributes other than tick liquidity
        for key in delta.deleted_attributes.iter() {
            // tick liquidity keys are in the format "ticks/{tick_index}/net_liquidity"
            if key.starts_with("ticks/") {
                let parts: Vec<&str> = key.split('/').collect();
                Arc::make_mut(&mut self.ticks)
                    .set_tick_liquidity(
                        parts[1]
                            .parse::<i32>()
                            .map_err(|err| TransitionError::DecodeError(err.to_string()))?,
                        0,
                    )
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
            }
        }
        Ok(())
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
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
            .downcast_ref::<VelodromeSlipstreamsState>()
        {
            self.liquidity == other_state.liquidity &&
                self.sqrt_price == other_state.sqrt_price &&
                self.get_fee() == other_state.get_fee() &&
                self.tick == other_state.tick &&
                self.ticks == other_state.ticks
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Sign, I256, U256};
    use tycho_common::simulation::errors::SimulationError;

    use super::*;
    use crate::evm::protocol::utils::uniswap::{
        tick_list::TickInfo,
        tick_math::{
            get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MIN_SQRT_RATIO,
            MIN_TICK,
        },
    };

    fn create_basic_test_pool() -> VelodromeSlipstreamsState {
        let sqrt_price = get_sqrt_ratio_at_tick(0).expect("Failed to calculate sqrt price");
        let ticks = vec![TickInfo::new(-120, 0).unwrap(), TickInfo::new(120, 0).unwrap()];
        VelodromeSlipstreamsState::new(
            100_000_000_000_000_000_000u128,
            sqrt_price,
            3000,
            0,
            1,
            0,
            ticks,
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
        let pool = VelodromeSlipstreamsState::new(
            100_000_000_000_000_000_000u128,
            sqrt_price,
            3000,
            0,
            1,
            tick,
            ticks,
        )
        .expect("Failed to create pool");

        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000u64)).unwrap();
        let result = pool.swap(true, amount, None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }
}

#[cfg(test)]
mod tick_list_sharing_tests {
    use std::collections::{HashMap, HashSet};

    use tycho_common::{dto::ProtocolStateDelta, hex_bytes::Bytes};

    use super::*;
    use crate::evm::protocol::utils::uniswap::tick_math::get_sqrt_ratio_at_tick;

    #[test]
    fn test_delta_transition_leaves_clones_ticks_unchanged() {
        let original = VelodromeSlipstreamsState::new(
            100_000_000_000_000_000_000u128,
            get_sqrt_ratio_at_tick(0).unwrap(),
            3000,
            0,
            1,
            0,
            vec![TickInfo::new(-120, 10000).unwrap(), TickInfo::new(120, -10000).unwrap()],
        )
        .unwrap();
        let mut updated = original.clone();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: HashMap::from([(
                "ticks/-120/net_liquidity".to_string(),
                Bytes::from(20000_i128.to_be_bytes().to_vec()),
            )]),
            deleted_attributes: HashSet::new(),
        };

        updated
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(
            updated
                .ticks
                .get_tick(-120)
                .unwrap()
                .net_liquidity,
            20000
        );
        assert_eq!(
            original
                .ticks
                .get_tick(-120)
                .unwrap()
                .net_liquidity,
            10000
        );
    }
}

#[cfg(test)]
mod step_cache_tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use tycho_common::{dto::ProtocolStateDelta, hex_bytes::Bytes};

    use super::*;
    use crate::evm::protocol::utils::uniswap::swap_step_cache::test_fixtures::{
        amount_orders, test_amounts, test_ticks, wbtc_weth, LIQUIDITY, SQRT_PRICE, TICK,
    };

    fn test_pool() -> VelodromeSlipstreamsState {
        VelodromeSlipstreamsState::new(
            LIQUIDITY,
            U256::from_str(SQRT_PRICE).unwrap(),
            500,
            0,
            10,
            TICK,
            test_ticks(),
        )
        .unwrap()
    }

    /// A quote's amount, gas and new state, or its error, as text.
    fn outcome(
        pool: &VelodromeSlipstreamsState,
        amount: &BigUint,
        sell: &Token,
        buy: &Token,
    ) -> String {
        match pool.get_amount_out(amount.clone(), sell, buy) {
            Ok(result) => {
                let state = result
                    .new_state
                    .as_any()
                    .downcast_ref::<VelodromeSlipstreamsState>()
                    .unwrap();
                format!(
                    "{} {} {} {} {}",
                    result.amount, result.gas, state.sqrt_price, state.tick, state.liquidity
                )
            }
            Err(error) => format!("error {error}"),
        }
    }

    #[test]
    fn test_step_cache_quotes_match_a_fresh_state_in_any_order() {
        let (wbtc, weth) = wbtc_weth();
        for (sell, buy, smallest) in [(&wbtc, &weth, 1_000u64), (&weth, &wbtc, 1_000_000_000)] {
            for order in amount_orders(&test_amounts(smallest)) {
                let shared = test_pool();
                for amount in &order {
                    assert_eq!(
                        outcome(&shared, amount, sell, buy),
                        outcome(&test_pool(), amount, sell, buy),
                        "selling {amount} {}",
                        sell.symbol
                    );
                }
                let steps = shared
                    .step_cache
                    .min_inputs(sell < buy)
                    .len();
                assert!(steps > 2, "only {steps} steps cached");
            }
        }
    }

    #[test]
    fn test_step_cache_quotes_match_a_fresh_state_at_each_cached_step() {
        let (wbtc, weth) = wbtc_weth();
        // Amounts that run past the last tick, so the cache covers the whole tick list. Selling
        // WETH needs a far larger amount to leave its first tick.
        for (sell, buy, doublings) in [(&wbtc, &weth, 30u32), (&weth, &wbtc, 70)] {
            let cached = test_pool();
            let _ = cached.get_amount_out(BigUint::from(10u32) << doublings, sell, buy);
            let boundaries = cached.step_cache.min_inputs(sell < buy);
            assert!(boundaries.len() > 2, "only {} steps cached", boundaries.len());
            for boundary in boundaries.into_iter().skip(1) {
                for amount in [boundary - U256::from(1u64), boundary, boundary + U256::from(1u64)] {
                    let amount = u256_to_biguint(amount);
                    assert_eq!(
                        outcome(&cached, &amount, sell, buy),
                        outcome(&test_pool(), &amount, sell, buy),
                        "selling {amount} {}",
                        sell.symbol
                    );
                }
            }
        }
    }

    #[test]
    fn test_step_cache_of_a_swapped_state() {
        let (wbtc, weth) = wbtc_weth();
        let pool = test_pool();
        let swapped = pool
            .get_amount_out(BigUint::from(300_000_000u64), &wbtc, &weth)
            .unwrap()
            .new_state;
        let swapped = swapped
            .as_any()
            .downcast_ref::<VelodromeSlipstreamsState>()
            .unwrap();
        // A serde round trip gives the same state with an empty step cache.
        let fresh: VelodromeSlipstreamsState =
            serde_json::from_value(serde_json::to_value(swapped).unwrap()).unwrap();

        for amount in test_amounts(1_000).iter().rev() {
            assert_eq!(
                outcome(swapped, amount, &wbtc, &weth),
                outcome(&fresh, amount, &wbtc, &weth),
                "selling {amount} WBTC"
            );
        }
    }

    #[test]
    fn test_step_cache_restarts_after_delta_transition() {
        let (wbtc, weth) = wbtc_weth();
        let large = BigUint::from(3_000_000_000u64);
        // Only a crossed tick's liquidity changes, so the price, tick and liquidity the cache
        // starts from stay the same.
        let delta = || ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: HashMap::from([(
                "ticks/255820/net_liquidity".to_string(),
                Bytes::from(
                    1_000_000_000_000_000i128
                        .to_be_bytes()
                        .to_vec(),
                ),
            )]),
            deleted_attributes: HashSet::new(),
        };
        let mut cached = test_pool();
        cached
            .get_amount_out(large.clone(), &wbtc, &weth)
            .unwrap();
        let mut fresh = test_pool();

        cached
            .delta_transition(delta(), &HashMap::new(), &Balances::default())
            .unwrap();
        fresh
            .delta_transition(delta(), &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(outcome(&cached, &large, &wbtc, &weth), outcome(&fresh, &large, &wbtc, &weth));
    }
}
