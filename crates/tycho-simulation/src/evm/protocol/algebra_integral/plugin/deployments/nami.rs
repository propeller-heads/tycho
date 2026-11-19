//! Nami V4's plugins.
//!
//! Nami runs two implementations per chain: `NamiPluginV1Diamond`, and
//! `NamiPluginV1WithAntiSniperDiamond` for pools launched behind the AntiSniper module. Both
//! answer `beforeSwap` with the fee override module's fee, and their `afterSwap` runs the MEVX
//! and farming modules.

use alloy::primitives::{address, Address};
use tycho_common::{models::Chain, simulation::errors::SimulationError};

use crate::{
    evm::protocol::algebra_integral::plugin::{
        composed::{ComposedPlugin, GasModule, GateModule},
        modules::{
            field_bytes,
            nami::{
                anti_sniper::AntiSniper, farming::Farming, fee_override::FeeOverride,
                fee_source::FeeSource, mevx::Mevx,
            },
        },
        registry::{register_plugin_handler, PluginCreationParams, PluginHandlerCreator},
        PluginHandler, AFTER_SWAP_FLAG, PLUGIN_CONFIG_ATTRIBUTE,
    },
    protocol::errors::InvalidSnapshotError,
};

// Implementations the beacons pointed at on 2026-11-19, read on chain. A beacon upgrade needs an
// entry here before it happens.
const PLUGIN_ETHEREUM: Address = address!("7360C8207AdD8B8dd0332831A19c20bba6e3CABD");
const ANTI_SNIPER_PLUGIN_ETHEREUM: Address = address!("FD1eff2688EeA5D8579b335491c8dec6fdf8C78d");
const PLUGIN_BASE: Address = address!("d1dfA0aEDE1dd17153457d8D8C7C042B8e37a642");
const ANTI_SNIPER_PLUGIN_BASE: Address = address!("9c5D328a1bDdb841E889d1976670F3C0C153D037");
const PLUGIN_ROBINHOOD: Address = address!("1a2898f77957B9C5B7bAfD29a2dAFBA4404b6874");
const ANTI_SNIPER_PLUGIN_ROBINHOOD: Address = address!("E7fA5e8057E28b44155E345AE13f5F0eD06E7F1B");

// The fee source contracts a pool's `fee_override/source` can name. A new source contract needs
// an entry here before a pool is pointed at it, even when its code is unchanged.
const FEE_SOURCES_ETHEREUM: [(Address, FeeSource); 1] =
    [(address!("A7f5e79C7c88A917993dd4785dA700214C4429a6"), FeeSource::Gold)];
const FEE_SOURCES_BASE: [(Address, FeeSource); 1] =
    [(address!("146669E5670c08730599ED8182AC979c5d04E5d2"), FeeSource::Nyse)];
const FEE_SOURCES_ROBINHOOD: [(Address, FeeSource); 2] = [
    (address!("622B1971F9792A2d410B14b8108d5481c1D2C362"), FeeSource::Nyse),
    (address!("55B8ca9B7B6679458C84Ee32e7F0741130a28040"), FeeSource::Gold),
];

/// Builds the handler of a Nami plugin from its module attributes.
struct NamiPluginCreator {
    /// The fee sources of the plugin's chain.
    sources: &'static [(Address, FeeSource)],
    /// Whether the implementation contains the AntiSniper module.
    anti_sniper: bool,
}

impl PluginHandlerCreator for NamiPluginCreator {
    fn instantiate_plugin_handler(
        &self,
        params: PluginCreationParams<'_>,
    ) -> Result<Box<dyn PluginHandler>, InvalidSnapshotError> {
        let fee = FeeOverride::from_attributes(params.attributes, self.sources)?;
        let [plugin_config] = field_bytes::<1>(params.attributes, PLUGIN_CONFIG_ATTRIBUTE)?;

        let mut gates: Vec<Box<dyn GateModule>> = Vec::new();
        let mut gas: Vec<Box<dyn GasModule>> = vec![Box::new(fee)];
        if self.anti_sniper {
            let anti_sniper = AntiSniper::from_attributes(params.attributes)?;
            gates.push(Box::new(anti_sniper));
            gas.push(Box::new(anti_sniper));
        }
        // The pool calls `afterSwap`, where these two modules run, only with this flag.
        if plugin_config & AFTER_SWAP_FLAG != 0 {
            gas.push(Box::new(Mevx::from_attributes(params.attributes)?));
            gas.push(Box::new(Farming::from_attributes(params.attributes)?));
        }
        Ok(Box::new(ComposedPlugin::new(Box::new(fee), gates, gas, params.block)))
    }
}

/// Nami's plugins on one chain.
struct Deployment {
    chain: Chain,
    plugin: Address,
    anti_sniper_plugin: Address,
    sources: &'static [(Address, FeeSource)],
}

const DEPLOYMENTS: [Deployment; 3] = [
    Deployment {
        chain: Chain::Ethereum,
        plugin: PLUGIN_ETHEREUM,
        anti_sniper_plugin: ANTI_SNIPER_PLUGIN_ETHEREUM,
        sources: &FEE_SOURCES_ETHEREUM,
    },
    Deployment {
        chain: Chain::Base,
        plugin: PLUGIN_BASE,
        anti_sniper_plugin: ANTI_SNIPER_PLUGIN_BASE,
        sources: &FEE_SOURCES_BASE,
    },
    Deployment {
        chain: Chain::Robinhood,
        plugin: PLUGIN_ROBINHOOD,
        anti_sniper_plugin: ANTI_SNIPER_PLUGIN_ROBINHOOD,
        sources: &FEE_SOURCES_ROBINHOOD,
    },
];

/// Registers the handler of every Nami plugin implementation.
pub(super) fn register_plugin_handlers() -> Result<(), SimulationError> {
    for deployment in &DEPLOYMENTS {
        register_plugin_handler(
            deployment.chain,
            deployment.plugin,
            Box::new(NamiPluginCreator { sources: deployment.sources, anti_sniper: false }),
        )?;
        register_plugin_handler(
            deployment.chain,
            deployment.anti_sniper_plugin,
            Box::new(NamiPluginCreator { sources: deployment.sources, anti_sniper: true }),
        )?;
    }
    Ok(())
}
