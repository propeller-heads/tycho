use std::{any::Any, collections::HashMap};

use alloy::primitives::{Sign, I256, U256};
use num_bigint::BigUint;
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
    utils::{
        add_fee_markup,
        uniswap::{
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
    },
};

// Gas model for a Ramses V3 swap. Constants start from the Uniswap V3 estimator and are nudged for
// Ramses' extra gauge/period accounting (the second `grossFeeGrowthGlobal` accumulator, the
// `advancePeriod` bookkeeping, and the richer `Tick.cross`). Calibrated against two on-chain
// Polygon swaps on the USDC/USD.e pool:
// - 0 tick crossings: ~141.5k gas total (~74k base, ~56k settlement, ~5k step)
//   0x8fddae1260f40d7d22c0dc4b995ab1e84423adc523e71bbda503f735610db995
// - 1 tick crossing:  ~184.1k gas total
//   0xe884fa71e94c4c88d8e225fdc425e4df2321692343e51d06f8e5427729c34010
// The ~42.5k delta = one extra math step + bitmap scan + the cross itself (its feeGrowthOutside /
// gauge / period SSTOREs plus the oracle observation a tick change triggers).

// Pre/post loop overhead: cold SLOADs (slot0, liquidity, feeGrowthGlobal) + cold SSTOREs (slot0,
// liquidity) at end of swap. Uniswap V3 budgets ~70k here; Ramses adds the `grossFeeGrowthGlobal`
// write and `advancePeriod` period bookkeeping (~+8k on-chain), hence ~78k.
const SWAP_BASE_GAS: u64 = 78_000;
// Bitmap word scan (cold SLOAD of tickBitmap word)
const GAS_PER_BITMAP_WORD: u64 = 2_100;
// swap math step: getSqrtRatioAtTick + computeSwapStep + amount accounting + getTickAtSqrtRatio
const GAS_PER_SWAP_MATH_STEP: u64 = 5_400;
// Initialized tick crossing: Ramses' cross() writes feeGrowthOutside plus gauge/period checkpoints
// (seconds-per-liquidity, tickCumulative, period start tick) and triggers an oracle observation.
// Measured at ~35k (the ~42.5k one-cross delta minus the extra math step + bitmap scan).
const GAS_PER_INITIALIZED_TICK_CROSS: u64 = 35_000;
// Output transfer + balanceBefore + callback + balanceAfter.
const V3_CALLBACK_SETTLEMENT_GAS: u64 = 70_000;
// Conservative max gas budget for a single swap (Ethereum transaction gas limit)
const MAX_SWAP_GAS: u64 = 16_700_000;
const MAX_TICKS_CROSSED: u64 = (MAX_SWAP_GAS - SWAP_BASE_GAS) / GAS_PER_INITIALIZED_TICK_CROSS;

/// State of a Ramses V3 pool.
///
/// Mirrors `UniswapV3State` but, because the Ramses swap fee is governance-mutable and pools are
/// keyed by tick spacing, `fee` is stored as a raw `u32` (hundredths of a bip) and `tick_spacing`
/// is tracked explicitly rather than being derived from the fee tier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RamsesV3State {
    liquidity: u128,
    sqrt_price: U256,
    fee: u32,
    tick: i32,
    tick_spacing: u16,
    ticks: TickList,
}

impl RamsesV3State {
    /// Creates a new instance of `RamsesV3State`.
    ///
    /// # Arguments
    /// - `liquidity`: The initial liquidity of the pool.
    /// - `sqrt_price`: The square root of the current price.
    /// - `fee`: The current swap fee, in hundredths of a bip (1_000_000 = 100%).
    /// - `tick_spacing`: The tick spacing of the pool (its immutable key).
    /// - `tick`: The current tick of the pool.
    /// - `ticks`: A vector of `TickInfo` representing the tick information for the pool.
    pub fn new(
        liquidity: u128,
        sqrt_price: U256,
        fee: u32,
        tick_spacing: u16,
        tick: i32,
        ticks: Vec<TickInfo>,
    ) -> Result<Self, SimulationError> {
        let tick_list = TickList::from(tick_spacing, ticks)?;
        Ok(RamsesV3State { liquidity, sqrt_price, fee, tick, tick_spacing, ticks: tick_list })
    }

