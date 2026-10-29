use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
};

use alloy::primitives::{Address, Sign, I256, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::trace;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use super::{
    plugin::{self, BeforeSwapRequest, StorageOverrides, AFTER_SWAP_FLAG, BEFORE_SWAP_FLAG},
    swap_math::{amount_to_i256, get_token0_delta, get_token1_delta, move_price_towards_target},
};
use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        clmm::clmm_swap_to_price,
        safe_math::{safe_add_u256, safe_sub_u256},
        u256_num::u256_to_biguint,
        utils::{
            add_fee_markup,
            uniswap::{
                liquidity_math::add_liquidity_delta,
                sqrt_price_math::sqrt_price_q96_to_f64,
                tick_math::{
                    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK,
                    MIN_SQRT_RATIO, MIN_TICK,
                },
                SwapResults,
            },
        },
    },
};

// Gas model for a Nami V4 (Algebra Integral) swap. Initial estimates from the Uniswap V3 model,
// without the bitmap word scans (Algebra walks a linked list of initialized ticks). They are meant
// to be recalibrated against on-chain swaps once pools with liquidity exist.

// Pre/post loop overhead: globalState, liquidity and fee growth reads and writes.
const SWAP_BASE_GAS: u64 = 70_000;
// One `movePriceTowardsTarget` step plus its accounting.
const GAS_PER_SWAP_MATH_STEP: u64 = 5_400;
// Crossing an initialized tick: `ticks.cross` updates the tick's fee growth outside.
const GAS_PER_INITIALIZED_TICK_CROSS: u64 = 25_000;
// Output transfer, callback, balance checks and reserve update.
const SWAP_SETTLEMENT_GAS: u64 = 70_000;
// Calling into the plugin for `beforeSwap`, on top of the gas the hook itself reports.
const PLUGIN_CALL_OVERHEAD_GAS: u64 = 5_000;
// `afterSwap`: the farming tick update and the MEVX back-run check. The back-run can spend up to
// the plugin's `callGasBudget` when an arbitrage exists; this is the cost when none does, which is
// what a solver should compare quotes on.
const AFTER_SWAP_GAS: u64 = 60_000;
// Conservative max gas budget for a single swap (Ethereum transaction gas limit).
const MAX_SWAP_GAS: u64 = 16_700_000;
const MAX_TICKS_CROSSED: u64 = (MAX_SWAP_GAS - SWAP_BASE_GAS) / GAS_PER_INITIALIZED_TICK_CROSS;

/// State of a Nami V4 pool, an Algebra Integral 1.2 fork.
///
/// Swap math runs natively. When the pool's plugin config enables `beforeSwap` and `DYNAMIC_FEE`,
/// the fee of every quote comes from running the plugin's `beforeSwap` hook in the EVM against
/// indexed plugin state; otherwise the pool's stored fee applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamiV4State {
    pool: Address,
    liquidity: u128,
    sqrt_price: U256,
    tick: i32,
    /// Initialized ticks and their net liquidity. A tick stays initialized while any position
    /// references it, so it can carry zero net liquidity and still bound a swap step.
    ticks: BTreeMap<i32, i128>,
    /// `globalState.lastFee`: the fee used when the plugin does not override it.
    last_fee: u32,
    plugin: Option<Address>,
    plugin_config: u8,
    /// The plugin's MEVX router. While it is set and the pool calls `afterSwap`, the plugin
    /// requires `min_gas_left` gas to remain at that hook and reverts the swap otherwise.
    mevx_router: Option<Address>,
    min_gas_left: u64,
    /// Gas the plugin forwards to its back-run self-call. It does not change a quote.
    call_gas_budget: u64,
    /// The farming incentive the plugin's `afterSwap` updates, when one is set.
    incentive: Option<Address>,
    /// Block the next quote is expected to execute in.
    execution_block_number: u64,
    execution_block_timestamp: u64,
    /// Plugin storage written by the simulated swaps this state was chained from, all in the
    /// execution block. The plugin writes its oracle timepoint once per block, so a later swap in
    /// the same block must see it.
    plugin_storage: StorageOverrides,
}

