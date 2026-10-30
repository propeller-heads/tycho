//! Swap fee of AERO v1.1 concentrated-liquidity pools, set by the factory's swap hook.
//!
//! Mirrors `DynamicSwapFeeHook.beforeSwap` (and `ArbDynamicSwapFeeHook`, which only keys its fee
//! cache on the L2 block number, which is the block number Tycho tracks on Arbitrum chains) and
//! `SwapHookLib.beforeSwap`. Quotes assume the swap gets no caller discount and pays no MEV tax:
//! both depend on the executing transaction, a discount only lowers the fee, and the MEV tax is
//! only charged above a priority fee threshold.

use std::collections::HashMap;

use alloy::primitives::{address, Address};
use serde::{Deserialize, Serialize};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::protocol::utils::slipstreams::observations::Observations;

/// The `DynamicSwapFeeHook` deployment; the same address on every chain AERO v1.1 runs on.
const DYNAMIC_SWAP_FEE_HOOK: Address = address!("C0ed888AfE87ed1AcF01D9f19bB850159e3ce434");
/// `type(uint24).max`: a base or initial fee of this value means "use the tick spacing fee".
const ZERO_FEE_INDICATOR: u32 = 16_777_215;
/// `SwapHookLib` caps every hook fee at 10%.
const SWAP_FEE_CEIL: u32 = 100_000;
const SCALING_PRECISION: u128 = 1_000_000;

/// The hook's settings for one pool, plus the fee it cached for the pool's last traded block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AeroHookFee {
    default_swap_hook: Bytes,
    pool_swap_hook: Bytes,
    base_fee: u32,
    fee_cap: u32,
    scaling_factor: u64,
    initial_fee_enabled: bool,
    initial_fee: u32,
    default_scaling_factor: u64,
    default_fee_cap: u32,
    seconds_ago: u32,
    /// `blockFee[pool][block]` the hook last stored: the fee every later swap in that block pays.
    block_fee: Option<(u64, u32)>,
}

/// The pool's oracle state the TWAP reads, before the swap.
pub(crate) struct PoolOracle<'a> {
    pub tick: i32,
    pub liquidity: u128,
    pub observation_index: u16,
    pub observation_cardinality: u16,
    pub observations: &'a Observations,
}

/// Where a quoted swap executes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Execution {
    pub block: u64,
    pub timestamp: u32,
    /// Whether the quote may assume it is the first swap on the pool in its block.
    pub first_in_block: bool,
}

/// The fee a swap pays, and the block fee the hook stores when the swap is the block's first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HookFee {
    pub fee: u32,
    pub block_fee_to_store: Option<u32>,
    /// Whether resolving the fee ran the TWAP `observe` (for gas).
    pub observed_twap: bool,
}

fn attribute<'a>(attributes: &'a HashMap<String, Bytes>, name: &str) -> Result<&'a Bytes, String> {
    attributes
        .get(name)
        .ok_or_else(|| format!("missing hook fee attribute `{name}`"))
}

fn number<T: TryFrom<u128>>(value: &Bytes, name: &str) -> Result<T, String> {
    let bytes = value.as_ref();
    let significant = bytes
        .iter()
        .skip_while(|byte| **byte == 0)
        .count();
    if significant > 16 {
        return Err(format!("hook fee attribute `{name}` does not fit: {value}"));
    }
    let raw = bytes
        .iter()
        .fold(0u128, |acc, byte| (acc << 8) | u128::from(*byte));
    T::try_from(raw).map_err(|_| format!("hook fee attribute `{name}` does not fit: {value}"))
}

impl AeroHookFee {
    /// Builds the hook settings from a full snapshot of an `aero_cl` component.
    pub(crate) fn from_attributes(attributes: &HashMap<String, Bytes>) -> Result<Self, String> {
        let mut fee = Self::default();
        for name in [
            "default_swap_hook",
            "pool_swap_hook",
            "dfc_baseFee",
            "dfc_feeCap",
            "dfc_scalingFactor",
            "dfc_initialFeeEnabled",
            "dfc_initialFee",
            "hook_default_scaling_factor",
            "hook_default_fee_cap",
            "hook_seconds_ago",
        ] {
            attribute(attributes, name)?;
        }
        fee.update_from_attributes(attributes)?;
        Ok(fee)
    }

