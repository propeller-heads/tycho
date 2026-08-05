use std::{any::Any, collections::HashMap};

use alloy::primitives::{Sign, I256, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
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
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    utils::{
        add_fee_markup,
        uniswap::{
            liquidity_math,
            sqrt_price_math::{get_amount0_delta, get_amount1_delta, sqrt_price_q96_to_f64},
            swap_math,
            tick_list::{TickInfo, TickList},
            tick_math::{
                get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK,
                MIN_SQRT_RATIO, MIN_TICK,
            },
            StepComputation, SwapState,
        },
    },
};

const TOKENS_DECIMALS_PRECISION: u8 = 9;
const GAS_BASE: u64 = 155_000;
const GAS_CROSS_INIT_TICK: u64 = 21_492;
const MAX_SQRT_PRICE_CHANGE_PERCENTAGE: u64 = 2_000_000_000;
const MIN_SQRT_PRICE_CHANGE_PERCENTAGE: u64 = 5;
const DEFAULT_EXPONENT_SIZE: u32 = 8;
const DEFAULT_EXPONENT_MASK: u64 = 0xFF;
const ROUNDING_FACTOR: u64 = 1_000_000_000;
const ROUNDING_FACTOR_MINUS_ONE: u64 = ROUNDING_FACTOR - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DexType {
    D3,
    D4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FluidV2State {
    dex_id: Bytes,
    token0: Token,
    token1: Token,
    dex_type: DexType,
    fee: u32,
    dynamic_fee: bool,
    tick_spacing: i32,
    controller: Bytes,
    dex_variables: DexVariables,
    dex_variables2: DexVariables2,
    token0_reserve: U256,
    token1_reserve: U256,
    token0_borrow_exchange_price: U256,
    token0_supply_exchange_price: U256,
    token1_borrow_exchange_price: U256,
    token1_supply_exchange_price: U256,
    ticks: Vec<crate::evm::protocol::utils::uniswap::tick_list::TickInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DexVariables {
    pub current_tick: i32,
    pub current_sqrt_price_x96: U256,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DexVariables2 {
    pub protocol_fee_0_to_1: U256,
    pub protocol_fee_1_to_0: U256,
    pub protocol_cut_fee: U256,
    pub token0_decimals: u8,
    pub token1_decimals: u8,
    pub active_liquidity: U256,
    pub pool_accounting_flag: bool,
    pub fetch_dynamic_fee_flag: bool,
    pub fee_version: U256,
    pub lp_fee: U256,
    pub max_decay_time: U256,
    pub price_impact_to_fee_division_factor: U256,
    pub min_fee: U256,
    pub max_fee: U256,
    pub net_price_impact: i64,
    pub last_update_timestamp: U256,
    pub decay_time_remaining: U256,
}

#[derive(Debug, Clone)]
struct SwapInCalculatedVars {
    token_out_num_precision: U256,
    token_out_den_precision: U256,
    token_out_exchange_price: U256,
}

#[derive(Debug, Clone)]
struct SwapInPrepared {
    zero_for_one: bool,
    amount_in_raw_adjusted: U256,
    vars: SwapInCalculatedVars,
}

#[derive(Debug, Clone)]
struct SwapInInternalResult {
    amount_out_raw_adjusted: U256,
    amount_remaining: I256,
    sqrt_price: U256,
    liquidity: u128,
    tick: i32,
    gas_used: U256,
}

impl FluidV2State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dex_id: Bytes,
        token0: Token,
        token1: Token,
        dex_type: DexType,
        fee: u32,
        dynamic_fee: bool,
        tick_spacing: i32,
        controller: Bytes,
        dex_variables: DexVariables,
        dex_variables2: DexVariables2,
        token0_reserve: U256,
        token1_reserve: U256,
        token0_borrow_exchange_price: U256,
        token0_supply_exchange_price: U256,
        token1_borrow_exchange_price: U256,
        token1_supply_exchange_price: U256,
        ticks: Vec<TickInfo>,
    ) -> Self {
        FluidV2State {
            dex_id,
            token0,
            token1,
            dex_type,
            fee,
            dynamic_fee,
            tick_spacing,
            controller,
            dex_variables,
            dex_variables2,
            token0_reserve,
            token1_reserve,
            token0_borrow_exchange_price,
            token0_supply_exchange_price,
            token1_borrow_exchange_price,
            token1_supply_exchange_price,
            ticks,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn swap_in(
        &self,
        ticks: &TickList,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: U256,
        protocol_fee_pips: u32,
        lp_fee_pips: u32,
        liquidity: u128,
        tick: i32,
        sqrt_price: U256,
    ) -> Result<SwapInInternalResult, SimulationError> {
        if liquidity == 0 {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }

        if zero_for_one {
            if sqrt_price_limit <= MIN_SQRT_RATIO || sqrt_price_limit >= sqrt_price {
                return Err(SimulationError::InvalidInput(
                    "Invalid sqrt price limit".to_string(),
                    None,
                ));
            }
        } else if sqrt_price_limit >= MAX_SQRT_RATIO || sqrt_price_limit <= sqrt_price {
            return Err(SimulationError::InvalidInput("Invalid sqrt price limit".to_string(), None));
        }

        let exact_input = amount_specified > I256::from_raw(U256::from(0u64));
        let mut state = SwapState {
            amount_remaining: amount_specified,
            amount_calculated: I256::from_raw(U256::from(0u64)),
            sqrt_price,
            tick,
            liquidity,
        };
        let mut cross_init_tick_loops: u64 = 0;
        let mut amount_out_raw = U256::from(0u64);
        let mut protocol_fee_accrued_raw = U256::from(0u64);
        let mut lp_fee_accrued_raw = U256::from(0u64);

        let protocol_cut_fee = self
            .dex_variables2
            .protocol_cut_fee
            .to::<u32>();
        let fee_version = self
            .dex_variables2
            .fee_version
            .to::<u32>();
        let mut constant_lp_fee_pips = lp_fee_pips;

        // Keep the same high-level fee-version split as the Solidity module.
        // For now we only simulate the static-fee shape and explicitly leave
        // dynamic-fee/controller overrides out of scope.
        // TODO: For `fee_version == 1`, apply dynamic-fee variables and controller override logic.
        if fee_version == 1 {
            if self
                .dex_variables2
                .price_impact_to_fee_division_factor ==
                U256::from(0u64)
            {
                constant_lp_fee_pips = self.dex_variables2.min_fee.to::<u32>();
            } else {
                // TODO: Implement the dynamic-fee path. For now, fall back to `min_fee`.
                constant_lp_fee_pips = self.dex_variables2.min_fee.to::<u32>();
            }
        }
        // TODO: If `fetch_dynamic_fee_flag` is enabled and controller != swapper,
        // override `constant_lp_fee_pips` with `fetchDynamicFeeForSwapIn`.

        let active_liquidity_start = state.liquidity;
        let mut sqrt_price_start = state.sqrt_price;
        let mut sqrt_price_start_changed = false;

        while state.amount_remaining != I256::from_raw(U256::from(0u64)) &&
            state.sqrt_price != sqrt_price_limit
        {
            let (mut next_tick, initialized) = ticks
                .next_initialized_tick_within_one_word(state.tick, zero_for_one)
                .map_err(|err| match err.kind {
                    crate::evm::protocol::utils::uniswap::tick_list::TickListErrorKind::TicksExeeded => {
                        SimulationError::InvalidInput("Ticks exceeded".to_string(), None)
                    }
                    _ => SimulationError::FatalError("Unknown tick list error".to_string()),
                })?;

            next_tick = next_tick.clamp(MIN_TICK, MAX_TICK);
            let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;
            // Stored sqrt price is lossy-compressed; when that creates a tiny directional
            // anomaly, align to the tick price as long as the deviation stays within 0.01%.
            if (zero_for_one && sqrt_price_next > state.sqrt_price) ||
                (!zero_for_one && sqrt_price_next < state.sqrt_price)
            {
                let diff = if sqrt_price_next > state.sqrt_price {
                    safe_sub_u256(sqrt_price_next, state.sqrt_price)?
                } else {
                    safe_sub_u256(state.sqrt_price, sqrt_price_next)?
                };
                if safe_mul_u256(diff, pow10_u256(4))? > state.sqrt_price {
                    return Err(SimulationError::InvalidInput(
                        "Sqrt price deviation too high".to_string(),
                        None,
                    ));
                }
                state.sqrt_price = sqrt_price_next;
            }
            let (sqrt_price_next, amount_in, amount_out, _fee_amount) =
                swap_math::compute_swap_step(
                    state.sqrt_price,
                    FluidV2State::get_sqrt_ratio_target(
                        sqrt_price_next,
                        sqrt_price_limit,
                        zero_for_one,
                    ),
                    state.liquidity,
                    state.amount_remaining,
                    0, // TODO: Only constant LP fee is currently supported here.
                )?;

            let step = StepComputation {
                sqrt_price_start: state.sqrt_price,
                tick_next: next_tick,
                initialized,
                sqrt_price_next,
                amount_in,
                amount_out,
                fee_amount: U256::from(0u64),
            };

            state.sqrt_price = sqrt_price_next;
            if exact_input {
                let mut step_amount_out = step.amount_out;
                let mut step_protocol_fee = U256::from(0u64);
                let mut step_lp_fee = U256::from(0u64);

                // In swap-in, both protocol fee and LP fee are taken from tokenOut,
                // matching the contract's exact-input flow.
                if step_amount_out > U256::from(0u64) && protocol_fee_pips > 0 {
                    let numerator = safe_add_u256(
                        safe_mul_u256(step_amount_out, U256::from(protocol_fee_pips))?,
                        U256::from(1u64),
                    )?;
                    step_protocol_fee = safe_add_u256(
                        safe_div_u256(numerator, U256::from(1_000_000u64))?,
                        U256::from(1u64),
                    )?;
                }
                if step_amount_out > step_protocol_fee {
                    step_amount_out = safe_sub_u256(step_amount_out, step_protocol_fee)?;
                } else {
                    step_amount_out = U256::from(0u64);
                }

                if step_amount_out > U256::from(0u64) && constant_lp_fee_pips > 0 {
                    let numerator = safe_add_u256(
                        safe_mul_u256(step_amount_out, U256::from(constant_lp_fee_pips))?,
                        U256::from(1u64),
                    )?;
                    step_lp_fee = safe_add_u256(
                        safe_div_u256(numerator, U256::from(1_000_000u64))?,
                        U256::from(1u64),
                    )?;
                }
                if step_amount_out > step_lp_fee {
                    step_amount_out = safe_sub_u256(step_amount_out, step_lp_fee)?;
                } else {
                    step_amount_out = U256::from(0u64);
                }

                if step_lp_fee > U256::from(0u64) && protocol_cut_fee > 0 {
                    let numerator = safe_add_u256(
                        safe_mul_u256(step_lp_fee, U256::from(protocol_cut_fee))?,
                        U256::from(1u64),
                    )?;
                    let step_protocol_cut = safe_add_u256(
                        safe_div_u256(numerator, U256::from(100u64))?,
                        U256::from(1u64),
                    )?;
                    if step_lp_fee > step_protocol_cut {
                        step_lp_fee = safe_sub_u256(step_lp_fee, step_protocol_cut)?;
                        step_protocol_fee = safe_add_u256(step_protocol_fee, step_protocol_cut)?;
                    } else {
                        step_protocol_fee = safe_add_u256(step_protocol_fee, step_lp_fee)?;
                        step_lp_fee = U256::from(0u64);
                    }
                }

                state.amount_remaining -=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_in).unwrap();
                state.amount_calculated -=
                    I256::checked_from_sign_and_abs(Sign::Positive, step_amount_out).unwrap();

                amount_out_raw = safe_add_u256(amount_out_raw, step_amount_out)?;
                protocol_fee_accrued_raw =
                    safe_add_u256(protocol_fee_accrued_raw, step_protocol_fee)?;
                lp_fee_accrued_raw = safe_add_u256(lp_fee_accrued_raw, step_lp_fee)?;
            } else {
                // This branch mirrors the exact-output math shape from the shared Uniswap
                // helper, but the public simulator currently exposes only exact-input quotes.
                let amount_in_with_lp = safe_div_u256(
                    safe_mul_u256(step.amount_in, U256::from(1_000_000u64))?,
                    safe_sub_u256(U256::from(1_000_000u64), U256::from(constant_lp_fee_pips))?,
                )?;
                let amount_in_with_fees = safe_div_u256(
                    safe_mul_u256(amount_in_with_lp, U256::from(1_000_000u64))?,
                    safe_sub_u256(U256::from(1_000_000u64), U256::from(protocol_fee_pips))?,
                )?;

                state.amount_remaining +=
                    I256::checked_from_sign_and_abs(Sign::Positive, step.amount_out).unwrap();
                state.amount_calculated +=
                    I256::checked_from_sign_and_abs(Sign::Positive, amount_in_with_fees).unwrap();
            }

            // TODO: Update `feeGrowthGlobal` and per-tick `feeGrowthOutside`.

            // If we reached the next price target, apply tick crossing updates.
            if state.sqrt_price == step.sqrt_price_next {
                if step.initialized {
                    let liquidity_raw = ticks
                        .get_tick(step.tick_next)
                        .unwrap()
                        .net_liquidity;
                    let liquidity_net = if zero_for_one { -liquidity_raw } else { liquidity_raw };
                    state.liquidity =
                        liquidity_math::add_liquidity_delta(state.liquidity, liquidity_net)?;
                    cross_init_tick_loops += 1;

                    if active_liquidity_start == 0 &&
                        !sqrt_price_start_changed &&
                        state.liquidity > 0
                    {
                        sqrt_price_start = state.sqrt_price;
                        sqrt_price_start_changed = true;
                    }
                }
                state.tick = if zero_for_one { step.tick_next - 1 } else { step.tick_next };
            } else if state.sqrt_price != step.sqrt_price_start {
                state.tick = get_tick_at_sqrt_ratio(state.sqrt_price)?;
            }
        }

        verify_sqrt_price_change_limits(sqrt_price_start, state.sqrt_price)?;
        // TODO: For `fee_version == 1`, update dynamic-fee variables from final price impact.

        let gas_used = U256::from(GAS_BASE) +
            U256::from(GAS_CROSS_INIT_TICK.saturating_mul(cross_init_tick_loops));

        Ok(SwapInInternalResult {
            amount_out_raw_adjusted: amount_out_raw,
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
        let cond = if zero_for_one {
            sqrt_price_next < sqrt_price_limit
        } else {
            sqrt_price_next > sqrt_price_limit
        };
        if cond {
            sqrt_price_limit
        } else {
            sqrt_price_next
        }
    }

    fn prepare_swap_in(
        &self,
        amount_in: U256,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<SwapInPrepared, SimulationError> {
        let zero_for_one = if token_in.address == self.token0.address {
            true
        } else if token_in.address == self.token1.address {
            false
        } else {
            return Err(SimulationError::InvalidInput(
                "Token in not part of pool".to_string(),
                None,
            ));
        };

        if !(token_out.address == self.token0.address || token_out.address == self.token1.address) {
            return Err(SimulationError::InvalidInput(
                "Token out not part of pool".to_string(),
                None,
            ));
        }

        if amount_in < pow10_u256(4) || amount_in > max_x(128) {
            return Err(SimulationError::InvalidInput("Amount out of limits".to_string(), None));
        }

        let (token_in_decimals, token_out_decimals) = if zero_for_one {
            (self.dex_variables2.token0_decimals, self.dex_variables2.token1_decimals)
        } else {
            (self.dex_variables2.token1_decimals, self.dex_variables2.token0_decimals)
        };

        let (token_in_num_precision, token_in_den_precision) =
            calculate_precisions(token_in_decimals);
        let (token_out_num_precision, token_out_den_precision) =
            calculate_precisions(token_out_decimals);

        let (token_in_exchange_price, token_out_exchange_price) = if zero_for_one {
            match self.dex_type {
                DexType::D3 => {
                    (self.token0_supply_exchange_price, self.token1_supply_exchange_price)
                }
                DexType::D4 => {
                    (self.token0_borrow_exchange_price, self.token1_borrow_exchange_price)
                }
            }
        } else {
            match self.dex_type {
                DexType::D3 => {
                    (self.token1_supply_exchange_price, self.token0_supply_exchange_price)
                }
                DexType::D4 => {
                    (self.token1_borrow_exchange_price, self.token0_borrow_exchange_price)
                }
            }
        };

        // Convert token amount into the pool's raw-adjusted accounting unit using the
        // exchange price, then round down so the simulator stays on the protocol side.
        let amount_in_raw_adjusted = amount_to_adjusted(
            amount_in,
            token_in_num_precision,
            token_in_den_precision,
            token_in_exchange_price,
        )?;
        let amount_in_raw_adjusted = round_down_raw(amount_in_raw_adjusted);
        if amount_in_raw_adjusted < pow10_u256(4) || amount_in_raw_adjusted > max_x(86) {
            return Err(SimulationError::InvalidInput("Adjusted amount limits".to_string(), None));
        }

        Ok(SwapInPrepared {
            zero_for_one,
            amount_in_raw_adjusted,
            vars: SwapInCalculatedVars {
                token_out_num_precision,
                token_out_den_precision,
                token_out_exchange_price,
            },
        })
    }

    fn execute_swap_in_internal(
        &self,
        prepared: &SwapInPrepared,
    ) -> Result<SwapInInternalResult, SimulationError> {
        if self.tick_spacing <= 0 {
            return Err(SimulationError::FatalError("Tick spacing must be positive".to_string()));
        }

        let tick_list = TickList::from(self.tick_spacing as u16, self.ticks.clone())?;
        let (tick_min, tick_max) = match (self.ticks.first(), self.ticks.last()) {
            (Some(min), Some(max)) => (min.index, max.index),
            _ => return Err(SimulationError::FatalError("No ticks available".to_string())),
        };
        // Like the Solidity module, push the limit one unit inside the boundary tick so the
        // swap can consume liquidity up to the last reachable initialized range.
        let tick_limit = if prepared.zero_for_one { tick_min } else { tick_max };
        let mut sqrt_price_limit = get_sqrt_ratio_at_tick(tick_limit)?;
        sqrt_price_limit = if prepared.zero_for_one {
            safe_add_u256(sqrt_price_limit, U256::from(1u64))?
        } else {
            safe_sub_u256(sqrt_price_limit, U256::from(1u64))?
        };

        let amount_specified =
            I256::checked_from_sign_and_abs(Sign::Positive, prepared.amount_in_raw_adjusted)
                .ok_or_else(|| {
                    SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
                })?;

        let (protocol_fee_pips, lp_fee_pips) = if prepared.zero_for_one {
            (
                self.dex_variables2
                    .protocol_fee_0_to_1
                    .to::<u32>(),
                self.dex_variables2.lp_fee.to::<u32>(),
            )
        } else {
            (
                self.dex_variables2
                    .protocol_fee_1_to_0
                    .to::<u32>(),
                self.dex_variables2.lp_fee.to::<u32>(),
            )
        };

        self.swap_in(
            &tick_list,
            prepared.zero_for_one,
            amount_specified,
            sqrt_price_limit,
            protocol_fee_pips,
            lp_fee_pips,
            self.dex_variables2
                .active_liquidity
                .to::<u128>(),
            self.dex_variables.current_tick,
            decode_sqrt_price(
                self.dex_variables
                    .current_sqrt_price_x96,
                prepared.zero_for_one,
            ),
        )
    }

    fn effective_fee(&self, zero_for_one: bool) -> f64 {
        let lp_fee_pips = if self.dynamic_fee {
            // TODO: Dynamic fee is not fully supported yet.
            self.dex_variables2.min_fee.to::<u32>()
        } else {
            self.dex_variables2.lp_fee.to::<u32>()
        };
        let protocol_fee_pips = if zero_for_one {
            self.dex_variables2
                .protocol_fee_0_to_1
                .to::<u32>()
        } else {
            self.dex_variables2
                .protocol_fee_1_to_0
                .to::<u32>()
        };

        let protocol_fee = protocol_fee_pips as f64 / 1_000_000.0;
        let lp_fee = lp_fee_pips as f64 / 1_000_000.0;
        1.0 - ((1.0 - protocol_fee) * (1.0 - lp_fee))
    }
}

impl DexVariables {
    pub fn from_packed(value: U256) -> Self {
        let sign_is_positive = (value & U256::from(1u64)) == U256::from(1u64);
        let abs_tick = ((value >> 1u32) & u256_mask(19)).to::<u32>();
        let current_tick = if abs_tick == 0 {
            0
        } else if sign_is_positive {
            abs_tick as i32
        } else {
            -(abs_tick as i32)
        };

        let current_sqrt_price_x96 = (value >> 20u32) & u256_mask(72);
        // Reserved for future support of packed fee-growth fields:
        // let fee_growth_global0_x102 = (value >> 92u32) & u256_mask(82);
        // let fee_growth_global1_x102 = (value >> 174u32) & u256_mask(82);

        Self {
            current_tick,
            current_sqrt_price_x96,
            // fee_growth_global0_x102,
            // fee_growth_global1_x102,
        }
    }
}

impl DexVariables2 {
    pub fn from_packed(value: U256) -> Self {
        let protocol_fee_0_to_1 = value & u256_mask(12);
        let protocol_fee_1_to_0 = (value >> 12u32) & u256_mask(12);
        let protocol_cut_fee = (value >> 24u32) & u256_mask(6);

        let token0_decimals_raw = ((value >> 30u32) & u256_mask(4)).to::<u8>();
        let token1_decimals_raw = ((value >> 34u32) & u256_mask(4)).to::<u8>();
        let token0_decimals = if token0_decimals_raw == 15 { 18 } else { token0_decimals_raw };
        let token1_decimals = if token1_decimals_raw == 15 { 18 } else { token1_decimals_raw };

        let active_liquidity = (value >> 38u32) & u256_mask(102);
        let pool_accounting_flag = ((value >> 140u32) & U256::from(1u64)) == U256::from(1u64);
        let fetch_dynamic_fee_flag = ((value >> 141u32) & U256::from(1u64)) == U256::from(1u64);
        let fee_version = (value >> 152u32) & u256_mask(4);

        // Bits [156:] are interpreted differently based on fee_version.
        // fee_version 0: [156:171] = lp_fee (16 bits), remaining bits unused.
        // fee_version 1+: [156:167] = max_decay_time (12 bits),
        //   [168:175] = price_impact_to_fee_division_factor, [176:191] = min_fee,
        //   [192:207] = max_fee, [208] = net_price_impact sign, [209:228] = magnitude,
        //   [229:243] = last_update_timestamp, [244:255] = decay_time_remaining.
        let (
            lp_fee,
            max_decay_time,
            price_impact_to_fee_division_factor,
            min_fee,
            max_fee,
            net_price_impact,
            last_update_timestamp,
            decay_time_remaining,
        ) = if fee_version == U256::ZERO {
            let lp_fee = (value >> 156u32) & u256_mask(16);
            (lp_fee, U256::ZERO, U256::ZERO, U256::ZERO, U256::ZERO, 0i64, U256::ZERO, U256::ZERO)
        } else {
            let max_decay_time = (value >> 156u32) & u256_mask(12);
            let price_impact_to_fee_division_factor = (value >> 168u32) & u256_mask(8);
            let min_fee = (value >> 176u32) & u256_mask(16);
            let max_fee = (value >> 192u32) & u256_mask(16);

            let net_price_impact_sign = ((value >> 208u32) & U256::from(1u64)) == U256::from(1u64);
            let net_price_impact_abs = ((value >> 209u32) & u256_mask(20)).to::<u32>() as i64;
            let net_price_impact = if net_price_impact_abs == 0 {
                0
            } else if net_price_impact_sign {
                net_price_impact_abs
            } else {
                -net_price_impact_abs
            };

            let last_update_timestamp = (value >> 229u32) & u256_mask(15);
            let decay_time_remaining = (value >> 244u32) & u256_mask(12);

            (
                U256::ZERO,
                max_decay_time,
                price_impact_to_fee_division_factor,
                min_fee,
                max_fee,
                net_price_impact,
                last_update_timestamp,
                decay_time_remaining,
            )
        };

        Self {
            protocol_fee_0_to_1,
            protocol_fee_1_to_0,
            protocol_cut_fee,
            token0_decimals,
            token1_decimals,
            active_liquidity,
            pool_accounting_flag,
            fetch_dynamic_fee_flag,
            fee_version,
            lp_fee,
            max_decay_time,
            price_impact_to_fee_division_factor,
            min_fee,
            max_fee,
            net_price_impact,
            last_update_timestamp,
            decay_time_remaining,
        }
    }
}

fn u256_mask(bits: u32) -> U256 {
    if bits == 0 {
        return U256::from(0u64);
    }
    (U256::from(1u64) << bits) - U256::from(1u64)
}

fn pow10_u256(exp: u8) -> U256 {
    U256::from(10u64).pow(U256::from(exp as u64))
}

fn calculate_precisions(decimals: u8) -> (U256, U256) {
    if decimals > TOKENS_DECIMALS_PRECISION {
        (U256::from(1u64), pow10_u256(decimals - TOKENS_DECIMALS_PRECISION))
    } else {
        (pow10_u256(TOKENS_DECIMALS_PRECISION - decimals), U256::from(1u64))
    }
}

fn amount_to_adjusted(
    amount: U256,
    token_numerator_precision: U256,
    token_denominator_precision: U256,
    exchange_price: U256,
) -> Result<U256, SimulationError> {
    // Matches the contract's conversion between external token units and the
    // internal raw-adjusted accounting unit (1e12 exchange-price precision).
    let scaled = safe_mul_u256(amount, pow10_u256(12))?;
    let scaled = safe_mul_u256(scaled, token_numerator_precision)?;
    let denominator = safe_mul_u256(exchange_price, token_denominator_precision)?;
    safe_div_u256(scaled, denominator)
}

fn adjusted_to_amount(
    adjusted: U256,
    token_numerator_precision: U256,
    token_denominator_precision: U256,
    exchange_price: U256,
) -> Result<U256, SimulationError> {
    let scaled = safe_mul_u256(adjusted, token_denominator_precision)?;
    let scaled = safe_mul_u256(scaled, exchange_price)?;
    let denominator = safe_mul_u256(token_numerator_precision, pow10_u256(12))?;
    safe_div_u256(scaled, denominator)
}

fn round_down_raw(amount: U256) -> U256 {
    // Same conservative round-down used by swapIn entrypoint:
    // amount * (ROUNDING_FACTOR - 1) / ROUNDING_FACTOR, then subtract 1 if non-zero.
    let scaled = amount.saturating_mul(U256::from(ROUNDING_FACTOR_MINUS_ONE));
    let mut out = if ROUNDING_FACTOR == 0 { amount } else { scaled / U256::from(ROUNDING_FACTOR) };
    if out > U256::from(0u64) {
        out = out.saturating_sub(U256::from(1u64));
    }
    out
}

// `sqrtPriceX96` is rounded down when stored.
// For a 1->0 swap (price-increasing direction), this may slightly favor the swapper.
// To keep the protocol conservative, round up by adding 1 to the coefficient when needed.
fn decode_sqrt_price(raw: U256, swap0_to_1: bool) -> U256 {
    let mut coeff = raw >> DEFAULT_EXPONENT_SIZE;
    let exp = (raw & U256::from(DEFAULT_EXPONENT_MASK)).to::<u32>();
    if exp > 0 && !swap0_to_1 {
        coeff += U256::from(1u64)
    }
    coeff << exp
}

fn encode_big_number(value: U256, coeff_bits: u32) -> U256 {
    if value.is_zero() {
        return U256::from(0u64);
    }
    let max_coeff = (U256::from(1u64) << coeff_bits) - U256::from(1u64);
    let mut coeff = value;
    let mut exp: u32 = 0;
    while coeff > max_coeff && exp < 255 {
        coeff >>= 1u32;
        exp += 1;
    }
    (coeff << DEFAULT_EXPONENT_SIZE) | U256::from(exp as u64)
}

fn max_x(bits: u32) -> U256 {
    (U256::from(1u64) << bits) - U256::from(1u64)
}

fn verify_sqrt_price_change_limits(
    sqrt_price_start: U256,
    sqrt_price_end: U256,
) -> Result<(), SimulationError> {
    let diff = if sqrt_price_end > sqrt_price_start {
        safe_sub_u256(sqrt_price_end, sqrt_price_start)?
    } else {
        safe_sub_u256(sqrt_price_start, sqrt_price_end)?
    };
    let percentage_change = safe_div_u256(safe_mul_u256(diff, pow10_u256(10))?, sqrt_price_start)?;
    if percentage_change > U256::from(MAX_SQRT_PRICE_CHANGE_PERCENTAGE) ||
        percentage_change < U256::from(MIN_SQRT_PRICE_CHANGE_PERCENTAGE)
    {
        return Err(SimulationError::InvalidInput(
            "Sqrt price change out of bounds".to_string(),
            None,
        ));
    }
    Ok(())
}

#[typetag::serde]
impl ProtocolSim for FluidV2State {
    fn fee(&self) -> f64 {
        // Directional fee selection is not exposed by the trait; default to token0 -> token1.
        self.effective_fee(true)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let zero_for_one =
            if base.address == self.token0.address && quote.address == self.token1.address {
                true
            } else if base.address == self.token1.address && quote.address == self.token0.address {
                false
            } else {
                return Err(SimulationError::InvalidInput(
                    "Token pair not part of pool".to_string(),
                    None,
                ));
            };

        let (token_in_decimals, token_out_decimals) = if zero_for_one {
            (self.dex_variables2.token0_decimals, self.dex_variables2.token1_decimals)
        } else {
            (self.dex_variables2.token1_decimals, self.dex_variables2.token0_decimals)
        };
        let (token_in_num_precision, token_in_den_precision) =
            calculate_precisions(token_in_decimals);
        let (token_out_num_precision, token_out_den_precision) =
            calculate_precisions(token_out_decimals);

        let (exchange_price_in, exchange_price_out) = if zero_for_one {
            match self.dex_type {
                DexType::D3 => {
                    (self.token0_supply_exchange_price, self.token1_supply_exchange_price)
                }
                DexType::D4 => {
                    (self.token0_borrow_exchange_price, self.token1_borrow_exchange_price)
                }
            }
        } else {
            match self.dex_type {
                DexType::D3 => {
                    (self.token1_supply_exchange_price, self.token0_supply_exchange_price)
                }
                DexType::D4 => {
                    (self.token1_borrow_exchange_price, self.token0_borrow_exchange_price)
                }
            }
        };

        let sqrt_price = decode_sqrt_price(
            self.dex_variables
                .current_sqrt_price_x96,
            zero_for_one,
        );
        let adjusted_price = if zero_for_one {
            sqrt_price_q96_to_f64(sqrt_price, token_in_decimals as u32, token_out_decimals as u32)?
        } else {
            1.0f64 /
                sqrt_price_q96_to_f64(
                    sqrt_price,
                    token_out_decimals as u32,
                    token_in_decimals as u32,
                )?
        };

        let scale_num = u256_to_f64(token_in_num_precision)? *
            u256_to_f64(token_out_den_precision)? *
            u256_to_f64(exchange_price_out)?;
        let scale_den = u256_to_f64(exchange_price_in)? *
            u256_to_f64(token_in_den_precision)? *
            u256_to_f64(token_out_num_precision)?;
        let price = adjusted_price * (scale_num / scale_den);

        let effective_fee = self.effective_fee(zero_for_one);
        Ok(add_fee_markup(price, effective_fee))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::from(0u32) {
            return Ok(GetAmountOutResult::new(
                BigUint::from(0u32),
                BigUint::from(0u32),
                Box::new(self.clone()),
            ));
        }

        if !self.controller.is_zero() {
            return Err(SimulationError::InvalidInput(
                "Controller pools are not supported".to_string(),
                None,
            ));
        }

        let amount_in_u256 = biguint_to_u256(&amount_in);
        let prepared = self.prepare_swap_in(amount_in_u256, token_in, token_out)?;
        let internal = self.execute_swap_in_internal(&prepared)?;

        if internal.amount_remaining > I256::from_raw(U256::from(0u64)) {
            return Err(SimulationError::InvalidInput("Next tick out of bounds".to_string(), None));
        }

        // Keep the internal raw output for state updates. The extra round-down below only
        // affects the user-visible amount, just like the Solidity entrypoint.
        let amount_out_raw_adjusted_for_state = internal.amount_out_raw_adjusted;
        if amount_out_raw_adjusted_for_state < pow10_u256(4) ||
            amount_out_raw_adjusted_for_state > max_x(86)
        {
            return Err(SimulationError::InvalidInput("Adjusted amount limits".to_string(), None));
        }

        let amount_out_raw_adjusted = round_down_raw(amount_out_raw_adjusted_for_state);
        if amount_out_raw_adjusted < pow10_u256(4) || amount_out_raw_adjusted > max_x(86) {
            return Err(SimulationError::InvalidInput("Adjusted amount limits".to_string(), None));
        }
        let mut amount_out = adjusted_to_amount(
            amount_out_raw_adjusted,
            prepared.vars.token_out_num_precision,
            prepared.vars.token_out_den_precision,
            prepared.vars.token_out_exchange_price,
        )?;
        if amount_out > U256::from(0u64) {
            amount_out = safe_sub_u256(amount_out, U256::from(1u64))?;
        }
        if amount_out < pow10_u256(4) || amount_out > max_x(128) {
            return Err(SimulationError::InvalidInput("Amount out of limits".to_string(), None));
        }
        let mut new_state = self.clone();
        new_state.dex_variables.current_tick = internal.tick;
        new_state
            .dex_variables
            .current_sqrt_price_x96 = encode_big_number(internal.sqrt_price, 64);
        new_state
            .dex_variables2
            .active_liquidity = U256::from(internal.liquidity);

        // Reserve accounting follows `_swapIn`: tokenIn reserve increases by the prepared
        // raw-adjusted input and tokenOut reserve decreases by the internal raw output.
        if !self.dex_variables2.pool_accounting_flag {
            let reserve_limit = max_x(128);
            if prepared.zero_for_one {
                let reserve_in =
                    safe_add_u256(self.token0_reserve, prepared.amount_in_raw_adjusted)?;
                if reserve_in > reserve_limit {
                    return Err(SimulationError::InvalidInput(
                        "Token reserves overflow".to_string(),
                        None,
                    ));
                }
                if self.token1_reserve < amount_out_raw_adjusted_for_state {
                    return Err(SimulationError::InvalidInput(
                        "Token reserves underflow".to_string(),
                        None,
                    ));
                }
                new_state.token0_reserve = reserve_in;
                new_state.token1_reserve =
                    safe_sub_u256(self.token1_reserve, amount_out_raw_adjusted_for_state)?;
            } else {
                let reserve_in =
                    safe_add_u256(self.token1_reserve, prepared.amount_in_raw_adjusted)?;
                if reserve_in > reserve_limit {
                    return Err(SimulationError::InvalidInput(
                        "Token reserves overflow".to_string(),
                        None,
                    ));
                }
                if self.token0_reserve < amount_out_raw_adjusted_for_state {
                    return Err(SimulationError::InvalidInput(
                        "Token reserves underflow".to_string(),
                        None,
                    ));
                }
                new_state.token1_reserve = reserve_in;
                new_state.token0_reserve =
                    safe_sub_u256(self.token0_reserve, amount_out_raw_adjusted_for_state)?;
            }
        }

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            u256_to_biguint(internal.gas_used),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let zero_for_one = if sell_token == self.token0.address && buy_token == self.token1.address
        {
            true
        } else if sell_token == self.token1.address && buy_token == self.token0.address {
            false
        } else {
            return Err(SimulationError::InvalidInput(
                "Token pair not part of pool".to_string(),
                None,
            ));
        };

        if self.ticks.is_empty() {
            return Ok((BigUint::from(0u32), BigUint::from(0u32)));
        }
        if self.tick_spacing <= 0 {
            return Err(SimulationError::FatalError("Tick spacing must be positive".to_string()));
        }

        let mut current_liquidity = self
            .dex_variables2
            .active_liquidity
            .to::<u128>();
        if current_liquidity == 0 {
            return Ok((BigUint::from(0u32), BigUint::from(0u32)));
        }

        let ticks = TickList::from(self.tick_spacing as u16, self.ticks.clone())?;
        let mut current_tick = self.dex_variables.current_tick;
        let mut current_sqrt_price = decode_sqrt_price(
            self.dex_variables
                .current_sqrt_price_x96,
            zero_for_one,
        );

        let mut total_amount_in_raw_adjusted = U256::ZERO;
        let mut total_amount_out_raw_adjusted = U256::ZERO;

        while let Ok((tick, initialized)) =
            ticks.next_initialized_tick_within_one_word(current_tick, zero_for_one)
        {
            let next_tick = tick.clamp(MIN_TICK, MAX_TICK);
            let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;

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

            total_amount_in_raw_adjusted = safe_add_u256(total_amount_in_raw_adjusted, amount_in)?;
            total_amount_out_raw_adjusted =
                safe_add_u256(total_amount_out_raw_adjusted, amount_out)?;

            if initialized {
                let liquidity_raw = ticks
                    .get_tick(next_tick)
                    .unwrap()
                    .net_liquidity;
                let liquidity_delta = if zero_for_one { -liquidity_raw } else { liquidity_raw };
                current_liquidity =
                    liquidity_math::add_liquidity_delta(current_liquidity, liquidity_delta)?;
                if current_liquidity == 0 {
                    break;
                }
            }

            current_tick = if zero_for_one { next_tick - 1 } else { next_tick };
            current_sqrt_price = sqrt_price_next;
        }

        let (token_in_decimals, token_out_decimals) = if zero_for_one {
            (self.dex_variables2.token0_decimals, self.dex_variables2.token1_decimals)
        } else {
            (self.dex_variables2.token1_decimals, self.dex_variables2.token0_decimals)
        };
        let (token_in_num_precision, token_in_den_precision) =
            calculate_precisions(token_in_decimals);
        let (token_out_num_precision, token_out_den_precision) =
            calculate_precisions(token_out_decimals);

        let (token_in_exchange_price, token_out_exchange_price) = if zero_for_one {
            match self.dex_type {
                DexType::D3 => {
                    (self.token0_supply_exchange_price, self.token1_supply_exchange_price)
                }
                DexType::D4 => {
                    (self.token0_borrow_exchange_price, self.token1_borrow_exchange_price)
                }
            }
        } else {
            match self.dex_type {
                DexType::D3 => {
                    (self.token1_supply_exchange_price, self.token0_supply_exchange_price)
                }
                DexType::D4 => {
                    (self.token1_borrow_exchange_price, self.token0_borrow_exchange_price)
                }
            }
        };

        let amount_in_limit = adjusted_to_amount(
            total_amount_in_raw_adjusted,
            token_in_num_precision,
            token_in_den_precision,
            token_in_exchange_price,
        )?;
        let mut amount_out_limit = adjusted_to_amount(
            total_amount_out_raw_adjusted,
            token_out_num_precision,
            token_out_den_precision,
            token_out_exchange_price,
        )?;

        // Reserve caps apply only when pool accounting is disabled, matching the on-chain
        // reserve checks inside `_swapIn` / `_swapOut`.
        if !self.dex_variables2.pool_accounting_flag {
            let reserve_out_raw_adjusted =
                if zero_for_one { self.token1_reserve } else { self.token0_reserve };
            let reserve_out_limit = adjusted_to_amount(
                reserve_out_raw_adjusted,
                token_out_num_precision,
                token_out_den_precision,
                token_out_exchange_price,
            )?;
            if amount_out_limit > reserve_out_limit {
                amount_out_limit = reserve_out_limit;
            }
        }

        Ok((u256_to_biguint(amount_in_limit), u256_to_biguint(amount_out_limit)))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(dex_variables) = delta
            .updated_attributes
            .get("dex_variables")
        {
            self.dex_variables = DexVariables::from_packed(U256::from_be_slice(dex_variables));
        }
        if let Some(dex_variables2) = delta
            .updated_attributes
            .get("dex_variables2")
        {
            self.dex_variables2 = DexVariables2::from_packed(U256::from_be_slice(dex_variables2));
        }
        if let Some(token0_reserve) = delta
            .updated_attributes
            .get("token0/token_reserves")
        {
            self.token0_reserve = U256::from_be_slice(token0_reserve);
        }
        if let Some(token1_reserve) = delta
            .updated_attributes
            .get("token1/token_reserves")
        {
            self.token1_reserve = U256::from_be_slice(token1_reserve);
        }
        if let Some(token0_borrow_exchange_price) = delta
            .updated_attributes
            .get("token0/borrow_exchange_price")
        {
            self.token0_borrow_exchange_price = U256::from_be_slice(token0_borrow_exchange_price);
        }
        if let Some(token0_supply_exchange_price) = delta
            .updated_attributes
            .get("token0/supply_exchange_price")
        {
            self.token0_supply_exchange_price = U256::from_be_slice(token0_supply_exchange_price);
        }
        if let Some(token1_borrow_exchange_price) = delta
            .updated_attributes
            .get("token1/borrow_exchange_price")
        {
            self.token1_borrow_exchange_price = U256::from_be_slice(token1_borrow_exchange_price);
        }
        if let Some(token1_supply_exchange_price) = delta
            .updated_attributes
            .get("token1/supply_exchange_price")
        {
            self.token1_supply_exchange_price = U256::from_be_slice(token1_supply_exchange_price);
        }

        for (key, value) in &delta.updated_attributes {
            if key.starts_with("ticks/") {
                let parts: Vec<&str> = key.split('/').collect();
                let tick_index = parts
                    .get(1)
                    .ok_or_else(|| {
                        TransitionError::DecodeError(format!(
                            "Malformed tick key in updated attributes: {key}"
                        ))
                    })?
                    .parse::<i32>()
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
                let net_liquidity = i128::from(value.clone());

                if let Some(existing) = self
                    .ticks
                    .iter_mut()
                    .find(|tick| tick.index == tick_index)
                {
                    existing.net_liquidity = net_liquidity;
                } else {
                    let tick_info = crate::evm::protocol::utils::uniswap::tick_list::TickInfo::new(
                        tick_index,
                        net_liquidity,
                    )
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
                    self.ticks.push(tick_info);
                }
            }
        }

        for key in &delta.deleted_attributes {
            if key.starts_with("ticks/") {
                let parts: Vec<&str> = key.split('/').collect();
                let tick_index = parts
                    .get(1)
                    .ok_or_else(|| {
                        TransitionError::DecodeError(format!(
                            "Malformed tick key in deleted attributes: {key}"
                        ))
                    })?
                    .parse::<i32>()
                    .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
                self.ticks
                    .retain(|tick| tick.index != tick_index);
            }
        }

        self.ticks
            .retain(|tick| tick.net_liquidity != 0);
        self.ticks
            .sort_by_key(|tick| tick.index);

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
        if let Some(other_state) = other.as_any().downcast_ref::<Self>() {
            self == other_state
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use tycho_common::{dto::ProtocolStateDelta, models::Chain};

    use super::*;

    fn u256(value: &str) -> U256 {
        U256::from_str(value).unwrap()
    }

    fn u256_bytes(value: &str) -> Bytes {
        Bytes::from(u256(value).to_be_bytes_vec())
    }

    fn i128_bytes(value: &str) -> Bytes {
        Bytes::from(
            i128::from_str(value)
                .unwrap()
                .to_be_bytes()
                .to_vec(),
        )
    }

    fn test_token(address: &str, symbol: &'static str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0x3c499c542cef5e3811e1192ce70d8cc03d5c3359").unwrap(),
            "USDC",
            6,
            0,
            &[Some(44_000)],
            Chain::Ethereum,
            10,
        )
    }

    fn usdt0() -> Token {
        Token::new(
            &Bytes::from_str("0xc2132d05d31c914a87c6611c10748aeb04b58e8f").unwrap(),
            "USDT0",
            6,
            0,
            &[Some(44_000)],
            Chain::Ethereum,
            10,
        )
    }

    fn test_state() -> FluidV2State {
        FluidV2State::new(
            Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap(),
            test_token("0x0000000000000000000000000000000000000001", "TK0", 18),
            test_token("0x0000000000000000000000000000000000000002", "TK1", 6),
            DexType::D3,
            3000,
            false,
            60,
            Bytes::zero(20),
            DexVariables {
                current_tick: 100,
                current_sqrt_price_x96: u256("79228162514264337593543950336"),
            },
            DexVariables2 {
                protocol_fee_0_to_1: U256::from(50u64),
                protocol_fee_1_to_0: U256::from(70u64),
                protocol_cut_fee: U256::from(10u64),
                token0_decimals: 18,
                token1_decimals: 6,
                active_liquidity: u256("1000000"),
                pool_accounting_flag: false,
                fetch_dynamic_fee_flag: false,
                fee_version: U256::from(0u64),
                lp_fee: U256::from(3000u64),
                max_decay_time: U256::from(0u64),
                price_impact_to_fee_division_factor: U256::from(1u64),
                min_fee: U256::from(200u64),
                max_fee: U256::from(5000u64),
                net_price_impact: 0,
                last_update_timestamp: U256::from(0u64),
                decay_time_remaining: U256::from(0u64),
            },
            u256("500000000000000"),
            u256("700000000000"),
            u256("1000000000000"),
            u256("1000000000000"),
            u256("1000000000000"),
            u256("1000000000000"),
            vec![TickInfo::new(-120, 1500).unwrap(), TickInfo::new(120, -1500).unwrap()],
        )
    }

    fn onchain_state() -> FluidV2State {
        FluidV2State::new(
            Bytes::from_str("0x8d1b5f8da63fa29b191672231d3845740a11fcbef6c76e077cfffe56cc27c707")
                .unwrap(),
            usdc(),
            usdt0(),
            DexType::D4,
            100,
            false,
            1,
            Bytes::zero(20),
            DexVariables {
                current_tick: 20,
                current_sqrt_price_x96: u256("2363625190206393341985"),
            },
            DexVariables2 {
                protocol_fee_0_to_1: U256::from(0u64),
                protocol_fee_1_to_0: U256::from(0u64),
                protocol_cut_fee: U256::from(0u64),
                token0_decimals: 6,
                token1_decimals: 6,
                active_liquidity: U256::from(101337485047035u64),
                pool_accounting_flag: false,
                fetch_dynamic_fee_flag: false,
                fee_version: U256::from(0u64),
                lp_fee: U256::from(100u64),
                max_decay_time: U256::from(0u64),
                price_impact_to_fee_division_factor: U256::from(0u64),
                min_fee: U256::from(0u64),
                max_fee: U256::from(0u64),
                net_price_impact: 0,
                last_update_timestamp: U256::from(0u64),
                decay_time_remaining: U256::from(0u64),
            },
            u256("317527473125"),
            u256("478945217925"),
            u256("1018669616155"),
            u256("1011576356290"),
            u256("1016315199661"),
            u256("1010140557270"),
            vec![
                TickInfo::new(-100, 79191010414114).unwrap(),
                TickInfo::new(18, 4782182157850).unwrap(),
                TickInfo::new(19, 17364292475071).unwrap(),
                TickInfo::new(24, -17364292475071).unwrap(),
                TickInfo::new(27, -4782182157850).unwrap(),
                TickInfo::new(100, -79191010414114).unwrap(),
            ],
        )
    }

    #[test]
    fn test_dex_variables_from_packed_with_sign_and_sqrt_price() {
        let abs_tick = U256::from(12345u64);
        let sqrt_price = u256("12345678901234567890");
        let packed_negative = (sqrt_price << 20u32) | (abs_tick << 1u32);
        let decoded_negative = DexVariables::from_packed(packed_negative);
        assert_eq!(decoded_negative.current_tick, -12345);
        assert_eq!(decoded_negative.current_sqrt_price_x96, sqrt_price);

        let packed_positive = (sqrt_price << 20u32) | (abs_tick << 1u32) | U256::from(1u64);
        let decoded_positive = DexVariables::from_packed(packed_positive);
        assert_eq!(decoded_positive.current_tick, 12345);
        assert_eq!(decoded_positive.current_sqrt_price_x96, sqrt_price);
    }

    #[test]
    fn test_dex_variables2_from_packed_decodes_fields() {
        let active_liquidity = u256("123456789012345678901234567890");
        let packed = U256::from(21u64) |
            (U256::from(34u64) << 12u32) |
            (U256::from(11u64) << 24u32) |
            (U256::from(15u64) << 30u32) |
            (U256::from(6u64) << 34u32) |
            (active_liquidity << 38u32) |
            (U256::from(1u64) << 140u32) |
            (U256::from(1u64) << 141u32) |
            (U256::from(3u64) << 152u32) |
            (U256::from(3600u64) << 156u32) |
            (U256::from(42u64) << 168u32) |
            (U256::from(150u64) << 176u32) |
            (U256::from(400u64) << 192u32) |
            (U256::from(1u64) << 208u32) |
            (U256::from(777u64) << 209u32) |
            (U256::from(12345u64) << 229u32) |
            (U256::from(876u64) << 244u32);

        let decoded = DexVariables2::from_packed(packed);

        assert_eq!(decoded.protocol_fee_0_to_1, U256::from(21u64));
        assert_eq!(decoded.protocol_fee_1_to_0, U256::from(34u64));
        assert_eq!(decoded.protocol_cut_fee, U256::from(11u64));
        assert_eq!(decoded.token0_decimals, 18);
        assert_eq!(decoded.token1_decimals, 6);
        assert_eq!(decoded.active_liquidity, active_liquidity);
        assert!(decoded.pool_accounting_flag);
        assert!(decoded.fetch_dynamic_fee_flag);
        assert_eq!(decoded.fee_version, U256::from(3u64));
        // fee_version != 0 → dynamic fee mode: lp_fee is unused, max_decay_time is active
        assert_eq!(decoded.lp_fee, U256::ZERO);
        assert_eq!(decoded.max_decay_time, U256::from(3600u64));
        assert_eq!(decoded.price_impact_to_fee_division_factor, U256::from(42u64),);
        assert_eq!(decoded.min_fee, U256::from(150u64));
        assert_eq!(decoded.max_fee, U256::from(400u64));
        assert_eq!(decoded.net_price_impact, 777);
        assert_eq!(decoded.last_update_timestamp, U256::from(12345u64));
        assert_eq!(decoded.decay_time_remaining, U256::from(876u64));
    }

    #[test]
    fn test_delta_transition_updates_state_and_ticks() {
        let mut state = test_state();

        let new_sqrt_price = u256("999999999999");
        let new_dex_variables =
            (new_sqrt_price << 20u32) | (U256::from(77u64) << 1u32) | U256::from(1u64);
        let new_active_liquidity = u256("987654321012345678");
        let new_dex_variables2 = U256::from(12u64) |
            (U256::from(24u64) << 12u32) |
            (U256::from(5u64) << 24u32) |
            (U256::from(15u64) << 30u32) |
            (U256::from(6u64) << 34u32) |
            (new_active_liquidity << 38u32) |
            (U256::from(1u64) << 152u32) |
            (U256::from(2500u64) << 156u32) |
            (U256::from(99u64) << 176u32) |
            (U256::from(199u64) << 192u32);

        let updated_attributes: HashMap<String, Bytes> = [
            ("dex_variables".to_string(), Bytes::from(new_dex_variables.to_be_bytes_vec())),
            ("dex_variables2".to_string(), Bytes::from(new_dex_variables2.to_be_bytes_vec())),
            ("token0/token_reserves".to_string(), u256_bytes("111111111")),
            ("token1/token_reserves".to_string(), u256_bytes("222222222")),
            ("token0/borrow_exchange_price".to_string(), u256_bytes("333333333")),
            ("token0/supply_exchange_price".to_string(), u256_bytes("444444444")),
            ("token1/borrow_exchange_price".to_string(), u256_bytes("555555555")),
            ("token1/supply_exchange_price".to_string(), u256_bytes("666666666")),
            ("ticks/-120".to_string(), i128_bytes("2222")),
            ("ticks/240".to_string(), i128_bytes("3333")),
            ("ticks/0".to_string(), i128_bytes("0")),
        ]
        .into_iter()
        .collect();

        let deleted_attributes = HashSet::from([String::from("ticks/120")]);
        let delta = ProtocolStateDelta {
            component_id: "fluid-v2-test".to_string(),
            updated_attributes,
            deleted_attributes,
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(state.dex_variables.current_tick, 77);
        assert_eq!(
            state
                .dex_variables
                .current_sqrt_price_x96,
            new_sqrt_price
        );
        assert_eq!(state.dex_variables2.active_liquidity, new_active_liquidity);
        assert_eq!(state.token0_reserve, u256("111111111"));
        assert_eq!(state.token1_reserve, u256("222222222"));
        assert_eq!(state.token0_borrow_exchange_price, u256("333333333"));
        assert_eq!(state.token0_supply_exchange_price, u256("444444444"));
        assert_eq!(state.token1_borrow_exchange_price, u256("555555555"));
        assert_eq!(state.token1_supply_exchange_price, u256("666666666"));

        assert_eq!(state.ticks.len(), 2);
        assert_eq!(state.ticks[0].index, -120);
        assert_eq!(state.ticks[0].net_liquidity, 2222);
        assert_eq!(state.ticks[1].index, 240);
        assert_eq!(state.ticks[1].net_liquidity, 3333);
    }

    #[test]
    fn test_get_limits() {
        let state = onchain_state();

        let (limit_in, limit_out) = state
            .get_limits(
                Bytes::from_str("0x3c499c542cef5e3811e1192ce70d8cc03d5c3359").unwrap(),
                Bytes::from_str("0xc2132d05d31c914a87c6611c10748aeb04b58e8f").unwrap(),
            )
            .unwrap();

        assert_eq!(limit_in, BigUint::from(489809940u32));
        assert_eq!(limit_out, BigUint::from(486756162u32));
    }

    #[test]
    fn test_get_limits_only_caps_with_reserve_accounting() {
        let mut capped_state = onchain_state();
        capped_state
            .dex_variables2
            .pool_accounting_flag = false;
        capped_state.token1_reserve = U256::from(1_000_000u64);

        let (_, capped_limit_out) = capped_state
            .get_limits(usdc().address.clone(), usdt0().address.clone())
            .unwrap();

        let mut uncapped_state = capped_state.clone();
        uncapped_state
            .dex_variables2
            .pool_accounting_flag = true;

        let (_, uncapped_limit_out) = uncapped_state
            .get_limits(usdc().address.clone(), usdt0().address.clone())
            .unwrap();

        assert_eq!(capped_limit_out, BigUint::from(1_016u32));
        assert_eq!(uncapped_limit_out, BigUint::from(486_756_162u32));
    }

    #[test]
    fn test_get_amount_out() {
        let state = onchain_state();
        let res = state
            .get_amount_out(BigUint::from_str("4898099").unwrap(), &usdc(), &usdt0())
            .unwrap();
        assert_eq!(res.amount, BigUint::from(4896168u32));

        let prepared = state
            .prepare_swap_in(U256::from(4_898_099u64), &usdc(), &usdt0())
            .unwrap();
        let internal = state
            .execute_swap_in_internal(&prepared)
            .unwrap();
        let rounded_amount_out_raw = round_down_raw(internal.amount_out_raw_adjusted);

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<FluidV2State>()
            .unwrap();

        assert_eq!(
            new_state.token1_reserve,
            state.token1_reserve - internal.amount_out_raw_adjusted
        );
        assert_ne!(new_state.token1_reserve, state.token1_reserve - rounded_amount_out_raw);
    }
}
