//! Runs the pool's plugin `beforeSwap` hook in the EVM to obtain the fee a swap would pay.
use std::{collections::HashMap, fmt::Debug};

use alloy::{
    core::sol,
    primitives::{Address, Bytes as AlloyBytes, I256, U256},
    sol_types::SolCall,
};
use revm::DatabaseRef;
use tycho_common::simulation::errors::SimulationError;

use super::swap_math::FEE_DENOMINATOR;
use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::vm::constants::EXTERNAL_ACCOUNT,
    simulation::{BlockEnvOverrides, SimulationEngine, SimulationParameters},
};

sol! {
    function beforeSwap(
        address sender,
        address recipient,
        bool zeroToOne,
        int256 amountRequired,
        uint160 limitSqrtPrice,
        bool withPaymentInAdvance,
        bytes data
    ) external returns (bytes4 selector, uint24 feeOverride, uint24 pluginFee);
}

/// Pool calls the plugin's `beforeSwap` when this `pluginConfig` flag is set.
pub(super) const BEFORE_SWAP_FLAG: u8 = 1;
/// Pool calls the plugin's `afterSwap` when this `pluginConfig` flag is set.
pub(super) const AFTER_SWAP_FLAG: u8 = 1 << 1;
/// Pool accepts a fee returned by `beforeSwap` only when this `pluginConfig` flag is set.
pub(super) const DYNAMIC_FEE_FLAG: u8 = 1 << 7;

/// Storage slot of `AlgebraPool.globalState` (after `totalFeeGrowth0Token`/`totalFeeGrowth1Token`).
const GLOBAL_STATE_SLOT: u64 = 2;
/// `globalState` packs `price` (160 bits) then `tick` (24 bits) from the low end of the slot.
const PRICE_AND_TICK_BITS: usize = 184;
/// Gas every simulated transaction is charged up front. In a real swap the pool calls the plugin
/// from inside the transaction, so this part is not spent.
const INTRINSIC_TX_GAS: u64 = 21_000;

/// Storage slot values per contract.
pub(super) type StorageOverrides = HashMap<Address, HashMap<U256, U256>>;

/// The pool state and swap the plugin is asked to price.
#[derive(Debug, Clone)]
pub(super) struct BeforeSwapRequest<'a> {
    pub(super) pool: Address,
    pub(super) plugin: Address,
    pub(super) sqrt_price: U256,
    pub(super) tick: i32,
    pub(super) zero_to_one: bool,
    pub(super) amount_specified: I256,
    pub(super) sqrt_price_limit: U256,
    pub(super) block_number: u64,
    pub(super) block_timestamp: u64,
    /// Storage written by earlier simulated swaps in the same block, applied over indexed state.
    pub(super) storage_overrides: &'a StorageOverrides,
}

/// Fees returned by the plugin for one swap, in hundredths of a bip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PluginFees {
    pub(super) override_fee: u32,
    pub(super) plugin_fee: u32,
    /// Gas the hook consumed, excluding the simulated transaction's intrinsic cost.
    pub(super) gas_used: u64,
    /// Storage the hook wrote (the volatility oracle timepoint), merged over the request's
    /// overrides. A later swap in the same block must run against it.
    pub(super) storage_writes: StorageOverrides,
}

/// Calls `plugin.beforeSwap` as the pool would and returns the fees it reports.
///
/// The call runs against the engine's indexed state at the requested block, overlaid with the
/// request's storage overrides and with the pool's `globalState` price and tick replaced by the
/// request's, so a quote chained onto a simulated swap is priced from the post-swap tick and sees
/// the oracle timepoint that swap wrote.
pub(super) fn before_swap<D>(
    engine: &SimulationEngine<D>,
    request: &BeforeSwapRequest<'_>,
) -> Result<PluginFees, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let data = beforeSwapCall {
        sender: *EXTERNAL_ACCOUNT,
        recipient: *EXTERNAL_ACCOUNT,
        zeroToOne: request.zero_to_one,
        amountRequired: request.amount_specified,
        limitSqrtPrice: request
            .sqrt_price_limit
            .to::<alloy::primitives::U160>(),
        withPaymentInAdvance: false,
        data: AlloyBytes::new(),
    }
    .abi_encode();

    let global_state = engine
        .state
        .storage_ref(request.pool, U256::from(GLOBAL_STATE_SLOT))
        .map_err(|err| {
            SimulationError::RecoverableError(format!(
                "Nami V4 pool {} globalState missing from the simulation DB: {err:?}",
                request.pool
            ))
        })?;
    let mut overrides = request.storage_overrides.clone();
    overrides
        .entry(request.pool)
        .or_default()
        .insert(
            U256::from(GLOBAL_STATE_SLOT),
            with_price_and_tick(global_state, request.sqrt_price, request.tick),
        );

    let params = SimulationParameters {
        caller: request.pool,
        to: request.plugin,
        data,
        overrides: Some(overrides),
        block_overrides: Some(BlockEnvOverrides {
            number: Some(request.block_number),
            timestamp: Some(request.block_timestamp),
        }),
        ..Default::default()
    };
    let result = engine
        .simulate(&params)
        .map_err(|err| {
            SimulationError::RecoverableError(format!(
                "Nami V4 plugin {} beforeSwap failed for pool {}: {err}",
                request.plugin, request.pool
            ))
        })?;
    let decoded = beforeSwapCall::abi_decode_returns(result.result.as_ref()).map_err(|err| {
        SimulationError::RecoverableError(format!(
            "Nami V4 plugin {} returned undecodable beforeSwap data: {err}",
            request.plugin
        ))
    })?;
    if decoded.selector != beforeSwapCall::SELECTOR {
        return Err(SimulationError::RecoverableError(format!(
            "Nami V4 plugin {} returned the wrong beforeSwap selector; the pool would revert",
            request.plugin
        )));
    }

    let mut storage_writes = request.storage_overrides.clone();
    for (address, update) in result.state_updates {
        if let Some(storage) = update.storage {
            storage_writes
                .entry(address)
                .or_default()
                .extend(storage);
        }
    }

    Ok(PluginFees {
        override_fee: decoded.feeOverride.to::<u32>(),
        plugin_fee: decoded.pluginFee.to::<u32>(),
        gas_used: result
            .gas_used
            .saturating_sub(INTRINSIC_TX_GAS),
        storage_writes,
    })
}

