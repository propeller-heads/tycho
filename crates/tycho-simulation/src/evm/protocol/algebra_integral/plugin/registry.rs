//! Which handler serves which plugin.
//!
//! A plugin is a beacon proxy, so many pools run one implementation. Native handlers are
//! registered per chain and implementation address; a DEX adds one entry for each implementation
//! its beacons have pointed at.

use std::{
    collections::HashMap,
    sync::{LazyLock, RwLock},
};

use alloy::primitives::Address;
use tycho_common::{
    models::Chain,
    simulation::{errors::SimulationError, protocol_sim::BlockContext},
    Bytes,
};

use crate::{
    evm::protocol::algebra_integral::plugin::PluginHandler, protocol::errors::InvalidSnapshotError,
};

/// What a creator builds a handler from.
pub struct PluginCreationParams<'a> {
    pub pool: Address,
    pub plugin: Address,
    /// State attributes of the pool's component.
    pub attributes: &'a HashMap<String, Bytes>,
    /// The block the first quote is expected to execute in.
    pub block: BlockContext,
}

pub trait PluginHandlerCreator: Send + Sync {
    fn instantiate_plugin_handler(
        &self,
        params: PluginCreationParams<'_>,
    ) -> Result<Box<dyn PluginHandler>, InvalidSnapshotError>;
}

/// Chains on which a plugin with no registered native handler is simulated with the generic VM
/// handler.
///
/// Anywhere else such a plugin is rejected at decode time, so a pool whose plugin sets the fee is
/// never quoted at a fee the plugin did not choose.
pub const GENERIC_VM_PLUGIN_CHAINS: [Chain; 1] = [Chain::Ethereum];

/// Handler creators by chain and plugin implementation, plus the generic VM creator.
#[derive(Default)]
struct PluginRegistry {
    native: HashMap<(Chain, Address), Box<dyn PluginHandlerCreator>>,
    generic_vm: Option<Box<dyn PluginHandlerCreator>>,
}

impl PluginRegistry {
    fn instantiate(
        &self,
        chain: Chain,
        implementation: Option<Address>,
        params: PluginCreationParams<'_>,
    ) -> Result<Box<dyn PluginHandler>, InvalidSnapshotError> {
        let native = implementation.and_then(|implementation| {
            self.native
                .get(&(chain, implementation))
        });
        if let Some(creator) = native {
            return creator.instantiate_plugin_handler(params);
        }

        let pool = params.pool;
        let plugin = params.plugin;
        let implementation = implementation.map_or_else(
            || "an unknown implementation".to_string(),
            |i| format!("implementation {i}"),
        );
        if !GENERIC_VM_PLUGIN_CHAINS.contains(&chain) {
            return Err(InvalidSnapshotError::ValueError(format!(
                "unsupported algebra plugin {plugin} of pool {pool} on {chain}, running \
                 {implementation}: no native handler registered and the generic VM plugin handler \
                 is not enabled on this chain"
            )));
        }
        let Some(creator) = &self.generic_vm else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "unsupported algebra plugin {plugin} of pool {pool} on {chain}, running \
                 {implementation}: no native handler registered and no generic VM plugin handler \
                 registered"
            )));
        };
        creator.instantiate_plugin_handler(params)
    }
}

// Workaround for stateless decoder trait.
static REGISTRY: LazyLock<RwLock<PluginRegistry>> =
    LazyLock::new(|| RwLock::new(PluginRegistry::default()));

fn poisoned<T>(error: &std::sync::PoisonError<T>) -> SimulationError {
    SimulationError::FatalError(format!("algebra plugin registry lock poisoned: {error}"))
}

/// Registers `creator` as the native handler for plugins running `implementation` on `chain`,
/// replacing any previous registration for that pair.
pub fn register_plugin_handler(
    chain: Chain,
    implementation: Address,
    creator: Box<dyn PluginHandlerCreator>,
) -> Result<(), SimulationError> {
    REGISTRY
        .write()
        .map_err(|e| poisoned(&e))?
        .native
        .insert((chain, implementation), creator);
    Ok(())
}

/// Registers `creator` as the handler for plugins with no native handler on the chains in
/// [`GENERIC_VM_PLUGIN_CHAINS`], replacing any previous one.
pub fn register_generic_vm_plugin_handler(
    creator: Box<dyn PluginHandlerCreator>,
) -> Result<(), SimulationError> {
    REGISTRY
        .write()
        .map_err(|e| poisoned(&e))?
        .generic_vm = Some(creator);
    Ok(())
}

