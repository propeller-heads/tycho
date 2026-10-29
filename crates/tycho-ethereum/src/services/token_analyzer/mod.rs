mod bytecode;
mod common;
mod ethcall_detector;
#[allow(dead_code)] // Nothing builds the simulation analyzer yet.
mod simulation;

use std::sync::Arc;

use alloy::primitives::Address;
pub(crate) use common::{
    arbitrary_recipient, calculate_fee_bps, call_request, map_block_tag, ObservedTransfer,
};
pub use ethcall_detector::EthCallDetector;
use tycho_common::traits::{TokenAnalyzer, TokenOwnerFinding};

use crate::rpc::EthereumRpcClient;

/// Which `TokenAnalyzer` to build.
#[derive(Debug, Clone)]
pub enum AnalyzerKind {
    /// `EthCallDetector`, transferring through `settlement_contract`.
    EthCall { settlement_contract: Address },
}

impl AnalyzerKind {
    pub fn build(
        &self,
        rpc: &EthereumRpcClient,
        finder: Arc<dyn TokenOwnerFinding>,
    ) -> Box<dyn TokenAnalyzer<Error = String>> {
        match self {
            AnalyzerKind::EthCall { settlement_contract } => {
                Box::new(EthCallDetector::new(rpc, finder, *settlement_contract))
            }
        }
    }
}