/// Fee a swap pays, the gas spent resolving it and the plugin storage it leaves behind.
struct ResolvedFee {
    fee: u32,
    gas: u64,
    plugin_storage: StorageOverrides,
}

impl NamiV4State {
    /// Creates a Nami V4 pool state.
    ///
    /// `fees` are in hundredths of a bip (1_000_000 = 100%). `ticks` lists every initialized tick
    /// with its net liquidity, including ticks whose net liquidity is zero.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Address,
        liquidity: u128,
        sqrt_price: U256,
        tick: i32,
        ticks: Vec<(i32, i128)>,
        last_fee: u32,
        plugin: Option<Address>,
        plugin_config: u8,
        execution_block: BlockContext,
    ) -> Self {
        NamiV4State {
            pool,
            liquidity,
            sqrt_price,
            tick,
            ticks: ticks.into_iter().collect(),
            last_fee,
            plugin,
            plugin_config,
            mevx_router: None,
            min_gas_left: 0,
            call_gas_budget: 0,
            incentive: None,
            execution_block_number: execution_block.number(),
            execution_block_timestamp: execution_block.timestamp(),
            plugin_storage: StorageOverrides::new(),
        }
    }

    /// Sets the plugin's settings, as indexed from its setter events: the MEVX router and gas
    /// requirements, and the farming incentive.
    pub fn with_plugin_settings(
        mut self,
        mevx_router: Option<Address>,
        min_gas_left: u64,
        call_gas_budget: u64,
        incentive: Option<Address>,
    ) -> Self {
        self.mevx_router = mevx_router;
        self.min_gas_left = min_gas_left;
        self.call_gas_budget = call_gas_budget;
        self.incentive = incentive;
        self
    }

    /// The fee a swap in the given direction would pay now, as a ratio, falling back to the
    /// stored fee when the plugin cannot be queried.
    fn fee_ratio(&self, zero_for_one: bool) -> f64 {
        let fee = Self::default_price_limit(zero_for_one)
            .and_then(|limit| self.resolve_fee(zero_for_one, I256::ONE, limit))
            .map_or(self.last_fee, |resolved| resolved.fee);
        fee as f64 / 1_000_000.0
    }

    fn has_dynamic_fee_plugin(&self) -> bool {
        self.plugin.is_some() && self.plugin_config & BEFORE_SWAP_FLAG != 0
    }

    /// Resolves the fee the pool would charge for this swap, asking the plugin when the pool
    /// calls it before swaps.
    fn resolve_fee(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: U256,
    ) -> Result<ResolvedFee, SimulationError> {
        if self.plugin_config & BEFORE_SWAP_FLAG == 0 {
            return Ok(ResolvedFee {
                fee: self.last_fee,
                gas: 0,
                plugin_storage: self.plugin_storage.clone(),
            });
        }
        let Some(plugin) = self.plugin else {
            return Err(SimulationError::RecoverableError(format!(
                "Nami V4 pool {} calls beforeSwap but has no plugin; the pool would revert",
                self.pool
            )));
        };

        let engine = create_engine(SHARED_TYCHO_DB.clone(), false)?;
        let fees = plugin::before_swap(
            &engine,
            &BeforeSwapRequest {
                pool: self.pool,
                plugin,
                sqrt_price: self.sqrt_price,
                tick: self.tick,
                zero_to_one: zero_for_one,
                amount_specified,
                sqrt_price_limit,
                block_number: self.execution_block_number,
                block_timestamp: self.execution_block_timestamp,
                storage_overrides: &self.plugin_storage,
            },
        )?;
        Ok(ResolvedFee {
            fee: plugin::effective_fee(self.last_fee, self.plugin_config, &fees)?,
            gas: fees.gas_used + PLUGIN_CALL_OVERHEAD_GAS,
            plugin_storage: fees.storage_writes,
        })
    }

    /// Largest initialized tick at or below `tick`, or the `MIN_TICK` sentinel.
    fn prev_initialized_tick(&self, tick: i32) -> i32 {
        self.ticks
            .range(..=tick)
            .next_back()
            .map_or(MIN_TICK, |(index, _)| *index)
    }

    /// Smallest initialized tick above `tick`, or the `MAX_TICK` sentinel.
    fn next_initialized_tick(&self, tick: i32) -> i32 {
        self.ticks
            .range(tick.saturating_add(1)..)
            .next()
            .map_or(MAX_TICK, |(index, _)| *index)
    }

    fn default_price_limit(zero_for_one: bool) -> Result<U256, SimulationError> {
        if zero_for_one {
            safe_add_u256(MIN_SQRT_RATIO, U256::ONE)
        } else {
            safe_sub_u256(MAX_SQRT_RATIO, U256::ONE)
        }
    }

    /// Mirrors `SwapCalculation._calculateSwap`: walks initialized ticks from the current price
    /// until the amount is used up or the price limit is reached. Also returns the plugin storage
    /// the swap leaves behind.
    fn swap(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
    ) -> Result<(SwapResults, StorageOverrides), SimulationError> {
        if self.sqrt_price.is_zero() {
            return Err(SimulationError::RecoverableError("Pool not initialized".to_string()));
        }
        if self.liquidity == 0 && self.ticks.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }
        if amount_specified.is_zero() || amount_specified == I256::MIN {
            return Err(SimulationError::InvalidInput("Invalid swap amount".to_string(), None));
        }

        let price_limit = match sqrt_price_limit {
            Some(limit) => limit,
            None => Self::default_price_limit(zero_for_one)?,
        };
        let price_limit_valid = if zero_for_one {
            price_limit < self.sqrt_price && price_limit > MIN_SQRT_RATIO
        } else {
            price_limit > self.sqrt_price && price_limit < MAX_SQRT_RATIO
        };
        if !price_limit_valid {
            return Err(SimulationError::InvalidInput("Price limit out of range".into(), None));
        }

        let resolved_fee = self.resolve_fee(zero_for_one, amount_specified, price_limit)?;
        let exact_input = amount_specified.is_positive();

        let mut amount_required = amount_specified;
        let mut amount_calculated = I256::ZERO;
        let mut current_price = self.sqrt_price;
        let mut current_tick = self.tick;
        let mut current_liquidity = self.liquidity;
        let mut prev_tick = self.prev_initialized_tick(self.tick);
        let mut next_tick = self.next_initialized_tick(self.tick);
        let mut gas_used = SWAP_BASE_GAS + resolved_fee.gas;

        loop {
            let step_tick = if zero_for_one { prev_tick } else { next_tick };
            let step_start_price = current_price;
            let step_tick_price = get_sqrt_ratio_at_tick(step_tick)?;
            let target_price = if zero_for_one == (step_tick_price < price_limit) {
                price_limit
            } else {
                step_tick_price
            };

            let movement = move_price_towards_target(
                zero_for_one,
                current_price,
                target_price,
                current_liquidity,
                amount_required,
                resolved_fee.fee,
            )?;
            current_price = movement.result_price;
            gas_used += GAS_PER_SWAP_MATH_STEP;

            let paid = amount_to_i256(safe_add_u256(movement.input, movement.fee_amount)?)?;
            let received = amount_to_i256(movement.output)?;
            let (required_change, calculated_change) = if exact_input {
                (paid.checked_neg(), received.checked_neg())
            } else {
                (Some(received), Some(paid))
            };
            amount_required = required_change
                .and_then(|change| amount_required.checked_add(change))
                .ok_or_else(|| SimulationError::FatalError("Swap amount overflow".into()))?;
            amount_calculated = calculated_change
                .and_then(|change| amount_calculated.checked_add(change))
                .ok_or_else(|| SimulationError::FatalError("Swap amount overflow".into()))?;

            if current_price == step_tick_price {
                let net_liquidity = self
                    .ticks
                    .get(&step_tick)
                    .copied()
                    .unwrap_or(0);
                let liquidity_delta = if zero_for_one {
                    current_tick = step_tick - 1;
                    next_tick = step_tick;
                    prev_tick = self.prev_initialized_tick(step_tick - 1);
                    -net_liquidity
                } else {
                    current_tick = step_tick;
                    prev_tick = step_tick;
                    next_tick = self.next_initialized_tick(step_tick);
                    net_liquidity
                };
                current_liquidity = add_liquidity_delta(current_liquidity, liquidity_delta)?;
                gas_used += GAS_PER_INITIALIZED_TICK_CROSS;
            } else if current_price != step_start_price {
                current_tick = get_tick_at_sqrt_ratio(current_price)?;
                break;
            }

            if amount_required.is_zero() || current_price == price_limit {
                break;
            }
        }

        if self.plugin_config & AFTER_SWAP_FLAG != 0 {
            gas_used += AFTER_SWAP_GAS;
        }

        let result = SwapResults {
            amount_calculated,
            amount_specified,
            amount_remaining: amount_required,
            sqrt_price: current_price,
            liquidity: current_liquidity,
            tick: current_tick.into(),
            gas_used: U256::from(gas_used + SWAP_SETTLEMENT_GAS),
        };
        Ok((result, resolved_fee.plugin_storage))
    }

    fn with_swap_result(&self, result: &SwapResults, plugin_storage: StorageOverrides) -> Self {
        let mut new_state = self.clone();
        new_state.liquidity = result.liquidity;
        new_state.tick = result.tick.value();
        new_state.sqrt_price = result.sqrt_price;
        new_state.plugin_storage = plugin_storage;
        new_state
    }
}

