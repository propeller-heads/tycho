use std::{any::Any, collections::HashMap, fmt};

use alloy::primitives::{Address, Sign, I256, U256};
use num_bigint::BigUint;
use num_traits::{CheckedSub, ToPrimitive, Zero};
use revm::primitives::I128;
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

use super::hooks::utils::{has_permission, HookOptions};
use crate::{
    evm::protocol::{
        clmm::clmm_swap_to_price,
        safe_math::{safe_add_u256, safe_sub_u256},
        u256_num::u256_to_biguint,
        uniswap_v4::hooks::{
            hook_handler::HookHandler,
            models::{
                AfterSwapParameters, BalanceDelta, BeforeSwapDelta, BeforeSwapParameters,
                StateContext, SwapParams,
            },
        },
        utils::{
            add_fee_markup,
            uniswap::{
                i24_be_bytes_to_i32, liquidity_math,
                lp_fee::{self, is_dynamic},
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
        vm::constants::EXTERNAL_ACCOUNT,
    },
    impl_non_serializable_protocol,
};

// Fixed overhead per swap: covers router overhead, executor preamble (decode, sync,
// unlock/callback pattern), and token transfer-in.
const SWAP_BASE_GAS: u64 = 185_000;
// Per loop: PoolManager.swap bitmap lookup + sqrt math + computeSwapStep.
// V4's singleton PoolManager hits warmer storage than V3 standalone pools: ~3,500/loop.
const GAS_PER_BITMAP_LOOKUP: u64 = 3_500;
// Initialized tick crossing: _updateTick() updates feeGrowthOutside0/1 (2 SSTOREs).
// Warm ≈ 10–17k, cold ≈ 40–52k. 17,540 reflects warm scenario.
const GAS_PER_TICK: u64 = 17_540;
// Settlement overhead within swapExactInputSingle: _settle() + _getFullCredit() + misc.
const V4_CALLBACK_SETTLEMENT_GAS: u64 = 30_000;
// Conservative max gas budget for a single swap (Ethereum transaction gas limit)
const MAX_SWAP_GAS: u64 = 16_700_000;
const MAX_TICKS_CROSSED: u64 = (MAX_SWAP_GAS - SWAP_BASE_GAS) / GAS_PER_TICK;

#[derive(Clone)]
pub struct UniswapV4State {
    liquidity: u128,
    sqrt_price: U256,
    fees: UniswapV4Fees,
    tick: i32,
    ticks: TickList,
    tick_spacing: i32,
    pub hook: Option<Box<dyn HookHandler>>,
}

impl_non_serializable_protocol!(UniswapV4State, "not supported due vm state deps");

impl fmt::Debug for UniswapV4State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UniswapV4State")
            .field("liquidity", &self.liquidity)
            .field("sqrt_price", &self.sqrt_price)
            .field("fees", &self.fees)
            .field("tick", &self.tick)
            .field("tick_spacing", &self.tick_spacing)
            .finish_non_exhaustive()
    }
}

