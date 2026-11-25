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
    models::{token::Token, Chain},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::{
    evm::{
        protocol::{
            algebra_integral::{
                plugin::{
                    registry::{instantiate_plugin_handler, PluginCreationParams},
                    vm::AFTER_SWAP_ESTIMATE_GAS,
                    PluginHandler, PluginSwap, StorageOverrides, AFTER_SWAP_FLAG, BEFORE_SWAP_FLAG,
                    DYNAMIC_FEE_FLAG, PLUGIN_CONFIG_ATTRIBUTE,
                },
                swap_math::{
                    amount_to_i256, get_token0_delta, get_token1_delta, move_price_towards_target,
                    FEE_DENOMINATOR,
                },
            },
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
        simulation::{BlockEnvOverrides, PendingOverrides},
    },
    protocol::errors::InvalidSnapshotError,
};

// Gas model of an Algebra Integral swap: what the pool's `swap` frame spends outside its plugin.
// The figures come from cold swaps on Nami's Base WETH/USDC pool `0x0E8AF367…`, traced on a Base
// fork at block 52562900 (no MEVX router) and block 52924436 (router set), in both directions, and
// from Base swap traces for what those swaps did not exercise. The input transfer made in the
// swap callback is part of the figure: the execution gas estimator leaves it to the quote for a
// protocol that pays in a callback. The executor's own gas is the estimator's business.

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
/// The swap callback paying the input token to the pool. The transfer alone is 8.9k in Base
/// transaction `0x4cd89607…f82c` (block 61.6M), a USDC transfer to a pool that already holds
/// USDC. Through TychoRouter the callback costs 7k (WETH) to 18k more than that per hop, as the
/// Tycho team measured on Base. Set to the upper figure, so a WETH hop is over-estimated by
/// about 11k.
const SWAP_CALLBACK_GAS: u64 = 27_000;
/// Gas budget of a single swap: the Ethereum transaction gas cap, as in the Uniswap V3 model.
const MAX_SWAP_GAS: u64 = 16_700_000;
/// Plugin costs a swap can carry, `beforeSwap` and `afterSwap` together. Set aside before
/// counting ticks.
const MAX_PLUGIN_GAS: u64 = 250_000;
/// Initialized ticks one swap can cross within `MAX_SWAP_GAS`.
const MAX_TICKS_CROSSED: u64 =
    (MAX_SWAP_GAS - SWAP_BASE_GAS - SWAP_SETTLEMENT_GAS - SWAP_CALLBACK_GAS - MAX_PLUGIN_GAS) /
        (GAS_PER_INITIALIZED_TICK_CROSS + GAS_PER_SWAP_MATH_STEP);

/// Attribute holding the implementation a pool's plugin runs.
const PLUGIN_IMPL_ATTRIBUTE: &str = "plugin_impl";

/// How a pool's plugin takes part in its swaps.
#[derive(Debug, Clone)]
enum PluginSlot {
    /// The pool does not call `beforeSwap`: it charges the fee it last stored.
    NotCalled,
    Handler(Box<dyn PluginHandler>),
    /// The pool calls a plugin the simulation cannot reproduce, for the reason held. Such a
    /// pool is not quoted.
    Unsupported(String),
}

/// A pool's plugin as the indexer reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSetup {
    pub plugin: Option<Address>,
    /// The pool's `pluginConfig`: which hooks it calls and whether it accepts the plugin's fee.
    pub config: u8,
    /// The implementation the plugin runs, known for beacon proxies.
    pub implementation: Option<Address>,
    /// The `<module>/<field>` attributes decoded from the plugin's storage.
    pub attributes: HashMap<String, Bytes>,
}

/// State of an Algebra Integral 1.2 pool.
///
/// Swap math runs natively. When the pool's plugin config enables `beforeSwap`, the fee and the
/// hook gas of every quote come from the pool's [`PluginHandler`], built from the plugin's
/// implementation and module attributes; otherwise the pool's stored fee applies.
///
/// # What a quote assumes
///
/// - **The block.** A plugin's fee can move with `block.timestamp`. Nami's moves by the minute
///   around the NYSE open and close on a pool with a fee source, and by the second while a keeper's
///   fee override fades. A quote is priced for the execution block the state was last pointed at,
///   and a swap that lands in a later block can pay another fee.
/// - **The sender.** A plugin can price by sender. Nami's charges its MEVX executor a fee of 1;
///   quotes are for any other sender.
/// - **No backrun.** With a MEVX router set, Nami's `afterSwap` may swap the same pool again when
///   it finds an arbitrage. The swap's own output is as quoted, but the pool ends at another price
///   than the state a quote returns, and the backrun's gas is not in the quote.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "StateData", into = "StateData")]
pub struct AlgebraIntegralState {
    pool: Address,
    /// Chain the pool is on, which the plugin registry is keyed by. `None` for a state built
    /// without one, which can only hold a pool that does not call `beforeSwap`.
    chain: Option<Chain>,
    liquidity: u128,
    sqrt_price: U256,
    tick: i32,
    /// Initialized ticks and their net liquidity, shared with the states a quote returns. A tick
    /// stays initialized while any position references it, so it can carry zero net liquidity
    /// and still bound a swap step.
    ticks: Arc<BTreeMap<i32, i128>>,
    /// `globalState.lastFee`: the fee used when the plugin does not override it.
    last_fee: u32,
    plugin: PluginSetup,
    /// Block the next quote is expected to execute in.
    execution_block_number: u64,
    execution_block_timestamp: u64,
    /// Derived from `plugin`, `chain` and the execution block.
    plugin_slot: PluginSlot,
    /// Storage the plugin's hooks would find changed from the indexed state: what the swaps that
    /// led to this state wrote, as their quotes reported. Passed to every plugin quote, which
    /// only a handler that runs the hooks in the VM reads. Emptied by a delta, and neither
    /// serialized nor part of equality.
    plugin_overrides: Arc<StorageOverrides>,
}

/// Everything an [`AlgebraIntegralState`] is rebuilt from: its serialized form.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StateData {
    pool: Address,
    chain: Option<Chain>,
    liquidity: u128,
    sqrt_price: U256,
    tick: i32,
    ticks: BTreeMap<i32, i128>,
    last_fee: u32,
    plugin: PluginSetup,
    execution_block_number: u64,
    execution_block_timestamp: u64,
}

impl From<AlgebraIntegralState> for StateData {
    fn from(state: AlgebraIntegralState) -> Self {
        Self {
            pool: state.pool,
            chain: state.chain,
            liquidity: state.liquidity,
            sqrt_price: state.sqrt_price,
            tick: state.tick,
            ticks: Arc::unwrap_or_clone(state.ticks),
            last_fee: state.last_fee,
            plugin: state.plugin,
            execution_block_number: state.execution_block_number,
            execution_block_timestamp: state.execution_block_timestamp,
        }
    }
}

