//! The generic handler: runs a plugin's own `beforeSwap` in the EVM.
//!
//! It serves a plugin with no native handler on the chains in
//! [`GENERIC_VM_PLUGIN_CHAINS`](super::registry::GENERIC_VM_PLUGIN_CHAINS), where the indexer
//! tracks the plugin's code and storage. The hook's interface is Algebra Integral 1.2's
//! `IAlgebraPlugin.beforeSwap`.
//!
//! `afterSwap` is not run: it needs the pool's state after the swap, which a quote does not
//! write anywhere. Its gas is a fixed estimate.

use alloy::{
    core::sol,
    primitives::{Address, Bytes as AlloyBytes, U160, U256},
    sol_types::SolCall,
};
use revm::DatabaseRef;
use tycho_common::simulation::{errors::SimulationError, protocol_sim::BlockContext};

use crate::{
    evm::{
        engine_db::{create_engine, engine_db_interface::EngineDatabaseInterface, SHARED_TYCHO_DB},
        protocol::{
            algebra_integral::plugin::{
                modules::field_bytes,
                registry::{
                    register_generic_vm_plugin_handler, PluginCreationParams, PluginHandlerCreator,
                },
                PluginHandler, PluginQuote, PluginSwap, StorageOverrides, AFTER_SWAP_FLAG,
                PLUGIN_CONFIG_ATTRIBUTE,
            },
            vm::{constants::EXTERNAL_ACCOUNT, utils::parse_solidity_error_message},
        },
        simulation::{
            BlockEnvOverrides, SimulationEngine, SimulationEngineError, SimulationParameters,
        },
    },
    protocol::errors::InvalidSnapshotError,
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

/// Storage slot of `AlgebraPool.globalState` (after `totalFeeGrowth0Token` and
/// `totalFeeGrowth1Token`).
const GLOBAL_STATE_SLOT: u64 = 2;
/// `globalState` packs `price` (160 bits) then `tick` (24 bits) from the low end of the slot.
const PRICE_AND_TICK_BITS: usize = 184;
/// The pool's cold `CALL` into the plugin, on top of the gas the hook itself reports.
const PLUGIN_CALL_OVERHEAD_GAS: u64 = 2_600;
/// `afterSwap` of a plugin whose code is not modelled. The figure is the dearer branch measured
/// on Nami's plugin (its back-run check with a router set, 65.8k to 66.8k on Base at block
/// 52924436), so a quote does not understate the hook.
const AFTER_SWAP_GAS: u64 = 66_000;

/// Gas every simulated transaction is charged up front: the base cost and its calldata. In a real
/// swap the pool calls the plugin from inside the transaction, so neither part is spent.
fn intrinsic_gas(calldata: &[u8]) -> u64 {
    21_000 +
        calldata
            .iter()
            .map(|byte| if *byte == 0 { 4 } else { 16 })
            .sum::<u64>()
}

fn with_price_and_tick(global_state: U256, sqrt_price: U256, tick: i32) -> U256 {
    let price_and_tick_mask = (U256::ONE << PRICE_AND_TICK_BITS) - U256::ONE;
    let tick_bits = U256::from(u32::from_ne_bytes(tick.to_ne_bytes()) & 0x00FF_FFFF);
    (global_state & !price_and_tick_mask) | sqrt_price | (tick_bits << 160)
}

/// A plugin whose `beforeSwap` is run in the EVM against the indexed state.
#[derive(Debug, Clone)]
pub struct VmPlugin {
    pool: Address,
    plugin: Address,
    /// Whether the pool also calls `afterSwap`.
    calls_after_swap: bool,
    block: BlockContext,
}

impl VmPlugin {
    /// Calls `plugin.beforeSwap` as the pool would.
    ///
    /// The call runs against the engine's indexed state at the execution block, overlaid with
    /// `overrides` and with the pool's `globalState` price and tick replaced by the swap's, so a
    /// quote chained onto a simulated swap is priced from the post-swap tick and sees what that
    /// swap's hook wrote.
    fn before_swap<D>(
        &self,
        engine: &SimulationEngine<D>,
        swap: &PluginSwap,
        overrides: &StorageOverrides,
    ) -> Result<PluginQuote, SimulationError>
    where
        D: EngineDatabaseInterface + Clone + std::fmt::Debug,
        <D as DatabaseRef>::Error: std::fmt::Debug,
        <D as EngineDatabaseInterface>::Error: std::fmt::Debug,
    {
        let (pool, plugin) = (self.pool, self.plugin);
        let data = beforeSwapCall {
            // A plugin may price by sender (Nami's charges its MEVX executor another fee). A
            // quote is for any other sender, which is what a solver's swap is.
            sender: *EXTERNAL_ACCOUNT,
            recipient: *EXTERNAL_ACCOUNT,
            zeroToOne: swap.zero_for_one,
            amountRequired: swap.amount_specified,
            limitSqrtPrice: swap.sqrt_price_limit.to::<U160>(),
            withPaymentInAdvance: false,
            data: AlloyBytes::new(),
        }
        .abi_encode();

        let mut overrides = overrides.clone();
        let global_state_slot = U256::from(GLOBAL_STATE_SLOT);
        let global_state = match overrides
            .get(&pool)
            .and_then(|slots| slots.get(&global_state_slot))
        {
            Some(value) => *value,
            None => engine
                .state
                .storage_ref(pool, global_state_slot)
                .map_err(|err| {
                    SimulationError::RecoverableError(format!(
                        "algebra pool {pool} globalState missing from the simulation DB: {err:?}"
                    ))
                })?,
        };
        overrides
            .entry(pool)
            .or_default()
            .insert(
                global_state_slot,
                with_price_and_tick(global_state, swap.sqrt_price, swap.tick),
            );

        let intrinsic_gas = intrinsic_gas(&data);
        let params = SimulationParameters {
            caller: pool,
            to: plugin,
            data,
            overrides: Some(overrides),
            block_overrides: Some(BlockEnvOverrides {
                number: Some(self.block.number()),
                timestamp: Some(self.block.timestamp()),
            }),
            ..Default::default()
        };
        let result = engine
            .simulate(&params)
            .map_err(|err| {
                let reason = match &err {
                    SimulationEngineError::TransactionError { data, .. }
                        if data.starts_with("0x") =>
                    {
                        parse_solidity_error_message(data)
                    }
                    SimulationEngineError::StorageError(_) |
                    SimulationEngineError::OutOfGas(..) |
                    SimulationEngineError::TransactionError { .. } |
                    SimulationEngineError::TraceError(_) => err.to_string(),
                };
                SimulationError::RecoverableError(format!(
                    "algebra plugin {plugin} beforeSwap failed for pool {pool}: {reason}"
                ))
            })?;
        let decoded =
            beforeSwapCall::abi_decode_returns(result.result.as_ref()).map_err(|err| {
                SimulationError::RecoverableError(format!(
                    "algebra plugin {plugin} returned undecodable beforeSwap data: {err}"
                ))
            })?;
        if decoded.selector != beforeSwapCall::SELECTOR {
            return Err(SimulationError::RecoverableError(format!(
                "algebra plugin {plugin} returned the wrong beforeSwap selector; the pool would \
                 revert"
            )));
        }

        let mut storage_writes = StorageOverrides::new();
        for (address, update) in result.state_updates {
            if let Some(storage) = update.storage {
                storage_writes
                    .entry(address)
                    .or_default()
                    .extend(storage);
            }
        }
        let override_fee = decoded.feeOverride.to::<u32>();
        let after_swap_gas = if self.calls_after_swap { AFTER_SWAP_GAS } else { 0 };
        Ok(PluginQuote {
            override_fee: (override_fee != 0).then_some(override_fee),
            plugin_fee: decoded.pluginFee.to::<u32>(),
            gas: result
                .gas_used
                .saturating_sub(intrinsic_gas) +
                PLUGIN_CALL_OVERHEAD_GAS +
                after_swap_gas,
            storage_writes,
        })
    }
}

impl PluginHandler for VmPlugin {
    fn quote(
        &self,
        swap: &PluginSwap,
        overrides: &StorageOverrides,
    ) -> Result<PluginQuote, SimulationError> {
        let engine = create_engine(SHARED_TYCHO_DB.clone(), false)?;
        self.before_swap(&engine, swap, overrides)
    }

    /// The hook may read the clock, so a new timestamp can change every quote.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let timestamp_changed = block.timestamp() != self.block.timestamp();
        self.block = *block;
        timestamp_changed
    }

    fn clone_box(&self) -> Box<dyn PluginHandler> {
        Box::new(self.clone())
    }
}