    fn swap(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
    ) -> Result<SwapResults, SimulationError> {
        if !self.ticks.has_initialized_ticks() {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }
        let price_limit = if let Some(limit) = sqrt_price_limit {
            limit
        } else if zero_for_one {
            safe_add_u256(MIN_SQRT_RATIO, U256::ONE)?
        } else {
            safe_sub_u256(MAX_SQRT_RATIO, U256::ONE)?
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
                RamsesV3State::get_sqrt_ratio_target(sqrt_price_next, price_limit, zero_for_one),
                state.liquidity,
                state.amount_remaining,
                self.fee,
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
impl ProtocolSim for RamsesV3State {
    fn fee(&self) -> f64 {
        self.fee as f64 / 1_000_000.0
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
        let amount_specified = U256::try_from_be_slice(&amount_in.to_bytes_be())
            .and_then(|unsigned_amount_in| {
                I256::checked_from_sign_and_abs(Sign::Positive, unsigned_amount_in)
            })
            .ok_or_else(|| {
                SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
            })?;

        let result = self.swap(zero_for_one, amount_specified, None)?;

        trace!(?amount_in, ?token_a, ?token_b, ?zero_for_one, ?result, "RAMSES V3 SWAP");
        let mut new_state = self.clone();
        new_state.liquidity = result.liquidity;
        new_state.tick = result.tick;
        new_state.sqrt_price = result.sqrt_price;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(
                result
                    .amount_calculated
                    .saturating_abs()
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
        if !self.ticks.has_initialized_ticks() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }

        let zero_for_one = token_in < token_out;
        let mut current_tick = self.tick;
        let mut current_sqrt_price = self.sqrt_price;
        let mut current_liquidity = self.liquidity;
        let mut total_amount_in = U256::ZERO;
        let mut total_amount_out = U256::ZERO;
        let mut ticks_crossed: u64 = 0;

        // Iterate through ticks in the direction of the swap
        // Stops when: no more liquidity, no more ticks, or gas limit would be exceeded
        while let Ok((tick, initialized)) = self
            .ticks
            .next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
            // Cap iteration to prevent exceeding Ethereum's gas limit
            if ticks_crossed == MAX_TICKS_CROSSED {
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
        mut delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // apply attribute changes
        if let Some(liquidity) = delta
            .updated_attributes
            .remove("liquidity")
        {
            self.liquidity = liquidity.into();
        }
        if let Some(sqrt_price) = delta
            .updated_attributes
            .get("sqrt_price_x96")
        {
            self.sqrt_price = U256::from_be_slice(sqrt_price);
        }
        if let Some(tick) = delta.updated_attributes.remove("tick") {
            self.tick = tick.into();
        }
        // The Ramses swap fee is governance-mutable, so apply fee updates here.
        if let Some(fee) = delta.updated_attributes.remove("fee") {
            self.fee = fee.into();
        }

        // update ticks
        for (key, value) in delta.updated_attributes {
            let Some(tick) = key.strip_prefix("ticks/") else {
                continue;
            };

            self.ticks
                .set_tick_liquidity(
                    tick.parse::<i32>()
                        .map_err(|err| TransitionError::DecodeError(err.to_string()))?,
                    i128::from(value),
                )
                .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
        }
        // delete ticks
        for key in delta.deleted_attributes {
            let Some(tick) = key.strip_prefix("ticks/") else {
                continue;
            };

            self.ticks
                .set_tick_liquidity(
                    tick.parse::<i32>()
                        .map_err(|err| TransitionError::DecodeError(err.to_string()))?,
                    0,
                )
                .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
        }
        Ok(())
    }

    /// See [`ProtocolSim::query_pool_swap`] for the trait documentation.
    ///
    /// This method uses Ramses V3 internal swap logic by swapping an infinite amount of token_in
    /// until the target price is reached.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        if !self.ticks.has_initialized_ticks() {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }

        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "Ramses V3 does not support TradeLimitPrice constraint in query_pool_swap"
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
                    self.fee,
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

    fn transitions_from_delta_alone(&self, _delta: &ProtocolStateDelta) -> bool {
        true
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
        let Some(RamsesV3State { liquidity, sqrt_price, fee, tick, tick_spacing, ticks }) = other
            .as_any()
            .downcast_ref::<RamsesV3State>()
        else {
            return false;
        };

        &self.liquidity == liquidity &&
            &self.sqrt_price == sqrt_price &&
            &self.fee == fee &&
            &self.tick == tick &&
            &self.tick_spacing == tick_spacing &&
            &self.ticks == ticks
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use num_bigint::ToBigUint;
    use tycho_common::models::Chain;

    use super::*;

    // Real WBTC/WETH pool data (Uniswap V3, 0.05% fee, tick spacing 10). Because the Ramses swap
    // math is identical to Uniswap V3, the same inputs must produce the same outputs — this test
    // guards math parity with the upstream implementation.
    #[test]
    fn test_get_amount_out_math_parity() {
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
        let pool = RamsesV3State::new(
            377952820878029838,
            U256::from_str("28437325270877025820973479874632004").unwrap(),
            500,
            10,
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

        let res = pool
            .get_amount_out(500000000.to_biguint().unwrap(), &wbtc, &weth)
            .unwrap();

        assert_eq!(res.amount, BigUint::from_str("64352395915550406461").unwrap());
    }

    #[test]
    fn test_delta_transition_updates_mutable_fee() {
        let mut pool = RamsesV3State::new(
            1000,
            U256::from_str("1000").unwrap(),
            500,
            10,
            100,
            vec![TickInfo::new(255760, 10000).unwrap(), TickInfo::new(255900, -10000).unwrap()],
        )
        .unwrap();

        let attributes: HashMap<String, Bytes> = [
            ("liquidity".to_string(), Bytes::from(2000_u64.to_be_bytes().to_vec())),
            // 3000 hundredths-of-a-bip, as emitted by the substreams (to_signed_bytes_be ->
            // 0x0bb8)
            ("fee".to_string(), Bytes::from(3000_u32.to_be_bytes().to_vec())),
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
        assert_eq!(pool.fee, 3000);
        assert_eq!(pool.fee(), 0.003);
    }
}