impl From<StateData> for AlgebraIntegralState {
    /// Rebuilds the plugin handler through the registry, so a plugin that has no handler
    /// registered in this process leaves the pool unquoted.
    fn from(data: StateData) -> Self {
        Self::new(
            data.pool,
            data.chain,
            data.liquidity,
            data.sqrt_price,
            data.tick,
            data.ticks.into_iter().collect(),
            data.last_fee,
            data.plugin,
            BlockContext::new(data.execution_block_number, data.execution_block_timestamp),
        )
    }
}

impl PartialEq for AlgebraIntegralState {
    /// The plugin handler is derived from the other fields, so it does not count. Neither do
    /// the plugin overrides, which are no part of the indexed state.
    fn eq(&self, other: &Self) -> bool {
        self.pool == other.pool &&
            self.chain == other.chain &&
            self.liquidity == other.liquidity &&
            self.sqrt_price == other.sqrt_price &&
            self.tick == other.tick &&
            self.ticks == other.ticks &&
            self.last_fee == other.last_fee &&
            self.plugin == other.plugin &&
            self.execution_block_number == other.execution_block_number &&
            self.execution_block_timestamp == other.execution_block_timestamp
    }
}

impl Eq for AlgebraIntegralState {}

/// Fee a swap pays and the gas the plugin's hooks spend on it.
#[derive(Clone, Copy)]
struct ResolvedFee {
    fee: u32,
    gas: u64,
}

/// Combines the pool's stored fee with the plugin's answer the way `AlgebraPool` does.
///
/// Errors when the pool would revert: a fee from the plugin without the `DYNAMIC_FEE` flag, or a
/// total fee at or above 100%.
fn effective_fee(
    last_fee: u32,
    plugin_config: u8,
    override_fee: Option<u32>,
    plugin_fee: u32,
) -> Result<u32, SimulationError> {
    let dynamic_fee_enabled = plugin_config & DYNAMIC_FEE_FLAG != 0;
    if !dynamic_fee_enabled && (override_fee.is_some() || plugin_fee > 0) {
        return Err(SimulationError::RecoverableError(
            "the plugin returned a fee while DYNAMIC_FEE is disabled; the pool would revert"
                .to_string(),
        ));
    }
    let fee = override_fee
        .unwrap_or(last_fee)
        .saturating_add(plugin_fee);
    if fee >= FEE_DENOMINATOR {
        return Err(SimulationError::RecoverableError(format!(
            "swap fee {fee} is not below 100%; the pool would revert"
        )));
    }
    Ok(fee)
}

