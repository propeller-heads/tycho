//! A native plugin handler assembled from modules.
//!
//! An Algebra plugin implementation is a set of modules (dynamic fee, MEVX, farming, ...) behind
//! one contract. Each module affects a quote in one of three ways, and gets one trait per way:
//! it sets the fee ([`FeeModule`]), it can stop the pool from swapping ([`GateModule`]), or it
//! only spends gas ([`GasModule`]). A [`ComposedPlugin`] holds the modules of one implementation.

use std::fmt::Debug;

use tycho_common::simulation::{errors::SimulationError, protocol_sim::BlockContext};

use crate::evm::protocol::algebra_integral::plugin::{
    PluginFees, PluginHandler, PluginQuote, PluginSwap, StorageOverrides,
};

/// The module whose `beforeSwap` logic decides the fees.
pub trait FeeModule: Debug + Send + Sync + 'static {
    /// Fees for a swap in the given direction executing in `block`.
    fn fees(&self, zero_for_one: bool, block: &BlockContext)
        -> Result<PluginFees, SimulationError>;

    fn clone_box(&self) -> Box<dyn FeeModule>;
}

/// A module that can make every swap revert, whatever its size.
pub trait GateModule: Debug + Send + Sync + 'static {
    /// Whether the module lets a swap executing in `block` through.
    fn swaps_enabled(&self, block: &BlockContext) -> bool;

    fn clone_box(&self) -> Box<dyn GateModule>;
}

/// A module that adds work to the plugin's hooks.
pub trait GasModule: Debug + Send + Sync + 'static {
    /// Gas the module adds to a swap in the given direction.
    fn gas(&self, zero_for_one: bool) -> u64;

    /// Gas the module adds for each initialized tick a swap in the given direction crosses.
    fn gas_per_crossed_tick(&self, _zero_for_one: bool) -> u64 {
        0
    }

    fn clone_box(&self) -> Box<dyn GasModule>;
}

/// The modules of one plugin implementation, at one execution block.
#[derive(Debug)]
pub struct ComposedPlugin {
    fee: Box<dyn FeeModule>,
    gates: Vec<Box<dyn GateModule>>,
    gas: Vec<Box<dyn GasModule>>,
    block: BlockContext,
}

impl ComposedPlugin {
    /// `block` is the block the first quote is expected to execute in; [`Self::apply_block`]
    /// moves it afterwards.
    pub fn new(
        fee: Box<dyn FeeModule>,
        gates: Vec<Box<dyn GateModule>>,
        gas: Vec<Box<dyn GasModule>>,
        block: BlockContext,
    ) -> Self {
        Self { fee, gates, gas, block }
    }

    /// Sums `cost` over the gas modules.
    fn sum_gas(&self, cost: impl Fn(&dyn GasModule) -> u64) -> Result<u64, SimulationError> {
        let mut total = 0u64;
        for module in &self.gas {
            total = total
                .checked_add(cost(module.as_ref()))
                .ok_or_else(|| {
                    SimulationError::FatalError("plugin hook gas overflows u64".to_string())
                })?;
        }
        Ok(total)
    }

    /// Everything a quote depends on besides the swap's direction, for both directions. A fee
    /// module that errors compares as `None`.
    fn quoting_behavior(&self) -> (bool, Option<PluginFees>, Option<PluginFees>) {
        (
            self.swaps_enabled(),
            self.fee.fees(true, &self.block).ok(),
            self.fee.fees(false, &self.block).ok(),
        )
    }
}

impl Clone for ComposedPlugin {
    fn clone(&self) -> Self {
        let mut gates = Vec::with_capacity(self.gates.len());
        for gate in &self.gates {
            gates.push(gate.clone_box());
        }
        let mut gas = Vec::with_capacity(self.gas.len());
        for module in &self.gas {
            gas.push(module.clone_box());
        }
        Self { fee: self.fee.clone_box(), gates, gas, block: self.block }
    }
}

