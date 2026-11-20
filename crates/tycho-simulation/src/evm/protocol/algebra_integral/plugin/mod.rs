//! Plugins of Algebra Integral pools.
//!
//! A pool whose plugin config has the `BEFORE_SWAP` flag asks its plugin for the fee of every
//! swap, and one with `AFTER_SWAP` calls it again once the swap is done. A [`PluginHandler`]
//! answers for one pool's plugin off chain: the fees the pool would be told, whether the plugin
//! lets the pool swap at all, and the gas the two calls cost.
//!
//! Handlers are built by [`registry::instantiate_plugin_handler`], keyed by the chain and the
//! implementation the plugin's beacon points at. A native handler is a
//! [`composed::ComposedPlugin`], assembled from the modules that implementation contains.
//!
//! [`modules`] mirrors the Substreams package's `plugin/modules`: one directory per vendor, one
//! file per module, each decoding the attributes its counterpart there emits.

use std::{collections::HashMap, fmt::Debug};

use alloy::primitives::{Address, I256, U256};
use tycho_common::simulation::{errors::SimulationError, protocol_sim::BlockContext};

pub mod composed;
pub mod deployments;
pub mod modules;
pub mod registry;

/// Attribute holding a pool's `pluginConfig`.
pub const PLUGIN_CONFIG_ATTRIBUTE: &str = "plugin_config";
/// A pool calls its plugin's `beforeSwap` when its `pluginConfig` has this flag.
pub const BEFORE_SWAP_FLAG: u8 = 1;
/// A pool calls its plugin's `afterSwap` when its `pluginConfig` has this flag.
pub const AFTER_SWAP_FLAG: u8 = 1 << 1;
/// A pool accepts a fee from `beforeSwap` only when its `pluginConfig` has this flag.
pub const DYNAMIC_FEE_FLAG: u8 = 1 << 7;

/// Storage slot values per contract.
pub type StorageOverrides = HashMap<Address, HashMap<U256, U256>>;

/// The swap a plugin is asked about, as the pool passes it to `beforeSwap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginSwap {
    pub zero_for_one: bool,
    /// Positive for an exact input, negative for an exact output.
    pub amount_specified: I256,
    pub sqrt_price_limit: U256,
    /// The pool's price before the swap.
    pub sqrt_price: U256,
    /// The pool's tick before the swap.
    pub tick: i32,
}

/// The fees `beforeSwap` reports, in hundredths of a bip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginFees {
    /// Fee the pool charges for this swap. `None` is the contract's zero: the pool keeps the fee
    /// it last stored.
    pub override_fee: Option<u32>,
    /// Fee the plugin takes on top of the pool's.
    pub plugin_fee: u32,
}

/// What a plugin does to one swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginQuote {
    /// Fee the pool charges for this swap, in hundredths of a bip. `None` keeps the pool's last
    /// fee.
    pub override_fee: Option<u32>,
    /// Fee the plugin takes on top, in hundredths of a bip.
    pub plugin_fee: u32,
    /// Gas of the plugin's `beforeSwap` and `afterSwap`, including what the pool spends calling
    /// them.
    pub gas: u64,
    /// Storage the hooks wrote, which a later swap in the same block must run against. Native
    /// handlers leave it empty: nothing they read is written during a swap.
    pub storage_writes: StorageOverrides,
}

/// One pool's plugin, rebuilt whenever the attributes it is decoded from change.
pub trait PluginHandler: Debug + Send + Sync + 'static {
    /// Fees and hook gas for `swap` at the current execution block.
    ///
    /// `overrides` is storage that differs from the indexed state: what earlier swaps in the
    /// block wrote, as their quotes' `storage_writes` reported, and pending-block state. The pool
    /// state keeps it and passes it in. Native handlers ignore it; a handler that runs the hooks
    /// in the VM runs them against it.
    fn quote(
        &self,
        swap: &PluginSwap,
        overrides: &StorageOverrides,
    ) -> Result<PluginQuote, SimulationError>;

    /// Whether the plugin lets the pool swap at all at the current execution block. While this is
    /// `false` the pool offers no liquidity.
    fn swaps_enabled(&self) -> bool {
        true
    }

    /// Advances the plugin to the block a quote is expected to execute in.
    ///
    /// Returns `true` when the plugin's quoting behavior changed, so the pool must be re-emitted.
    fn apply_block(&mut self, block: &BlockContext) -> bool;

    fn clone_box(&self) -> Box<dyn PluginHandler>;
}

impl Clone for Box<dyn PluginHandler> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