impl PartialEq for UniswapV4State {
    fn eq(&self, other: &Self) -> bool {
        match (&self.hook, &other.hook) {
            (Some(a), Some(b)) => a.is_equal(&**b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for UniswapV4State {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniswapV4Fees {
    // Protocol fees in the zero for one direction
    pub zero_for_one: u32,
    // Protocol fees in the one for zero direction
    pub one_for_zero: u32,
    // Liquidity providers fees
    pub lp_fee: u32,
}

impl UniswapV4Fees {
    pub fn new(zero_for_one: u32, one_for_zero: u32, lp_fee: u32) -> Self {
        Self { zero_for_one, one_for_zero, lp_fee }
    }

    fn calculate_swap_fees_pips(&self, zero_for_one: bool, lp_fee_override: Option<u32>) -> u32 {
        let protocol_fee = if zero_for_one { self.zero_for_one } else { self.one_for_zero };
        let lp_fee = lp_fee_override.unwrap_or_else(|| {
            // If a protocol has dynamic fees,
            if is_dynamic(self.lp_fee) {
                0
            } else {
                self.lp_fee
            }
        });

        // UniswapV4 formula: protocolFee + lpFee - (protocolFee * lpFee / 1_000_000)
        // Source: https://raw.githubusercontent.com/Uniswap/v4-core/main/src/libraries/ProtocolFeeLibrary.sol
        // This accounts for the fact that protocol fee is taken first, then LP fee applies to
        // remainder
        protocol_fee + lp_fee - ((protocol_fee as u64 * lp_fee as u64 / 1_000_000) as u32)
    }
}

impl UniswapV4State {
    /// Creates a new `UniswapV4State` with specified values.
    pub fn new(
        liquidity: u128,
        sqrt_price: U256,
        fees: UniswapV4Fees,
        tick: i32,
        tick_spacing: i32,
        ticks: Vec<TickInfo>,
    ) -> Result<Self, SimulationError> {
        let tick_spacing_u16 = tick_spacing.try_into().map_err(|_| {
            // even though it's given as int24, tick_spacing must be positive, see here:
            // https://github.com/Uniswap/v4-core/blob/a22414e4d7c0d0b0765827fe0a6c20dfd7f96291/src/libraries/TickMath.sol#L25-L28
            SimulationError::FatalError(format!(
                "tick_spacing {} must be positive (int24 -> u16 conversion failed)",
                tick_spacing
            ))
        })?;
        let tick_list = TickList::from(tick_spacing_u16, ticks)?;
        Ok(UniswapV4State {
            liquidity,
            sqrt_price,
            fees,
            tick,
            ticks: tick_list,
            tick_spacing,
            hook: None,
        })
    }

    fn swap(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
        lp_fee_override: Option<u32>,
    ) -> Result<SwapResults, SimulationError> {
        if amount_specified == I256::ZERO {
            return Ok(SwapResults {
                amount_calculated: I256::ZERO,
                amount_specified: I256::ZERO,
                amount_remaining: I256::ZERO,
                sqrt_price: self.sqrt_price,
                liquidity: self.liquidity,
                tick: self.tick,
                gas_used: U256::from(3_000), // baseline gas cost for no-op swap
            });
        }

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

        let exact_input = amount_specified < I256::ZERO;

        let mut state = SwapState {
            amount_remaining: amount_specified,
            amount_calculated: I256::ZERO,
            sqrt_price: self.sqrt_price,
            tick: self.tick,
            liquidity: self.liquidity,
        };
        let mut gas_used = U256::from(SWAP_BASE_GAS);

        while state.amount_remaining != I256::ZERO && state.sqrt_price != price_limit {
            let (mut next_tick, initialized) = match self
                .ticks
                .next_initialized_tick_within_one_word(state.tick, zero_for_one)
            {
                Ok((tick, init)) => {
                    gas_used = safe_add_u256(gas_used, U256::from(GAS_PER_BITMAP_LOOKUP))?;
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

            let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;
            let fee_pips = self
                .fees
                .calculate_swap_fees_pips(zero_for_one, lp_fee_override);

            let sqrt_price_start = state.sqrt_price;
            let (sqrt_price, amount_in, amount_out, fee_amount) = swap_math::compute_swap_step(
                state.sqrt_price,
                UniswapV4State::get_sqrt_ratio_target(sqrt_price_next, price_limit, zero_for_one),
                state.liquidity,
                // The core univ4 swap logic assumes that if the amount is > 0 it's exact in, and
                // if it's < 0 it's exact out. The compute_swap_step assumes the
                // opposite (it's like that for univ3).
                -state.amount_remaining,
                fee_pips,
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
                state.amount_remaining += I256::checked_from_sign_and_abs(
                    Sign::Positive,
                    safe_add_u256(step.amount_in, step.fee_amount)?,
                )
                .unwrap();
                state.amount_calculated -=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_out).unwrap();
            } else {
                state.amount_remaining -=
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
        }

        Ok(SwapResults {
            amount_calculated: state.amount_calculated,
            amount_specified,
            amount_remaining: state.amount_remaining,
            sqrt_price: state.sqrt_price,
            liquidity: state.liquidity,
            tick: state.tick,
            gas_used: safe_add_u256(gas_used, U256::from(V4_CALLBACK_SETTLEMENT_GAS))?,
        })
    }

    pub fn set_hook_handler(&mut self, handler: Box<dyn HookHandler>) {
        self.hook = Some(handler);
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

    fn find_limits_experimentally(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        // Create dummy token objects with proper addresses. This is fine since `get_amount_out`
        // only uses the token addresses.
        let token_in_obj =
            Token::new(&token_in, "TOKEN_IN", 18, 0, &[Some(10_000)], Default::default(), 100);
        let token_out_obj =
            Token::new(&token_out, "TOKEN_OUT", 18, 0, &[Some(10_000)], Default::default(), 100);

        self.find_max_amount(&token_in_obj, &token_out_obj)
    }

    /// Finds max amount by performing exponential search.
    ///
    /// Reasoning:
    /// - get_amount_out(I256::MAX) will almost always fail, so this will waste time checking values
    ///   unrealistically high.
    /// - If you were to start binary search from 1 to 10^76, you'd need hundreds of iterations.
    ///
    /// More about exponential search: https://en.wikipedia.org/wiki/Exponential_search
    ///
    /// # Returns
    ///
    /// Returns a tuple containing the max amount in and max amount out respectively.
    fn find_max_amount(
        &self,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let mut low = BigUint::from(1u64);

        // The max you can swap on a USV4 is I256::MAX is 5.7e76, since input amount is I256.
        // So start with something much smaller to search for a reasonable upper bound.
        let mut high = BigUint::from(10u64).pow(18); // 1 ether in wei
        let mut last_successful_amount_in = BigUint::from(1u64);
        let mut last_successful_amount_out = BigUint::from(0u64);

        // First, find an upper bound where the swap fails using exponential search.
        // Save and return both the amount in and amount out.
        while let Ok(result) = self.get_amount_out(high.clone(), token_in, token_out) {
            // We haven't found the upper bound yet, increase the attempted upper bound
            // by order of magnitude and store the last success as the lower bound.
            low = last_successful_amount_in.clone();
            last_successful_amount_in = high.clone();
            last_successful_amount_out = result.amount;
            high *= BigUint::from(10u64);

            // Stop if we're getting too large for I256 (about 10^75)
            if high > BigUint::from(10u64).pow(75) {
                return Ok((last_successful_amount_in, last_successful_amount_out));
            }
        }

        // Use binary search to narrow down value between low and high
        while &high - &low > BigUint::from(1u64) {
            let mid = (&low + &high) / BigUint::from(2u64);

            match self.get_amount_out(mid.clone(), token_in, token_out) {
                Ok(result) => {
                    last_successful_amount_in = mid.clone();
                    last_successful_amount_out = result.amount;
                    low = mid;
                }
                Err(_) => {
                    high = mid;
                }
            }
        }

        Ok((last_successful_amount_in, last_successful_amount_out))
    }

    /// Helper method to check if there are no initialized ticks in either direction
    fn has_no_initialized_ticks(&self) -> bool {
        !self.ticks.has_initialized_ticks()
    }
}

#[typetag::serde]
impl ProtocolSim for UniswapV4State {
    // Not possible to implement correctly with the current interface because we need to know the
    // swap direction.
    fn fee(&self) -> f64 {
        todo!()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if let Some(hook) = &self.hook {
            match hook.spot_price(base, quote) {
                Ok(price) => return Ok(price),
                Err(SimulationError::RecoverableError(_)) => {
                    // Calculate spot price by swapping two amounts and use the approximation
                    // to get the derivative, following the pattern from vm/state.rs

                    // Calculate the first sell amount (x1) as a small amount
                    let x1 = BigUint::from(10u64).pow(base.decimals) / BigUint::from(100u64); // 0.01 token

                    // Calculate the second sell amount (x2) as x1 + 1% of x1
                    let x2 = &x1 + (&x1 / BigUint::from(100u64));

                    // Perform swaps to get the received amounts
                    let y1 = self.get_amount_out(x1.clone(), base, quote)?;
                    let y2 = self.get_amount_out(x2.clone(), base, quote)?;

                    // Calculate the marginal price
                    let num = y2
                        .amount
                        .checked_sub(&y1.amount)
                        .ok_or_else(|| {
                            SimulationError::FatalError(
                                "Cannot calculate spot price: y2 < y1".to_string(),
                            )
                        })?;
                    let den = x2.checked_sub(&x1).ok_or_else(|| {
                        SimulationError::FatalError(
                            "Cannot calculate spot price: x2 < x1".to_string(),
                        )
                    })?;

                    if den == BigUint::from(0u64) {
                        return Err(SimulationError::FatalError(
                            "Cannot calculate spot price: denominator is zero".to_string(),
                        ));
                    }

                    // Convert to f64 and adjust for decimals
                    let num_f64 = num.to_f64().ok_or_else(|| {
                        SimulationError::FatalError(
                            "Failed to convert numerator to f64".to_string(),
                        )
                    })?;
                    let den_f64 = den.to_f64().ok_or_else(|| {
                        SimulationError::FatalError(
                            "Failed to convert denominator to f64".to_string(),
                        )
                    })?;

                    let token_correction = 10f64.powi(base.decimals as i32 - quote.decimals as i32);

                    return Ok(num_f64 / den_f64 * token_correction);
                }
                Err(e) => return Err(e),
            }
        }

        let zero_for_one = base < quote;
        let fee_pips = self
            .fees
            .calculate_swap_fees_pips(zero_for_one, None);
        let fee = fee_pips as f64 / 1_000_000.0;

        let price = if zero_for_one {
            sqrt_price_q96_to_f64(self.sqrt_price, base.decimals, quote.decimals)?
        } else {
            1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, quote.decimals, base.decimals)?
        };

        Ok(add_fee_markup(price, fee))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let zero_for_one = token_in < token_out;
        let amount_specified = I256::checked_from_sign_and_abs(
            Sign::Negative,
            U256::from_be_slice(&amount_in.to_bytes_be()),
        )
        .ok_or_else(|| {
            SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
        })?;

        let mut amount_to_swap = amount_specified;
        let mut lp_fee_override: Option<u32> = None;
        let mut before_swap_gas = 0u64;
        let mut after_swap_gas = 0u64;
        let mut before_swap_delta = BeforeSwapDelta(I256::ZERO);
        let mut storage_overwrites = None;

        let token_in_address = Address::from_slice(&token_in.address);
        let token_out_address = Address::from_slice(&token_out.address);

        let state_context = StateContext {
            currency_0: if zero_for_one { token_in_address } else { token_out_address },
            currency_1: if zero_for_one { token_out_address } else { token_in_address },
            fees: self.fees.clone(),
            tick_spacing: self.tick_spacing,
        };

        let swap_params = SwapParams {
            zero_for_one,
            amount_specified: amount_to_swap,
            sqrt_price_limit: self.sqrt_price,
        };

        // Check if hook is set and has before_swap permissions
        if let Some(ref hook) = self.hook {
            if has_permission(hook.address(), HookOptions::BeforeSwap) {
                let before_swap_params = BeforeSwapParameters {
                    context: state_context.clone(),
                    sender: *EXTERNAL_ACCOUNT,
                    swap_params: swap_params.clone(),
                    hook_data: Bytes::new(),
                };

                let before_swap_result = hook
                    .before_swap(before_swap_params, None, None)
                    .map_err(|e| {
                        SimulationError::FatalError(format!(
                            "BeforeSwap hook simulation failed: {e:?}"
                        ))
                    })?;

                before_swap_gas = before_swap_result.gas_estimate;
                before_swap_delta = before_swap_result.result.amount_delta;
                storage_overwrites = Some(before_swap_result.result.overwrites);

                // Convert amountDelta to amountToSwap as per Uniswap V4 spec
                // See: https://github.com/Uniswap/v4-core/blob/main/src/libraries/Hooks.sol#L270
                if before_swap_delta.as_i256() != I256::ZERO {
                    amount_to_swap += I256::from(before_swap_delta.get_specified_delta());
                    if amount_to_swap > I256::ZERO {
                        return Err(SimulationError::FatalError(
                            "Hook delta exceeds swap amount".into(),
                        ));
                    }
                }

                // Set LP fee override if provided by hook
                // The fee returned by beforeSwap may have the override flag (bit 22) set,
                // which needs to be removed before using the fee value.
                // See: https://github.com/Uniswap/v4-core/blob/main/src/libraries/LPFeeLibrary.sol
                let hook_fee = before_swap_result
                    .result
                    .fee
                    .to::<u32>();
                if hook_fee != 0 {
                    // Remove the override flag (bit 22) as per LPFeeLibrary.sol
                    let cleaned_fee = lp_fee::remove_override_flag(hook_fee);

                    // Validate the fee doesn't exceed MAX_LP_FEE (1,000,000 pips = 100%)
                    if !lp_fee::is_valid(cleaned_fee) {
                        return Err(SimulationError::FatalError(format!(
                            "LP fee override {} exceeds maximum {} pips",
                            cleaned_fee,
                            lp_fee::MAX_LP_FEE
                        )));
                    }

                    lp_fee_override = Some(cleaned_fee);
                }
            }
        }

        // Perform the swap with potential hook modifications
        let result = self.swap(zero_for_one, amount_to_swap, None, lp_fee_override)?;

        // Create BalanceDelta from swap result using the proper constructor
        let mut swap_delta = BalanceDelta::from_swap_result(result.amount_calculated, zero_for_one);

        // Get deltas (change in the specified/given and unspecified/computed token balances after
        // calling before swap)
        let hook_delta_specified = before_swap_delta.get_specified_delta();
        let mut hook_delta_unspecified = before_swap_delta.get_unspecified_delta();

        if let Some(ref hook) = self.hook {
            if has_permission(hook.address(), HookOptions::AfterSwap) {
                let after_swap_params = AfterSwapParameters {
                    context: state_context,
                    sender: *EXTERNAL_ACCOUNT,
                    swap_params,
                    delta: swap_delta,
                    hook_data: Bytes::new(),
                };

                let after_swap_result = hook
                    .after_swap(after_swap_params, storage_overwrites, None)
                    .map_err(|e| {
                        SimulationError::FatalError(format!(
                            "AfterSwap hook simulation failed: {e:?}"
                        ))
                    })?;
                after_swap_gas = after_swap_result.gas_estimate;
                hook_delta_unspecified += after_swap_result.result;
            }
        }

        // Replicates the behaviour of the Hooks library wrapper of the afterSwap method:
        // https://github.com/Uniswap/v4-core/blob/59d3ecf53afa9264a16bba0e38f4c5d2231f80bc/src/libraries/Hooks.sol
        if (hook_delta_specified != I128::ZERO) || (hook_delta_unspecified != I128::ZERO) {
            let hook_delta = if (amount_specified < I256::ZERO) == zero_for_one {
                BalanceDelta::new(hook_delta_specified, hook_delta_unspecified)
            } else {
                BalanceDelta::new(hook_delta_unspecified, hook_delta_specified)
            };
            // This is a BalanceDelta subtraction
            swap_delta = swap_delta - hook_delta
        }

        let amount_out = if (amount_specified < I256::ZERO) == zero_for_one {
            swap_delta.amount1()
        } else {
            swap_delta.amount0()
        };

        trace!(?amount_in, ?token_in, ?token_out, ?zero_for_one, ?result, "V4 SWAP");
        let mut new_state = self.clone();
        new_state.liquidity = result.liquidity;
        new_state.tick = result.tick;
        new_state.sqrt_price = result.sqrt_price;

        // Add hook gas costs to baseline swap cost
        let total_gas_used = result.gas_used + U256::from(before_swap_gas + after_swap_gas);
        Ok(GetAmountOutResult::new(
            u256_to_biguint(U256::from(amount_out.abs())),
            u256_to_biguint(total_gas_used),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if let Some(hook) = &self.hook {
            // Check if pool has no liquidity & ticks -> hook manages liquidity
            if self.liquidity == 0 && self.has_no_initialized_ticks() {
                // If the hook has a get_amount_ranges entrypoint, call it and return (0, limits[1])
                match hook.get_amount_ranges(token_in.clone(), token_out.clone()) {
                    Ok(amount_ranges) => {
                        return Ok((
                            u256_to_biguint(amount_ranges.amount_in_range.1),
                            u256_to_biguint(amount_ranges.amount_out_range.1),
                        ))
                    }
                    // Check if hook get_amount_ranges is not implemented or the limits entrypoint
                    // is not set for this hook
                    Err(SimulationError::RecoverableError(msg))
                        if msg.contains("not implemented") || msg.contains("not set") =>
                    {
                        // Hook manages liquidity but doesn't have get_amount_ranges
                        // Use binary search to find limits by calling swap with increasing amounts
                        return self.find_limits_experimentally(token_in, token_out);
                        // Otherwise fall back to default implementation
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // If the pool has no liquidity, return zeros for both limits
        if self.liquidity == 0 {
            return Ok((BigUint::zero(), BigUint::zero()));
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

            // If we've consumed all liquidity, no point continuing the loop
            if current_liquidity == 0 {
                break;
            }
        }

        Ok((u256_to_biguint(total_amount_in), u256_to_biguint(total_amount_out)))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(mut hook) = self.hook.clone() {
            match hook.delta_transition(delta.clone(), tokens, balances) {
                Ok(()) => self.set_hook_handler(hook),
                Err(TransitionError::SimulationError(SimulationError::RecoverableError(msg)))
                    if msg.contains("not implemented") =>
                {
                    // Fall back to default implementation
                }
                Err(e) => return Err(e),
            }
        }

        // Apply attribute changes
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
        if let Some(tick) = delta.updated_attributes.get("tick") {
            self.tick = i24_be_bytes_to_i32(tick);
        }
        if let Some(lp_fee) = delta.updated_attributes.get("fee") {
            self.fees.lp_fee = u32::from(lp_fee.clone());
        }
        if let Some(zero2one_protocol_fee) = delta
            .updated_attributes
            .get("protocol_fees/zero2one")
        {
            self.fees.zero_for_one = u32::from(zero2one_protocol_fee.clone());
        }
        if let Some(one2zero_protocol_fee) = delta
            .updated_attributes
            .get("protocol_fees/one2zero")
        {
            self.fees.one_for_zero = u32::from(one2zero_protocol_fee.clone());
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
    /// This method uses Uniswap V4 internal swap logic by swapping an infinite amount of token_in
    /// until the target price is reached. Takes into account V4-specific features like protocol
    /// fees and dynamic LP fees.
    ///
    /// Note: This implementation does not invoke hooks, as it is a query-only operation meant to
    /// determine available liquidity at a given price without executing an actual swap.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        if self.liquidity == 0 {
            return Err(SimulationError::FatalError("No liquidity".to_string()));
        }

        // Calculate total fee (protocol + LP fee) for V4
        let zero_for_one = params.token_in().address < params.token_out().address;
        let fee_pips = self
            .fees
            .calculate_swap_fees_pips(zero_for_one, None);

        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "Uniswap V4 does not support TradeLimitPrice constraint in query_pool_swap"
                    .to_string(),
                None,
            )),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: _,
                min_amount_in: _,
                max_amount_in: _,
            } => {
                if self.liquidity == 0 {
                    return Err(SimulationError::FatalError("No liquidity".to_string()));
                }

                let (amount_in, amount_out, swap_result) = clmm_swap_to_price(
                    self.sqrt_price,
                    &params.token_in().address,
                    &params.token_out().address,
                    target,
                    fee_pips,
                    Sign::Negative, // V4 uses negative for exact input
                    |zero_for_one, amount_specified, sqrt_price_limit| {
                        self.swap(zero_for_one, amount_specified, Some(sqrt_price_limit), None)
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
            .downcast_ref::<UniswapV4State>()
        {
            self.liquidity == other_state.liquidity &&
                self.sqrt_price == other_state.sqrt_price &&
                self.fees == other_state.fees &&
                self.tick == other_state.tick &&
                self.ticks == other_state.ticks
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, path::Path, str::FromStr};

    use alloy::primitives::aliases::U24;
    use num_traits::FromPrimitive;
    use rstest::rstest;
    use serde_json::Value;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{models::Chain, simulation::protocol_sim::Price};

    use super::*;
    use crate::{
        evm::{
            engine_db::{
                create_engine,
                simulation_db::SimulationDB,
                utils::{get_client, get_runtime},
            },
            protocol::{
                uniswap_v4::hooks::{
                    angstrom::hook_handler::{AngstromFees, AngstromHookHandler},
                    generic_vm_hook_handler::GenericVMHookHandler,
                },
                utils::uniswap::{lp_fee, sqrt_price_math::get_sqrt_price_q96},
            },
        },
        protocol::models::{DecoderContext, TryFromWithBlock},
    };

    // Helper methods to create commonly used tokens
    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Default::default(),
            100,
        )
    }

    fn weth() -> Token {
        Token::new(
            &Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Default::default(),
            100,
        )
    }

    fn eth() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            "ETH",
            18,
            0,
            &[Some(10_000)],
            Default::default(),
            100,
        )
    }

    fn token_x() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "X",
            18,
            0,
            &[Some(10_000)],
            Default::default(),
            100,
        )
    }

    fn token_y() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
            "Y",
            18,
            0,
            &[Some(10_000)],
            Default::default(),
            100,
        )
    }

    #[test]
    fn test_delta_transition() {
        let mut pool = UniswapV4State::new(
            1000,
            U256::from_str("1000").unwrap(),
            UniswapV4Fees { zero_for_one: 100, one_for_zero: 90, lp_fee: 700 },
            100,
            60,
            vec![TickInfo::new(120, 10000).unwrap(), TickInfo::new(180, -10000).unwrap()],
        )
        .unwrap();

        let attributes: HashMap<String, Bytes> = [
            ("liquidity".to_string(), Bytes::from(2000_u64.to_be_bytes().to_vec())),
            ("sqrt_price_x96".to_string(), Bytes::from(1001_u64.to_be_bytes().to_vec())),
            ("tick".to_string(), Bytes::from(120_i32.to_be_bytes().to_vec())),
            ("protocol_fees/zero2one".to_string(), Bytes::from(50_u32.to_be_bytes().to_vec())),
            ("protocol_fees/one2zero".to_string(), Bytes::from(75_u32.to_be_bytes().to_vec())),
            ("fee".to_string(), Bytes::from(100_u32.to_be_bytes().to_vec())),
            ("ticks/-120/net_liquidity".to_string(), Bytes::from(10200_u64.to_be_bytes().to_vec())),
            ("ticks/120/net_liquidity".to_string(), Bytes::from(9800_u64.to_be_bytes().to_vec())),
            ("block_number".to_string(), Bytes::from(2000_u64.to_be_bytes().to_vec())),
            ("block_timestamp".to_string(), Bytes::from(1758201935_u64.to_be_bytes().to_vec())),
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
        assert_eq!(pool.fees.zero_for_one, 50);
        assert_eq!(pool.fees.one_for_zero, 75);
        assert_eq!(pool.fees.lp_fee, 100);
        assert_eq!(
            pool.ticks
                .get_tick(-120)
                .unwrap()
                .net_liquidity,
            10200
        );
        assert_eq!(
            pool.ticks
                .get_tick(120)
                .unwrap()
                .net_liquidity,
            9800
        );
    }

    #[tokio::test]
    /// Compares a quote from the UniswapV4 Quoter contract on Sepolia with a simulation.
    async fn test_swap_sim() {
        use tycho_client::feed::dto;
        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path = Path::new(project_root)
            .join("tests/assets/decoder/uniswap_v4_snapshot_sepolia_block_7239119.json");
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        let data: Value = serde_json::from_str(&json_data).expect("Failed to parse JSON");
        let state: ComponentWithState = serde_json::from_value::<dto::ComponentWithState>(data)
            .expect("Expected json to match ComponentWithState structure")
            .into();

        let block = BlockHeader {
            number: 7239119,
            hash: Bytes::from_str(
                "0x28d41d40f2ac275a4f5f621a636b9016b527d11d37d610a45ac3a821346ebf8c",
            )
            .expect("Invalid block hash"),
            parent_hash: Bytes::from(vec![0; 32]),
            ..Default::default()
        };

        let t0 = Token::new(
            &Bytes::from_str("0x647e32181a64f4ffd4f0b0b4b052ec05b277729c").unwrap(),
            "T0",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let t1 = Token::new(
            &Bytes::from_str("0xe390a1c311b26f14ed0d55d3b0261c2320d15ca5").unwrap(),
            "T1",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let all_tokens = [t0.clone(), t1.clone()]
            .iter()
            .map(|t| (t.address.clone(), t.clone()))
            .collect();

        let usv4_state = UniswapV4State::try_from_with_header(
            state,
            block,
            &Default::default(),
            &all_tokens,
            &DecoderContext::new(),
        )
        .await
        .unwrap();

        let res = usv4_state
            .get_amount_out(BigUint::from_u64(1000000000000000000).unwrap(), &t0, &t1)
            .unwrap();

        let expected_amount = BigUint::from(9999909699895_u64);
        assert_eq!(res.amount, expected_amount);
    }

    #[tokio::test]
    async fn test_get_limits() {
        use tycho_client::feed::dto;
        let block = BlockHeader {
            number: 22689129,
            hash: Bytes::from_str(
                "0x7763ea30d11aef68da729b65250c09a88ad00458c041064aad8c9a9dbf17adde",
            )
            .expect("Invalid block hash"),
            parent_hash: Bytes::from(vec![0; 32]),
            ..Default::default()
        };

        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path =
            Path::new(project_root).join("tests/assets/decoder/uniswap_v4_snapshot.json");
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        let data: Value = serde_json::from_str(&json_data).expect("Failed to parse JSON");
        let state: ComponentWithState = serde_json::from_value::<dto::ComponentWithState>(data)
            .expect("Expected json to match ComponentWithState structure")
            .into();

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
            &Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(),
            "USDT",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let all_tokens = [t0.clone(), t1.clone()]
            .iter()
            .map(|t| (t.address.clone(), t.clone()))
            .collect();

        let usv4_state = UniswapV4State::try_from_with_header(
            state,
            block,
            &Default::default(),
            &all_tokens,
            &DecoderContext::new(),
        )
        .await
        .unwrap();

        let res = usv4_state
            .get_limits(t0.address.clone(), t1.address.clone())
            .unwrap();

        assert_eq!(&res.0, &BigUint::from_u128(71698353688830259750744466706).unwrap());

        let out = usv4_state
            .get_amount_out(res.0, &t0, &t1)
            .expect("swap for limit in didn't work");

        assert_eq!(&res.1, &out.amount);
    }
    #[test]
    fn test_get_amount_out_no_hook() {
        // Test using transaction 0x78ea4bbb7d4405000f33fdf6f3fa08b5e557d50e5e7f826a79766d50bd643b6f

        // Pool ID: 0x00b9edc1583bf6ef09ff3a09f6c23ecb57fd7d0bb75625717ec81eed181e22d7
        // Information taken from Tenderly simulation / event emitted on Etherscan
        let usv4_state = UniswapV4State::new(
            541501951282951892,
            U256::from_str("5362798333066270795901222").unwrap(), // Sqrt price
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 100 },
            -192022,
            1,
            // Ticks taken from indexer logs
            vec![
                TickInfo {
                    index: -887272,
                    net_liquidity: 460382969070005,
                    sqrt_price: U256::from(4295128739_u64),
                },
                TickInfo {
                    index: -207244,
                    net_liquidity: 561268407024557,
                    sqrt_price: U256::from_str("2505291706254206075074035").unwrap(),
                },
                TickInfo {
                    index: -196411,
                    net_liquidity: 825711941800452,
                    sqrt_price: U256::from_str("4306080513146952705853399").unwrap(),
                },
                TickInfo {
                    index: -196257,
                    net_liquidity: 64844666874010,
                    sqrt_price: U256::from_str("4339363644587371378270009").unwrap(),
                },
                TickInfo {
                    index: -195611,
                    net_liquidity: 2344045150766798,
                    sqrt_price: U256::from_str("4481806029599743916020126").unwrap(),
                },
                TickInfo {
                    index: -194715,
                    net_liquidity: 391037380558274654,
                    sqrt_price: U256::from_str("4687145946111116896040494").unwrap(),
                },
                TickInfo {
                    index: -194599,
                    net_liquidity: 89032603464508,
                    sqrt_price: U256::from_str("4714409015946702405379370").unwrap(),
                },
                TickInfo {
                    index: -194389,
                    net_liquidity: 66635600426483168,
                    sqrt_price: U256::from_str("4764168603367683402636621").unwrap(),
                },
                TickInfo {
                    index: -194160,
                    net_liquidity: 6123093436523361,
                    sqrt_price: U256::from_str("4819029067726467394386780").unwrap(),
                },
                TickInfo {
                    index: -194025,
                    net_liquidity: 79940813798964,
                    sqrt_price: U256::from_str("4851665907541490407930032").unwrap(),
                },
                TickInfo {
                    index: -193922,
                    net_liquidity: 415630967437234,
                    sqrt_price: U256::from_str("4876715181040466809166531").unwrap(),
                },
                TickInfo {
                    index: -193876,
                    net_liquidity: 9664144015186047,
                    sqrt_price: U256::from_str("4887943972687250473582419").unwrap(),
                },
                TickInfo {
                    index: -193818,
                    net_liquidity: 435344726052344,
                    sqrt_price: U256::from_str("4902138873132735049121973").unwrap(),
                },
                TickInfo {
                    index: -193804,
                    net_liquidity: 221726179374067,
                    sqrt_price: U256::from_str("4905571399964683340605904").unwrap(),
                },
                TickInfo {
                    index: -193719,
                    net_liquidity: 101340835774487,
                    sqrt_price: U256::from_str("4926463397882393957462188").unwrap(),
                },
                TickInfo {
                    index: -193690,
                    net_liquidity: 193367475630077,
                    sqrt_price: U256::from_str("4933611593595025190448924").unwrap(),
                },
                TickInfo {
                    index: -193643,
                    net_liquidity: 357016631583746,
                    sqrt_price: U256::from_str("4945218633428068823432932").unwrap(),
                },
                TickInfo {
                    index: -193520,
                    net_liquidity: 917243184365178,
                    sqrt_price: U256::from_str("4975723910367862081017120").unwrap(),
                },
                TickInfo {
                    index: -193440,
                    net_liquidity: 114125890211958292,
                    sqrt_price: U256::from_str("4995665665861492533686137").unwrap(),
                },
                TickInfo {
                    index: -193380,
                    net_liquidity: -65980729148766579,
                    sqrt_price: U256::from_str("5010674414300823856025303").unwrap(),
                },
                TickInfo {
                    index: -192891,
                    net_liquidity: 1687883551433195,
                    sqrt_price: U256::from_str("5134689105039642314202223").unwrap(),
                },
                TickInfo {
                    index: -192573,
                    net_liquidity: 11108903221360975,
                    sqrt_price: U256::from_str("5216979018647067786855495").unwrap(),
                },
                TickInfo {
                    index: -192448,
                    net_liquidity: 32888457482352,
                    sqrt_price: U256::from_str("5249685603828944002327927").unwrap(),
                },
                TickInfo {
                    index: -191525,
                    net_liquidity: -221726179374067,
                    sqrt_price: U256::from_str("5497623359964843320146512").unwrap(),
                },
                TickInfo {
                    index: -191447,
                    net_liquidity: -32888457482352,
                    sqrt_price: U256::from_str("5519104878745833608097296").unwrap(),
                },
                TickInfo {
                    index: -191444,
                    net_liquidity: -114125890211958292,
                    sqrt_price: U256::from_str("5519932765173943847315221").unwrap(),
                },
                TickInfo {
                    index: -191417,
                    net_liquidity: -101340835774487,
                    sqrt_price: U256::from_str("5527389333636021285046380").unwrap(),
                },
                TickInfo {
                    index: -191384,
                    net_liquidity: -9664144015186047,
                    sqrt_price: U256::from_str("5536516597603056457376182").unwrap(),
                },
                TickInfo {
                    index: -191148,
                    net_liquidity: -561268407024557,
                    sqrt_price: U256::from_str("5602231161238705865493165").unwrap(),
                },
                TickInfo {
                    index: -191147,
                    net_liquidity: -1687883551433195,
                    sqrt_price: U256::from_str("5602511265794328966803451").unwrap(),
                },
                TickInfo {
                    index: -191091,
                    net_liquidity: -89032603464508,
                    sqrt_price: U256::from_str("5618219493196441347292357").unwrap(),
                },
                TickInfo {
                    index: -190950,
                    net_liquidity: -189177935487638,
                    sqrt_price: U256::from_str("5657965894785859782969011").unwrap(),
                },
                TickInfo {
                    index: -190756,
                    net_liquidity: -6123093436523361,
                    sqrt_price: U256::from_str("5713112435031881967192022").unwrap(),
                },
                TickInfo {
                    index: -190548,
                    net_liquidity: -193367475630077,
                    sqrt_price: U256::from_str("5772835841671084402427710").unwrap(),
                },
                TickInfo {
                    index: -190430,
                    net_liquidity: -11108903221360975,
                    sqrt_price: U256::from_str("5806994534290341208820930").unwrap(),
                },
                TickInfo {
                    index: -190195,
                    net_liquidity: -391583014714302569,
                    sqrt_price: U256::from_str("5875625707132601785181387").unwrap(),
                },
                TickInfo {
                    index: -190043,
                    net_liquidity: -357016631583746,
                    sqrt_price: U256::from_str("5920448331650864936739481").unwrap(),
                },
                TickInfo {
                    index: -189779,
                    net_liquidity: -917243184365178,
                    sqrt_price: U256::from_str("5999112356918485175181346").unwrap(),
                },
                TickInfo {
                    index: -189663,
                    net_liquidity: -2344045150766798,
                    sqrt_price: U256::from_str("6034006559279282606084981").unwrap(),
                },
                TickInfo {
                    index: -189620,
                    net_liquidity: -435344726052344,
                    sqrt_price: U256::from_str("6046992979471024289177519").unwrap(),
                },
                TickInfo {
                    index: -189409,
                    net_liquidity: -825711941800452,
                    sqrt_price: U256::from_str("6111123241285165242130911").unwrap(),
                },
                TickInfo {
                    index: -189325,
                    net_liquidity: -3947182209207,
                    sqrt_price: U256::from_str("6136842645893819031257990").unwrap(),
                },
                TickInfo {
                    index: -189324,
                    net_liquidity: -415630967437234,
                    sqrt_price: U256::from_str("6137149480355443943537284").unwrap(),
                },
                TickInfo {
                    index: -115136,
                    net_liquidity: 462452451821,
                    sqrt_price: U256::from_str("250529060232794967902094762").unwrap(),
                },
                TickInfo {
                    index: -92109,
                    net_liquidity: -462452451821,
                    sqrt_price: U256::from_str("792242363124136400178523925").unwrap(),
                },
                TickInfo {
                    index: 887272,
                    net_liquidity: -521280453734808,
                    sqrt_price: U256::from_str("1461446703485210103287273052203988822378723970342")
                        .unwrap(),
                },
            ],
        )
        .unwrap();

        let t0 = usdc();
        let t1 = eth();

        let out = usv4_state
            .get_amount_out(BigUint::from_u64(2000000).unwrap(), &t0, &t1)
            .unwrap();

        assert_eq!(out.amount, BigUint::from_str("436478419853848").unwrap())
    }

    #[test]
    fn test_get_amount_out_euler_hook() {
        // Test using transaction 0xb372306a81c6e840f4ec55f006da6b0b097f435802a2e6fd216998dd12fb4aca
        //
        // Output of beforeSwap:
        // "output":{
        //      "amountToSwap":"0"
        //      "hookReturn":"2520471492123673565794154180707800634502860978735"
        //      "lpFeeOverride":"0"
        // }
        //
        // Output of entire swap, including hooks:
        // "swapDelta":"-2520471491783391198873215717244426027071092767279"
        //
        // Get amount out:
        // "amountOut":"2681115183499232721"

        let block = BlockHeader {
            number: 22689128,
            hash: Bytes::from_str(
                "0xfbfa716523d25d6d5248c18d001ca02b1caf10cabd1ab7321465e2262c41157b",
            )
            .expect("Invalid block hash"),
            timestamp: 1749739055,
            ..Default::default()
        };

        // Pool ID: 0xdd8dd509e58ec98631b800dd6ba86ee569c517ffbd615853ed5ab815bbc48ccb
        // Information taken from Tenderly simulation
        let mut usv4_state = UniswapV4State::new(
            0,
            U256::from_str("4295128740").unwrap(),
            UniswapV4Fees { zero_for_one: 100, one_for_zero: 90, lp_fee: 500 },
            0,
            1,
            vec![],
        )
        .unwrap();

        let hook_address: Address = Address::from_str("0x69058613588536167ba0aa94f0cc1fe420ef28a8")
            .expect("Invalid hook address");

        let db = SimulationDB::new(
            get_client(None).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block.clone()),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");
        let pool_manager = Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90")
            .expect("Invalid pool manager address");

        let hook_handler = GenericVMHookHandler::new(
            hook_address,
            engine,
            pool_manager,
            HashMap::new(),
            HashMap::new(),
            None,
            true, // Euler hook
        )
        .unwrap();

        let t0 = usdc();
        let t1 = weth();

        usv4_state.set_hook_handler(Box::new(hook_handler));
        let out = usv4_state
            .get_amount_out(BigUint::from_u64(7407000000).unwrap(), &t0, &t1)
            .unwrap();

        assert_eq!(out.amount, BigUint::from_str("2681115183499232721").unwrap())
    }

    #[test]
    fn test_get_amount_out_angstrom_hook() {
        // Test using transaction 0x671b8e1d0966cee520dc2bb9628de8e22a17b036e70077504796d0a476932d21
        let mut usv4_state = UniswapV4State::new(
            // Liquidity and tick taken from tycho indexer for same block as transaction
            66319800403673162,
            U256::from_str("1314588940601923011323000261788004").unwrap(),
            // 8388608 (i.e. 0x800000) signifies a dynamic fee.
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 8388608 },
            194343,
            10,
            vec![
                TickInfo::new(-887270, 198117767801).unwrap(),
                TickInfo::new(191990, 24561988698695).unwrap(),
                TickInfo::new(192280, 2839631428751224).unwrap(),
                TickInfo::new(193130, 318786492813931).unwrap(),
                TickInfo::new(194010, 26209207141081).unwrap(),
                TickInfo::new(194210, -26209207141081).unwrap(),
                TickInfo::new(194220, 63136622375641511).unwrap(),
                TickInfo::new(194420, -63136622375641511).unwrap(),
                TickInfo::new(195130, -318786492813931).unwrap(),
                TickInfo::new(196330, -2839631428751224).unwrap(),
                TickInfo::new(197100, -24561988698695).unwrap(),
                TickInfo::new(887270, -198117767801).unwrap(),
            ],
        )
        .unwrap();

        let fees = AngstromFees {
            // To get these values, enable storage access logs on tenderly,
            // and look at the hex value retrieved right after calling afterSwap
            //
            // The value (hex: 0x70000152) contains two packed uint24 values:
            // Lower 24 bits (unlockedFee):         0x152   = 338
            // Upper 24 bits (protocolUnlockedFee): 0x70    = 112
            unlock: U24::from(338),
            protocol_unlock: U24::from(112),
        };
        let hook_handler = AngstromHookHandler::new(
            Address::from_str("0x0000000aa232009084bd71a5797d089aa4edfad4").unwrap(),
            Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap(),
            fees,
            false,
        );

        let t0 = usdc();
        let t1 = weth();

        usv4_state.set_hook_handler(Box::new(hook_handler));
        let out = usv4_state
            .get_amount_out(
                BigUint::from_u64(
                    6645198144, // usdc
                )
                .unwrap(),
                &t0, // usdc IN
                &t1, // weth OUT
            )
            .unwrap();

        assert_eq!(out.amount, BigUint::from_str("1825627051870330472").unwrap())
    }

    #[test]
    fn test_spot_price_with_recoverable_error() {
        // Test that spot_price correctly falls back to swap-based calculation
        // when a RecoverableError (other than "not implemented") is returned

        let usv4_state = UniswapV4State::new(
            1000000000000000000u128,                                  // 1e18 liquidity
            U256::from_str("79228162514264337593543950336").unwrap(), // 1:1 price
            UniswapV4Fees { zero_for_one: 100, one_for_zero: 100, lp_fee: 100 },
            0,
            60,
            vec![
                TickInfo::new(-600, 500000000000000000i128).unwrap(),
                TickInfo::new(600, -500000000000000000i128).unwrap(),
            ],
        )
        .unwrap();

        // Test spot price calculation without a hook (should use default implementation)
        let spot_price_result = usv4_state.spot_price(&usdc(), &weth());
        assert!(spot_price_result.is_ok());

        // The price should be approximately 1.0 (since we set sqrt_price for 1:1)
        // Adjusting for decimals difference (USDC has 6, WETH has 18)
        let price = spot_price_result.unwrap();
        assert!(price > 0.0);
    }

    #[test]
    fn test_get_limits_with_hook_managed_liquidity_no_ranges_entrypoint() {
        // This test demonstrates the experimental limit finding logic for hooks that:
        // 1. Manage liquidity (pool has no liquidity & no ticks)
        // 2. Don't have get_amount_ranges entrypoint

        let block = BlockHeader {
            number: 22689128,
            hash: Bytes::from_str(
                "0xfbfa716523d25d6d5248c18d001ca02b1caf10cabd1ab7321465e2262c41157b",
            )
            .expect("Invalid block hash"),
            timestamp: 1749739055,
            ..Default::default()
        };

        let hook_address: Address = Address::from_str("0x69058613588536167ba0aa94f0cc1fe420ef28a8")
            .expect("Invalid hook address");

        let db = SimulationDB::new(
            get_client(None).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block.clone()),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");
        let pool_manager = Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90")
            .expect("Invalid pool manager address");

        // Create a GenericVMHookHandler without limits_entrypoint
        // This will trigger the "not set" error path and use experimental limit finding
        let hook_handler = GenericVMHookHandler::new(
            hook_address,
            engine,
            pool_manager,
            HashMap::new(),
            HashMap::new(),
            None,
            true, // Euler hook
        )
        .unwrap();

        // Create a UniswapV4State with NO liquidity and NO ticks (hook manages all liquidity)
        let mut usv4_state = UniswapV4State::new(
            0, // no liquidity - hook provides it
            U256::from_str("4295128740").unwrap(),
            UniswapV4Fees { zero_for_one: 100, one_for_zero: 90, lp_fee: 500 },
            0,      // current tick
            1,      // tick spacing
            vec![], // no ticks - hook manages liquidity
        )
        .unwrap();

        usv4_state.set_hook_handler(Box::new(hook_handler));

        let token_in = usdc().address;
        let token_out = weth().address;

        let (amount_in_limit, amount_out_limit) = usv4_state
            .get_limits(token_in, token_out)
            .expect("Should find limits through experimental swapping");

        // Assuming pool supply doesn't change drastically at time of this test
        // At least 1 million USDC, not more than 100 million USDC
        assert!(amount_in_limit > BigUint::from(10u64).pow(12));
        assert!(amount_in_limit < BigUint::from(10u64).pow(14));

        // At least 100 ETH, not more than 10 000 ETH
        assert!(amount_out_limit > BigUint::from(10u64).pow(20));
        assert!(amount_out_limit < BigUint::from(10u64).pow(22));
    }

    #[rstest]
    #[case::high_liquidity(u128::MAX / 2)] // Very large liquidity
    #[case::medium_liquidity(10000000000000000000u128)] // Moderate liquidity: 10e18
    #[case::minimal_liquidity(1000u128)] // Very small liquidity
    fn test_find_max_amount(#[case] liquidity: u128) {
        // Use fixed configuration for all test cases
        let fees = UniswapV4Fees { zero_for_one: 100, one_for_zero: 100, lp_fee: 100 };
        let tick_spacing = 60;
        let ticks = vec![
            TickInfo::new(-600, (liquidity / 4) as i128).unwrap(),
            TickInfo::new(600, -((liquidity / 4) as i128)).unwrap(),
        ];

        let usv4_state = UniswapV4State::new(
            liquidity,
            U256::from_str("79228162514264337593543950336").unwrap(),
            fees,
            0,
            tick_spacing,
            ticks,
        )
        .unwrap();

        let token_in = usdc();
        let token_out = weth();

        let (max_amount_in, _max_amount_out) = usv4_state
            .find_max_amount(&token_in, &token_out)
            .unwrap();

        let success = usv4_state
            .get_amount_out(max_amount_in.clone(), &token_in, &token_out)
            .is_ok();
        assert!(success, "Should be able to swap the exact max amount.");

        let one_more = &max_amount_in + BigUint::from(1u64);
        let should_fail = usv4_state
            .get_amount_out(one_more, &token_in, &token_out)
            .is_err();
        assert!(should_fail, "Swapping max_amount + 1 should fail.");
    }

    #[test]
    fn test_calculate_swap_fees_with_override() {
        // Test that calculate_swap_fees_pips works correctly with overridden fees
        let fees = UniswapV4Fees::new(100, 90, 500);

        // Without override, should use UniswapV4 formula: protocol + lp - (protocol * lp /
        // 1_000_000)
        let total_zero_for_one = fees.calculate_swap_fees_pips(true, None);
        // 100 + 500 - (100 * 500 / 1_000_000) = 600 - 0 = 600 (rounded down)
        assert_eq!(total_zero_for_one, 600);

        // With override, should use override fee + protocol fee with same formula
        let total_with_override = fees.calculate_swap_fees_pips(true, Some(1000));
        // 100 + 1000 - (100 * 1000 / 1_000_000) = 1100 - 0 = 1100 (rounded down)
        assert_eq!(total_with_override, 1100);
    }

    #[test]
    fn test_max_combined_fees_stays_valid() {
        // Test that even with max protocol + max LP fees, we stay under compute_swap_step limit
        let fees = UniswapV4Fees::new(1000, 1000, 1000);
        let total = fees.calculate_swap_fees_pips(true, Some(lp_fee::MAX_LP_FEE));

        // Using UniswapV4 formula: 1000 + 1000000 - (1000 * 1000000 / 1_000_000)
        // = 1001000 - 1000 = 1000000
        assert_eq!(total, 1_000_000);
    }

    #[test]
    fn test_get_limits_graceful_underflow() {
        // Verifies graceful handling of liquidity underflow in get_limits for V4
        let usv4_state = UniswapV4State::new(
            1000000,
            U256::from_str("79228162514264337593543950336").unwrap(), // 1:1 price
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 3000 },
            0,
            60,
            vec![
                // A tick with net_liquidity > current_liquidity
                // When zero_for_one=true, this gets negated and would cause underflow
                TickInfo {
                    index: -60,
                    net_liquidity: 2000000, // 2x current liquidity
                    sqrt_price: U256::from_str("79051508376726796163471739988").unwrap(),
                },
            ],
        )
        .unwrap();

        let usdc = usdc();
        let weth = weth();

        let (limit_in, limit_out) = usv4_state
            .get_limits(usdc.address.clone(), weth.address.clone())
            .unwrap();

        // Should return some conservative limits
        assert!(limit_in > BigUint::zero());
        assert!(limit_out > BigUint::zero());
    }

    // Tests based on Uniswap V4's ProtocolFeeLibrary.t.sol
    // See: https://github.com/Uniswap/v4-core/blob/main/test/libraries/ProtocolFeeLibrary.t.sol

    /// Maximum protocol fee in pips (1000 = 0.1%)
    const MAX_PROTOCOL_FEE: u32 = 1000;

    #[rstest]
    #[case::max_protocol_and_max_lp(MAX_PROTOCOL_FEE, lp_fee::MAX_LP_FEE, lp_fee::MAX_LP_FEE)]
    #[case::max_protocol_with_3000_lp(MAX_PROTOCOL_FEE, 3000, 3997)]
    #[case::max_protocol_with_zero_lp(MAX_PROTOCOL_FEE, 0, MAX_PROTOCOL_FEE)]
    #[case::zero_protocol_zero_lp(0, 0, 0)]
    #[case::zero_protocol_with_1000_lp(0, 1000, 1000)]
    fn test_calculate_swap_fees_uniswap_test_cases(
        #[case] protocol_fee: u32,
        #[case] lp_fee: u32,
        #[case] expected: u32,
    ) {
        let fees = UniswapV4Fees::new(protocol_fee, protocol_fee, lp_fee);
        let result = fees.calculate_swap_fees_pips(true, None);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_calculate_swap_fees_with_dynamic_fee() {
        // Test that dynamic fees default to 0 when no override is provided
        let fees = UniswapV4Fees::new(100, 90, lp_fee::DYNAMIC_FEE_FLAG);

        // Without override, dynamic fee should be treated as 0
        let total_zero_for_one = fees.calculate_swap_fees_pips(true, None);
        // 100 + 0 - (100 * 0 / 1_000_000) = 100
        assert_eq!(total_zero_for_one, 100);

        // With override, should use the override value
        let total_with_override = fees.calculate_swap_fees_pips(true, Some(500));
        // 100 + 500 - (100 * 500 / 1_000_000) = 600 - 0 = 600
        assert_eq!(total_with_override, 600);
    }

    #[test]
    fn test_calculate_swap_fees_direction_matters() {
        // Test that zero_for_one direction affects which protocol fee is used
        let fees = UniswapV4Fees::new(100, 200, 500);

        let zero_for_one_fee = fees.calculate_swap_fees_pips(true, None);
        // 100 + 500 - (100 * 500 / 1_000_000) = 600 - 0 = 600
        assert_eq!(zero_for_one_fee, 600);

        let one_for_zero_fee = fees.calculate_swap_fees_pips(false, None);
        // 200 + 500 - (200 * 500 / 1_000_000) = 700 - 0 = 700
        assert_eq!(one_for_zero_fee, 700);
    }

    #[rstest]
    #[case::high_lp_fee(1000, 500_000, 500_500)] // 1000 + 500k - 500 = 500.5k
    #[case::mid_fees(500, 500_000, 500_250)] // 500 + 500k - 250 = 500.25k
    #[case::low_fees(100, 100_000, 100_090)] // 100 + 100k - 10 = 100.09k
    fn test_calculate_swap_fees_formula_precision(
        #[case] protocol_fee: u32,
        #[case] lp_fee: u32,
        #[case] expected: u32,
    ) {
        // Test cases where the subtraction term (protocol * lp / 1M) significantly affects the
        // result
        let fees = UniswapV4Fees::new(protocol_fee, protocol_fee, lp_fee);
        let result = fees.calculate_swap_fees_pips(true, None);
        assert_eq!(result, expected, "Failed for protocol={}, lp={}", protocol_fee, lp_fee);
    }

    #[test]
    fn test_calculate_swap_fees_override_takes_precedence() {
        // Test that lp_fee_override completely replaces stored lp_fee
        let fees = UniswapV4Fees::new(100, 100, 3000);

        // With override, stored lp_fee should be ignored
        let result = fees.calculate_swap_fees_pips(true, Some(5000));
        // 100 + 5000 - (100 * 5000 / 1_000_000) = 5100 - 0 = 5100
        assert_eq!(result, 5100);

        // Without override, should use stored lp_fee
        let result_no_override = fees.calculate_swap_fees_pips(true, None);
        // 100 + 3000 - (100 * 3000 / 1_000_000) = 3100 - 0 = 3100
        assert_eq!(result_no_override, 3100);
    }

    #[test]
    fn test_calculate_swap_fees_zero_protocol_fee() {
        // When protocol fee is 0, formula simplifies to just lpFee
        let fees = UniswapV4Fees::new(0, 0, 3000);
        let result = fees.calculate_swap_fees_pips(true, None);
        // 0 + 3000 - (0 * 3000 / 1_000_000) = 3000
        assert_eq!(result, 3000);
    }

    #[test]
    fn test_calculate_swap_fees_zero_lp_fee() {
        // When lp fee is 0, formula simplifies to just protocolFee
        let fees = UniswapV4Fees::new(500, 500, 0);
        let result = fees.calculate_swap_fees_pips(true, None);
        // 500 + 0 - (500 * 0 / 1_000_000) = 500
        assert_eq!(result, 500);
    }

    // Helper to create a basic test pool for swap_to_price tests
    fn create_basic_v4_test_pool() -> UniswapV4State {
        let liquidity = 100_000_000_000_000_000_000u128; // 100e18
        let sqrt_price = get_sqrt_price_q96(U256::from(20_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        UniswapV4State::new(
            liquidity,
            sqrt_price,
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 3000 }, // 0.3% fee
            tick,
            60, // tick spacing
            ticks,
        )
        .expect("Failed to create pool")
    }

    fn create_tick_boundary_v4_test_pool() -> UniswapV4State {
        let sqrt_price = get_sqrt_ratio_at_tick(0).expect("Failed to calculate sqrt price");
        let ticks = vec![TickInfo::new(-120, 0).unwrap(), TickInfo::new(120, 0).unwrap()];

        UniswapV4State::new(
            100_000_000_000_000_000_000u128,
            sqrt_price,
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 3000 },
            0,
            60,
            ticks,
        )
        .expect("Failed to create pool")
    }

    #[test]
    fn test_partial_step_updates_tick_when_price_moves_without_crossing_initialized_tick() {
        let pool = create_tick_boundary_v4_test_pool();
        let amount = -I256::from_raw(U256::from(100_000_000_000_000_000u64));

        let result = pool
            .swap(true, amount, None, None)
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
        let mut pool = create_tick_boundary_v4_test_pool();
        pool.tick = -1;
        let amount = -I256::from_raw(U256::from(1u64));

        let result = pool
            .swap(true, amount, None, None)
            .expect("swap should consume the input as fee without moving price");

        assert_eq!(result.sqrt_price, pool.sqrt_price);
        assert_eq!(get_tick_at_sqrt_ratio(result.sqrt_price).unwrap(), 0);
        assert_eq!(result.tick, pool.tick);
    }

    #[test]
    fn test_swap_to_price_price_too_high() {
        let pool = create_basic_v4_test_pool();

        let token_x = token_x();
        let token_y = token_y();

        // Price far above pool price - should return zero
        let target_price = Price::new(BigUint::from(10_000_000u64), BigUint::from(1_000_000u64));

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
    fn test_swap_to_price_no_liquidity() {
        // Test that swap_to_price returns zero for pool with no liquidity
        let pool = UniswapV4State::new(
            0, // No liquidity
            U256::from_str("79228162514264337593543950336").unwrap(),
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 3000 },
            0,
            60,
            vec![],
        )
        .unwrap();

        let token_x = token_x();
        let token_y = token_y();

        let target_price = Price::new(BigUint::from(2_000_000u64), BigUint::from(1_000_000u64));

        let pool_swap = pool.query_pool_swap(&QueryPoolSwapParams::new(
            token_x,
            token_y,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));

        assert!(pool_swap.is_err());
    }

    #[test]
    fn test_swap_to_price_with_protocol_fees() {
        let liquidity = 100_000_000_000_000_000_000u128;
        let sqrt_price = get_sqrt_price_q96(U256::from(20_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        // Create pool with different protocol fees for each direction
        let pool = UniswapV4State::new(
            liquidity,
            sqrt_price,
            UniswapV4Fees {
                zero_for_one: 1000, // 0.1% protocol fee for zero_for_one
                one_for_zero: 200,  // 0.02% protocol fee for one_for_zero
                lp_fee: 3000,       // 0.3% LP fee
            },
            tick,
            60,
            ticks,
        )
        .expect("Failed to create pool");

        let token_x = token_x();
        let token_y = token_y();

        // Pool at 2.0 Y/X = 0.5 X/Y, swap_to_price moves price DOWN to target

        // Test zero_for_one direction (X -> Y, uses zero_for_one fee)
        let target_price = Price::new(BigUint::from(2_000_000u64), BigUint::from(1_010_000u64));
        let pool_swap_forward = pool
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

        // Test one_for_zero direction (Y -> X, uses one_for_zero fee)
        let target_price_reverse =
            Price::new(BigUint::from(1_010_000u64), BigUint::from(2_040_000u64));
        let pool_swap_backward = pool
            .query_pool_swap(&QueryPoolSwapParams::new(
                token_y,
                token_x,
                SwapConstraint::PoolTargetPrice {
                    target: target_price_reverse,
                    tolerance: 0f64,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .expect("swap_to_price failed");

        assert!(
            pool_swap_backward.amount_out().clone() > BigUint::ZERO,
            "One for zero swap should return non-zero output"
        );

        // Higher fees require more volume to reach the same price target
        // trade_zfo has 0.1% protocol fee, trade_ofz has 0.02% protocol fee
        assert!(
            pool_swap_forward.amount_out() < pool_swap_backward.amount_in(),
            "Backward fees should be lower therefore backward swap should be bigger"
        );
        assert!(
            pool_swap_forward.amount_in() < pool_swap_backward.amount_out(),
            "Backward fees should be lower therefore backward swap should be bigger"
        );
    }

    #[test]
    fn test_swap_to_price_different_targets() {
        // Test with various target prices using working format
        let pool = create_basic_v4_test_pool();

        let token_x = token_x();
        let token_y = token_y();

        // Pool at 2.0 Y/X (20M/10M)
        // Test 1: Target close to spot (1.98 Y/X)
        let target_price = Price::new(BigUint::from(2_000_000u64), BigUint::from(1_010_000u64));
        let pool_swap_close = pool
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
        assert!(
            *pool_swap_close.amount_out() > BigUint::ZERO,
            "Expected non-zero for 1.98 Y/X target"
        );

        // Test 2: Target further from spot (1.90 Y/X)
        let target_price = Price::new(BigUint::from(1_900_000u64), BigUint::from(1_000_000u64));
        let pool_swap_below = pool
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
        assert!(
            pool_swap_below.amount_out().clone() > BigUint::ZERO,
            "Expected non-zero for 1.90 Y/X target"
        );

        // Test 3: Target far from spot (1.5 Y/X)
        let target_price = Price::new(BigUint::from(1_500_000u64), BigUint::from(1_000_000u64));
        let pool_swap_far = pool
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
        assert!(
            pool_swap_far.amount_out().clone() > BigUint::ZERO,
            "Expected non-zero for 1.5 Y/X target"
        );

        // Verify that further targets require more volume
        assert!(
            pool_swap_close.amount_out().clone() < pool_swap_below.amount_out().clone(),
            "Closer target (1.98 Y/X) should require less volume than medium target (1.90 Y/X). \
             Got close: {}, medium: {}",
            pool_swap_close.amount_out().clone(),
            pool_swap_below.amount_out().clone()
        );
        assert!(
            pool_swap_below.amount_out().clone() < pool_swap_far.amount_out().clone(),
            "Medium target (1.90 Y/X) should require less volume than far target (1.5 Y/X). \
             Got medium: {}, far: {}",
            pool_swap_below.amount_out().clone(),
            pool_swap_far.amount_out().clone()
        );
    }

    #[test]
    fn test_swap_to_price_around_spot_price() {
        let liquidity = 10_000_000_000_000_000u128;
        let sqrt_price =
            get_sqrt_price_q96(U256::from(2_000_000_000u64), U256::from(1_000_000_000u64))
                .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        // Use FeeAmount::Low equivalent (500 pips = 0.05%)
        let pool = UniswapV4State::new(
            liquidity,
            sqrt_price,
            UniswapV4Fees {
                zero_for_one: 0,
                one_for_zero: 0,
                lp_fee: 500, // 0.05% to match V3 FeeAmount::Low
            },
            tick,
            60,
            ticks,
        )
        .expect("Failed to create pool");

        let token_x = token_x();
        let token_y = token_y();

        // Test 1: Price just above spot price, too little to cover fees
        let target_price = Price::new(BigUint::from(1_999_750u64), BigUint::from(1_000_250u64));

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

        // Test 2: Price far enough from spot prices to enable trading despite fees (0.1% lower)
        let target_price = Price::new(BigUint::from(1_999_000u64), BigUint::from(1_001_000u64));

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

        // Should match V3 output exactly with same fees
        let expected_amount_out =
            BigUint::from_str("7062236922008").expect("Failed to parse expected value");
        assert_eq!(
            pool_swap.amount_out().clone(),
            expected_amount_out,
            "V4 should match V3 output with same fees (0.05%)"
        );
    }

    #[test]
    fn test_swap_to_price_matches_get_amount_out() {
        let pool = create_basic_v4_test_pool();

        let token_x = token_x();
        let token_y = token_y();

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
        assert!(*pool_swap.amount_in() > BigUint::ZERO, "Amount in should be positive");

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
    fn test_swap_to_price_basic() {
        let liquidity = 100_000_000_000_000_000_000u128;
        let sqrt_price = get_sqrt_price_q96(U256::from(20_000_000u64), U256::from(10_000_000u64))
            .expect("Failed to calculate sqrt price");
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");

        let ticks = vec![TickInfo::new(0, 0).unwrap(), TickInfo::new(46080, 0).unwrap()];

        let pool = UniswapV4State::new(
            liquidity,
            sqrt_price,
            UniswapV4Fees {
                zero_for_one: 0,
                one_for_zero: 0,
                lp_fee: 3000, // 0.3% LP fee
            },
            tick,
            60,
            ticks,
        )
        .expect("Failed to create pool");

        let token_x = token_x();
        let token_y = token_y();

        // Target price: 2_000_000/1_010_000 ≈ 1.98 Y/X
        let target_price = Price::new(BigUint::from(2_000_000u64), BigUint::from(1_010_000u64));

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

        // Should match V3's output exactly with same fees (0.3%)
        let expected_amount_in = BigUint::from_str("246739021727519745").unwrap();
        let expected_amount_out = BigUint::from_str("490291909043340795").unwrap();

        assert_eq!(
            *pool_swap.amount_in(),
            expected_amount_in,
            "amount_in should match expected value"
        );
        assert_eq!(
            *pool_swap.amount_out(),
            expected_amount_out,
            "amount_out should match expected value"
        );
    }

    #[test]
    fn test_swap_price_limit_out_of_range_returns_error() {
        let pool = create_basic_v4_test_pool();
        let amount = -I256::from_raw(U256::from(1000u64)); // V4 uses negative for exact input

        // zero_for_one: price_limit equal to sqrt_price is invalid (must be strictly less)
        let result = pool.swap(true, amount, Some(pool.sqrt_price), None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        // zero_for_one: price_limit at MIN_SQRT_RATIO is invalid (must be strictly greater)
        let result = pool.swap(true, amount, Some(MIN_SQRT_RATIO), None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        // one_for_zero: price_limit equal to sqrt_price is invalid (must be strictly greater)
        let result = pool.swap(false, amount, Some(pool.sqrt_price), None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));

        // one_for_zero: price_limit at MAX_SQRT_RATIO is invalid (must be strictly less)
        let result = pool.swap(false, amount, Some(MAX_SQRT_RATIO), None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn test_swap_at_extreme_price_returns_error() {
        // Simulates the depth calculation scenario: pool sqrt_price is at MIN_SQRT_RATIO + 1,
        // so the default price limit for zero_for_one equals sqrt_price and fails validation.
        let sqrt_price = MIN_SQRT_RATIO + U256::from(1u64);
        let tick = get_tick_at_sqrt_ratio(sqrt_price).expect("Failed to calculate tick");
        // tick_spacing 60; ticks must be aligned
        let aligned_tick = (MIN_TICK / 60) * 60 + 60; // first multiple of 60 above MIN_TICK
        let ticks = vec![
            TickInfo::new(aligned_tick, 0).unwrap(),
            TickInfo::new(aligned_tick + 60, 0).unwrap(),
        ];
        let pool = UniswapV4State::new(
            100_000_000_000_000_000_000u128,
            sqrt_price,
            UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 3000 },
            tick,
            60,
            ticks,
        )
        .unwrap();

        let amount = -I256::from_raw(U256::from(1000u64));
        // Default price limit for zero_for_one is MIN_SQRT_RATIO + 1 == sqrt_price, so invalid
        let result = pool.swap(true, amount, None, None);
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }
}