impl PluginHandler for ComposedPlugin {
    fn quote(
        &self,
        swap: &PluginSwap,
        _overrides: &StorageOverrides,
    ) -> Result<PluginQuote, SimulationError> {
        if !self.swaps_enabled() {
            return Err(SimulationError::RecoverableError(format!(
                "the plugin does not let the pool swap in block {}",
                self.block.number()
            )));
        }
        let fees = self
            .fee
            .fees(swap.zero_for_one, &self.block)?;
        Ok(PluginQuote {
            override_fee: fees.override_fee,
            plugin_fee: fees.plugin_fee,
            gas: self.sum_gas(|module| module.gas(swap.zero_for_one))?,
            gas_per_crossed_tick: self
                .sum_gas(|module| module.gas_per_crossed_tick(swap.zero_for_one))?,
            storage_writes: StorageOverrides::new(),
        })
    }

    fn swaps_enabled(&self) -> bool {
        for gate in &self.gates {
            if !gate.swaps_enabled(&self.block) {
                return false;
            }
        }
        true
    }

    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let before = self.quoting_behavior();
        self.block = *block;
        before != self.quoting_behavior()
    }

    fn clone_box(&self) -> Box<dyn PluginHandler> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{I256, U256};
    use rstest::rstest;

    use super::*;

    /// Charges `before` until `switch_at`, then `after`, and twice as much one-for-zero.
    #[derive(Debug, Clone)]
    struct SteppedFee {
        before: u32,
        after: u32,
        switch_at: u64,
    }

    impl FeeModule for SteppedFee {
        fn fees(
            &self,
            zero_for_one: bool,
            block: &BlockContext,
        ) -> Result<PluginFees, SimulationError> {
            let fee = if block.timestamp() < self.switch_at { self.before } else { self.after };
            let fee = if zero_for_one { fee } else { fee * 2 };
            Ok(PluginFees { override_fee: Some(fee), plugin_fee: 0 })
        }

        fn clone_box(&self) -> Box<dyn FeeModule> {
            Box::new(self.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct FailingFee;

    impl FeeModule for FailingFee {
        fn fees(&self, _: bool, _: &BlockContext) -> Result<PluginFees, SimulationError> {
            Err(SimulationError::RecoverableError("fee module failed".to_string()))
        }

        fn clone_box(&self) -> Box<dyn FeeModule> {
            Box::new(self.clone())
        }
    }

    /// Closed before `opens_at`.
    #[derive(Debug, Clone)]
    struct OpensAt {
        opens_at: u64,
    }

    impl GateModule for OpensAt {
        fn swaps_enabled(&self, block: &BlockContext) -> bool {
            block.timestamp() >= self.opens_at
        }

        fn clone_box(&self) -> Box<dyn GateModule> {
            Box::new(self.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct FixedGas(u64);

    impl GasModule for FixedGas {
        fn gas(&self, zero_for_one: bool) -> u64 {
            if zero_for_one {
                self.0
            } else {
                self.0 + 1
            }
        }

        fn gas_per_crossed_tick(&self, _zero_for_one: bool) -> u64 {
            self.0 * 100
        }

        fn clone_box(&self) -> Box<dyn GasModule> {
            Box::new(self.clone())
        }
    }

    fn flat_fee(fee: u32) -> Box<dyn FeeModule> {
        Box::new(SteppedFee { before: fee, after: fee, switch_at: 0 })
    }

    fn swap(zero_for_one: bool) -> PluginSwap {
        PluginSwap {
            zero_for_one,
            amount_specified: I256::ONE,
            sqrt_price_limit: U256::ZERO,
            sqrt_price: U256::from(1u64) << 96,
            tick: 0,
        }
    }

    fn at(timestamp: u64) -> BlockContext {
        BlockContext::new(1, timestamp)
    }

    #[rstest]
    #[case::zero_for_one(true, 500, 30)]
    #[case::one_for_zero(false, 1000, 32)]
    fn quote_takes_fees_from_the_fee_module_and_sums_gas(
        #[case] zero_for_one: bool,
        #[case] fee: u32,
        #[case] gas: u64,
    ) {
        let plugin = ComposedPlugin::new(
            flat_fee(500),
            Vec::new(),
            vec![Box::new(FixedGas(10)), Box::new(FixedGas(20))],
            at(100),
        );

        let quote = plugin
            .quote(&swap(zero_for_one), &StorageOverrides::new())
            .expect("an open plugin should quote");

        assert_eq!(
            quote,
            PluginQuote {
                override_fee: Some(fee),
                plugin_fee: 0,
                gas,
                gas_per_crossed_tick: 3_000,
                storage_writes: StorageOverrides::new(),
            }
        );
    }

    #[test]
    fn quote_without_gas_modules_costs_nothing() {
        let plugin = ComposedPlugin::new(flat_fee(500), Vec::new(), Vec::new(), at(100));

        let quote = plugin
            .quote(&swap(true), &StorageOverrides::new())
            .expect("an open plugin should quote");

        assert_eq!(quote.gas, 0);
        assert_eq!(quote.gas_per_crossed_tick, 0);
    }

    #[test]
    fn gas_overflow_is_an_error() {
        let plugin = ComposedPlugin::new(
            flat_fee(500),
            Vec::new(),
            vec![Box::new(FixedGas(u64::MAX - 1)), Box::new(FixedGas(2))],
            at(100),
        );

        let error = plugin
            .quote(&swap(true), &StorageOverrides::new())
            .expect_err("summed gas above u64::MAX should not wrap");

        let SimulationError::FatalError(message) = error else {
            panic!("expected a fatal error, got {error:?}");
        };
        assert!(message.contains("overflows"), "{message}");
    }

    #[rstest]
    #[case::first_gate_closed(200, 50)]
    #[case::second_gate_closed(50, 200)]
    #[case::both_closed(200, 300)]
    fn any_closed_gate_disables_swaps(#[case] first_opens: u64, #[case] second_opens: u64) {
        let plugin = ComposedPlugin::new(
            flat_fee(500),
            vec![
                Box::new(OpensAt { opens_at: first_opens }),
                Box::new(OpensAt { opens_at: second_opens }),
            ],
            Vec::new(),
            at(100),
        );

        assert!(!plugin.swaps_enabled());
        let error = plugin
            .quote(&swap(true), &StorageOverrides::new())
            .expect_err("a gated plugin should not quote");
        let SimulationError::RecoverableError(message) = error else {
            panic!("expected a recoverable error, got {error:?}");
        };
        assert!(message.contains("block 1"), "{message}");
    }

    #[test]
    fn fee_module_error_reaches_the_caller() {
        let plugin = ComposedPlugin::new(Box::new(FailingFee), Vec::new(), Vec::new(), at(100));

        let error = plugin
            .quote(&swap(true), &StorageOverrides::new())
            .expect_err("the fee module's error should propagate");

        let SimulationError::RecoverableError(message) = error else {
            panic!("expected the fee module's error, got {error:?}");
        };
        assert_eq!(message, "fee module failed");
    }

    #[rstest]
    #[case::nothing_changes(100, 150, false)]
    #[case::fee_switches(100, 200, true)]
    #[case::gate_opens(40, 60, true)]
    fn apply_block_reports_whether_quotes_changed(
        #[case] from: u64,
        #[case] to: u64,
        #[case] changed: bool,
    ) {
        let mut plugin = ComposedPlugin::new(
            Box::new(SteppedFee { before: 500, after: 800, switch_at: 200 }),
            vec![Box::new(OpensAt { opens_at: 50 })],
            Vec::new(),
            at(from),
        );

        assert_eq!(plugin.apply_block(&at(to)), changed);
    }

    #[test]
    fn apply_block_moves_every_module_to_the_new_block() {
        let mut plugin = ComposedPlugin::new(
            Box::new(SteppedFee { before: 500, after: 800, switch_at: 200 }),
            vec![Box::new(OpensAt { opens_at: 150 })],
            Vec::new(),
            at(100),
        );
        assert!(!plugin.swaps_enabled());

        plugin.apply_block(&at(200));

        assert!(plugin.swaps_enabled());
        let quote = plugin
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the plugin opened at 150");
        assert_eq!(quote.override_fee, Some(800));
    }

    #[test]
    fn clone_is_independent_of_the_original() {
        let mut plugin = ComposedPlugin::new(
            Box::new(SteppedFee { before: 500, after: 800, switch_at: 200 }),
            Vec::new(),
            vec![Box::new(FixedGas(10))],
            at(100),
        );
        let clone = plugin.clone_box();

        plugin.apply_block(&at(200));

        let quote = clone
            .quote(&swap(true), &StorageOverrides::new())
            .expect("the clone should quote");
        assert_eq!(quote.override_fee, Some(500));
        assert_eq!(quote.gas, 10);
    }
}
