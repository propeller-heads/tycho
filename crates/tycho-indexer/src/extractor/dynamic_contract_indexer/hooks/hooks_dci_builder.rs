#![allow(dead_code)] // TODO: Remove this once the setup is fully implemented

use std::sync::Arc;

use tycho_common::{
    models::{Address, Chain},
    storage::{EntryPointGateway, ProtocolGateway},
    traits::{AccountExtractor, EntryPointTracer},
    Bytes,
};
use tycho_ethereum::{
    rpc::EthereumRpcClient,
    services::entrypoint_tracer::balance_slot_detector::EVMBalanceSlotDetector,
};

use crate::extractor::{
    dynamic_contract_indexer::{
        dci::DynamicContractIndexer,
        hooks::{
            component_metadata::{
                MetadataGeneratorRegistry, MetadataResponseParserRegistry, ProviderRegistry,
            },
            entrypoint_generator::{
                DefaultSwapAmountEstimator, HookEntrypointConfig, HookEntrypointGenerator,
                UniswapV4DefaultHookEntrypointGenerator, UNICHAIN_V4_MINI_ROUTER_BYTECODE,
                V4_MINI_ROUTER_BYTECODE,
            },
            hook_dci::UniswapV4HookDCI,
            hook_orchestrator::{DefaultUniswapV4HookOrchestrator, HookOrchestratorRegistry},
            integrations::register_integrations,
            metadata_orchestrator::BlockMetadataOrchestrator,
            rpc_metadata_provider::RPCMetadataProvider,
        },
    },
    ExtractionError,
};

// The defaults below shape the deltas the hooks DCI emits. They live in this module, not in the
// extractor factory, so the hooks source digest in every extractor fingerprint covers them.

/// Where traced swaps place the mini router, through a state override. Any address without a
/// contract works, but it appears in the traced calls and their pruned addresses.
const MINI_ROUTER_ADDRESS: &str = "0x2e234DAe75C793f67A35089C9d99245E1C58470b";
/// Failed attempts after which the hooks DCI pauses a component's tracing.
const PAUSE_AFTER_RETRIES: u32 = 3;
/// Failed attempts after which the hooks DCI stops retrying a component's tracing.
const MAX_RETRIES: u32 = 5;

/// Builder for creating a fully configured UniswapV4HookDCI
pub(crate) struct UniswapV4HookDCIBuilder<AE, T, G>
where
    AE: AccountExtractor + Send + Sync,
    T: EntryPointTracer + Send + Sync,
    G: EntryPointGateway + ProtocolGateway + Send + Sync,
{
    inner_dci: DynamicContractIndexer<AE, T, G>,
    rpc: EthereumRpcClient,
    router_address: Address,
    pool_manager: Address,
    db_gateway: G,
    chain: Chain,

    // Optional configuration with defaults
    pause_after_retries: u32,
    max_retries: u32,
    rpc_batch_size: usize,
}

