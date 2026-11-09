use std::sync::Arc;

use alloy::{
    network::Ethereum,
    providers::{ProviderBuilder, RootProvider},
};
use alloy_chains::NamedChain;
use miette::{IntoDiagnostic, WrapErr};
use tycho_ethereum::{
    rpc::EthereumRpcClient,
    services::entrypoint_tracer::{
        allowance_slot_detector::EVMAllowanceSlotDetector,
        balance_slot_detector::EVMBalanceSlotDetector,
    },
};
use tycho_simulation::tycho_common::models::{Chain, NativeAsset};

#[derive(Clone)]
pub struct RPCTools {
    pub rpc_url: String,
    pub native_asset: NativeAsset,
    pub provider: RootProvider<Ethereum>,
    pub evm_balance_slot_detector: Arc<EVMBalanceSlotDetector>,
    pub evm_allowance_slot_detector: Arc<EVMAllowanceSlotDetector>,
}

impl RPCTools {
    pub async fn new(rpc_url: &str, chain: &Chain) -> miette::Result<Self> {
        let builder = ProviderBuilder::default();
        // A named chain only tunes the polling interval. alloy-chains lags new chains such as
        // Elysium, so a chain it does not know keeps alloy's default interval.
        let provider: RootProvider<Ethereum> = match NamedChain::try_from(chain.id()) {
            Ok(named_chain) => {
                builder
                    .with_chain(named_chain)
                    .connect(rpc_url)
                    .await
            }
            Err(_unknown_to_alloy_chains) => builder.connect(rpc_url).await,
        }
        .into_diagnostic()
        .wrap_err("Failed to connect to provider")?;

        let rpc = EthereumRpcClient::new(rpc_url)
            .into_diagnostic()
            .wrap_err("Failed to create Ethereum RPC client")?;

        let evm_balance_slot_detector = Arc::new(EVMBalanceSlotDetector::new(&rpc));
        let evm_allowance_slot_detector = Arc::new(EVMAllowanceSlotDetector::new(&rpc));

        Ok(Self {
            rpc_url: rpc_url.to_string(),
            native_asset: chain.native_asset(),
            provider,
            evm_balance_slot_detector,
            evm_allowance_slot_detector,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_on_a_chain_alloy_chains_does_not_know() {
        assert!(NamedChain::try_from(Chain::Elysium.id()).is_err());

        let tools = RPCTools::new("http://localhost:8545", &Chain::Elysium)
            .await
            .unwrap();

        assert_eq!(tools.native_asset, Chain::Elysium.native_asset());
    }

    #[tokio::test]
    async fn connects_on_a_named_chain() {
        RPCTools::new("http://localhost:8545", &Chain::Ethereum)
            .await
            .unwrap();
    }
}
