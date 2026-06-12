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

use super::enums::FeeAmount;
use crate::evm::protocol::{
    clmm::clmm_swap_to_price,
    safe_math::{safe_add_u256, safe_sub_u256},
    u256_num::u256_to_biguint,
    utils::{
        add_fee_markup,
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

// Pre/post loop overhead: cold SLOADs (slot0, liquidity, feeGrowthGlobal) +
// cold SSTOREs (slot0 write, liquidity write) at end of swap.
const SWAP_BASE_GAS: u64 = 70_000;
// Bitmap word scan (cold SLOAD of tickBitmap word)
const GAS_PER_BITMAP_WORD: u64 = 2_100;
// swap math step: getSqrtRatioAtTick + computeSwapStep + amount accounting + getTickAtSqrtRatio
const GAS_PER_SWAP_MATH_STEP: u64 = 5_400;
// Initialized tick crossing: cross() updates feeGrowthOutside0/1 (2 SSTOREs).
// Warm ≈ 10-17k, cold ≈ 40-52k. 24k biases toward cold for overestimation.
const GAS_PER_INITIALIZED_TICK_CROSS: u64 = 24_000;
// Output transfer + balanceBefore + callback + balanceAfter.
const V3_CALLBACK_SETTLEMENT_GAS: u64 = 70_000;
// Conservative max gas budget for a single swap (Ethereum transaction gas limit)
const MAX_SWAP_GAS: u64 = 16_700_000;
const MAX_TICKS_CROSSED: u64 = (MAX_SWAP_GAS - SWAP_BASE_GAS) / GAS_PER_INITIALIZED_TICK_CROSS;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniswapV3State {
    liquidity: u128,
    sqrt_price: U256,
    fee: FeeAmount,
    tick: i32,
    ticks: TickList,
}

impl UniswapV3State {
    /// Creates a new instance of `UniswapV3State`.
    ///
    /// # Arguments
    /// - `liquidity`: The initial liquidity of the pool.
    /// - `sqrt_price`: The square root of the current price.
    /// - `fee`: The fee tier for the pool.
    /// - `tick`: The current tick of the pool.
    /// - `ticks`: A vector of `TickInfo` representing the tick information for the pool.
    pub fn new(
        liquidity: u128,
        sqrt_price: U256,
        fee: FeeAmount,
        tick: i32,
        ticks: Vec<TickInfo>,
    ) -> Result<Self, SimulationError> {
        let spacing = UniswapV3State::get_spacing(fee);
        let tick_list = TickList::from(spacing, ticks)?;
        Ok(UniswapV3State { liquidity, sqrt_price, fee, tick, ticks: tick_list })
    }

    fn get_spacing(fee: FeeAmount) -> u16 {
        match fee {
            FeeAmount::Lowest => 1,
            FeeAmount::Lowest2 => 2,
            FeeAmount::Lowest3 => 3,
            FeeAmount::Lowest4 => 4,
            FeeAmount::Low => 10,
            FeeAmount::MediumLow => 50,
            FeeAmount::Medium => 60,
            FeeAmount::MediumHigh => 100,
            FeeAmount::High => 200,
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
        let mut gas_used = U256::from(SWAP_BASE_GAS);

        while state.amount_remaining != I256::from_raw(U256::from(0u64)) &&
            state.sqrt_price != price_limit
        {
            let (mut next_tick, initialized) = match self
                .ticks
                .next_initialized_tick_within_one_word(state.tick, zero_for_one)
            {
                Ok((tick, init)) => {
                    gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_BITMAP_WORD))?;
                    (tick, init)
                }
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
                UniswapV3State::get_sqrt_ratio_target(sqrt_price_next, price_limit, zero_for_one),
                state.liquidity,
                state.amount_remaining,
                self.fee as u32,
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

            gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_SWAP_MATH_STEP))?;

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
                    gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_INITIALIZED_TICK_CROSS))?;
                }
                state.tick = if zero_for_one { step.tick_next - 1 } else { step.tick_next };
            } else if state.sqrt_price != step.sqrt_price_start {
                state.tick = get_tick_at_sqrt_ratio(state.sqrt_price)?;
            }
        }
        Ok(SwapResults {
            amount_calculated: state.amount_calculated,
            amount_specified,
            amount_remaining: state.amount_remaining,
            sqrt_price: state.sqrt_price,
            liquidity: state.liquidity,
            tick: state.tick,
            gas_used: safe_add_u256(gas_used, U256::from(V3_CALLBACK_SETTLEMENT_GAS))?,
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
impl ProtocolSim for UniswapV3State {
    fn fee(&self) -> f64 {
        (self.fee as u32) as f64 / 1_000_000.0
    }

    fn spot_price(&self, a: &Token, b: &Token) -> Result<f64, SimulationError> {
        let price = if a < b {
            sqrt_price_q96_to_f64(self.sqrt_price, a.decimals, b.decimals)?
        } else {
            1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, b.decimals, a.decimals)?
        };
        Ok(add_fee_markup(price, self.fee()))
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

        trace!(?amount_in, ?token_a, ?token_b, ?zero_for_one, ?result, "V3 SWAP");
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
        let mut ticks_crossed: u64 = 0;

        // Iterate through ticks in the direction of the swap
        // Stops when: no more liquidity, no more ticks, or gas limit would be exceeded
        while let Ok((tick, initialized)) = self
            .ticks
            .next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
            // Cap iteration to prevent exceeding Ethereum's gas limit
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

                // Check if applying this liquidity delta would cause underflow
                // If so, stop here rather than continuing with invalid state
                match liquidity_math::add_liquidity_delta(current_liquidity, liquidity_delta) {
                    Ok(new_liquidity) => {
                        current_liquidity = new_liquidity;
                    }
                    Err(_) => {
                        // Liquidity would underflow, stop iteration here
                        // This represents the maximum liquidity we can actually use
                        break;
                    }
                }
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

        // apply tick changes
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

    /// See [`ProtocolSim::query_pool_swap`] for the trait documentation.
    ///
    /// This method uses Uniswap V3 internal swap logic by swapping an infinite amount of token_in
    /// until the target price is reached.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        if self.liquidity == 0 {
            return Err(SimulationError::FatalError("No liquidity".to_string()));
        }

        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "Uniswap V3 does not support TradeLimitPrice constraint in query_pool_swap"
                    .to_string(),
                None,
            )),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: _,
                min_amount_in: _,
                max_amount_in: _,
            } => {
                let (amount_in, amount_out, swap_result) = clmm_swap_to_price(
                    self.sqrt_price,
                    &params.token_in().address,
                    &params.token_out().address,
                    target,
                    self.fee as u32,
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
            .downcast_ref::<UniswapV3State>()
        {
            self.liquidity == other_state.liquidity &&
                self.sqrt_price == other_state.sqrt_price &&
                self.fee == other_state.fee &&
                self.tick == other_state.tick &&
                self.ticks == other_state.ticks
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        fs,
        path::Path,
        str::FromStr,
    };

    use num_bigint::ToBigUint;
    use num_traits::FromPrimitive;
    use serde_json::Value;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{hex_bytes::Bytes, models::Chain, simulation::protocol_sim::Price};

    use super::*;
    use crate::{
        evm::protocol::utils::uniswap::sqrt_price_math::get_sqrt_price_q96,
        protocol::models::{DecoderContext, TryFromWithBlock},
    };

    #[test]
    fn test_get_amount_out_full_range_liquidity() {
        let token_x = Token::new(
            &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            "X",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let token_y = Token::new(
            &Bytes::from_str("0xf1ca9cb74685755965c7458528a36934df52a3ef").unwrap(),
            "Y",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let pool = UniswapV3State::new(
            8330443394424070888454257,
            U256::from_str("188562464004052255423565206602").unwrap(),
            FeeAmount::Medium,
            17342,
            vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()],
        )
        .unwrap();
        let sell_amount = BigUint::from_str("11_000_000000000000000000").unwrap();
        let expected = BigUint::from_str("61927070842678722935941").unwrap();

        let res = pool
            .get_amount_out(sell_amount, &token_x, &token_y)
            .unwrap();

        assert_eq!(res.amount, expected);
    }

    struct SwapTestCase {
        symbol: &'static str,
        sell: BigUint,
        exp: BigUint,
    }

    #[test]
    fn test_get_amount_out() {
        let wbtc = Token::new(
            &Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            "WBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let pool = UniswapV3State::new(
            377952820878029838,
            U256::from_str("28437325270877025820973479874632004").unwrap(),
            FeeAmount::Low,
            255830,
            vec![
                TickInfo::new(255760, 1759015528199933i128).unwrap(),
                TickInfo::new(255770, 6393138051835308i128).unwrap(),
                TickInfo::new(255780, 228206673808681i128).unwrap(),
                TickInfo::new(255820, 1319490609195820i128).unwrap(),
                TickInfo::new(255830, 678916926147901i128).unwrap(),
                TickInfo::new(255840, 12208947683433103i128).unwrap(),
                TickInfo::new(255850, 1177970713095301i128).unwrap(),
                TickInfo::new(255860, 8752304680520407i128).unwrap(),
                TickInfo::new(255880, 1486478248067104i128).unwrap(),
                TickInfo::new(255890, 1878744276123248i128).unwrap(),
                TickInfo::new(255900, 77340284046725227i128).unwrap(),
            ],
        )
        .unwrap();
        let cases = vec![
            SwapTestCase {
                symbol: "WBTC",
                sell: 500000000.to_biguint().unwrap(),
                exp: BigUint::from_str("64352395915550406461").unwrap(),
            },
            SwapTestCase {
                symbol: "WBTC",
                sell: 550000000.to_biguint().unwrap(),
                exp: BigUint::from_str("70784271504035662865").unwrap(),
            },
            SwapTestCase {
                symbol: "WBTC",
                sell: 600000000.to_biguint().unwrap(),
                exp: BigUint::from_str("77215534856185613494").unwrap(),
            },
            SwapTestCase {
                symbol: "WBTC",
                sell: BigUint::from_str("1000000000").unwrap(),
                exp: BigUint::from_str("128643569649663616249").unwrap(),
            },
            SwapTestCase {
                symbol: "WBTC",
                sell: BigUint::from_str("3000000000").unwrap(),
                exp: BigUint::from_str("385196519076234662939").unwrap(),
            },
            SwapTestCase {
                symbol: "WETH",
                sell: BigUint::from_str("64000000000000000000").unwrap(),
                exp: BigUint::from_str("496294784").unwrap(),
            },
            SwapTestCase {
                symbol: "WETH",
                sell: BigUint::from_str("70000000000000000000").unwrap(),
                exp: BigUint::from_str("542798479").unwrap(),
            },
            SwapTestCase {
                symbol: "WETH",
                sell: BigUint::from_str("77000000000000000000").unwrap(),
                exp: BigUint::from_str("597047757").unwrap(),
            },
            SwapTestCase {
                symbol: "WETH",
                sell: BigUint::from_str("128000000000000000000").unwrap(),
                exp: BigUint::from_str("992129037").unwrap(),
            },
            SwapTestCase {
                symbol: "WETH",
                sell: BigUint::from_str("385000000000000000000").unwrap(),
                exp: BigUint::from_str("2978713582").unwrap(),
            },
        ];

        for case in cases {
            let (token_a, token_b) =
                if case.symbol == "WBTC" { (&wbtc, &weth) } else { (&weth, &wbtc) };
            let res = pool
                .get_amount_out(case.sell, token_a, token_b)
                .unwrap();

            assert_eq!(res.amount, case.exp);
        }
    }

    #[test]
    fn test_err_with_partial_trade() {
        let dai = Token::new(
            &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            "DAI",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let pool = UniswapV3State::new(
            73015811375239994,
            U256::from_str("148273042406850898575413").unwrap(),
            FeeAmount::High,
            -263789,
            vec![
                TickInfo::new(-269600, 3612326326695492i128).unwrap(),
                TickInfo::new(-268800, 1487613939516867i128).unwrap(),
                TickInfo::new(-267800, 1557587121322546i128).unwrap(),
                TickInfo::new(-267400, 424592076717375i128).unwrap(),
                TickInfo::new(-267200, 11691597431643916i128).unwrap(),
                TickInfo::new(-266800, -218742815100986i128).unwrap(),
                TickInfo::new(-266600, 1118947532495477i128).unwrap(),
                TickInfo::new(-266200, 1233064286622365i128).unwrap(),
                TickInfo::new(-265000, 4252603063356107i128).unwrap(),
                TickInfo::new(-263200, -351282010325232i128).unwrap(),
                TickInfo::new(-262800, -2352011819117842i128).unwrap(),
                TickInfo::new(-262600, -424592076717375i128).unwrap(),
                TickInfo::new(-262200, -11923662433672566i128).unwrap(),
                TickInfo::new(-261600, -2432911749667741i128).unwrap(),
                TickInfo::new(-260200, -4032727022572273i128).unwrap(),
                TickInfo::new(-260000, -22889492064625028i128).unwrap(),
                TickInfo::new(-259400, -1557587121322546i128).unwrap(),
                TickInfo::new(-259200, -1487613939516867i128).unwrap(),
                TickInfo::new(-258400, -400137022888262i128).unwrap(),
            ],
        )
        .unwrap();
        let amount_in = BigUint::from_str("50000000000").unwrap();
        let exp = BigUint::from_str("6820591625999718100883").unwrap();

        let err = pool
            .get_amount_out(amount_in, &usdc, &dai)
            .unwrap_err();

        match err {
            SimulationError::InvalidInput(ref _err, ref amount_out_result) => {
                match amount_out_result {
                    Some(amount_out_result) => {
                        assert_eq!(amount_out_result.amount, exp);
                        let new_state = amount_out_result
                            .new_state
                            .as_any()
                            .downcast_ref::<UniswapV3State>()
                            .unwrap();
                        assert_ne!(new_state.tick, pool.tick);
                        assert_ne!(new_state.liquidity, pool.liquidity);
                    }
                    _ => panic!("Partial amount out result is None. Expected partial result."),
                }
            }
            _ => panic!("Test failed: was expecting a SimulationError::InsufficientData"),
        }
    }

    #[test]
    fn test_delta_transition() {
        let mut pool = UniswapV3State::new(
            1000,
            U256::from_str("1000").unwrap(),
            FeeAmount::Low,
            100,
            vec![TickInfo::new(255760, 10000).unwrap(), TickInfo::new(255900, -10000).unwrap()],
        )
        .unwrap();
        let attributes: HashMap<String, Bytes> = [
            ("liquidity".to_string(), Bytes::from(2000_u64.to_be_bytes().to_vec())),
            ("sqrt_price_x96".to_string(), Bytes::from(1001_u64.to_be_bytes().to_vec())),
            ("tick".to_string(), Bytes::from(120_i32.to_be_bytes().to_vec())),
            (
                "ticks/-255760/net_liquidity".to_string(),
                Bytes::from(10200_u64.to_be_bytes().to_vec()),
            ),
            (
                "ticks/255900/net_liquidity".to_string(),
                Bytes::from(9800_u64.to_be_bytes().to_vec()),
            ),
        ]
        .into_iter()
        .collect();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(pool.liquidity, 2000);
        assert_eq!(pool.sqrt_price, U256::from(1001));
        assert_eq!(pool.tick, 120);
        assert_eq!(
            pool.ticks
                .get_tick(-255760)
                .unwrap()
                .net_liquidity,
            10200
        );
        assert_eq!(
            pool.ticks
                .get_tick(255900)
                .unwrap()
                .net_liquidity,
            9800
        );
    }

    #[tokio::test]
    async fn test_get_limits() {
        use tycho_client::feed::dto;
        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path =
            Path::new(project_root).join("tests/assets/decoder/uniswap_v3_snapshot.json");
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        let data: Value = serde_json::from_str(&json_data).expect("Failed to parse JSON");
        let state: ComponentWithState = serde_json::from_value::<dto::ComponentWithState>(data)
            .expect("Expected json to match ComponentWithState structure")
            .into();

        let usv3_state = UniswapV3State::try_from_with_header(
            state,
            Default::default(),
            &Default::default(),
            &Default::default(),
            &DecoderContext::new(),
        )
        .await
        .unwrap();

        let t0 = Token::new(
            &Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap(),
            "WBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let t1 = Token::new(
            &Bytes::from_str("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf").unwrap(),
            "cbBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let res = usv3_state
            .get_limits(t0.address.clone(), t1.address.clone())
            .unwrap();

        assert_eq!(&res.0, &BigUint::from_u128(29160572556).unwrap());

        let out = usv3_state
            .get_amount_out(res.0, &t0, &t1)
            .expect("swap for limit in didn't work");

        // Allow 1-unit rounding difference: get_limits uses ceiling/floor delta math
        // while get_amount_out uses the full swap path.
        let diff = if res.1 > out.amount {
            res.1.clone() - out.amount.clone()
        } else {
            out.amount.clone() - res.1.clone()
        };
        assert!(diff <= BigUint::from(1u64), "limit_out and amount_out differ by {diff}");
    }

    // Helper to create a basic test pool
    fn create_basic_test_pool() -> UniswapV3State {
        let liquidity = 100_000_000_000_000_000_000u128; // 100e18
        let sqrt_price = get_sqrt_price_q96(U256::from(20_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Low, tick, ticks)
            .expect("Failed to create pool")
    }

    fn create_tick_boundary_test_pool() -> UniswapV3State {
        let sqrt_price = get_sqrt_ratio_at_tick(0).expect("Failed to calculate sqrt price");
        let ticks = vec![TickInfo::new(-120, 0).unwrap(), TickInfo::new(120, 0).unwrap()];

        UniswapV3State::new(100_000_000_000_000_000_000u128, sqrt_price, FeeAmount::Low, 0, ticks)
            .expect("Failed to create pool")
    }

    #[test]
    fn test_partial_step_updates_tick_when_price_moves_without_crossing_initialized_tick() {
        let pool = create_tick_boundary_test_pool();
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
        let mut pool = create_tick_boundary_test_pool();
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
    fn test_swap_basic() {
        let pool = create_basic_test_pool();

        // Test selling token X for token Y
        let amount_in =
            I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000000u64)).unwrap();
        let result = pool
            .swap(true, amount_in, None)
            .unwrap();

        // At current pool price, we should get a little less than 2 times the amount of X
        let expected_amount = U256::from(2000000u64);
        let actual_amount = result
            .amount_calculated
            .abs()
            .into_raw();
        assert_eq!(expected_amount - actual_amount, U256::from(1001u64));
        println!("Swap X->Y: amount_in={}, amount_out={}", amount_in, actual_amount);
    }

    #[test]
    fn test_swap_to_price_basic() {
        // Create pool with Medium fee (0.3%) to match V4's basic test
        let liquidity = 100_000_000_000_000_000_000u128;
        let sqrt_price = get_sqrt_price_q96(U256::from(20_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");
        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        let pool = UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Medium, tick, ticks)
            .expect("Failed to create pool");

        // Token X has lower address (0x01), Y has higher address (0x02)
        let token_x = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "X",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let token_y = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
            "Y",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        // Swap price: buying X for Y (token_out/token_in)
        let target_price =
            Price::new(2_000_000u64.to_biguint().unwrap(), 1_010_000u64.to_biguint().unwrap());

        // Query how much Y the pool can supply when buying X at this price
        let trade = pool
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_x,
                token_y,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .expect("swap_to_price failed");

        // Should match V4's output exactly with same fees (0.3%)
        let expected_amount_in =
            BigUint::from_str("246739021727519745").expect("Failed to parse expected amount_in");
        let expected_amount_out =
            BigUint::from_str("490291909043340795").expect("Failed to parse expected amount_out");

        assert_eq!(
            trade.amount_in().clone(),
            expected_amount_in,
            "amount_in should match expected value"
        );
        assert_eq!(
            trade.amount_out().clone(),
            expected_amount_out,
            "amount_out should match expected value"
        );
    }

    #[test]
    fn test_swap_to_price_price_too_high() {
        let pool = create_basic_test_pool();

        let token_x = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "X",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let token_y = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
            "Y",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        // Price far above pool price - should return zero
        let target_price =
            Price::new(10_000_000u64.to_biguint().unwrap(), 1_000_000u64.to_biguint().unwrap());

        let result = pool.query_pool_swap(&QueryPoolSwapParams::new(
            token_x,
            token_y,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));
        assert!(result.is_err(), "Should return error when target price is unreachable");
    }

    #[test]
    fn test_swap_parameterized() {
        // Parameterized swap tests with real WBTC/WETH pool data
        let liquidity = 377_952_820_878_029_838u128;
        let sqrt_price = U256::from_str("28437325270877025820973479874632004")
            .expect("Failed to parse sqrt_price");
        let tick = 255830;

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

        let pool = UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Low, tick, ticks)
            .expect("Failed to create pool");

        // Test cases: (zero_for_one, amount_in, expected_amount_out, test_id)
        // WBTC address (0x2260...) < WETH address (0xC02a...), so WBTC is token0
        let test_cases = vec![
            // WBTC to WETH cases (zero_for_one = true)
            (true, "500000000", "64352395915550406461", "WBTC->WETH 500000000"),
            (true, "550000000", "70784271504035662865", "WBTC->WETH 550000000"),
            (true, "600000000", "77215534856185613494", "WBTC->WETH 600000000"),
            (true, "1000000000", "128643569649663616249", "WBTC->WETH 1000000000"),
            (true, "3000000000", "385196519076234662939", "WBTC->WETH 3000000000"),
            // WETH to WBTC cases (zero_for_one = false)
            (false, "64000000000000000000", "496294784", "WETH->WBTC 64 ETH"),
            (false, "70000000000000000000", "542798479", "WETH->WBTC 70 ETH"),
            (false, "77000000000000000000", "597047757", "WETH->WBTC 77 ETH"),
            (false, "128000000000000000000", "992129037", "WETH->WBTC 128 ETH"),
            (false, "385000000000000000000", "2978713582", "WETH->WBTC 385 ETH"),
        ];

        for (zero_for_one, amount_in_str, expected_amount_out_str, test_id) in test_cases {
            let amount_in = U256::from_str(amount_in_str).expect("Failed to parse amount_in");
            let amount_specified = I256::checked_from_sign_and_abs(Sign::Positive, amount_in)
                .unwrap_or_else(|| panic!("{} - Failed to convert amount to I256", test_id));

            let result = pool
                .swap(zero_for_one, amount_specified, None)
                .unwrap_or_else(|e| panic!("{} - swap failed: {:?}", test_id, e));

            let amount_out = result
                .amount_calculated
                .abs()
                .into_raw();
            let expected = U256::from_str(expected_amount_out_str)
                .expect("Failed to parse expected_amount_out");

            assert_eq!(amount_out, expected, "{}", test_id);
        }
    }

    #[test]
    fn test_swap_to_price_parameterized() {
        // Tests query_supply with various price points
        let wbtc = Token::new(
            &Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            "WBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let liquidity = 377_952_820_878_029_838u128;
        let sqrt_price = get_sqrt_price_q96(U256::from(130_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![
            TickInfo::new(25560, 1759015528199933).unwrap(),
            TickInfo::new(25570, 6393138051835308).unwrap(),
            TickInfo::new(25580, 228206673808681).unwrap(),
            TickInfo::new(25620, 1319490609195820).unwrap(),
            TickInfo::new(25630, 678916926147901).unwrap(),
            TickInfo::new(25640, 12208947683433103).unwrap(),
            TickInfo::new(25660, 8752304680520407).unwrap(),
            TickInfo::new(25680, 1486478248067104).unwrap(),
            TickInfo::new(25690, 1878744276123248).unwrap(),
            TickInfo::new(25700, 77340284046725227).unwrap(),
        ];

        let pool = UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Low, tick, ticks)
            .expect("Failed to create pool");

        // Test cases: (sell_token, sell_price, buy_price, expected_supply, test_id)
        let test_cases = vec![
            (&wbtc, 129u64, 10u64, "0", "WBTC sell_price=129, buy_price=10"),
            (&wbtc, 130u64, 10u64, "0", "WBTC sell_price=130, buy_price=10"),
            (&wbtc, 1305u64, 100u64, "163535995630461", "WBTC sell_price=1305, buy_price=100"),
            (&weth, 99u64, 1300u64, "0", "WETH sell_price=99, buy_price=1300"),
            (&weth, 100u64, 1300u64, "0", "WETH sell_price=100, buy_price=1300"),
            (&weth, 101u64, 1299u64, "524227092059180", "WETH sell_price=101, buy_price=1299"),
        ];

        for (sell_token, sell_price, buy_price, expected_str, test_id) in test_cases {
            let buy_token = if sell_token == &wbtc { &weth } else { &wbtc };

            let target_price =
                Price::new(buy_price.to_biguint().unwrap(), sell_price.to_biguint().unwrap());

            if expected_str == "0" {
                let result = pool.query_pool_swap(&QueryPoolSwapParams::new(
                    buy_token.clone(),
                    sell_token.clone(),
                    SwapConstraint::PoolTargetPrice {
                        target: target_price,
                        tolerance: 0f64,
                        min_amount_in: None,
                        max_amount_in: None,
                    },
                ));
                assert!(result.is_err(), "Should return error when target price is unreachable");
            } else {
                let expected =
                    BigUint::from_str(expected_str).expect("Failed to parse expected value");

                let trade = pool
                    .query_pool_swap(&QueryPoolSwapParams::new(
                        buy_token.clone(),
                        sell_token.clone(),
                        SwapConstraint::PoolTargetPrice {
                            target: target_price,
                            tolerance: 0f64,
                            min_amount_in: None,
                            max_amount_in: None,
                        },
                    ))
                    .unwrap_or_else(|e| panic!("{} - query_supply failed: {:?}", test_id, e));
                assert_eq!(trade.amount_out().clone(), expected, "{}", test_id);
            }
        }
    }

    #[test]
    fn test_swap_to_price_around_spot_price() {
        // Tests query_supply edge cases around the spot price with fees
        let liquidity = 10_000_000_000_000_000u128;
        let sqrt_price =
            get_sqrt_price_q96(U256::from(2_000_000_000u64), U256::from(1_000_000_000u64))
                .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        let pool = UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Low, tick, ticks)
            .expect("Failed to create pool");

        let token_x = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "X",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let token_y = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
            "Y",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        // Test 1: Price just above spot price, too little to cover fees
        let target_price =
            Price::new(1_999_750u64.to_biguint().unwrap(), 1_000_250u64.to_biguint().unwrap());

        let result = pool.query_pool_swap(&QueryPoolSwapParams::new(
            token_x.clone(),
            token_y.clone(),
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));
        assert!(result.is_err(), "Should return error when target price is unreachable");

        // Test 2: Price high enough to cover fees (0.1% higher)
        let target_price =
            Price::new(1_999_000u64.to_biguint().unwrap(), 1_001_000u64.to_biguint().unwrap());

        let pool_swap = pool
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_x,
                token_y,
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .expect("swap_to_price failed");

        let expected_amount_out =
            BigUint::from_str("7062236922008").expect("Failed to parse expected value");
        assert_eq!(
            pool_swap.amount_out().clone(),
            expected_amount_out,
            "Expected amount out when price covers fees"
        );
    }

    #[test]
    fn test_swap_to_price_matches_get_amount_out() {
        let liquidity = 100_000_000_000_000_000_000u128;
        let sqrt_price = get_sqrt_price_q96(U256::from(20_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        let pool = UniswapV3State::new(liquidity, sqrt_price, FeeAmount::Medium, tick, ticks)
            .expect("Failed to create pool");

        let token_x_addr = Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap();
        let token_y_addr = Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap();

        let token_x = Token::new(&token_x_addr, "X", 18, 0, &[], Chain::Ethereum, 1);
        let token_y = Token::new(&token_y_addr, "Y", 18, 0, &[], Chain::Ethereum, 1);

        // Get the trade from swap_to_price
        let target_price = Price::new(BigUint::from(2_000_000u64), BigUint::from(1_010_000u64));
        let pool_swap = pool
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_x.clone(),
                token_y.clone(),
                SwapConstraint::PoolTargetPrice {
                    target: target_price,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .expect("swap_to_price failed");
        assert!(pool_swap.amount_in().clone() > BigUint::ZERO, "Amount in should be positive");

        // Use the amount_in from swap_to_price with get_amount_out
        let result = pool
            .get_amount_out(pool_swap.amount_in().clone(), &token_x, &token_y)
            .expect("get_amount_out failed");

        // The amount_out from get_amount_out should be close to swap_to_price's amount_out
        // Allow for small rounding differences
        assert!(result.amount > BigUint::ZERO);
        assert!(result.amount >= *pool_swap.amount_out());
    }

    #[test]
    fn test_swap_price_limit_out_of_range_returns_error() {
        let pool = create_basic_test_pool();
        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000u64)).unwrap();

        // zero_for_one: price_limit equal to sqrt_price is invalid (must be strictly less)
        let result = pool.swap(true, amount, Some(pool.sqrt_price));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        // zero_for_one: price_limit at MIN_SQRT_RATIO is invalid (must be strictly greater)
        let result = pool.swap(true, amount, Some(MIN_SQRT_RATIO));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        // one_for_zero: price_limit equal to sqrt_price is invalid (must be strictly greater)
        let result = pool.swap(false, amount, Some(pool.sqrt_price));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        // one_for_zero: price_limit at MAX_SQRT_RATIO is invalid (must be strictly less)
        let result = pool.swap(false, amount, Some(MAX_SQRT_RATIO));
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn test_swap_at_extreme_price_returns_error() {
        // Simulates the depth calculation scenario: pool sqrt_price is at MIN_SQRT_RATIO + 1,
        // so the default price limit for zero_for_one equals sqrt_price and fails validation.
        let sqrt_price = MIN_SQRT_RATIO + U256::from(1u64);
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");
        // FeeAmount::Low has tick spacing 10; ticks must be aligned
        let aligned_tick = (MIN_TICK / 10) * 10 + 10; // first multiple of 10 above MIN_TICK
        let ticks = vec![
            TickInfo::new(aligned_tick, 0).unwrap(),
            TickInfo::new(aligned_tick + 10, 0).unwrap(),
        ];
        let pool = UniswapV3State::new(
            100_000_000_000_000_000_000u128,
            sqrt_price,
            FeeAmount::Low,
            tick,
            ticks,
        )
        .unwrap();

        let amount = I256::checked_from_sign_and_abs(Sign::Positive, U256::from(1000u64)).unwrap();
        // Default price limit for zero_for_one is MIN_SQRT_RATIO + 1 == sqrt_price, so invalid
        let result = pool.swap(true, amount, None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }
}

#[cfg(test)]
mod tests_forks {
    use std::str::FromStr;

    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{hex_bytes::Bytes, models::Chain};

    use super::*;
    use crate::protocol::models::{DecoderContext, TryFromWithBlock};

    #[tokio::test]
    async fn test_pancakeswap_get_amount_out() {
        use std::{fs, path::Path};

        use serde_json::Value;
        use tycho_client::feed::dto;
        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path =
            Path::new(project_root).join("tests/assets/decoder/pancakeswap_v3_snapshot.json");
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        let data: Value = serde_json::from_str(&json_data).expect("Failed to parse JSON");
        let state: ComponentWithState = serde_json::from_value::<dto::ComponentWithState>(data)
            .expect("Expected json to match ComponentWithState structure")
            .into();

        let pool_state = UniswapV3State::try_from_with_header(
            state,
            Default::default(),
            &Default::default(),
            &Default::default(),
            &DecoderContext::new(),
        )
        .await
        .unwrap();

        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let usdt = Token::new(
            &Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(),
            "USDT",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let res = pool_state
            .get_amount_out(BigUint::from_str("5976361609").unwrap(), &usdt, &usdc)
            .unwrap();

        assert_eq!(res.amount, BigUint::from_str("5975901673").unwrap());
    }

    #[test]
    fn test_get_limits_graceful_underflow() {
        // Verifies graceful handling of liquidity underflow in get_limits for V3
        let pool = UniswapV3State::new(
            1000000,
            U256::from_str("79228162514264337593543950336").unwrap(),
            FeeAmount::Medium,
            0,
            vec![
                // A tick with net_liquidity > current_liquidity
                // When zero_for_one=true, this gets negated and would cause underflow
                TickInfo::new(-60, 2000000).unwrap(), // 2x current liquidity
            ],
        )
        .unwrap();

        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let (limit_in, limit_out) = pool
            .get_limits(usdc.address.clone(), weth.address.clone())
            .unwrap();

        // Should return some conservative limits
        assert!(limit_in > BigUint::zero());
        assert!(limit_out > BigUint::zero());
    }
}