impl AlgebraIntegralState {
    /// Creates an Algebra Integral pool state.
    ///
    /// `last_fee` is in hundredths of a bip (1_000_000 = 100%). `ticks` lists every initialized
    /// tick with its net liquidity, including ticks whose net liquidity is zero. `block` is the
    /// block the first quote is expected to execute in.
    ///
    /// A pool that calls `beforeSwap` on a plugin the simulation cannot reproduce (no plugin
    /// named, no chain given, no handler for the implementation, or a module configuration its
    /// handler rejects) is built all the same and is not quoted, as after a delta that brings it
    /// there. Failing instead would fail the decode of its snapshot, and with it every protocol's
    /// state in that message.
    #[expect(
        clippy::too_many_arguments,
        reason = "the pool's own fields in the pool's order; a wrapper type would only move the list"
    )]
    pub fn new(
        pool: Address,
        chain: Option<Chain>,
        liquidity: u128,
        sqrt_price: U256,
        tick: i32,
        ticks: Vec<(i32, i128)>,
        last_fee: u32,
        plugin: PluginSetup,
        block: BlockContext,
    ) -> Self {
        let mut state = AlgebraIntegralState {
            pool,
            chain,
            liquidity,
            sqrt_price,
            tick,
            ticks: Arc::new(ticks.into_iter().collect()),
            last_fee,
            plugin,
            execution_block_number: block.number(),
            execution_block_timestamp: block.timestamp(),
            plugin_slot: PluginSlot::NotCalled,
            plugin_overrides: Arc::default(),
        };
        state.rebuild_plugin_slot();
        state
    }

    /// Builds the handler for the plugin the state now describes. A plugin the simulation cannot
    /// reproduce stops the pool's quotes; it is not an error of the snapshot or delta, which
    /// would end the stream for every pool.
    fn rebuild_plugin_slot(&mut self) {
        self.plugin_slot = self
            .build_plugin_slot()
            .unwrap_or_else(|error| {
                warn!(pool = %self.pool, %error, "algebra pool's plugin is not supported");
                PluginSlot::Unsupported(error.to_string())
            });
    }

    /// The handler for the pool's plugin as the state now describes it.
    fn build_plugin_slot(&self) -> Result<PluginSlot, InvalidSnapshotError> {
        if self.plugin.config & BEFORE_SWAP_FLAG == 0 {
            return Ok(PluginSlot::NotCalled);
        }
        let Some(plugin) = self.plugin.plugin else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "algebra pool {} calls beforeSwap but names no plugin; the pool would revert",
                self.pool
            )));
        };
        let Some(chain) = self.chain else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "algebra pool {} calls its plugin {plugin}, which needs the pool's chain; register \
                 the decoder through TychoStreamDecoder or set DecoderContext::chain",
                self.pool
            )));
        };
        // A handler's composition depends on which hooks the pool calls.
        let mut attributes = self.plugin.attributes.clone();
        attributes
            .insert(PLUGIN_CONFIG_ATTRIBUTE.to_string(), Bytes::from(vec![self.plugin.config]));
        let handler = instantiate_plugin_handler(
            chain,
            self.plugin.implementation,
            PluginCreationParams {
                pool: self.pool,
                plugin,
                attributes: &attributes,
                block: BlockContext::new(
                    self.execution_block_number,
                    self.execution_block_timestamp,
                ),
            },
        )?;
        Ok(PluginSlot::Handler(handler))
    }

    /// The swap the plugin is asked about when only the fee of a direction matters. Nothing a
    /// native handler answers depends on the amount.
    fn fee_probe(&self, zero_for_one: bool) -> Result<PluginSwap, SimulationError> {
        Ok(PluginSwap {
            zero_for_one,
            amount_specified: I256::ONE,
            sqrt_price_limit: Self::default_price_limit(zero_for_one)?,
            sqrt_price: self.sqrt_price,
            tick: self.tick,
        })
    }

    /// The fee a swap in the given direction would pay now, as a ratio.
    fn fee_ratio(&self, zero_for_one: bool) -> Result<f64, SimulationError> {
        let (resolved_fee, _) = self.resolve_fee(&self.fee_probe(zero_for_one)?)?;
        Ok(f64::from(resolved_fee.fee) / f64::from(FEE_DENOMINATOR))
    }

    /// Resolves the fee the pool would charge for `swap` and the gas of the plugin's hooks,
    /// asking the plugin when the pool calls it before swaps. Also returns the storage the hooks
    /// would write, which the state after the swap carries to its own quotes.
    fn resolve_fee(
        &self,
        swap: &PluginSwap,
    ) -> Result<(ResolvedFee, StorageOverrides), SimulationError> {
        match &self.plugin_slot {
            PluginSlot::NotCalled => {
                // The pool still calls `afterSwap` when that flag is set. No handler is built for
                // such a pool, so the hook's gas is the estimate for an unmeasured plugin.
                let gas = if self.plugin.config & AFTER_SWAP_FLAG != 0 {
                    AFTER_SWAP_ESTIMATE_GAS
                } else {
                    0
                };
                Ok((ResolvedFee { fee: self.last_fee, gas }, StorageOverrides::new()))
            }
            PluginSlot::Unsupported(reason) => Err(SimulationError::RecoverableError(format!(
                "algebra pool {} is not quoted: {reason}",
                self.pool
            ))),
            PluginSlot::Handler(handler) => {
                let quote = handler.quote(swap, &self.plugin_overrides)?;
                let resolved_fee = ResolvedFee {
                    fee: effective_fee(
                        self.last_fee,
                        self.plugin.config,
                        quote.override_fee,
                        quote.plugin_fee,
                    )?,
                    gas: quote.gas,
                };
                Ok((resolved_fee, quote.storage_writes))
            }
        }
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
        let (resolved_fee, storage_writes) = self.resolve_fee(&PluginSwap {
            zero_for_one,
            amount_specified,
            sqrt_price_limit: price_limit,
            sqrt_price: self.sqrt_price,
            tick: self.tick,
        })?;
        let result =
            self.swap_with_fee(zero_for_one, amount_specified, price_limit, resolved_fee)?;
        Ok((result, storage_writes))
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
    /// until the amount is used up or the price limit is reached.
    fn swap_with_fee(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        price_limit: U256,
        resolved_fee: ResolvedFee,
    ) -> Result<SwapResults, SimulationError> {
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

        Ok(SwapResults {
            amount_calculated,
            amount_specified,
            amount_remaining: amount_required,
            sqrt_price: current_price,
            liquidity: current_liquidity,
            tick: current_tick.into(),
            gas_used: U256::from(gas_used + SWAP_SETTLEMENT_GAS + SWAP_CALLBACK_GAS),
        })
    }

    /// The state after a swap, carrying what the plugin's hooks wrote during it on top of what
    /// earlier swaps wrote.
    fn with_swap_result(&self, result: &SwapResults, storage_writes: StorageOverrides) -> Self {
        let mut new_state = self.clone();
        if !storage_writes.is_empty() {
            let overrides = Arc::make_mut(&mut new_state.plugin_overrides);
            for (address, slots) in storage_writes {
                overrides
                    .entry(address)
                    .or_default()
                    .extend(slots);
            }
        }
        new_state.liquidity = result.liquidity;
        new_state.tick = result.tick.value();
        new_state.sqrt_price = result.sqrt_price;
        new_state
    }
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

/// The tick index of a `ticks/<index>` attribute key, or `None` for any other key.
pub(super) fn parse_tick_key(key: &str) -> Option<Result<i32, String>> {
    let index = key.strip_prefix("ticks/")?;
    Some(
        index
            .parse::<i32>()
            .map_err(|err| format!("attribute {key}: {err}")),
    )
}

/// Decodes a `sqrt_price_x96` attribute, a big-endian unsigned integer of at most 32 bytes.
pub(super) fn decode_sqrt_price(value: &Bytes) -> Result<U256, String> {
    U256::try_from_be_slice(value)
        .ok_or_else(|| format!("sqrt_price_x96 is {} bytes wide, at most 32 fit", value.len()))
}

/// Whether `key` is a `<module>/<field>` attribute of the pool's plugin.
pub(super) fn is_plugin_module_attribute(key: &str) -> bool {
    key.contains('/') && !key.starts_with("ticks/")
}

#[typetag::serde]
impl ProtocolSim for AlgebraIntegralState {
    /// The fee a zero-for-one swap would pay now. The trait cannot report an error, so when the
    /// plugin refuses the probe the stored fee stands in, with a warning. The plugin may charge
    /// the other direction a different fee; `spot_price` accounts for that, this single value
    /// cannot.
    fn fee(&self) -> f64 {
        self.fee_ratio(true).unwrap_or_else(|err| {
            warn!(pool = %self.pool, %err, "algebra plugin refused the fee probe, using the stored fee");
            f64::from(self.last_fee) / f64::from(FEE_DENOMINATOR)
        })
    }

    /// The price of `base` in `quote`, marked up by the fee of the swap that buys `base`: the
    /// one whose input is `quote`. The plugin may charge the two directions different fees.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let price = if base < quote {
            sqrt_price_q96_to_f64(self.sqrt_price, base.decimals, quote.decimals)?
        } else {
            1.0f64 / sqrt_price_q96_to_f64(self.sqrt_price, quote.decimals, base.decimals)?
        };
        Ok(add_fee_markup(price, self.fee_ratio(quote < base)?))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let zero_for_one = token_in < token_out;
        let amount_specified = U256::try_from_be_slice(&amount_in.to_bytes_be())
            .and_then(|amount| I256::checked_from_sign_and_abs(Sign::Positive, amount))
            .ok_or_else(|| {
                SimulationError::InvalidInput("I256 overflow: amount_in".to_string(), None)
            })?;

        let (result, storage_writes) = self.swap(zero_for_one, amount_specified, None)?;
        trace!(?amount_in, ?token_in, ?token_out, ?zero_for_one, ?result, "ALGEBRA INTEGRAL SWAP");

        let amount_out = u256_to_biguint(
            result
                .amount_calculated
                .saturating_abs()
                .into_raw(),
        );
        let gas = u256_to_biguint(result.gas_used);
        let new_state = Box::new(self.with_swap_result(&result, storage_writes));
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
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let zero_for_one = sell_token < buy_token;
        // A plugin that stops the pool from swapping leaves nothing to trade.
        if let PluginSlot::Handler(handler) = &self.plugin_slot {
            if !handler.swaps_enabled() {
                return Ok((BigUint::ZERO, BigUint::ZERO));
            }
        }
        // The fee comes out of the input.
        let (ResolvedFee { fee, .. }, _) = self.resolve_fee(&self.fee_probe(zero_for_one)?)?;
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
        let liquidity = take("liquidity")
            .map(u128::try_from)
            .transpose()?;
        let sqrt_price = take("sqrt_price_x96")
            .map(|value| decode_sqrt_price(&value))
            .transpose()
            .map_err(TransitionError::DecodeError)?;
        let tick = take("tick")
            .map(i32::try_from)
            .transpose()?;
        let last_fee = take("fee")
            .map(u32::try_from)
            .transpose()?;
        let plugin = take("plugin")
            .map(|value| decode_optional_address(&value))
            .transpose()
            .map_err(TransitionError::DecodeError)?;
        let plugin_config = take(PLUGIN_CONFIG_ATTRIBUTE)
            .map(u8::try_from)
            .transpose()?;
        let implementation = take(PLUGIN_IMPL_ATTRIBUTE)
            .map(|value| decode_optional_address(&value))
            .transpose()
            .map_err(TransitionError::DecodeError)?;
        let mut tick_updates = Vec::new();
        let mut module_updates = Vec::new();
        for (key, value) in delta.updated_attributes {
            if let Some(index) = parse_tick_key(&key) {
                tick_updates
                    .push((index.map_err(TransitionError::DecodeError)?, i128::try_from(value)?));
            } else if is_plugin_module_attribute(&key) {
                module_updates.push((key, value));
            }
        }
        let mut tick_removals = Vec::new();
        for key in &delta.deleted_attributes {
            if let Some(index) = parse_tick_key(key) {
                tick_removals.push(index.map_err(TransitionError::DecodeError)?);
            }
        }

        let mut plugin_changed = plugin.is_some() ||
            plugin_config.is_some() ||
            implementation.is_some() ||
            !module_updates.is_empty();
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
            self.plugin.plugin = plugin;
        }
        if let Some(plugin_config) = plugin_config {
            self.plugin.config = plugin_config;
        }
        if let Some(implementation) = implementation {
            self.plugin.implementation = implementation;
        }
        for (key, value) in module_updates {
            self.plugin
                .attributes
                .insert(key, value);
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
                "plugin" => {
                    self.plugin.plugin = None;
                    plugin_changed = true;
                }
                PLUGIN_IMPL_ATTRIBUTE => {
                    self.plugin.implementation = None;
                    plugin_changed = true;
                }
                key if is_plugin_module_attribute(key) => {
                    plugin_changed |= self
                        .plugin
                        .attributes
                        .remove(key)
                        .is_some();
                }
                _ => {}
            }
        }
        // The delta is the indexed state of a new block, which already holds what its swaps wrote.
        self.plugin_overrides = Arc::default();
        if plugin_changed {
            self.rebuild_plugin_slot();
        }
        Ok(())
    }

    /// See [`ProtocolSim::query_pool_swap`] for the trait documentation.
    ///
    /// Swaps an unbounded amount of `token_in` until the pool reaches the target price.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "Algebra Integral does not support TradeLimitPrice constraint in query_pool_swap"
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
                let (resolved_fee, storage_writes) =
                    self.resolve_fee(&self.fee_probe(zero_for_one)?)?;
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
                        self.swap_with_fee(
                            zero_for_one,
                            amount_specified,
                            price_limit,
                            resolved_fee,
                        )
                    },
                )?;
                if amount_in == BigUint::ZERO {
                    // No active liquidity between the price and the target: the helper reports a
                    // zeroed swap, and on chain the price would move for nothing.
                    return Err(SimulationError::RecoverableError(format!(
                        "algebra pool {} has no liquidity between its price and the target",
                        self.pool
                    )));
                }

                Ok(PoolSwap::new(
                    amount_in,
                    amount_out,
                    Box::new(self.with_swap_result(&swap_result, storage_writes)),
                    None,
                ))
            }
        }
    }

    /// Records the block the next quote executes in and moves the plugin to it. Reports a
    /// change when the plugin's fees or gate differ in the new block.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        self.execution_block_number = block.number();
        self.execution_block_timestamp = block.timestamp();
        match &mut self.plugin_slot {
            PluginSlot::Handler(handler) => handler.apply_block(block),
            PluginSlot::NotCalled | PluginSlot::Unsupported(_) => false,
        }
    }

    /// Runs the plugin of every quote from this state against the pending block: its storage
    /// joins the overrides the plugin is quoted with, under what swaps from this state wrote,
    /// and its block becomes the execution block. `overrides` must be [`PendingOverrides`].
    fn set_pending_overrides(
        &mut self,
        overrides: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), SimulationError> {
        #[expect(
            clippy::map_err_ignore,
            reason = "the downcast error is the original `Arc<dyn Any>`, which has no message"
        )]
        let pending = overrides
            .downcast::<PendingOverrides>()
            .map_err(|_| {
                SimulationError::FatalError(
                    "Algebra Integral pending overrides must be `PendingOverrides`".to_string(),
                )
            })?;
        if let Some(storage) = &pending.storage {
            let mut merged = storage.clone();
            for (address, slots) in self.plugin_overrides.iter() {
                merged
                    .entry(*address)
                    .or_default()
                    .extend(
                        slots
                            .iter()
                            .map(|(slot, value)| (*slot, *value)),
                    );
            }
            self.plugin_overrides = Arc::new(merged);
        }
        if let Some(BlockEnvOverrides { number: Some(number), timestamp: Some(timestamp) }) =
            &pending.block
        {
            self.apply_block(&BlockContext::new(*number, *timestamp));
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
        other
            .as_any()
            .downcast_ref::<AlgebraIntegralState>()
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{collections::HashMap, str::FromStr};

    use alloy::primitives::{address, Address};
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use super::PluginSetup;
    use crate::evm::protocol::algebra_integral::plugin::deployments::initialize_plugin_handlers;

    /// `NamiPluginV1Diamond` on Robinhood.
    pub const PLUGIN_IMPL_ROBINHOOD: Address = address!("1a2898f77957B9C5B7bAfD29a2dAFBA4404b6874");
    /// `NamiPluginV1WithAntiSniperDiamond` on Base.
    pub const ANTI_SNIPER_IMPL_BASE: Address = address!("9c5D328a1bDdb841E889d1976670F3C0C153D037");
    pub const PLUGIN: Address = address!("b0918bddea10fcc127bbe816725b2be536cebb8a");
    /// BEFORE_SWAP, AFTER_SWAP, AFTER_INIT and DYNAMIC_FEE: the config of Nami's pools.
    pub const NAMI_PLUGIN_CONFIG: u8 = 195;

    /// Monday 6 July 2026 15:00 UTC: the NYSE session, segment 2.
    pub const SESSION: u64 = 1_783_350_000;
    /// Saturday 11 July 2026 12:00 UTC: closed, segment 0.
    pub const WEEKEND: u64 = 1_783_771_200;

    pub fn register_handlers() {
        initialize_plugin_handlers().expect("plugin handler registration should succeed");
    }

    fn token(address: &str, symbol: &str) -> Token {
        Token::new(
            &Bytes::from_str(address).expect("a valid address"),
            symbol,
            18,
            0,
            &[Some(10_000)],
            Chain::Robinhood,
            100,
        )
    }

    pub fn token0() -> Token {
        token("0x1000000000000000000000000000000000000000", "T0")
    }

    pub fn token1() -> Token {
        token("0x2000000000000000000000000000000000000000", "T1")
    }

    /// The module attributes of plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at
    /// block 118127446: base fee 500, floor 1000, cap 20000, the NYSE source with a fee per
    /// segment, a MEVX router and a farming incentive.
    pub fn robinhood_module_attributes() -> HashMap<String, Bytes> {
        let fields: [(&str, &str); 19] = [
            ("dynamic_fee/alpha1", "0000"),
            ("dynamic_fee/alpha2", "0000"),
            ("dynamic_fee/base_fee", "01f4"),
            ("fee_override/floor", "03e8"),
            ("fee_override/cap", "4e20"),
            ("fee_override/premium_zero_for_one", "0000"),
            ("fee_override/premium_one_for_zero", "0000"),
            ("fee_override/decay_window", "00003840"),
            ("fee_override/fee_zero_for_one", "0000"),
            ("fee_override/fee_one_for_zero", "0000"),
            ("fee_override/expires_at", "0000000000"),
            ("fee_override/source_enabled", "01"),
            ("fee_override/max_discount_bps", "0000"),
            ("fee_override/source", "622b1971f9792a2d410b14b8108d5481c1d2c362"),
            // Segment 0 is the low 16 bits of the word.
            (
                "fee_override/segment_fees",
                "000000000000000000000000000000000bb804b006400bb809600578044c03e8",
            ),
            ("mevx/router", "011242bf9f5c4832f152be5a5838daae40e0405c"),
            ("farming/incentive", "d877d4a9ffddd1b03fdfe709b9b0770a4d5dd3fe"),
            // The AntiSniper module of Base plugin 0x53ce1d109b8ffe35010baaac91a3d839f4b05fea:
            // a 30 minute period. Read only by the implementation that contains the module.
            ("anti_sniper/period", "00000708"),
            ("anti_sniper/starting_fee", "0cb5da"),
        ];
        fields
            .into_iter()
            .map(|(name, value)| {
                (name.to_string(), Bytes::from(alloy::hex::decode(value).expect("valid hex")))
            })
            .collect()
    }

    pub fn robinhood_plugin() -> PluginSetup {
        PluginSetup {
            plugin: Some(PLUGIN),
            config: NAMI_PLUGIN_CONFIG,
            implementation: Some(PLUGIN_IMPL_ROBINHOOD),
            attributes: robinhood_module_attributes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rstest::rstest;

    use super::{test_support::*, *};

    const LIQUIDITY: u128 = 1_000_000_000_000_000_000_000;
    const LIQUIDITY_NET: i128 = 1_000_000_000_000_000_000_000;
    /// Hook gas of the Robinhood plugin: `beforeSwap` with a fee source, `afterSwap` with a MEVX
    /// router, and the farming incentive.
    const ROBINHOOD_HOOK_GAS: u64 = 67_000 + 6_000 + 44_000 + 3_000;

    /// A pool at price 1 (tick 0) with one position over [-600, 600] and a stored fee of 0.3%.
    fn pool(chain: Option<Chain>, plugin: PluginSetup, timestamp: u64) -> AlgebraIntegralState {
        register_handlers();
        AlgebraIntegralState::new(
            Address::repeat_byte(0x11),
            chain,
            LIQUIDITY,
            U256::ONE << 96,
            0,
            vec![(-600, LIQUIDITY_NET), (600, -LIQUIDITY_NET)],
            3_000,
            plugin,
            BlockContext::new(100, timestamp),
        )
    }

    fn static_fee_pool() -> AlgebraIntegralState {
        pool(None, PluginSetup::default(), 1_000)
    }

    fn robinhood_pool(timestamp: u64) -> AlgebraIntegralState {
        pool(Some(Chain::Robinhood), robinhood_plugin(), timestamp)
    }

    fn delta(updated: &[(&str, &[u8])], deleted: &[&str]) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: "pool".to_owned(),
            updated_attributes: updated
                .iter()
                .map(|(name, value)| (name.to_string(), Bytes::from(value.to_vec())))
                .collect(),
            deleted_attributes: deleted
                .iter()
                .map(|name| name.to_string())
                .collect::<HashSet<_>>(),
        }
    }

    /// A pool on Ethereum whose plugin has no native handler, so its hook runs in the VM: it
    /// answers with the fee in its storage slot 0, 1000 at first, and then stores 3000 there.
    fn vm_plugin_pool(pool_byte: u8) -> (AlgebraIntegralState, Address) {
        use crate::evm::protocol::algebra_integral::plugin::vm::test_support::{
            load_pool_and_plugin, stateful_plugin_code,
        };
        let (address, plugin) =
            (Address::repeat_byte(pool_byte), Address::repeat_byte(pool_byte + 1));
        load_pool_and_plugin(
            address,
            plugin,
            stateful_plugin_code(3_000),
            HashMap::from([(U256::ZERO, U256::from(1_000))]),
        );
        let setup = PluginSetup {
            plugin: Some(plugin),
            config: BEFORE_SWAP_FLAG | DYNAMIC_FEE_FLAG,
            implementation: Some(Address::repeat_byte(0xc4)),
            attributes: HashMap::new(),
        };
        let mut state = pool(Some(Chain::Ethereum), setup, 1_000);
        state.pool = address;
        state.plugin_slot = state
            .build_plugin_slot()
            .expect("the VM handler should build");
        (state, plugin)
    }

    #[test]
    fn vm_plugin_sees_what_an_earlier_swap_wrote_until_the_next_delta() {
        let (pool, _) = vm_plugin_pool(0xB1);
        assert_eq!(pool.fee(), 0.001);

        let quote = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect("the VM plugin should quote");
        let mut after_swap = quote
            .new_state
            .as_any()
            .downcast_ref::<AlgebraIntegralState>()
            .expect("an Algebra state")
            .clone();

        // The hook stored 3000 during the swap; the pool it was quoted from is unchanged.
        assert_eq!(after_swap.fee(), 0.003);
        assert_eq!(pool.fee(), 0.001);

        // A delta is the indexed state of a new block, which replaces what was anticipated.
        apply(&mut after_swap, delta(&[], &[]));
        assert_eq!(after_swap.fee(), 0.001);
    }

    #[test]
    fn pending_overrides_reach_the_vm_plugin_under_later_swap_writes() {
        let (mut pool, plugin) = vm_plugin_pool(0xB3);
        let pending = PendingOverrides {
            storage: Some(HashMap::from([(
                plugin,
                HashMap::from([(U256::ZERO, U256::from(2_000))]),
            )])),
            native_balances: None,
            block: Some(BlockEnvOverrides { number: Some(101), timestamp: Some(1_012) }),
        };

        pool.set_pending_overrides(Arc::new(pending))
            .expect("pending overrides should be accepted");

        assert_eq!(pool.fee(), 0.002);
        assert_eq!(pool.execution_block_number, 101);
        assert_eq!(pool.execution_block_timestamp, 1_012);
        let quote = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect("the VM plugin should quote");
        assert_eq!(quote.new_state.fee(), 0.003);
    }

    #[test]
    fn pending_overrides_of_another_type_are_rejected() {
        let mut pool = static_fee_pool();

        assert!(pool
            .set_pending_overrides(Arc::new(0u8))
            .is_err());
    }

    fn apply(pool: &mut AlgebraIntegralState, delta: ProtocolStateDelta) {
        pool.delta_transition(delta, &HashMap::new(), &Balances::default())
            .expect("the delta should apply");
    }

    fn one_token() -> BigUint {
        BigUint::from(10u64).pow(18)
    }

    #[test]
    fn exact_input_within_one_range_charges_the_stored_fee() {
        let pool = static_fee_pool();

        let result = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect("the swap should quote");

        // At price 1 with deep liquidity, output is just below input minus the 0.3% fee.
        let fee_free = one_token() - one_token() * 3u32 / 1000u32;
        assert!(result.amount < fee_free);
        assert!(result.amount > &fee_free - &fee_free / 1000u32);
        // No plugin: the swap frame, one step and the settlement.
        assert_eq!(result.gas, BigUint::from(54_000u64 + 5_400 + 30_000 + 27_000));
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<AlgebraIntegralState>()
            .expect("the new state should be an Algebra pool");
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
            .expect_err("the price limit should be reached");

        let SimulationError::InvalidInput(_, Some(partial)) = err else {
            panic!("expected a partial result, got {err:?}");
        };
        let new_state = partial
            .new_state
            .as_any()
            .downcast_ref::<AlgebraIntegralState>()
            .expect("the new state should be an Algebra pool");
        assert_eq!(new_state.liquidity, 0);
        assert!(new_state.tick < -600);
        let (max_in, max_out) = pool
            .get_limits(token0().address, token1().address)
            .expect("limits");
        assert!(partial.amount <= max_out);
        assert!(max_in > BigUint::ZERO);
    }

    #[test]
    fn zero_net_liquidity_tick_still_bounds_a_step() {
        let mut pool = static_fee_pool();
        Arc::make_mut(&mut pool.ticks).insert(-60, 0);

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
    fn decode_optional_address_treats_empty_and_zero_as_none() {
        assert_eq!(decode_optional_address(&Bytes::new()), Ok(None));
        assert_eq!(decode_optional_address(&Bytes::from(vec![0u8; 20])), Ok(None));
        assert!(decode_optional_address(&Bytes::from(vec![1u8; 3])).is_err());
    }

    #[rstest]
    #[case::override_replaces_the_stored_fee(Some(3_000), 0, Some(3_000))]
    #[case::no_override_keeps_the_stored_fee(None, 100, Some(600))]
    #[case::plugin_fee_is_added(Some(3_000), 100, Some(3_100))]
    #[case::total_at_one_hundred_percent_reverts(Some(999_999), 1, None)]
    fn effective_fee_follows_the_pool(
        #[case] override_fee: Option<u32>,
        #[case] plugin_fee: u32,
        #[case] expected: Option<u32>,
    ) {
        let fee = effective_fee(500, NAMI_PLUGIN_CONFIG, override_fee, plugin_fee);

        assert_eq!(fee.ok(), expected);
    }

    #[rstest]
    #[case::override_fee(Some(3_000), 0)]
    #[case::plugin_fee(None, 100)]
    fn fee_from_the_plugin_without_the_dynamic_fee_flag_reverts(
        #[case] override_fee: Option<u32>,
        #[case] plugin_fee: u32,
    ) {
        assert!(effective_fee(500, BEFORE_SWAP_FLAG, override_fee, plugin_fee).is_err());
        assert_eq!(effective_fee(500, BEFORE_SWAP_FLAG, None, 0).expect("no fee, no revert"), 500);
    }

    #[rstest]
    #[case::session(SESSION, 1_400)]
    #[case::weekend(WEEKEND, 1_000)]
    fn plugin_sets_the_fee_of_the_execution_block(#[case] timestamp: u64, #[case] fee: u32) {
        let pool = robinhood_pool(timestamp);

        assert_eq!(pool.fee(), f64::from(fee) / 1e6);
        let quote = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect("the swap should quote");
        let stored_fee_quote = static_fee_pool()
            .get_amount_out(one_token(), &token0(), &token1())
            .expect("the swap should quote");
        // The plugin's fee is below the stored 0.3%, so the swap returns more.
        assert!(quote.amount > stored_fee_quote.amount);
        assert_eq!(quote.gas, stored_fee_quote.gas + BigUint::from(ROBINHOOD_HOOK_GAS));
    }

    #[test]
    fn fee_comes_out_of_the_input_in_both_directions() {
        let pool = robinhood_pool(SESSION);
        let mut free = robinhood_pool(SESSION);
        free.plugin_slot = PluginSlot::NotCalled;
        free.last_fee = 0;

        for (token_in, token_out) in [(token0(), token1()), (token1(), token0())] {
            let charged = pool
                .get_amount_out(one_token(), &token_in, &token_out)
                .expect("the swap should quote");
            let fee_free = free
                .get_amount_out(one_token(), &token_in, &token_out)
                .expect("the swap should quote");
            assert!(
                charged.amount < fee_free.amount,
                "{} -> {}",
                token_in.symbol,
                token_out.symbol
            );
        }
    }

    #[test]
    fn apply_block_reports_a_change_when_the_segment_changes() {
        let mut pool = robinhood_pool(SESSION);

        assert!(!pool.apply_block(&BlockContext::new(101, SESSION + 12)));
        assert!(pool.apply_block(&BlockContext::new(102, WEEKEND)));
        assert_eq!(pool.fee(), 0.001);
        assert!(!static_fee_pool().apply_block(&BlockContext::new(101, WEEKEND)));
    }

    #[test]
    fn delta_updates_pool_fields_and_ticks() {
        let mut pool = static_fee_pool();

        apply(
            &mut pool,
            delta(
                &[
                    ("fee", &500_u32.to_be_bytes()),
                    ("liquidity", &7_u128.to_be_bytes()),
                    ("tick", &(-5_i32).to_be_bytes()),
                    ("sqrt_price_x96", &[0x01, 0x00]),
                    ("ticks/-60", &0_i128.to_be_bytes()),
                    ("some_future_attribute", &[0x01]),
                ],
                &["ticks/600"],
            ),
        );

        assert_eq!(pool.last_fee, 500);
        assert_eq!(pool.liquidity, 7);
        assert_eq!(pool.tick, -5);
        assert_eq!(pool.sqrt_price, U256::from(256));
        assert_eq!(pool.ticks.get(&-60), Some(&0));
        assert!(!pool.ticks.contains_key(&600));
    }

    #[test]
    fn delta_to_a_module_attribute_rebuilds_the_plugin() {
        let mut pool = robinhood_pool(SESSION);

        apply(&mut pool, delta(&[("fee_override/floor", &2_000_u16.to_be_bytes())], &[]));

        assert_eq!(pool.fee(), 0.002);
    }

    #[test]
    fn pool_that_loses_a_module_attribute_is_not_quoted() {
        let mut pool = robinhood_pool(SESSION);

        apply(&mut pool, delta(&[], &["fee_override/cap"]));

        let error = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect_err("a module with a missing field should not quote");
        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
    }

    #[rstest]
    #[case::unknown_fee_source(&[("fee_override/source", &[0xf5u8; 20] as &[u8])], &[] as &[&str])]
    #[case::adaptive_fee(&[("dynamic_fee/alpha1", &[0x05u8, 0xdc] as &[u8])], &[] as &[&str])]
    #[case::unknown_implementation(&[("plugin_impl", &[0xc4u8; 20] as &[u8])], &[] as &[&str])]
    #[case::implementation_removed(&[] as &[(&str, &[u8])], &["plugin_impl"] as &[&str])]
    #[case::plugin_removed(&[] as &[(&str, &[u8])], &["plugin"] as &[&str])]
    fn delta_the_simulation_cannot_reproduce_stops_the_quotes_until_it_is_undone(
        #[case] updated: &[(&str, &[u8])],
        #[case] deleted: &[&str],
    ) {
        let mut pool = robinhood_pool(SESSION);

        apply(&mut pool, delta(updated, deleted));

        let error = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect_err("the pool should not be quoted");
        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
        assert!(pool
            .get_limits(token0().address, token1().address)
            .is_err());
        assert!(pool
            .spot_price(&token0(), &token1())
            .is_err());

        let original = robinhood_plugin();
        let implementation = original
            .implementation
            .expect("the fixture has an implementation");
        apply(
            &mut pool,
            delta(
                &[
                    ("plugin", PLUGIN.as_slice()),
                    ("plugin_impl", implementation.as_slice()),
                    ("fee_override/source", &original.attributes["fee_override/source"]),
                    ("dynamic_fee/alpha1", &[0, 0]),
                ],
                &[],
            ),
        );
        assert_eq!(pool.fee(), 0.0014);
    }

    #[test]
    fn malformed_delta_leaves_the_state_untouched() {
        let mut pool = robinhood_pool(SESSION);
        let before = pool.clone();

        let result = pool.delta_transition(
            delta(
                &[("fee", &500_u32.to_be_bytes()), ("tick", &[0x01; 9]), ("ticks/x", &[0x01])],
                &[],
            ),
            &HashMap::new(),
            &Balances::default(),
        );

        assert!(result.is_err());
        assert_eq!(pool, before);
        assert_eq!(pool.fee(), before.fee());
    }

    #[rstest]
    #[case::no_handler_on_this_chain(
        Some(Chain::Base),
        robinhood_plugin(),
        "no native handler registered"
    )]
    #[case::no_chain(None, robinhood_plugin(), "needs the pool's chain")]
    #[case::no_plugin(Some(Chain::Robinhood), PluginSetup { plugin: None, ..robinhood_plugin() }, "names no plugin")]
    #[case::no_implementation(Some(Chain::Robinhood), PluginSetup { implementation: None, ..robinhood_plugin() }, "an unknown implementation")]
    fn pool_calling_a_plugin_that_cannot_be_reproduced_builds_and_is_not_quoted(
        #[case] chain: Option<Chain>,
        #[case] plugin: PluginSetup,
        #[case] reason: &str,
    ) {
        let pool = pool(chain, plugin, SESSION);

        let error = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect_err("the pool should not be quoted");
        let SimulationError::RecoverableError(message) = &error else {
            panic!("expected a recoverable error, got {error:?}");
        };
        assert!(message.contains(reason), "{message}");
        assert!(pool
            .get_limits(token0().address, token1().address)
            .is_err());
        assert!(pool
            .spot_price(&token0(), &token1())
            .is_err());
    }

    /// A pool whose fee is adaptive in its snapshot is in the same place as one that turns
    /// adaptive in a delta: built, not quoted, and quoted again once the fee is made flat.
    #[test]
    fn pool_with_an_adaptive_fee_at_snapshot_is_quoted_once_the_fee_is_flat() {
        let mut plugin = robinhood_plugin();
        plugin
            .attributes
            .insert("dynamic_fee/alpha1".to_string(), Bytes::from(vec![0x05, 0xdc]));
        let mut pool = pool(Some(Chain::Robinhood), plugin, SESSION);
        assert!(pool
            .get_amount_out(one_token(), &token0(), &token1())
            .is_err());

        apply(&mut pool, delta(&[("dynamic_fee/alpha1", &[0, 0])], &[]));

        assert_eq!(pool.fee(), 0.0014);
    }

    /// Quotes against the contract: real swaps of Nami's Base WETH/USDC pool, in both directions,
    /// one of them across an initialized tick. Each is quoted from the pool's state at the block
    /// before with the fee the plugin set, and has to give the amount the pool paid and the price,
    /// tick and liquidity it ended at.
    #[test]
    fn quotes_equal_the_pools_swaps_on_chain() {
        let field = |fields: &[&str], index: usize| -> String {
            fields
                .get(index)
                .expect("a fixture line has thirteen fields")
                .to_string()
        };
        let mut directions = HashSet::new();
        let mut crossed_a_tick = false;
        for line in include_str!("testdata/base_weth_usdc_swaps.csv")
            .lines()
            .filter(|line| !line.starts_with('#'))
        {
            let fields: Vec<&str> = line.split(',').collect();
            let number = |index: usize| field(&fields, index);
            let tx = number(0);
            let zero_for_one = number(2) == "true";
            let liquidity: u128 = number(8).parse().expect("liquidity");
            let liquidity_after: u128 = number(11)
                .parse()
                .expect("liquidity after");
            let ticks = number(12)
                .split(';')
                .map(|tick| {
                    let (index, net) = tick.split_once(':').expect("index:net");
                    (index.parse().expect("a tick index"), net.parse().expect("net liquidity"))
                })
                .collect();
            let pool = AlgebraIntegralState::new(
                Address::repeat_byte(0x11),
                None,
                liquidity,
                U256::from_str_radix(&number(6), 10).expect("price"),
                number(7).parse().expect("tick"),
                ticks,
                number(5).parse().expect("fee"),
                PluginSetup::default(),
                BlockContext::new(number(1).parse().expect("block"), SESSION),
            );
            let (token_in, token_out) =
                if zero_for_one { (token0(), token1()) } else { (token1(), token0()) };

            let quote = pool
                .get_amount_out(number(3).parse().expect("amount in"), &token_in, &token_out)
                .unwrap_or_else(|error| panic!("{tx}: {error}"));

            assert_eq!(
                quote.amount,
                number(4)
                    .parse::<BigUint>()
                    .expect("amount out"),
                "{tx}"
            );
            let after = quote
                .new_state
                .as_any()
                .downcast_ref::<AlgebraIntegralState>()
                .expect("an Algebra Integral state");
            assert_eq!(
                after.sqrt_price,
                U256::from_str_radix(&number(9), 10).expect("price after"),
                "{tx}"
            );
            assert_eq!(
                after.tick,
                number(10)
                    .parse::<i32>()
                    .expect("tick after"),
                "{tx}"
            );
            assert_eq!(after.liquidity, liquidity_after, "{tx}");
            directions.insert(zero_for_one);
            crossed_a_tick |= liquidity_after != liquidity;
        }
        assert_eq!(directions.len(), 2, "the fixtures cover both directions");
        assert!(crossed_a_tick, "the fixtures cover a tick crossing");
    }

    #[test]
    fn plugin_the_pool_does_not_call_is_not_needed() {
        let plugin =
            PluginSetup { config: AFTER_SWAP_ONLY, implementation: None, ..robinhood_plugin() };

        let pool = pool(None, plugin, SESSION);

        assert_eq!(pool.fee(), 0.003);
    }

    #[test]
    fn pool_that_calls_only_after_swap_carries_the_hooks_estimated_gas() {
        let plugin =
            PluginSetup { config: AFTER_SWAP_ONLY, implementation: None, ..robinhood_plugin() };
        let amount = one_token() / BigUint::from(1_000u32);

        let with_hook = pool(None, plugin, SESSION)
            .get_amount_out(amount.clone(), &token0(), &token1())
            .expect("the pool's own fee applies");
        let without = static_fee_pool()
            .get_amount_out(amount, &token0(), &token1())
            .expect("the pool's own fee applies");

        assert_eq!(with_hook.amount, without.amount);
        assert_eq!(with_hook.gas, without.gas + BigUint::from(AFTER_SWAP_ESTIMATE_GAS));
    }

    /// AFTER_SWAP without BEFORE_SWAP.
    const AFTER_SWAP_ONLY: u8 = 2;

    fn anti_sniper_pool(start: u64, timestamp: u64) -> AlgebraIntegralState {
        let mut plugin = robinhood_plugin();
        plugin.implementation = Some(ANTI_SNIPER_IMPL_BASE);
        plugin
            .attributes
            .insert("fee_override/source_enabled".to_string(), Bytes::from(vec![0]));
        plugin.attributes.insert(
            "anti_sniper/start_timestamp".to_string(),
            Bytes::from(start.to_be_bytes().to_vec()),
        );
        pool(Some(Chain::Base), plugin, timestamp)
    }

    #[test]
    fn anti_sniper_period_leaves_nothing_to_trade() {
        let mut pool = anti_sniper_pool(SESSION, SESSION + 60);

        let limits = pool
            .get_limits(token0().address, token1().address)
            .expect("limits");
        assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO));
        let error = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect_err("the gate is closed");
        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");

        // The 30 minute period ends: the pool trades at its bounded base fee, and the module's
        // check still costs gas.
        assert!(pool.apply_block(&BlockContext::new(101, SESSION + 1_800)));
        let (max_in, _) = pool
            .get_limits(token0().address, token1().address)
            .expect("limits");
        assert!(max_in > BigUint::ZERO);
        let quote = pool
            .get_amount_out(one_token(), &token0(), &token1())
            .expect("the swap should quote");
        assert_eq!(pool.fee(), 0.001);
        assert_eq!(
            quote.gas,
            BigUint::from(54_000u64 + 5_400 + 30_000 + 27_000 + 67_000 + 9_000 + 44_000 + 3_000)
        );
    }

    #[test]
    fn limits_can_be_swapped_and_one_unit_more_cannot() {
        let pool = robinhood_pool(SESSION);

        for (token_in, token_out) in [(token0(), token1()), (token1(), token0())] {
            let (max_in, max_out) = pool
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .expect("limits");
            let at_limit = pool
                .get_amount_out(max_in.clone(), &token_in, &token_out)
                .expect("the limit should be swappable");
            assert!(at_limit.amount <= max_out);
            assert!(pool
                .get_amount_out(max_in * 2u32, &token_in, &token_out)
                .is_err());
        }
    }

    #[test]
    fn serialized_state_rebuilds_its_plugin() {
        let pool = robinhood_pool(SESSION);

        let json = serde_json::to_string(&pool).expect("the state should serialize");
        let restored: AlgebraIntegralState =
            serde_json::from_str(&json).expect("the state should deserialize");

        assert_eq!(restored, pool);
        assert_eq!(restored.fee(), 0.0014);
    }

    #[test]
    fn swap_to_price_uses_the_plugin_fee() {
        let pool = robinhood_pool(SESSION);
        let target = tycho_common::simulation::protocol_sim::Price::new(
            // 0.99 token1 per token0, below the 0.9986 the pool gives after its 0.14% fee.
            BigUint::from(990u32),
            BigUint::from(1_000u32),
        );
        let params = QueryPoolSwapParams::new(
            token0(),
            token1(),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let swap = pool
            .query_pool_swap(&params)
            .expect("the target is inside the range");

        assert!(swap.amount_in() > &BigUint::ZERO);
        assert!(swap.amount_out() < swap.amount_in());
    }
}
