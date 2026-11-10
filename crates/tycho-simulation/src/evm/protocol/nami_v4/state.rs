use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use alloy::primitives::{Address, Sign, I256, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::{trace, warn};
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
    swap_math::{
        amount_to_i256, get_token0_delta, get_token1_delta, move_price_towards_target,
        FEE_DENOMINATOR,
    },
};
use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        clmm::clmm_swap_to_price,
        safe_math::{safe_add_u256, safe_sub_u256},
        u256_num::u256_to_biguint,
        utils::{
            add_fee_markup,
            solidity_math::mul_div_rounding_up,
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
    simulation::PendingOverrides,
};

// Gas model of a Nami V4 (Algebra Integral) swap: what the pool's `swap` frame spends. The
// figures come from cold swaps on Nami's Base WETH/USDC pool `0x0E8AF367…`, traced on a Base fork
// at block 52562900 (no MEVX router) and block 52924436 (router set), in both directions, and
// from Base swap traces for what those swaps did not exercise. The executor's and callback's own
// gas, the input transfer included, is the execution gas estimator's business.

/// `swap` outside its external calls: state reads and writes, fee accounting and the swap loop.
/// Measured at 54.0k to 54.3k.
const SWAP_BASE_GAS: u64 = 54_000;
/// One `movePriceTowardsTarget` step plus its accounting, as in the Uniswap V3 model.
const GAS_PER_SWAP_MATH_STEP: u64 = 5_400;
/// Crossing an initialized tick: `ticks.cross` updates the tick's fee growth outside. About 22k
/// in Base swap traces.
const GAS_PER_INITIALIZED_TICK_CROSS: u64 = 22_000;
/// The pool's balance reads around the swap and the output transfer. Measured at 24.7k to 44.7k:
/// a recipient holding none of the output token yet costs 20k more, and USDC's proxy costs more
/// than WETH. Set near the recurring case, so the fork test's cold first swap comes out slightly
/// under and the warm ones slightly over.
const SWAP_SETTLEMENT_GAS: u64 = 30_000;
/// The pool's cold `CALL` into the plugin, on top of the gas the hook itself reports.
const PLUGIN_CALL_OVERHEAD_GAS: u64 = 2_600;
/// `afterSwap` with no MEVX router set: the plugin returns after its checks. Measured at 9.1k.
const AFTER_SWAP_GAS: u64 = 9_000;
/// `afterSwap` with a MEVX router set: the back-run check finds no arbitrage. Measured at 65.8k
/// to 66.8k. When it finds one it spends up to `call_gas_budget` more, and a solver should compare
/// quotes on the cost without it.
const AFTER_SWAP_ROUTER_GAS: u64 = 66_000;
/// `afterSwap` with a farming incentive set: updating the incentive's virtual pool, about 60k
/// more in Base swap traces when the swap crossed a tick.
const AFTER_SWAP_INCENTIVE_GAS: u64 = 60_000;
/// Gas budget of a single swap: the Ethereum transaction gas cap, as in the Uniswap V3 model.
const MAX_SWAP_GAS: u64 = 16_700_000;
/// Fixed plugin costs a swap can carry: `beforeSwap`, the call into it, and the dearest
/// `afterSwap` branch. Set aside before counting ticks.
const MAX_PLUGIN_GAS: u64 = 200_000;
/// Initialized ticks one swap can cross within `MAX_SWAP_GAS`.
const MAX_TICKS_CROSSED: u64 =
    (MAX_SWAP_GAS - SWAP_BASE_GAS - SWAP_SETTLEMENT_GAS - MAX_PLUGIN_GAS) /
        (GAS_PER_INITIALIZED_TICK_CROSS + GAS_PER_SWAP_MATH_STEP);