/// Builds the [`PluginHandler`] for a plugin running `implementation` on `chain`.
///
/// The native handler registered for the pair wins. With none registered, or with no
/// implementation known for the plugin, the generic VM handler serves the chains in
/// [`GENERIC_VM_PLUGIN_CHAINS`]; on every other chain the plugin is unsupported and this returns
/// [`InvalidSnapshotError::ValueError`] naming the chain, pool and implementation.
pub fn instantiate_plugin_handler(
    chain: Chain,
    implementation: Option<Address>,
    params: PluginCreationParams<'_>,
) -> Result<Box<dyn PluginHandler>, InvalidSnapshotError> {
    REGISTRY
        .read()
        .map_err(|e| InvalidSnapshotError::VMError(poisoned(&e)))?
        .instantiate(chain, implementation, params)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use rstest::rstest;

    use super::*;

    const POOL: Address = address!("00000000000000000000000000000000000000a1");
    const PLUGIN: Address = address!("00000000000000000000000000000000000000b2");
    const IMPLEMENTATION: Address = address!("00000000000000000000000000000000000000c3");
    const OTHER_IMPLEMENTATION: Address = address!("00000000000000000000000000000000000000c4");

    /// A creator whose only job is to be recognisable in the error it returns.
    struct NamedCreator(&'static str);

    impl PluginHandlerCreator for NamedCreator {
        fn instantiate_plugin_handler(
            &self,
            _params: PluginCreationParams<'_>,
        ) -> Result<Box<dyn PluginHandler>, InvalidSnapshotError> {
            Err(InvalidSnapshotError::ValueError(self.0.to_string()))
        }
    }

    fn registry(generic_vm: bool) -> PluginRegistry {
        let mut registry = PluginRegistry::default();
        registry
            .native
            .insert((Chain::Ethereum, IMPLEMENTATION), Box::new(NamedCreator("native ethereum")));
        registry
            .native
            .insert((Chain::Base, IMPLEMENTATION), Box::new(NamedCreator("native base")));
        if generic_vm {
            registry.generic_vm = Some(Box::new(NamedCreator("generic vm")));
        }
        registry
    }

    /// The message of the error the selected creator, or the registry itself, returns.
    fn outcome(registry: &PluginRegistry, chain: Chain, implementation: Option<Address>) -> String {
        let attributes = HashMap::new();
        let params = PluginCreationParams {
            pool: POOL,
            plugin: PLUGIN,
            attributes: &attributes,
            block: BlockContext::new(1, 1),
        };
        let Err(error) = registry.instantiate(chain, implementation, params) else {
            panic!("every test creator returns an error");
        };
        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        message
    }

    #[rstest]
    #[case::ethereum(Chain::Ethereum, "native ethereum")]
    #[case::base(Chain::Base, "native base")]
    fn registered_implementation_uses_its_chains_native_creator(
        #[case] chain: Chain,
        #[case] creator: &str,
    ) {
        assert_eq!(outcome(&registry(true), chain, Some(IMPLEMENTATION)), creator);
    }

    #[rstest]
    #[case::unregistered(Some(OTHER_IMPLEMENTATION))]
    #[case::no_implementation(None)]
    fn unknown_implementation_falls_back_to_the_generic_vm_on_ethereum(
        #[case] implementation: Option<Address>,
    ) {
        assert_eq!(outcome(&registry(true), Chain::Ethereum, implementation), "generic vm");
    }

    #[test]
    fn unknown_implementation_is_rejected_off_the_generic_vm_chains() {
        let message = outcome(&registry(true), Chain::Base, Some(OTHER_IMPLEMENTATION));

        assert!(message.contains("unsupported algebra plugin"), "{message}");
        assert!(message.contains("base"), "{message}");
        assert!(message.contains(&POOL.to_string()), "{message}");
        assert!(message.contains(&PLUGIN.to_string()), "{message}");
        assert!(message.contains(&OTHER_IMPLEMENTATION.to_string()), "{message}");
        assert!(message.contains("not enabled on this chain"), "{message}");
    }

    #[test]
    fn missing_implementation_is_rejected_off_the_generic_vm_chains() {
        let message = outcome(&registry(true), Chain::Robinhood, None);

        assert!(message.contains("an unknown implementation"), "{message}");
        assert!(message.contains("robinhood"), "{message}");
    }

    #[test]
    fn implementation_registered_on_another_chain_is_not_reused() {
        let message = outcome(&registry(true), Chain::Robinhood, Some(IMPLEMENTATION));

        assert!(message.contains("unsupported algebra plugin"), "{message}");
    }

    #[test]
    fn generic_vm_chain_without_a_generic_vm_creator_is_rejected() {
        let message = outcome(&registry(false), Chain::Ethereum, Some(OTHER_IMPLEMENTATION));

        assert!(message.contains("no generic VM plugin handler registered"), "{message}");
        assert!(message.contains(&OTHER_IMPLEMENTATION.to_string()), "{message}");
    }

    #[test]
    fn global_registration_replaces_the_previous_creator() {
        let implementation = address!("00000000000000000000000000000000000000d5");
        let attributes = HashMap::new();
        let instantiate = || {
            let params = PluginCreationParams {
                pool: POOL,
                plugin: PLUGIN,
                attributes: &attributes,
                block: BlockContext::new(1, 1),
            };
            let Err(InvalidSnapshotError::ValueError(message)) =
                instantiate_plugin_handler(Chain::Base, Some(implementation), params)
            else {
                panic!("every test creator returns a value error");
            };
            message
        };

        register_plugin_handler(Chain::Base, implementation, Box::new(NamedCreator("first")))
            .expect("registration should succeed");
        assert_eq!(instantiate(), "first");

        register_plugin_handler(Chain::Base, implementation, Box::new(NamedCreator("second")))
            .expect("registration should succeed");
        assert_eq!(instantiate(), "second");
    }
}