/// Builds a [`VmPlugin`] for any plugin: it needs only the pool's `plugin_config`.
struct VmPluginCreator;

impl PluginHandlerCreator for VmPluginCreator {
    fn instantiate_plugin_handler(
        &self,
        params: PluginCreationParams<'_>,
    ) -> Result<Box<dyn PluginHandler>, InvalidSnapshotError> {
        let [plugin_config] = field_bytes::<1>(params.attributes, PLUGIN_CONFIG_ATTRIBUTE)?;
        Ok(Box::new(VmPlugin {
            pool: params.pool,
            plugin: params.plugin,
            calls_after_swap: plugin_config & AFTER_SWAP_FLAG != 0,
            block: params.block,
        }))
    }
}

/// Registers [`VmPlugin`] as the generic VM handler.
pub(super) fn register_plugin_handler() -> Result<(), SimulationError> {
    register_generic_vm_plugin_handler(Box::new(VmPluginCreator))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;

    use alloy::{
        primitives::{Address, U256},
        sol_types::{SolCall, SolValue},
    };
    use tycho_client::feed::BlockHeader;
    use tycho_common::models::Chain;

    use super::beforeSwapCall;
    use crate::evm::{
        engine_db::SHARED_TYCHO_DB,
        tycho_models::{AccountUpdate, ChangeType},
    };

    /// `PUSH4 selector, PUSH1 0xE0, SHL, PUSH1 0, MSTORE`: the selector word at memory 0.
    fn selector_word() -> Vec<u8> {
        let mut code = vec![0x63];
        code.extend_from_slice(&beforeSwapCall::SELECTOR);
        code.extend_from_slice(&[0x60, 0xE0, 0x1B, 0x60, 0x00, 0x52]);
        code
    }

    /// `PUSH1 0x60, PUSH1 0, RETURN`: three words, selector, override fee and plugin fee.
    const RETURN_THREE_WORDS: [u8; 5] = [0x60, 0x60, 0x60, 0x00, 0xF3];

    /// Bytecode of a plugin whose `beforeSwap` answers with the override fee `zero_for_one_fee`
    /// for zero-for-one swaps and `one_for_zero_fee` otherwise, and no plugin fee.
    pub fn fee_plugin_code(zero_for_one_fee: u16, one_for_zero_fee: u16) -> Vec<u8> {
        assert!(one_for_zero_fee >= zero_for_one_fee);
        let mut code = selector_word();
        // one_for_zero_fee - zeroToOne * (one_for_zero_fee - zero_for_one_fee) at memory 32:
        // PUSH2, PUSH2, PUSH1 0x44, CALLDATALOAD, MUL, SWAP1, SUB, PUSH1 0x20, MSTORE.
        code.push(0x61);
        code.extend_from_slice(&one_for_zero_fee.to_be_bytes());
        code.push(0x61);
        code.extend_from_slice(&(one_for_zero_fee - zero_for_one_fee).to_be_bytes());
        code.extend_from_slice(&[0x60, 0x44, 0x35, 0x02, 0x90, 0x03, 0x60, 0x20, 0x52]);
        code.extend_from_slice(&RETURN_THREE_WORDS);
        code
    }

    /// Bytecode of a plugin whose `beforeSwap` answers with the override fee in its storage slot
    /// 0 and then stores `next_fee` there, as a hook that keeps state between swaps does.
    pub fn stateful_plugin_code(next_fee: u16) -> Vec<u8> {
        let mut code = selector_word();
        // PUSH1 0, SLOAD, PUSH1 0x20, MSTORE: the stored fee at memory 32.
        code.extend_from_slice(&[0x60, 0x00, 0x54, 0x60, 0x20, 0x52]);
        // PUSH2 next_fee, PUSH1 0, SSTORE.
        code.push(0x61);
        code.extend_from_slice(&next_fee.to_be_bytes());
        code.extend_from_slice(&[0x60, 0x00, 0x55]);
        code.extend_from_slice(&RETURN_THREE_WORDS);
        code
    }

    /// Bytecode that reverts every call with `Error(reason)`.
    pub fn reverting_plugin_code(reason: &str) -> Vec<u8> {
        let mut data = vec![0x08, 0xC3, 0x79, 0xA0];
        data.extend_from_slice(&reason.to_string().abi_encode());
        let mut code = Vec::new();
        for (index, word) in data.chunks(32).enumerate() {
            let mut padded = [0u8; 32];
            padded[..word.len()].copy_from_slice(word);
            // PUSH32 word, PUSH1 offset, MSTORE
            code.push(0x7F);
            code.extend_from_slice(&padded);
            code.extend_from_slice(&[
                0x60,
                u8::try_from(index * 32).expect("a short reason"),
                0x52,
            ]);
        }
        // PUSH1 length, PUSH1 0, REVERT
        code.extend_from_slice(&[
            0x60,
            u8::try_from(data.len()).expect("a short reason"),
            0x60,
            0x00,
            0xFD,
        ]);
        code
    }

    /// Loads a pool with a `globalState` slot and a plugin running `plugin_code`, with
    /// `plugin_storage`, into the shared simulation DB.
    pub fn load_pool_and_plugin(
        pool: Address,
        plugin: Address,
        plugin_code: Vec<u8>,
        plugin_storage: HashMap<U256, U256>,
    ) {
        SHARED_TYCHO_DB
            .update(
                vec![
                    AccountUpdate::new(
                        pool,
                        Chain::Ethereum,
                        HashMap::from([(U256::from(2), U256::ONE)]),
                        Some(U256::ZERO),
                        Some(vec![0x00]),
                        ChangeType::Creation,
                    ),
                    AccountUpdate::new(
                        plugin,
                        Chain::Ethereum,
                        plugin_storage,
                        Some(U256::ZERO),
                        Some(plugin_code),
                        ChangeType::Creation,
                    ),
                ],
                Some(BlockHeader { number: 100, timestamp: 1_000, ..Default::default() }),
            )
            .expect("the accounts should load");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::I256;

    use super::{test_support::*, *};
    use crate::evm::protocol::algebra_integral::plugin::BEFORE_SWAP_FLAG;

    fn plugin_at(pool: Address, plugin: Address, plugin_config: u8) -> Box<dyn PluginHandler> {
        let attributes = HashMap::from([(
            PLUGIN_CONFIG_ATTRIBUTE.to_string(),
            tycho_common::Bytes::from(vec![plugin_config]),
        )]);
        VmPluginCreator
            .instantiate_plugin_handler(PluginCreationParams {
                pool,
                plugin,
                attributes: &attributes,
                block: BlockContext::new(101, 1_012),
            })
            .expect("the VM plugin should build")
    }

    fn swap(zero_for_one: bool) -> PluginSwap {
        PluginSwap {
            zero_for_one,
            amount_specified: I256::ONE,
            sqrt_price_limit: U256::from(4_295_128_740u64),
            sqrt_price: U256::ONE << 96,
            tick: 0,
        }
    }

    #[test]
    fn fee_follows_the_swap_direction() {
        let (pool, plugin) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xA2));
        load_pool_and_plugin(pool, plugin, fee_plugin_code(1_000, 2_000), HashMap::new());
        let handler = plugin_at(pool, plugin, BEFORE_SWAP_FLAG);

        let zero_for_one = handler
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the hook should answer");
        let one_for_zero = handler
            .quote(&swap(false), &StorageOverrides::new())
            .expect("the hook should answer");

        assert_eq!(zero_for_one.override_fee, Some(1_000));
        assert_eq!(one_for_zero.override_fee, Some(2_000));
        assert_eq!(zero_for_one.plugin_fee, 0);
        assert!(zero_for_one.storage_writes.is_empty());
        // The overhead of the pool's call is charged on top of the hook's own gas.
        assert!(zero_for_one.gas > PLUGIN_CALL_OVERHEAD_GAS, "{}", zero_for_one.gas);
    }

    #[test]
    fn zero_override_keeps_the_pools_fee() {
        let (pool, plugin) = (Address::repeat_byte(0xA3), Address::repeat_byte(0xA4));
        load_pool_and_plugin(pool, plugin, fee_plugin_code(0, 0), HashMap::new());

        let quote = plugin_at(pool, plugin, BEFORE_SWAP_FLAG)
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the hook should answer");

        assert_eq!(quote.override_fee, None);
    }

    #[test]
    fn after_swap_adds_its_estimate() {
        let (pool, plugin) = (Address::repeat_byte(0xA5), Address::repeat_byte(0xA6));
        load_pool_and_plugin(pool, plugin, fee_plugin_code(500, 500), HashMap::new());

        let before_only = plugin_at(pool, plugin, BEFORE_SWAP_FLAG)
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the hook should answer");
        let both = plugin_at(pool, plugin, BEFORE_SWAP_FLAG | AFTER_SWAP_FLAG)
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the hook should answer");

        assert_eq!(both.gas, before_only.gas + AFTER_SWAP_GAS);
    }

    #[test]
    fn hook_runs_against_the_overrides_and_reports_its_writes() {
        let (pool, plugin) = (Address::repeat_byte(0xA7), Address::repeat_byte(0xA8));
        load_pool_and_plugin(
            pool,
            plugin,
            stateful_plugin_code(3_000),
            HashMap::from([(U256::ZERO, U256::from(1_000))]),
        );
        let handler = plugin_at(pool, plugin, BEFORE_SWAP_FLAG);

        let indexed = handler
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the hook should answer");
        let overridden = handler
            .quote(
                &swap(true),
                &HashMap::from([(plugin, HashMap::from([(U256::ZERO, U256::from(1_500))]))]),
            )
            .expect("the hook should answer");

        assert_eq!(indexed.override_fee, Some(1_000));
        assert_eq!(overridden.override_fee, Some(1_500));
        assert_eq!(
            indexed.storage_writes,
            HashMap::from([(plugin, HashMap::from([(U256::ZERO, U256::from(3_000))]))])
        );
    }

    #[test]
    fn revert_names_the_reason() {
        let (pool, plugin) = (Address::repeat_byte(0xA9), Address::repeat_byte(0xAA));
        load_pool_and_plugin(pool, plugin, reverting_plugin_code("swaps paused"), HashMap::new());

        let error = plugin_at(pool, plugin, BEFORE_SWAP_FLAG)
            .quote(&swap(true), &StorageOverrides::new())
            .expect_err("a reverting hook should not quote");

        let SimulationError::RecoverableError(message) = error else {
            panic!("expected a recoverable error, got {error:?}");
        };
        assert!(message.contains("swaps paused"), "{message}");
    }

    #[test]
    fn answer_without_the_selector_is_rejected() {
        let (pool, plugin) = (Address::repeat_byte(0xAB), Address::repeat_byte(0xAC));
        // PUSH1 0x60, PUSH1 0, RETURN: three zero words.
        load_pool_and_plugin(pool, plugin, vec![0x60, 0x60, 0x60, 0x00, 0xF3], HashMap::new());

        let error = plugin_at(pool, plugin, BEFORE_SWAP_FLAG)
            .quote(&swap(true), &StorageOverrides::new())
            .expect_err("the pool would revert on a wrong selector");

        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
    }

    #[test]
    fn new_timestamp_is_a_change() {
        let mut handler = plugin_at(Address::ZERO, Address::ZERO, BEFORE_SWAP_FLAG);

        assert!(!handler.apply_block(&BlockContext::new(102, 1_012)));
        assert!(handler.apply_block(&BlockContext::new(103, 1_024)));
    }

    #[test]
    fn global_state_override_keeps_the_upper_bits() {
        let upper_bits = U256::from(0xABCDu64) << PRICE_AND_TICK_BITS;
        let price = U256::ONE << 96;

        let updated = with_price_and_tick(upper_bits | U256::from(12345u64), price, -1);

        assert_eq!(updated >> PRICE_AND_TICK_BITS, U256::from(0xABCDu64));
        assert_eq!(updated & ((U256::ONE << 160) - U256::ONE), price);
        assert_eq!((updated >> 160) & U256::from(0xFF_FFFFu64), U256::from(0xFF_FFFFu64));
    }
}