/// State of a Nami V4 pool, an Algebra Integral 1.2 fork.
///
/// Swap math runs natively. When the pool's plugin config enables `beforeSwap`, the fee of every
/// quote comes from running the plugin's `beforeSwap` hook in the EVM against indexed plugin
/// state; otherwise the pool's stored fee applies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamiV4State {
    pool: Address,
    liquidity: u128,
    sqrt_price: U256,
    tick: i32,
    /// Initialized ticks and their net liquidity, shared with the states a quote returns. A tick
    /// stays initialized while any position references it, so it can carry zero net liquidity
    /// and still bound a swap step.
    ticks: Arc<BTreeMap<i32, i128>>,
    /// `globalState.lastFee`: the fee used when the plugin does not override it.
    last_fee: u32,
    plugin: Option<Address>,
    plugin_config: u8,
    /// The plugin's MEVX router. While it is set, `afterSwap` checks for a back-run, which the
    /// gas estimate counts, and requires `min_gas_left` gas to remain at that hook.
    mevx_router: Option<Address>,
    /// Gas the plugin requires to remain when `afterSwap` runs with a router set. Reported for
    /// solvers; the quote does not depend on it.
    min_gas_left: u64,
    /// Gas the plugin forwards to its back-run self-call. Reported for solvers; the quote does
    /// not depend on it.
    call_gas_budget: u64,
    /// The farming incentive the plugin's `afterSwap` updates, when one is set. Its update is
    /// counted in the gas estimate.
    incentive: Option<Address>,
    /// Block the next quote is expected to execute in.
    execution_block_number: u64,
    execution_block_timestamp: u64,
    /// Plugin storage written by the simulated swaps this state was chained from, all in the
    /// execution block. The plugin writes its oracle timepoint once per block, so a later swap in
    /// the same block must see it.
    plugin_storage: StorageOverrides,
    /// Storage and block environment a pending quote runs the plugin under. `None` on confirmed
    /// state; set only on the clones `apply_deltas_ephemeral` hands out.
    #[serde(skip)]
    pending_overrides: Option<Arc<PendingOverrides>>,
}

impl PartialEq for NamiV4State {
    /// Pending overrides are quote-time context rather than pool state, so they do not count.
    fn eq(&self, other: &Self) -> bool {
        self.pool == other.pool &&
            self.liquidity == other.liquidity &&
            self.sqrt_price == other.sqrt_price &&
            self.tick == other.tick &&
            self.ticks == other.ticks &&
            self.last_fee == other.last_fee &&
            self.plugin == other.plugin &&
            self.plugin_config == other.plugin_config &&
            self.mevx_router == other.mevx_router &&
            self.min_gas_left == other.min_gas_left &&
            self.call_gas_budget == other.call_gas_budget &&
            self.incentive == other.incentive &&
            self.execution_block_number == other.execution_block_number &&
            self.execution_block_timestamp == other.execution_block_timestamp &&
            self.plugin_storage == other.plugin_storage
    }
}

impl Eq for NamiV4State {}

