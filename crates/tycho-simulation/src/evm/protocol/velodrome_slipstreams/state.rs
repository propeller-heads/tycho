use std::{any::Any, collections::HashMap};

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
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::evm::protocol::{
    clmm::clmm_swap_to_price,
    safe_math::{safe_add_u256, safe_sub_u256},
    u256_num::u256_to_biguint,
    utils::uniswap::{
        liquidity_math,
        sqrt_price_math::{get_amount0_delta, get_amount1_delta, sqrt_price_q96_to_f64},
        swap_math,
        tick_list::{TickInfo, TickList, TickListErrorKind},
        tick_math::{
            get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK,
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
    ticks: TickList,
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
            ticks: tick_list,
        })
    }

    fn get_fee(&self) -> u32 {
        if self.custom_fee > 0 {
            self.custom_fee
        } else {
            self.default_fee
        }
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
                VelodromeSlipstreamsState::get_sqrt_ratio_target(
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
                    gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_TICK))?;
                }
                state.tick = if zero_for_one { step.tick_next - 1 } else { step.tick_next };
            } else if state.sqrt_price != step.sqrt_price_start {
                state.tick = get_tick_at_sqrt_ratio(state.sqrt_price)?;
            }
            gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_LOOP))?;
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
        while let Ok((tick, initialized)) = self
            .ticks
            .next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
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
                self.ticks
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
                self.ticks
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

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            // The target is a pool price, so it converts into a `sqrtPriceLimit` and one bounded
            // swap answers it — the same closed form UniswapV3 uses, which this pool's swap loop
            // is a fork of.
            SwapConstraint::PoolTargetPrice { target, .. } => {
                let (amount_in, amount_out, swap_result) = clmm_swap_to_price(
                    self.sqrt_price,
                    &params.token_in().address,
                    &params.token_out().address,
                    target,
                    self.get_fee(),
                    Sign::Positive,
                    |zero_for_one, amount_specified, sqrt_price_limit| {
                        self.swap(zero_for_one, amount_specified, Some(sqrt_price_limit))
                    },
                )?;

                let mut new_state = self.clone();
                new_state.liquidity = swap_result.liquidity;
                new_state.tick = swap_result.tick;
                new_state.sqrt_price = swap_result.sqrt_price;

                Ok(PoolSwap::new(amount_in, amount_out, Box::new(new_state), None))
            }
            // An average execution price does not reduce to a price bound, so it keeps taking the
            // generic search.
            SwapConstraint::TradeLimitPrice { .. } => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
        }
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
    use std::str::FromStr;

    use alloy::primitives::{Sign, I256, U256};
    use num_traits::ToPrimitive;
    use tycho_common::{models::Chain, simulation::errors::SimulationError};

    use super::*;
    use crate::evm::protocol::utils::uniswap::{
        tick_list::TickInfo,
        tick_math::{
            get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MIN_SQRT_RATIO,
            MIN_TICK,
        },
    };

    // Real WBTC/WETH pool state, shared with the `uniswap_v3` agreement fixtures — same tick
    // spacing (10) and fee (500 = 0.05%), since slipstreams' swap loop is a fork of v3's.
    fn create_multi_tick_test_pool() -> VelodromeSlipstreamsState {
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
        VelodromeSlipstreamsState::new(
            377_952_820_878_029_838u128,
            sqrt_price,
            500,
            0,
            10,
            255830,
            ticks,
        )
        .expect("Failed to create pool")
    }

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

    /// Converts an f64 price (token_out/token_in) into the `Price` fraction `query_pool_swap`
    /// expects, matching `crate::evm::query_pool_swap`'s own decimal-adjustment convention.
    fn to_price(
        price_f64: f64,
        token_in: &Token,
        token_out: &Token,
    ) -> tycho_common::simulation::protocol_sim::Price {
        let decimal_adj = 10_f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
        let price_no_decimals = price_f64 / decimal_adj;
        tycho_common::simulation::protocol_sim::Price::new(
            BigUint::from((price_no_decimals * 1e18) as u128),
            BigUint::from(10u128.pow(18)),
        )
    }

    /// The closed form (`PoolTargetPrice` arm of `query_pool_swap`) and the generic Brent search
    /// it replaced must agree: both solve "how much do I trade to reach this target price" for
    /// the same pool. Grid of targets below spot, both swap directions, real multi-tick pool so
    /// the swap actually crosses ticks.
    #[test]
    fn test_pool_target_price_closed_form_matches_generic_search() {
        let pool = create_multi_tick_test_pool();
        let tolerance = 0.0001; // 1 bps, a realistic caller tolerance

        for (token_in, token_out) in [(wbtc(), weth()), (weth(), wbtc())] {
            let spot = pool
                .spot_price(&token_in, &token_out)
                .expect("spot price should be computable");

            for multiplier in [0.999, 0.99, 0.95, 0.90] {
                let target_f64 = spot * multiplier;
                let target = to_price(target_f64, &token_in, &token_out);

                let params = QueryPoolSwapParams::new(
                    token_in.clone(),
                    token_out.clone(),
                    SwapConstraint::PoolTargetPrice {
                        target,
                        tolerance,
                        min_amount_in: None,
                        max_amount_in: None,
                    },
                );

                let closed_form = pool
                    .query_pool_swap(&params)
                    .expect("closed form should succeed");
                let generic = crate::evm::query_pool_swap::query_pool_swap(&pool, &params)
                    .expect("generic search should succeed");

                let closed_form_in = closed_form
                    .amount_in()
                    .to_f64()
                    .unwrap();
                let generic_in = generic.amount_in().to_f64().unwrap();
                let relative_diff = (closed_form_in - generic_in).abs() / closed_form_in.max(1.0);

                assert!(
                    relative_diff < 0.001,
                    "direction {}->{}, multiplier {multiplier}: amount_in mismatch, \
                     closed_form={closed_form_in}, generic={generic_in}, \
                     relative_diff={relative_diff}",
                    token_in.symbol,
                    token_out.symbol,
                );

                let closed_form_spot = closed_form
                    .new_state()
                    .spot_price(&token_in, &token_out)
                    .unwrap();
                let error_bps = ((closed_form_spot - target_f64) / target_f64).abs() * 10_000.0;
                assert!(
                    error_bps < 1.0,
                    "closed form should land almost exactly on target: got {error_bps}bps error"
                );
            }
        }
    }

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