fn decode_error(err: impl ToString) -> TransitionError {
    TransitionError::DecodeError(err.to_string())
}

/// Decodes an address attribute; an empty value or the zero address means none.
pub(super) fn decode_optional_address(value: &Bytes) -> Result<Option<Address>, String> {
    match value.len() {
        0 => Ok(None),
        20 => {
            let address = Address::from_slice(value);
            Ok((address != Address::ZERO).then_some(address))
        }
        len => Err(format!("address attribute must be 0 or 20 bytes, got {len}")),
    }
}

/// Decodes an unsigned big-endian integer attribute, saturating at `u64::MAX`.
pub(super) fn decode_u64(value: &Bytes) -> u64 {
    u64::try_from(U256::from_be_slice(value)).unwrap_or(u64::MAX)
}

#[typetag::serde]
impl ProtocolSim for NamiV4State {
    /// The fee a zero-for-one swap would pay now, falling back to the stored fee when the plugin
    /// cannot be queried. The plugin may charge the other direction a different fee; `spot_price`
    /// accounts for that, this single value cannot.
    fn fee(&self) -> f64 {
        self.fee_ratio(true)
    }

    /// The price of `a` in `b` including the fee a swap in that direction would pay. The plugin
    /// may charge the two directions different fees.
    fn spot_price(&self, a: &Token, b: &Token) -> Result<f64, SimulationError> {
        let zero_for_one = a < b;
        let price = if zero_for_one {
            sqrt_price_q96_to_f64(self.sqrt_price, a.decimals, b.decimals)?
        } else {
            1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, b.decimals, a.decimals)?
        };
        Ok(add_fee_markup(price, self.fee_ratio(zero_for_one)))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_a: &Token,
        token_b: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let zero_for_one = token_a < token_b;
        let amount_specified = U256::try_from_be_slice(&amount_in.to_bytes_be())
            .and_then(|amount| I256::checked_from_sign_and_abs(Sign::Positive, amount))
            .ok_or_else(|| {
                SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
            })?;