impl<AE, T, G> UniswapV4HookDCIBuilder<AE, T, G>
where
    AE: AccountExtractor + Send + Sync,
    T: EntryPointTracer + Send + Sync,
    G: EntryPointGateway + ProtocolGateway + Send + Sync,
{
    /// Creates a new builder with sensible defaults
    pub(crate) fn new(
        inner_dci: DynamicContractIndexer<AE, T, G>,
        rpc: &EthereumRpcClient,
        pool_manager: Address,
        db_gateway: G,
        chain: Chain,
    ) -> Self {
        Self {
            inner_dci,
            rpc: rpc.clone(),
            router_address: Address::from(MINI_ROUTER_ADDRESS),
            pool_manager,
            db_gateway,
            chain,
            pause_after_retries: PAUSE_AFTER_RETRIES,
            max_retries: MAX_RETRIES,
            rpc_batch_size: 50,
        }
    }

    /// Sets the number of retries after which to pause
    pub(crate) fn pause_after_retries(mut self, retries: u32) -> Self {
        self.pause_after_retries = retries;
        self
    }

    /// Sets the maximum number of retries
    pub(crate) fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Sets the RPC batch size limit
    pub(super) fn rpc_batch_size(mut self, batch_size: usize) -> Self {
        self.rpc_batch_size = batch_size;
        self
    }

    /// Builds the UniswapV4HookDCI with the configured parameters
    pub(crate) fn build(self) -> Result<UniswapV4HookDCI<AE, T, G>, ExtractionError> {
        let mut generator_registry = MetadataGeneratorRegistry::new();
        let mut parser_registry = MetadataResponseParserRegistry::new();
        let mut provider_registry = ProviderRegistry::new();
        let mut hook_registry = HookOrchestratorRegistry::new();

        // Register custom hook integrations
        register_integrations(
            &mut generator_registry,
            &mut parser_registry,
            &mut provider_registry,
            self.rpc.get_url().to_string(),
        );

        // Register default RPC provider
        provider_registry.register_provider(
            "rpc_default".to_string(),
            Arc::new(RPCMetadataProvider::new_with_retry_config(
                self.rpc_batch_size,
                self.rpc.get_retry_config(),
            )),
        );

        // Create EVM balance slot detector
        let balance_slot_detector =
            EVMBalanceSlotDetector::new(&self.rpc).with_max_token_batch_size(5);

        // TODO: refactor the MiniRouter contract to take the pool manager address as a parameter
        let router_code = match self.chain {
            Chain::Ethereum => Bytes::from(V4_MINI_ROUTER_BYTECODE),
            Chain::Unichain => Bytes::from(UNICHAIN_V4_MINI_ROUTER_BYTECODE),
            _ => {
                return Err(ExtractionError::Unknown(format!(
                    "Unsupported chain for UniswapV4 Hooks DCI: {}",
                    self.chain
                )));
            }
        };

        let config = HookEntrypointConfig::new(
            Some(10),
            1,
            Some(self.router_address.clone()),
            Some(self.router_address),
            Some(router_code),
            self.pool_manager.clone(),
        );

        // Create entrypoint generator with default swap amount estimator (preferring balances)
        let mut entrypoint_generator = UniswapV4DefaultHookEntrypointGenerator::new(
            DefaultSwapAmountEstimator::with_limits(),
            self.pool_manager,
            balance_slot_detector,
        );
        entrypoint_generator.set_config(config);

        let default_orchestrator = DefaultUniswapV4HookOrchestrator::new(entrypoint_generator);

        // Set the default orchestrator for all hooks that don't have a specific orchestrator
        // registered
        hook_registry.set_default_orchestrator(Box::new(default_orchestrator));

        // Create metadata orchestrator
        let metadata_orchestrator =
            BlockMetadataOrchestrator::new(generator_registry, parser_registry, provider_registry);

        // Create and return configured Hook DCI
        Ok(UniswapV4HookDCI::new(
            self.inner_dci,
            metadata_orchestrator,
            hook_registry,
            self.db_gateway,
            self.chain,
            self.max_retries,
            self.pause_after_retries,
        ))
    }
}

#[cfg(test)]
mod tests {
    use tycho_common::traits::{MockAccountExtractor, MockEntryPointTracer};

    use super::*;
    use crate::testing::MockGateway;

    fn get_mock_gateway() -> MockGateway {
        let mut gateway = MockGateway::new();

        // Mock for inner DCI initialization
        gateway
            .expect_get_entry_points_tracing_params()
            .return_once(move |_, _| {
                Box::pin(async move {
                    Ok(tycho_common::storage::WithTotal {
                        entity: std::collections::HashMap::new(),
                        total: None,
                    })
                })
            });

        gateway
            .expect_get_entry_points()
            .return_once(move |_, _| {
                Box::pin(async move {
                    Ok(tycho_common::storage::WithTotal {
                        entity: std::collections::HashMap::new(),
                        total: None,
                    })
                })
            });

        gateway
            .expect_get_traced_entry_points()
            .return_once(move |_| Box::pin(async move { Ok(std::collections::HashMap::new()) }));

        gateway
            .expect_get_tokens()
            .return_once(move |_, _, _, _, _| {
                Box::pin(async move {
                    Ok(tycho_common::storage::WithTotal { entity: Vec::new(), total: Some(0) })
                })
            });

        gateway
    }

    #[test]
    fn test_hook_builder() {
        // Create mock dependencies
        let gateway = get_mock_gateway();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        // Create inner DCI
        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        // Create a mock RPC client URL
        let rpc_url = "http://localhost:8545".to_string();

        // Create test addresses
        let pool_manager = Address::from("0x0987654321098765432109876543210987654321");

        // Create mock RPC client (we can't actually create a real one without infrastructure)
        // For this test, we'll just verify the builder pattern works
        let rpc = EthereumRpcClient::new(&rpc_url).expect("Failed to create RPC client");

        let db_gateway = get_mock_gateway();

        // Test builder creation and configuration
        let builder = UniswapV4HookDCIBuilder::new(
            inner_dci,
            &rpc,
            pool_manager.clone(),
            db_gateway,
            Chain::Ethereum,
        );

        // Verify default values by building with custom values
        let builder = builder
            .pause_after_retries(5)
            .max_retries(10)
            .rpc_batch_size(100);

        // Verify the builder fields are set correctly
        assert_eq!(builder.pause_after_retries, 5);
        assert_eq!(builder.max_retries, 10);
        assert_eq!(builder.rpc_batch_size, 100);
        assert_eq!(builder.chain, Chain::Ethereum);
        assert_eq!(builder.router_address, Address::from(MINI_ROUTER_ADDRESS));
        assert_eq!(builder.pool_manager, pool_manager);
        assert_eq!(builder.rpc.get_url(), rpc_url);

        // Build the Hook DCI
        let _ = builder
            .build()
            .expect("Failed to build UniswapV4HookDCI");
    }
}