    /// Applies the hook fee attributes a delta carries.
    pub(crate) fn update_from_attributes(
        &mut self,
        attributes: &HashMap<String, Bytes>,
    ) -> Result<(), String> {
        for (name, value) in attributes {
            match name.as_str() {
                "default_swap_hook" => self.default_swap_hook = value.clone(),
                "pool_swap_hook" => self.pool_swap_hook = value.clone(),
                "dfc_baseFee" => self.base_fee = number(value, name)?,
                "dfc_feeCap" => self.fee_cap = number(value, name)?,
                "dfc_scalingFactor" => self.scaling_factor = number(value, name)?,
                "dfc_initialFeeEnabled" => {
                    self.initial_fee_enabled = value.iter().any(|byte| *byte != 0)
                }
                "dfc_initialFee" => self.initial_fee = number(value, name)?,
                "hook_default_scaling_factor" => self.default_scaling_factor = number(value, name)?,
                "hook_default_fee_cap" => self.default_fee_cap = number(value, name)?,
                "hook_seconds_ago" => self.seconds_ago = number(value, name)?,
                "block_fee" => {
                    let block = self
                        .block_fee
                        .map_or(0, |(block, _)| block);
                    self.block_fee = Some((block, number(value, name)?));
                }
                "block_fee_block" => {
                    let fee = self.block_fee.map_or(0, |(_, fee)| fee);
                    self.block_fee = Some((number(value, name)?, fee));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Records the block fee a simulated first swap of `block` stores.
    pub(crate) fn store_block_fee(&mut self, block: u64, fee: u32) {
        self.block_fee = Some((block, fee));
    }

    fn hook(&self) -> Result<Option<Address>, SimulationError> {
        let hook = if self
            .pool_swap_hook
            .iter()
            .any(|byte| *byte != 0)
        {
            &self.pool_swap_hook
        } else {
            &self.default_swap_hook
        };
        if hook.iter().all(|byte| *byte == 0) {
            return Ok(None);
        }
        if hook.as_ref() == DYNAMIC_SWAP_FEE_HOOK.as_slice() {
            return Ok(Some(DYNAMIC_SWAP_FEE_HOOK));
        }
        Err(SimulationError::FatalError(format!("Unsupported AERO swap hook {hook}")))
    }

    /// The fee a swap executing at `execution` pays. Errors when the pool's hook is not the
    /// known `DynamicSwapFeeHook`, whose fees are the only ones modelled.
    pub(crate) fn resolve(
        &self,
        pool: &PoolOracle,
        tick_spacing_fee: u32,
        execution: Execution,
    ) -> Result<HookFee, SimulationError> {
        if self.hook()?.is_none() {
            return Ok(flat(tick_spacing_fee));
        }
        let hook_fee = match self.block_fee {
            // A later swap of a block the pool already traded in pays the block's fee.
            Some((block, fee)) if block == execution.block && fee != 0 => flat(fee),
            _ => self.first_swap_fee(pool, tick_spacing_fee, execution)?,
        };
        // `SwapHookLib`: a zero hook fee falls back to the tick spacing fee.
        let fee =
            if hook_fee.fee == 0 { tick_spacing_fee } else { hook_fee.fee.min(SWAP_FEE_CEIL) };
        Ok(HookFee { fee, ..hook_fee })
    }

    /// `_getFirstSwapFee`: the fee of the block's first swap and the fee it stores for the block.
    fn first_swap_fee(
        &self,
        pool: &PoolOracle,
        tick_spacing_fee: u32,
        execution: Execution,
    ) -> Result<HookFee, SimulationError> {
        if self.base_fee == ZERO_FEE_INDICATOR {
            return Ok(HookFee {
                fee: tick_spacing_fee,
                block_fee_to_store: Some(tick_spacing_fee),
                observed_twap: false,
            });
        }
        let (scaling_factor, fee_cap) = if self.scaling_factor == 0 {
            (self.default_scaling_factor, self.default_fee_cap)
        } else {
            (self.scaling_factor, self.fee_cap)
        };
        let base_fee = if self.base_fee == 0 { tick_spacing_fee } else { self.base_fee };
        let (dynamic_fee, observed_twap) =
            self.dynamic_fee(pool, scaling_factor, execution.timestamp)?;
        let block_fee = base_fee
            .saturating_add(dynamic_fee)
            .min(fee_cap);
        let fee = if self.initial_fee_enabled {
            let initial_fee = match self.initial_fee {
                0 => base_fee,
                ZERO_FEE_INDICATOR => tick_spacing_fee,
                fee => fee,
            };
            // The initial fee applies only to the block's first transaction on the pool; a quote
            // that cannot assume that position pays the worse of the two.
            if execution.first_in_block {
                initial_fee
            } else {
                initial_fee.max(block_fee)
            }
        } else {
            block_fee
        };
        Ok(HookFee {
            fee,
            block_fee_to_store: (block_fee != 0).then_some(block_fee),
            observed_twap,
        })
    }

    /// `_getDynamicFee`: `|tick - twap| * scaling_factor / 1e6`, with the TWAP tick rounded
    /// towards negative infinity, or 0 when the oracle does not reach `secondsAgo` back.
    fn dynamic_fee(
        &self,
        pool: &PoolOracle,
        scaling_factor: u64,
        timestamp: u32,
    ) -> Result<(u32, bool), SimulationError> {
        let cardinality = pool.observation_cardinality;
        if cardinality == 0 {
            return Ok((0, false));
        }
        let next = (pool.observation_index + 1) % cardinality;
        let oldest = match pool.observations.get(next) {
            Some(observation) if observation.initialized => observation.block_timestamp,
            _ => pool
                .observations
                .get(0)
                .map_or(0, |observation| observation.block_timestamp),
        };
        let Some(available) = timestamp.checked_sub(oldest) else {
            return Err(SimulationError::FatalError(format!(
                "Oldest observation at {oldest} is after the execution timestamp {timestamp}"
            )));
        };
        if self.seconds_ago == 0 || self.seconds_ago > available {
            return Ok((0, false));
        }
        let Ok((tick_cumulatives, _)) = pool.observations.observe(
            timestamp,
            &[self.seconds_ago, 0],
            pool.tick,
            pool.observation_index,
            pool.liquidity,
            cardinality,
        ) else {
            // The hook catches a reverting `observe` and adds no dynamic fee.
            return Ok((0, true));
        };
        let delta = tick_cumulatives[1] - tick_cumulatives[0];
        let twap_tick = delta.div_euclid(i64::from(self.seconds_ago));
        let deviation = (i64::from(pool.tick) - twap_tick).unsigned_abs();
        let dynamic_fee = u128::from(deviation) * u128::from(scaling_factor) / SCALING_PRECISION;
        Ok((u32::try_from(dynamic_fee).unwrap_or(u32::MAX), true))
    }
}

fn flat(fee: u32) -> HookFee {
    HookFee { fee, block_fee_to_store: None, observed_twap: false }
}

#[cfg(test)]
#[path = "aero_hook_fee_tests.rs"]
mod tests;
