mod bytecode;
mod common;
mod ethcall_detector;
mod simulation;

use std::{collections::HashSet, sync::Arc};

use alloy::primitives::Address;
pub(crate) use common::{
    arbitrary_recipient, calculate_fee_bps, call_request, map_block_tag, ObservedTransfer,
};
pub use ethcall_detector::EthCallDetector;
pub use simulation::SimulationDetector;
use tycho_common::{
    models::Chain,
    traits::{TokenAnalyzer, TokenOwnerFinding},
};

use crate::rpc::EthereumRpcClient;

/// Which `TokenAnalyzer` to build.
#[derive(Debug, Clone)]
pub enum AnalyzerKind {
    /// `SimulationDetector` for `chain`, never quoting tokens that call a contract in
    /// `watchlist`.
    Simulation { watchlist: Arc<HashSet<Address>>, chain: Chain },
    /// `EthCallDetector`, transferring through `settlement_contract`.
    EthCall { settlement_contract: Address },
}

impl AnalyzerKind {
    /// `Simulation` when `rpc` supports `eth_simulateV1` and `debug_traceCall`, else `EthCall`.
    pub async fn select_for_rpc(
        rpc: &EthereumRpcClient,
        chain: Chain,
        settlement_contract: Address,
        watchlist: Arc<HashSet<Address>>,
    ) -> Self {
        match simulation::ensure_rpc_supports_simulation(rpc).await {
            Ok(()) => AnalyzerKind::Simulation { watchlist, chain },
            Err(error) => {
                tracing::warn!(
                    error,
                    "RPC lacks eth_simulateV1 or debug_traceCall; falling back to the eth_call \
                     token analyzer"
                );
                AnalyzerKind::EthCall { settlement_contract }
            }
        }
    }

    pub fn build(
        &self,
        rpc: &EthereumRpcClient,
        finder: Arc<dyn TokenOwnerFinding>,
    ) -> Box<dyn TokenAnalyzer<Error = String>> {
        match self {
            AnalyzerKind::Simulation { watchlist, chain } => {
                Box::new(SimulationDetector::new(rpc, finder, watchlist.clone(), *chain))
            }
            AnalyzerKind::EthCall { settlement_contract } => {
                Box::new(EthCallDetector::new(rpc, finder, *settlement_contract))
            }
        }
    }
}
