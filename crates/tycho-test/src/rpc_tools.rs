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
        let provider: RootProvider<Ethereum> = ProviderBuilder::default()
            .with_chain(named_chain(chain)?)
            .connect(rpc_url)
            .await
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

fn named_chain(chain: &Chain) -> miette::Result<NamedChain> {
    NamedChain::try_from(chain.id())
        .into_diagnostic()
        .wrap_err_with(|| {
            format!("alloy-chains has no named chain for {chain} (id {})", chain.id())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_chains_have_named_chains() {
        let chains = [
            Chain::Ethereum,
            Chain::Arbitrum,
            Chain::Base,
            Chain::Bsc,
            Chain::Unichain,
            Chain::Polygon,
            Chain::Plasma,
            Chain::Robinhood,
            Chain::Arc,
        ];
        for chain in chains {
            named_chain(&chain).unwrap_or_else(|e| panic!("{e:?}"));
        }
    }
}