/// Fee a swap pays, the gas spent resolving it and the plugin storage it leaves behind.
#[derive(Clone)]
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
    // The pool's own fields in the pool's order. A wrapper type would only move the list.
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
            ticks: Arc::new(ticks.into_iter().collect()),
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
            pending_overrides: None,
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

    /// The fee a swap in the given direction would pay now, as a ratio.
    fn fee_ratio(&self, zero_for_one: bool) -> Result<f64, SimulationError> {
        let fee = self
            .resolve_fee(zero_for_one, I256::ONE, Self::default_price_limit(zero_for_one)?)?
            .fee;
        Ok(f64::from(fee) / f64::from(FEE_DENOMINATOR))
    }

    /// Whether the pool calls its plugin's `beforeSwap`, which then sets the fee.
    fn calls_before_swap(&self) -> bool {
        self.plugin.is_some() && self.plugin_config & BEFORE_SWAP_FLAG != 0
    }

    /// Gas the plugin's `afterSwap` spends on the branch its settings select, when the pool calls
    /// it: the back-run check with a router set, and the incentive update with one set.
    fn after_swap_gas(&self) -> u64 {
        if self.plugin_config & AFTER_SWAP_FLAG == 0 {
            return 0;
        }
        let back_run_check =
            if self.mevx_router.is_some() { AFTER_SWAP_ROUTER_GAS } else { AFTER_SWAP_GAS };
        let incentive = if self.incentive.is_some() { AFTER_SWAP_INCENTIVE_GAS } else { 0 };
        back_run_check + incentive
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
                pending_overrides: self.pending_overrides.as_deref(),
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

    /// Swaps at the fee the plugin or the pool sets for it. See [`Self::swap_with_fee`].
    fn swap(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
    ) -> Result<(SwapResults, StorageOverrides), SimulationError> {
        let price_limit =
            self.swap_price_limit(zero_for_one, amount_specified, sqrt_price_limit)?;
        let resolved_fee = self.resolve_fee(zero_for_one, amount_specified, price_limit)?;
        self.swap_with_fee(zero_for_one, amount_specified, price_limit, resolved_fee)
    }

    /// Checks the pool can run this swap and returns its price limit, the loosest one when none
    /// is given.
    fn swap_price_limit(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit: Option<U256>,
    ) -> Result<U256, SimulationError> {
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
        Ok(price_limit)
    }

    /// Mirrors `SwapCalculation._calculateSwap`: walks initialized ticks from the current price
    /// until the amount is used up or the price limit is reached. Also returns the plugin storage
    /// the swap leaves behind.
    fn swap_with_fee(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        price_limit: U256,
        resolved_fee: ResolvedFee,
    ) -> Result<(SwapResults, StorageOverrides), SimulationError> {
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
                .ok_or_else(|| {
                    SimulationError::FatalError("Swap overflow: amount required".into())
                })?;
            amount_calculated = calculated_change
                .and_then(|change| amount_calculated.checked_add(change))
                .ok_or_else(|| {
                    SimulationError::FatalError("Swap overflow: amount calculated".into())
                })?;

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

        gas_used += self.after_swap_gas();

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
    if value.len() > 32 {
        return u64::MAX;
    }
    u64::try_from(U256::from_be_slice(value)).unwrap_or(u64::MAX)
}

/// The tick index of a `ticks/<index>` attribute key, or `None` for any other key.
pub(super) fn parse_tick_key(key: &str) -> Option<Result<i32, String>> {
    let index = key.strip_prefix("ticks/")?;
    Some(
        index
            .parse::<i32>()
            .map_err(|err| format!("attribute {key}: {err}")),
    )
}

#[typetag::serde]
impl ProtocolSim for NamiV4State {
    /// The fee a zero-for-one swap would pay now. The trait cannot report an error, so when the
    /// plugin refuses the probe the stored fee stands in, with a warning. The plugin may charge
    /// the other direction a different fee; `spot_price` accounts for that, this single value
    /// cannot.
    fn fee(&self) -> f64 {
        self.fee_ratio(true).unwrap_or_else(|err| {
            warn!(pool = %self.pool, %err, "Nami V4 plugin refused the fee probe, using the stored fee");
            f64::from(self.last_fee) / f64::from(FEE_DENOMINATOR)
        })
    }

    /// The price of `a` in `b`, marked up by the fee of the swap that buys `a`: the one whose
    /// input is `b`. The plugin may charge the two directions different fees.
    fn spot_price(&self, a: &Token, b: &Token) -> Result<f64, SimulationError> {
        let price = if a < b {
            sqrt_price_q96_to_f64(self.sqrt_price, a.decimals, b.decimals)?
        } else {
            1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, b.decimals, a.decimals)?
        };
        Ok(add_fee_markup(price, self.fee_ratio(b < a)?))
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
        // The plugin decides whether the pool swaps at all, and its fee comes out of the input.
        let fee = self
            .resolve_fee(zero_for_one, I256::ONE, Self::default_price_limit(zero_for_one)?)?
            .fee;
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
            let amount_in_with_fee = mul_div_rounding_up(
                amount_in,
                U256::from(FEE_DENOMINATOR),
                U256::from(FEE_DENOMINATOR - fee),
            )?;
            total_in = safe_add_u256(total_in, amount_in_with_fee)?;
            total_out = safe_add_u256(total_out, amount_out)?;

            let net_liquidity = self
                .ticks
                .get(&step_tick)
                .copied()
                .unwrap_or(0);
            let liquidity_delta = if zero_for_one { -net_liquidity } else { net_liquidity };
            // A tick the state cannot cross ends the walk, as in the Uniswap V3 model.
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
        // Decode everything first, so a delta that fails to decode leaves the state untouched.
        let mut take = |key: &str| delta.updated_attributes.remove(key);
        let liquidity = take("liquidity").map(u128::from);
        let sqrt_price = take("sqrt_price_x96").map(|value| U256::from_be_slice(&value));
        let tick = take("tick").map(i32::from);
        let last_fee = take("fee").map(u32::from);
        let plugin = take("plugin")
            .map(|value| decode_optional_address(&value))
            .transpose()
            .map_err(decode_error)?;
        let plugin_config = take("plugin_config").map(u8::from);
        let mevx_router = take("mevx_router")
            .map(|value| decode_optional_address(&value))
            .transpose()
            .map_err(decode_error)?;
        let min_gas_left = take("min_gas_left").map(|value| decode_u64(&value));
        let call_gas_budget = take("call_gas_budget").map(|value| decode_u64(&value));
        let incentive = take("incentive")
            .map(|value| decode_optional_address(&value))
            .transpose()
            .map_err(decode_error)?;
        let mut tick_updates = Vec::new();
        for (key, value) in &delta.updated_attributes {
            if let Some(index) = parse_tick_key(key) {
                tick_updates.push((index.map_err(decode_error)?, i128::from(value.clone())));
            }
        }
        let mut tick_removals = Vec::new();
        for key in &delta.deleted_attributes {
            if let Some(index) = parse_tick_key(key) {
                tick_removals.push(index.map_err(decode_error)?);
            }
        }

        // Indexed state now includes whatever the simulated swaps anticipated.
        self.plugin_storage.clear();
        if let Some(liquidity) = liquidity {
            self.liquidity = liquidity;
        }
        if let Some(sqrt_price) = sqrt_price {
            self.sqrt_price = sqrt_price;
        }
        if let Some(tick) = tick {
            self.tick = tick;
        }
        if let Some(last_fee) = last_fee {
            self.last_fee = last_fee;
        }
        if let Some(plugin) = plugin {
            self.plugin = plugin;
        }
        if let Some(plugin_config) = plugin_config {
            self.plugin_config = plugin_config;
        }
        if let Some(mevx_router) = mevx_router {
            self.mevx_router = mevx_router;
        }
        if let Some(min_gas_left) = min_gas_left {
            self.min_gas_left = min_gas_left;
        }
        if let Some(call_gas_budget) = call_gas_budget {
            self.call_gas_budget = call_gas_budget;
        }
        if let Some(incentive) = incentive {
            self.incentive = incentive;
        }
        if !tick_updates.is_empty() || !tick_removals.is_empty() {
            let ticks = Arc::make_mut(&mut self.ticks);
            for (index, net_liquidity) in tick_updates {
                ticks.insert(index, net_liquidity);
            }
            for index in tick_removals {
                ticks.remove(&index);
            }
        }
        for key in &delta.deleted_attributes {
            match key.as_str() {
                "plugin" => self.plugin = None,
                "mevx_router" => self.mevx_router = None,
                "incentive" => self.incentive = None,
                _ => {}
            }
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
                // The plugin's fee does not depend on the amount, so one probe prices the swap.
                let resolved_fee = self.resolve_fee(
                    zero_for_one,
                    I256::ONE,
                    Self::default_price_limit(zero_for_one)?,
                )?;
                let mut plugin_storage = StorageOverrides::new();
                let (amount_in, amount_out, swap_result) = clmm_swap_to_price(
                    self.sqrt_price,
                    &params.token_in().address,
                    &params.token_out().address,
                    target,
                    resolved_fee.fee,
                    Sign::Positive,
                    |zero_for_one, amount_specified, sqrt_price_limit| {
                        let price_limit = self.swap_price_limit(
                            zero_for_one,
                            amount_specified,
                            Some(sqrt_price_limit),
                        )?;
                        let (result, storage) = self.swap_with_fee(
                            zero_for_one,
                            amount_specified,
                            price_limit,
                            resolved_fee.clone(),
                        )?;
                        plugin_storage = storage;
                        Ok(result)
                    },
                )?;
                if amount_in == BigUint::ZERO {
                    // No active liquidity between the price and the target: the helper reports a
                    // zeroed swap, and on chain the price would move for nothing.
                    return Err(SimulationError::RecoverableError(format!(
                        "Nami V4 pool {} has no liquidity between its price and the target",
                        self.pool
                    )));
                }

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
        timestamp_changed && self.calls_before_swap()
    }

    fn set_pending_overrides(
        &mut self,
        overrides: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), SimulationError> {
        let overrides = overrides
            .downcast::<PendingOverrides>()
            .map_err(|_| {
                SimulationError::FatalError(
                    "Nami V4 pending overrides must be `PendingOverrides`".to_string(),
                )
            })?;
        self.pending_overrides = Some(overrides);
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

    use alloy::sol_types::SolValue;
    use tycho_client::feed::BlockHeader;
    use tycho_common::models::Chain;

    use super::*;
    use crate::evm::{
        protocol::nami_v4::plugin::{BEFORE_SWAP_SELECTOR, DYNAMIC_FEE_FLAG},
        tycho_models::{AccountUpdate, ChangeType},
    };

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
    fn test_exact_input_within_one_range_charges_the_stored_fee() {
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
    fn test_swap_crosses_out_of_range_and_stops_at_the_limit() {
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
    fn test_zero_net_liquidity_tick_still_bounds_a_step() {
        let mut pool = static_fee_pool();
        Arc::make_mut(&mut pool.ticks).insert(-60, 0);

        assert_eq!(pool.prev_initialized_tick(-1), -60);
        assert_eq!(pool.prev_initialized_tick(-61), -600);
        assert_eq!(pool.next_initialized_tick(600), MAX_TICK);
    }

    #[test]
    fn test_uninitialized_pool_is_not_quotable() {
        let mut pool = static_fee_pool();
        pool.sqrt_price = U256::ZERO;

        assert!(pool
            .get_amount_out(BigUint::from(1u64), &token0(), &token1())
            .is_err());
    }

    #[test]
    fn test_delta_transition_updates_fee_plugin_and_ticks() {
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
    fn test_apply_block_reports_changes_only_for_plugin_fees() {
        let mut static_pool = static_fee_pool();
        assert!(!static_pool.apply_block(&BlockContext::new(101, 1_002)));

        let mut dynamic_pool = static_fee_pool();
        dynamic_pool.plugin = Some(Address::repeat_byte(0x22));
        dynamic_pool.plugin_config = BEFORE_SWAP_FLAG | DYNAMIC_FEE_FLAG;
        assert!(dynamic_pool.apply_block(&BlockContext::new(101, 1_002)));
        assert!(!dynamic_pool.apply_block(&BlockContext::new(101, 1_002)));
    }

    #[test]
    fn test_before_swap_flag_without_plugin_is_an_error() {
        let mut pool = static_fee_pool();
        pool.plugin_config = BEFORE_SWAP_FLAG;

        assert!(pool
            .get_amount_out(BigUint::from(1_000u64), &token0(), &token1())
            .is_err());
    }

    #[test]
    fn test_decode_optional_address_treats_empty_and_zero_as_none() {
        assert_eq!(decode_optional_address(&Bytes::new()), Ok(None));
        assert_eq!(decode_optional_address(&Bytes::from(vec![0u8; 20])), Ok(None));
        assert!(decode_optional_address(&Bytes::from(vec![1u8; 3])).is_err());
    }

    #[test]
    fn test_decode_u64_reads_any_width_and_saturates() {
        assert_eq!(decode_u64(&Bytes::new()), 0);
        assert_eq!(decode_u64(&Bytes::from(vec![0x0F, 0x42, 0x40])), 1_000_000);
        assert_eq!(decode_u64(&Bytes::from(1_000_000_u64)), 1_000_000);
        assert_eq!(decode_u64(&Bytes::from(vec![0xFF; 32])), u64::MAX);
        assert_eq!(decode_u64(&Bytes::from(vec![0x01; 33])), u64::MAX);
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
    fn test_delta_transition_updates_and_clears_the_plugin_settings() {
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

    /// EVM bytecode of a plugin whose `beforeSwap` answers with the override fee
    /// `zero_for_one_fee` for zero-for-one swaps and `one_for_zero_fee` otherwise, and no plugin
    /// fee.
    fn fee_plugin_code(zero_for_one_fee: u16, one_for_zero_fee: u16) -> Vec<u8> {
        assert!(one_for_zero_fee >= zero_for_one_fee);
        // PUSH4 selector, PUSH1 0xE0, SHL, PUSH1 0, MSTORE: the selector word at memory 0.
        let mut code = vec![0x63];
        code.extend_from_slice(&BEFORE_SWAP_SELECTOR);
        code.extend_from_slice(&[0x60, 0xE0, 0x1B, 0x60, 0x00, 0x52]);
        // one_for_zero_fee - zeroToOne * (one_for_zero_fee - zero_for_one_fee) at memory 32:
        // PUSH2, PUSH2, PUSH1 0x44, CALLDATALOAD, MUL, SWAP1, SUB, PUSH1 0x20, MSTORE.
        code.push(0x61);
        code.extend_from_slice(&one_for_zero_fee.to_be_bytes());
        code.push(0x61);
        code.extend_from_slice(&(one_for_zero_fee - zero_for_one_fee).to_be_bytes());
        code.extend_from_slice(&[0x60, 0x44, 0x35, 0x02, 0x90, 0x03, 0x60, 0x20, 0x52]);
        // PUSH1 0x60, PUSH1 0, RETURN: three words, the plugin fee word still zero.
        code.extend_from_slice(&[0x60, 0x60, 0x60, 0x00, 0xF3]);
        code
    }

    /// EVM bytecode that reverts every call with `Error(reason)`.
    fn reverting_plugin_code(reason: &str) -> Vec<u8> {
        let mut data = vec![0x08, 0xC3, 0x79, 0xA0];
        data.extend_from_slice(&reason.to_string().abi_encode());
        let mut code = Vec::new();
        for (index, word) in data.chunks(32).enumerate() {
            let mut padded = [0u8; 32];
            padded[..word.len()].copy_from_slice(word);
            // PUSH32 word, PUSH1 offset, MSTORE
            code.push(0x7F);
            code.extend_from_slice(&padded);
            code.extend_from_slice(&[0x60, u8::try_from(index * 32).unwrap(), 0x52]);
        }
        // PUSH1 length, PUSH1 0, REVERT
        code.extend_from_slice(&[0x60, u8::try_from(data.len()).unwrap(), 0x60, 0x00, 0xFD]);
        code
    }

    /// The static pool at `pool`, calling `plugin` for its fee, with both loaded into the shared
    /// simulation DB: the pool with a `globalState` slot, the plugin with `plugin_code`.
    fn plugin_pool(pool: Address, plugin: Address, plugin_code: Vec<u8>) -> NamiV4State {
        SHARED_TYCHO_DB
            .update(
                vec![
                    AccountUpdate::new(
                        pool,
                        Chain::Base,
                        HashMap::from([(U256::from(2), U256::ONE)]),
                        Some(U256::ZERO),
                        Some(vec![0x00]),
                        ChangeType::Creation,
                    ),
                    AccountUpdate::new(
                        plugin,
                        Chain::Base,
                        HashMap::new(),
                        Some(U256::ZERO),
                        Some(plugin_code),
                        ChangeType::Creation,
                    ),
                ],
                Some(BlockHeader { number: 100, timestamp: 1_000, ..Default::default() }),
            )
            .unwrap();
        let mut state = static_fee_pool();
        state.pool = pool;
        state.plugin = Some(plugin);
        state.plugin_config = BEFORE_SWAP_FLAG | DYNAMIC_FEE_FLAG;
        state
    }

    #[test]
    fn test_plugin_fee_follows_the_swap_direction() {
        let pool = plugin_pool(
            Address::repeat_byte(0xA1),
            Address::repeat_byte(0xA2),
            fee_plugin_code(1_000, 2_000),
        );

        assert_eq!(pool.fee_ratio(true).unwrap(), 0.001);
        assert_eq!(pool.fee_ratio(false).unwrap(), 0.002);
        assert_eq!(pool.fee(), 0.001);
        // Buying token0 with token1 is a one-for-zero swap, so that fee marks up token0's price.
        assert_eq!(
            pool.spot_price(&token0(), &token1())
                .unwrap(),
            add_fee_markup(1.0, 0.002)
        );
        assert_eq!(
            pool.spot_price(&token1(), &token0())
                .unwrap(),
            add_fee_markup(1.0, 0.001)
        );

        let amount_in = BigUint::from(10u64).pow(18);
        let quote = pool
            .get_amount_out(amount_in.clone(), &token0(), &token1())
            .unwrap();
        let fee_free = &amount_in - &amount_in / 1000u32;
        assert!(quote.amount < fee_free);
        assert!(quote.amount > &fee_free - &fee_free / 1000u32);
        let without_plugin =
            SWAP_BASE_GAS + GAS_PER_SWAP_MATH_STEP + SWAP_SETTLEMENT_GAS + PLUGIN_CALL_OVERHEAD_GAS;
        assert!(quote.gas > BigUint::from(without_plugin), "{}", quote.gas);
    }

    #[test]
    fn test_plugin_refusal_fails_quotes_prices_and_limits() {
        let pool = plugin_pool(
            Address::repeat_byte(0xB1),
            Address::repeat_byte(0xB2),
            reverting_plugin_code("Trading disabled"),
        );

        let err = pool
            .get_amount_out(BigUint::from(1_000u64), &token0(), &token1())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Trading disabled"),
            "{err}"
        );
        assert!(pool
            .spot_price(&token0(), &token1())
            .is_err());
        assert!(pool
            .get_limits(token0().address, token1().address)
            .is_err());
        // The trait's `fee` cannot fail, so the stored fee stands in.
        assert_eq!(pool.fee(), 0.003);
    }

    #[test]
    fn test_after_swap_gas_follows_the_plugin_settings() {
        let gas = |pool: &NamiV4State| {
            pool.get_amount_out(BigUint::from(10u64).pow(18), &token0(), &token1())
                .unwrap()
                .gas
        };
        let no_hook = static_fee_pool();
        let mut hooked = static_fee_pool();
        hooked.plugin = Some(Address::repeat_byte(0x22));
        hooked.plugin_config = AFTER_SWAP_FLAG;
        let router = Some(Address::repeat_byte(0x33));
        let routed = hooked
            .clone()
            .with_plugin_settings(router, 0, 5_000_000, None);
        let farmed = hooked.clone().with_plugin_settings(
            router,
            0,
            5_000_000,
            Some(Address::repeat_byte(0x44)),
        );

        assert_eq!(gas(&hooked), &gas(&no_hook) + AFTER_SWAP_GAS);
        assert_eq!(gas(&routed), &gas(&no_hook) + AFTER_SWAP_ROUTER_GAS);
        assert_eq!(gas(&farmed), &gas(&no_hook) + AFTER_SWAP_ROUTER_GAS + AFTER_SWAP_INCENTIVE_GAS);
    }

    #[test]
    fn test_bad_delta_leaves_the_state_untouched() {
        let mut pool = static_fee_pool();
        let before = pool.clone();
        let delta = ProtocolStateDelta {
            component_id: "pool".to_owned(),
            updated_attributes: HashMap::from([
                ("fee".to_string(), Bytes::from(500_u32.to_be_bytes().to_vec())),
                ("ticks/x".to_string(), Bytes::from(0_i128.to_be_bytes().to_vec())),
            ]),
            deleted_attributes: HashSet::new(),
        };

        let err = pool
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap_err();

        assert!(format!("{err:?}").contains("ticks/x"), "{err:?}");
        assert_eq!(pool, before);
    }
}
