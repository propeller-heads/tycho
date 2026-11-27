//! Nami V4's plugins.
//!
//! Nami runs `NamiPluginV1Diamond` on every chain and, on Base,
//! `NamiPluginV1WithAntiSniperDiamond` for pools launched behind the AntiSniper module. Both
//! answer `beforeSwap` with the fee override module's fee, and their `afterSwap` runs the MEVX
//! and farming modules. The AntiSniper implementations on Ethereum and Robinhood have no pools
//! and are not registered.

use alloy::primitives::{address, Address};
use tycho_common::{models::Chain, simulation::errors::SimulationError};

use crate::{
    evm::protocol::algebra_integral::plugin::{
        composed::{ComposedPlugin, GasModule, GateModule},
        modules::{
            algebra::farming::Farming,
            field_bytes,
            nami::{
                anti_sniper::AntiSniper, fee_override::FeeOverride, fee_source::FeeSource,
                mevx::Mevx,
            },
        },
        registry::{register_plugin_handler, PluginCreationParams, PluginHandlerCreator},
        PluginHandler, AFTER_SWAP_FLAG, PLUGIN_CONFIG_ATTRIBUTE,
    },
    protocol::errors::InvalidSnapshotError,
};

// Every implementation a beacon has pointed at since 2026-11-19, read on chain, oldest first. A
// beacon upgrade needs an entry here before it happens, and the one it replaces stays: a sync
// from an earlier block still meets it.
//
// The second of each list is the build of 2026-11-25, which adds a list of pool callers the MEVX
// module does not back-run. `beforeSwap` and the module storage layout are unchanged: on one
// pool per list, `beforeSwap` called from the pool returned the same fee in both directions with
// the beacon pointed at either build.
const PLUGINS_ETHEREUM: [Address; 2] = [
    address!("7360C8207AdD8B8dd0332831A19c20bba6e3CABD"),
    address!("5312F83B99f0055A9DF784Cab0D9a31D7f919d9E"),
];
const PLUGINS_BASE: [Address; 2] = [
    address!("d1dfA0aEDE1dd17153457d8D8C7C042B8e37a642"),
    address!("1DEc404fD7431B2a05b9c57600E44f2969A8cF2E"),
];
pub(crate) const ANTI_SNIPER_PLUGINS_BASE: [Address; 2] = [
    address!("9c5D328a1bDdb841E889d1976670F3C0C153D037"),
    address!("5438F824dAFCD2436020c880A9e0222617E29F80"),
];
pub(crate) const PLUGINS_ROBINHOOD: [Address; 2] = [
    address!("1a2898f77957B9C5B7bAfD29a2dAFBA4404b6874"),
    address!("62930C50B0bB7f9942f6f448083340cFa265964C"),
];

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
    plugins: &'static [Address],
    anti_sniper_plugins: &'static [Address],
    sources: &'static [(Address, FeeSource)],
}

const DEPLOYMENTS: [Deployment; 3] = [
    Deployment {
        chain: Chain::Ethereum,
        plugins: &PLUGINS_ETHEREUM,
        anti_sniper_plugins: &[],
        sources: &FEE_SOURCES_ETHEREUM,
    },
    Deployment {
        chain: Chain::Base,
        plugins: &PLUGINS_BASE,
        anti_sniper_plugins: &ANTI_SNIPER_PLUGINS_BASE,
        sources: &FEE_SOURCES_BASE,
    },
    Deployment {
        chain: Chain::Robinhood,
        plugins: &PLUGINS_ROBINHOOD,
        anti_sniper_plugins: &[],
        sources: &FEE_SOURCES_ROBINHOOD,
    },
];

/// Registers the handler of every Nami plugin implementation.
pub(super) fn register_plugin_handlers() -> Result<(), SimulationError> {
    for deployment in &DEPLOYMENTS {
        for (plugins, anti_sniper) in
            [(deployment.plugins, false), (deployment.anti_sniper_plugins, true)]
        {
            for plugin in plugins {
                register_plugin_handler(
                    deployment.chain,
                    *plugin,
                    Box::new(NamiPluginCreator { sources: deployment.sources, anti_sniper }),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::Bytes;

    use super::*;
    use crate::evm::protocol::algebra_integral::{
        plugin::{
            deployments::initialize_plugin_handlers, registry::instantiate_plugin_handler,
            BlockContext,
        },
        state::test_support::{robinhood_module_attributes, NAMI_PLUGIN_CONFIG, PLUGIN, SESSION},
    };

    /// Every build a beacon has pointed at gets Nami's handler, with the AntiSniper gate only
    /// for the builds that contain the module and the fee sources of its own chain. On Ethereum a
    /// build with no entry would be served by the generic VM handler instead, so the handler's
    /// type is checked too. The sources are the contracts pools name on chain, written out
    /// rather than read from the lists above.
    #[rstest]
    #[case::ethereum(
        Chain::Ethereum,
        &PLUGINS_ETHEREUM,
        false,
        address!("A7f5e79C7c88A917993dd4785dA700214C4429a6")
    )]
    #[case::base(
        Chain::Base,
        &PLUGINS_BASE,
        false,
        address!("146669E5670c08730599ED8182AC979c5d04E5d2")
    )]
    #[case::base_anti_sniper(
        Chain::Base,
        &ANTI_SNIPER_PLUGINS_BASE,
        true,
        address!("146669E5670c08730599ED8182AC979c5d04E5d2")
    )]
    #[case::robinhood_nyse(
        Chain::Robinhood,
        &PLUGINS_ROBINHOOD,
        false,
        address!("622B1971F9792A2d410B14b8108d5481c1D2C362")
    )]
    #[case::robinhood_gold(
        Chain::Robinhood,
        &PLUGINS_ROBINHOOD,
        false,
        address!("55B8ca9B7B6679458C84Ee32e7F0741130a28040")
    )]
    fn every_build_a_beacon_has_pointed_at_has_its_handler(
        #[case] chain: Chain,
        #[case] implementations: &[Address],
        #[case] anti_sniper: bool,
        #[case] source: Address,
    ) {
        initialize_plugin_handlers().expect("the handlers should register");
        let mut attributes = robinhood_module_attributes();
        attributes
            .insert(PLUGIN_CONFIG_ATTRIBUTE.to_string(), Bytes::from(vec![NAMI_PLUGIN_CONFIG]));
        attributes.insert("fee_override/source".to_string(), Bytes::from(source.to_vec()));
        // An AntiSniper period that has not started: the gate refuses swaps.
        attributes.insert("anti_sniper/start_timestamp".to_string(), Bytes::from(vec![0; 8]));

        for implementation in implementations {
            let handler = instantiate_plugin_handler(
                chain,
                Some(*implementation),
                PluginCreationParams {
                    pool: PLUGIN,
                    plugin: PLUGIN,
                    attributes: &attributes,
                    block: BlockContext::new(100, SESSION),
                },
            )
            .unwrap_or_else(|error| panic!("{implementation}: {error:?}"));

            assert!(format!("{handler:?}").starts_with("ComposedPlugin"), "{implementation}");
            assert_eq!(handler.swaps_enabled(), !anti_sniper, "{implementation}");
        }
    }
}