/// Combines the pool's stored fee with the plugin's answer the way `AlgebraPool` does.
///
/// Errors when the pool would revert: a non-zero plugin fee without the `DYNAMIC_FEE` flag, or a
/// total fee at or above 100%.
pub(super) fn effective_fee(
    last_fee: u32,
    plugin_config: u8,
    fees: &PluginFees,
) -> Result<u32, SimulationError> {
    let dynamic_fee_enabled = plugin_config & DYNAMIC_FEE_FLAG != 0;
    if !dynamic_fee_enabled && (fees.override_fee > 0 || fees.plugin_fee > 0) {
        return Err(SimulationError::RecoverableError(
            "Nami V4 plugin returned a fee while DYNAMIC_FEE is disabled; the pool would revert"
                .to_string(),
        ));
    }
    let base_fee = if fees.override_fee != 0 { fees.override_fee } else { last_fee };
    let fee = base_fee + fees.plugin_fee;
    if fee >= FEE_DENOMINATOR {
        return Err(SimulationError::RecoverableError(format!(
            "Nami V4 swap fee {fee} is not below 100%; the pool would revert"
        )));
    }
    Ok(fee)
}

fn with_price_and_tick(global_state: U256, sqrt_price: U256, tick: i32) -> U256 {
    let price_and_tick_mask = (U256::ONE << PRICE_AND_TICK_BITS) - U256::ONE;
    let tick_bits = U256::from(u32::from_ne_bytes(tick.to_ne_bytes()) & 0x00FF_FFFF);
    (global_state & !price_and_tick_mask) | sqrt_price | (tick_bits << 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fees(override_fee: u32, plugin_fee: u32) -> PluginFees {
        PluginFees { override_fee, plugin_fee, gas_used: 0, storage_writes: HashMap::new() }
    }

    const DYNAMIC: u8 = BEFORE_SWAP_FLAG | DYNAMIC_FEE_FLAG;

    #[test]
    fn override_fee_replaces_the_stored_fee() {
        assert_eq!(effective_fee(500, DYNAMIC, &fees(3_000, 0)).unwrap(), 3_000);
    }

    #[test]
    fn zero_override_falls_back_to_the_stored_fee_plus_plugin_fee() {
        assert_eq!(effective_fee(500, DYNAMIC, &fees(0, 100)).unwrap(), 600);
    }

    #[test]
    fn plugin_fee_is_added_to_the_override() {
        assert_eq!(effective_fee(500, DYNAMIC, &fees(3_000, 100)).unwrap(), 3_100);
    }

    #[test]
    fn fee_without_dynamic_flag_reverts() {
        assert!(effective_fee(500, BEFORE_SWAP_FLAG, &fees(3_000, 0)).is_err());
    }

    #[test]
    fn fee_at_or_above_denominator_reverts() {
        assert!(effective_fee(0, DYNAMIC, &fees(999_999, 1)).is_err());
    }

    #[test]
    fn global_state_override_keeps_fee_config_and_lock_bits() {
        let upper_bits = U256::from(0xABCDu64) << PRICE_AND_TICK_BITS;
        let old = upper_bits | U256::from(12345u64);
        let price = U256::from(1u64) << 96;

        let updated = with_price_and_tick(old, price, -1);

        assert_eq!(updated >> PRICE_AND_TICK_BITS, U256::from(0xABCDu64));
        assert_eq!(updated & ((U256::ONE << 160) - U256::ONE), price);
        assert_eq!((updated >> 160) & U256::from(0xFF_FFFFu64), U256::from(0xFF_FFFFu64));
    }
}