        let (result, plugin_storage) = self.swap(zero_for_one, amount_specified, None)?;
        trace!(?amount_in, ?token_a, ?token_b, ?zero_for_one, ?result, "NAMI V4 SWAP");

        let amount_out = u256_to_biguint(
            result
                .amount_calculated
                .saturating_abs()
                .into_raw(),
        );
        let gas = u256_to_biguint(result.gas_used);
        let new_state = Box::new(self.with_swap_result(&result, plugin_storage));
        if !result.amount_remaining.is_zero() {
            return Err(SimulationError::InvalidInput(
                "Price limit reached before the full amount was swapped".into(),
                Some(GetAmountOutResult::new(amount_out, gas, new_state)),
            ));
        }
        Ok(GetAmountOutResult::new(amount_out, gas, new_state))
    }

    fn get_limits(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let zero_for_one = token_in < token_out;
        let mut current_tick = self.tick;
        let mut current_price = self.sqrt_price;
        let mut current_liquidity = self.liquidity;
        let mut total_in = U256::ZERO;
        let mut total_out = U256::ZERO;

        for _ in 0..MAX_TICKS_CROSSED {
            let step_tick = if zero_for_one {
                self.prev_initialized_tick(current_tick)
            } else {
                self.next_initialized_tick(current_tick)
            };
            if step_tick == MIN_TICK || step_tick == MAX_TICK {
                break;
            }
            let step_price = get_sqrt_ratio_at_tick(step_tick)?;
            let (amount_in, amount_out) = if zero_for_one {
                (
                    get_token0_delta(step_price, current_price, current_liquidity, true)?,
                    get_token1_delta(step_price, current_price, current_liquidity, false)?,
                )
            } else {
                (
                    get_token1_delta(current_price, step_price, current_liquidity, true)?,
                    get_token0_delta(current_price, step_price, current_liquidity, false)?,
                )
            };
            total_in = safe_add_u256(total_in, amount_in)?;
            total_out = safe_add_u256(total_out, amount_out)?;

            let net_liquidity = self
                .ticks
                .get(&step_tick)
                .copied()
                .unwrap_or(0);
            let liquidity_delta = if zero_for_one { -net_liquidity } else { net_liquidity };
            let Ok(new_liquidity) = add_liquidity_delta(current_liquidity, liquidity_delta) else {
                break;
            };
            current_liquidity = new_liquidity;
            current_tick = if zero_for_one { step_tick - 1 } else { step_tick };
            current_price = step_price;
        }

        Ok((u256_to_biguint(total_in), u256_to_biguint(total_out)))
    }

    fn delta_transition(
        &mut self,
        mut delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // Indexed state now includes whatever the simulated swaps anticipated.
        self.plugin_storage.clear();
        if let Some(liquidity) = delta
            .updated_attributes
            .remove("liquidity")
        {
            self.liquidity = liquidity.into();
        }
        if let Some(sqrt_price) = delta
            .updated_attributes
            .remove("sqrt_price_x96")
        {
            self.sqrt_price = U256::from_be_slice(&sqrt_price);
        }
        if let Some(tick) = delta.updated_attributes.remove("tick") {
            self.tick = tick.into();
        }
        if let Some(fee) = delta.updated_attributes.remove("fee") {
            self.last_fee = fee.into();
        }
        if let Some(plugin) = delta
            .updated_attributes
            .remove("plugin")
        {
            self.plugin = decode_optional_address(&plugin).map_err(decode_error)?;
        }
        if let Some(plugin_config) = delta
            .updated_attributes
            .remove("plugin_config")
        {
            self.plugin_config = u8::from(plugin_config);
        }
        if let Some(router) = delta
            .updated_attributes
            .remove("mevx_router")
        {
            self.mevx_router = decode_optional_address(&router).map_err(decode_error)?;
        }
        if let Some(min_gas_left) = delta
            .updated_attributes
            .remove("min_gas_left")
        {
            self.min_gas_left = decode_u64(&min_gas_left);
        }
        if let Some(call_gas_budget) = delta
            .updated_attributes
            .remove("call_gas_budget")
        {
            self.call_gas_budget = decode_u64(&call_gas_budget);
        }
        if let Some(incentive) = delta
            .updated_attributes
            .remove("incentive")
        {
            self.incentive = decode_optional_address(&incentive).map_err(decode_error)?;
        }

        for (key, value) in delta.updated_attributes {
            let Some(tick) = key.strip_prefix("ticks/") else {
                continue;
            };
            let tick = tick
                .parse::<i32>()
                .map_err(decode_error)?;
            self.ticks
                .insert(tick, i128::from(value));
        }
        for key in delta.deleted_attributes {
            if key == "plugin" {
                self.plugin = None;
                continue;
            }
            if key == "mevx_router" {
                self.mevx_router = None;
                continue;
            }
            if key == "incentive" {
                self.incentive = None;
                continue;
            }
            let Some(tick) = key.strip_prefix("ticks/") else {
                continue;
            };
            let tick = tick
                .parse::<i32>()
                .map_err(decode_error)?;
            self.ticks.remove(&tick);
        }
        Ok(())
    }

    /// See [`ProtocolSim::query_pool_swap`] for the trait documentation.
    ///
    /// Swaps an unbounded amount of `token_in` until the pool reaches the target price.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "Nami V4 does not support TradeLimitPrice constraint in query_pool_swap"
                    .to_string(),
                None,
            )),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: _,
                min_amount_in: _,
                max_amount_in: _,
            } => {
                let zero_for_one = params.token_in().address < params.token_out().address;
                let fee = Self::default_price_limit(zero_for_one)
                    .and_then(|limit| self.resolve_fee(zero_for_one, I256::ONE, limit))?
                    .fee;
                let mut plugin_storage = StorageOverrides::new();
                let (amount_in, amount_out, swap_result) = clmm_swap_to_price(
                    self.sqrt_price,
                    &params.token_in().address,
                    &params.token_out().address,
                    target,
                    fee,
                    Sign::Positive,
                    |zero_for_one, amount_specified, sqrt_price_limit| {
                        let (result, storage) =
                            self.swap(zero_for_one, amount_specified, Some(sqrt_price_limit))?;
                        plugin_storage = storage;
                        Ok(result)
                    },
                )?;

                Ok(PoolSwap::new(
                    amount_in,
                    amount_out,
                    Box::new(self.with_swap_result(&swap_result, plugin_storage)),
                    None,
                ))
            }
        }
    }

    /// Records the block the next quote executes in. A plugin's fee depends on the block
    /// timestamp, so pools whose fee comes from the plugin report a change on every new
    /// timestamp.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        if block.number() == self.execution_block_number &&
            block.timestamp() == self.execution_block_timestamp
        {
            return false;
        }
        let timestamp_changed = block.timestamp() != self.execution_block_timestamp;
        self.execution_block_number = block.number();
        self.execution_block_timestamp = block.timestamp();
        // Storage anticipated for the previous execution block does not carry over.
        self.plugin_storage.clear();
        timestamp_changed && self.has_dynamic_fee_plugin()
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
            .downcast_ref::<NamiV4State>()
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
#[path = "fork_differential_tests.rs"]
mod fork_differential_tests;

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use tycho_common::models::Chain;

    use super::*;
    use crate::evm::protocol::nami_v4::plugin::DYNAMIC_FEE_FLAG;

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn token0() -> Token {
        token("0x1000000000000000000000000000000000000000", "T0", 18)
    }

    fn token1() -> Token {
        token("0x2000000000000000000000000000000000000000", "T1", 18)
    }

    // A pool at price 1 (tick 0) with one position over [-600, 600], no plugin, 0.3% fee.
    fn static_fee_pool() -> NamiV4State {
        let liquidity = 1_000_000_000_000_000_000_000u128;
        NamiV4State::new(
            Address::repeat_byte(0x11),
            liquidity,
            U256::ONE << 96,
            0,
            vec![(-600, liquidity as i128), (600, -(liquidity as i128))],
            3_000,
            None,
            0,
            BlockContext::new(100, 1_000),
        )
    }

    #[test]
    fn exact_input_within_one_range_charges_the_stored_fee() {
        let pool = static_fee_pool();
        let amount_in = BigUint::from(10u64).pow(18);

        let result = pool
            .get_amount_out(amount_in.clone(), &token0(), &token1())
            .unwrap();

        // At price 1 with deep liquidity, output is just below input minus the 0.3% fee.
        let fee_free = &amount_in - &amount_in * 3u32 / 1000u32;
        assert!(result.amount < fee_free);
        assert!(result.amount > &fee_free - &fee_free / 1000u32);
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<NamiV4State>()
            .unwrap();
        assert!(new_state.sqrt_price < pool.sqrt_price);
        assert!(new_state.tick < 0);
    }

    #[test]
    fn swap_crosses_out_of_range_and_stops_at_the_limit() {
        let pool = static_fee_pool();
        // Far more than the range holds: the swap exhausts liquidity at tick -600, walks the
        // empty range to the price limit and leaves part of the input unswapped.
        let amount_in = BigUint::from(10u64).pow(30);

        let err = pool
            .get_amount_out(amount_in, &token0(), &token1())
            .unwrap_err();

        let SimulationError::InvalidInput(_, Some(partial)) = err else {
            panic!("expected a partial result, got {err:?}");
        };
        let new_state = partial
            .new_state
            .as_any()
            .downcast_ref::<NamiV4State>()
            .unwrap();
        assert_eq!(new_state.liquidity, 0);
        assert!(new_state.tick < -600);
        let (max_in, max_out) = pool
            .get_limits(token0().address, token1().address)
            .unwrap();
        assert!(partial.amount <= max_out);
        assert!(max_in > BigUint::ZERO);
    }

    #[test]
    fn zero_net_liquidity_tick_still_bounds_a_step() {
        let mut pool = static_fee_pool();
        pool.ticks.insert(-60, 0);

        assert_eq!(pool.prev_initialized_tick(-1), -60);
        assert_eq!(pool.prev_initialized_tick(-61), -600);
        assert_eq!(pool.next_initialized_tick(600), MAX_TICK);
    }

    #[test]
    fn uninitialized_pool_is_not_quotable() {
        let mut pool = static_fee_pool();
        pool.sqrt_price = U256::ZERO;

        assert!(pool
            .get_amount_out(BigUint::from(1u64), &token0(), &token1())
            .is_err());
    }

    #[test]
    fn delta_transition_updates_fee_plugin_and_ticks() {
        let mut pool = static_fee_pool();
        let plugin = Address::repeat_byte(0x22);
        let updated_attributes: HashMap<String, Bytes> = [
            ("fee".to_string(), Bytes::from(500_u32.to_be_bytes().to_vec())),
            ("plugin".to_string(), Bytes::from(plugin.to_vec())),
            ("plugin_config".to_string(), Bytes::from(vec![0xC3])),
            ("ticks/-60".to_string(), Bytes::from(0_i128.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let delta = ProtocolStateDelta {
            component_id: "pool".to_owned(),
            updated_attributes,
            deleted_attributes: HashSet::from(["ticks/600".to_string()]),
        };

        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(pool.last_fee, 500);
        assert_eq!(pool.plugin, Some(plugin));
        assert_eq!(pool.plugin_config, 0xC3);
        assert_eq!(pool.ticks.get(&-60), Some(&0));
        assert!(!pool.ticks.contains_key(&600));
    }

    #[test]
    fn apply_block_reports_changes_only_for_plugin_fees() {
        let mut static_pool = static_fee_pool();
        assert!(!static_pool.apply_block(&BlockContext::new(101, 1_002)));

        let mut dynamic_pool = static_fee_pool();
        dynamic_pool.plugin = Some(Address::repeat_byte(0x22));
        dynamic_pool.plugin_config = BEFORE_SWAP_FLAG | DYNAMIC_FEE_FLAG;
        assert!(dynamic_pool.apply_block(&BlockContext::new(101, 1_002)));
        assert!(!dynamic_pool.apply_block(&BlockContext::new(101, 1_002)));
    }

    #[test]
    fn before_swap_flag_without_plugin_is_an_error() {
        let mut pool = static_fee_pool();
        pool.plugin_config = BEFORE_SWAP_FLAG;

        assert!(pool
            .get_amount_out(BigUint::from(1_000u64), &token0(), &token1())
            .is_err());
    }

    #[test]
    fn decode_optional_address_treats_empty_and_zero_as_none() {
        assert_eq!(decode_optional_address(&Bytes::new()), Ok(None));
        assert_eq!(decode_optional_address(&Bytes::from(vec![0u8; 20])), Ok(None));
        assert!(decode_optional_address(&Bytes::from(vec![1u8; 3])).is_err());
    }

    #[test]
    fn decode_u64_reads_any_width_and_saturates() {
        assert_eq!(decode_u64(&Bytes::new()), 0);
        assert_eq!(decode_u64(&Bytes::from(vec![0x0F, 0x42, 0x40])), 1_000_000);
        assert_eq!(decode_u64(&Bytes::from(1_000_000_u64)), 1_000_000);
        assert_eq!(decode_u64(&Bytes::from(vec![0xFF; 32])), u64::MAX);
    }

    // The static pool with the plugin's `afterSwap` enabled and a minimum gas left of 1M.
    fn gated_pool() -> NamiV4State {
        let mut pool = static_fee_pool();
        pool.plugin = Some(Address::repeat_byte(0x22));
        pool.plugin_config = AFTER_SWAP_FLAG;
        pool.with_plugin_settings(
            Some(Address::repeat_byte(0x33)),
            1_000_000,
            5_000_000,
            Some(Address::repeat_byte(0x44)),
        )
    }

    #[test]
    fn delta_transition_updates_and_clears_the_plugin_settings() {
        let mut pool = gated_pool();
        let updated_attributes: HashMap<String, Bytes> = [
            ("min_gas_left".to_string(), Bytes::from(vec![0x03, 0xD0, 0x90])),
            ("call_gas_budget".to_string(), Bytes::from(1_000_000_u64)),
            ("incentive".to_string(), Bytes::from(vec![0x55; 20])),
        ]
        .into_iter()
        .collect();
        let delta = ProtocolStateDelta {
            component_id: "pool".to_owned(),
            updated_attributes,
            deleted_attributes: HashSet::new(),
        };

        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(pool.min_gas_left, 250_000);
        assert_eq!(pool.call_gas_budget, 1_000_000);
        assert_eq!(pool.incentive, Some(Address::repeat_byte(0x55)));

        let zero_router = ProtocolStateDelta {
            component_id: "pool".to_owned(),
            updated_attributes: HashMap::from([(
                "mevx_router".to_string(),
                Bytes::from(vec![0u8; 20]),
            )]),
            deleted_attributes: HashSet::new(),
        };
        pool.delta_transition(zero_router, &HashMap::new(), &Balances::default())
            .unwrap();
        assert_eq!(pool.mevx_router, None);

        let mut pool = gated_pool();
        let removed = ProtocolStateDelta {
            component_id: "pool".to_owned(),
            updated_attributes: HashMap::new(),
            deleted_attributes: HashSet::from(["mevx_router".to_string(), "incentive".to_string()]),
        };
        pool.delta_transition(removed, &HashMap::new(), &Balances::default())
            .unwrap();
        assert_eq!(pool.mevx_router, None);
        assert_eq!(pool.incentive, None);
    }
}
