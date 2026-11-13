mod bytecode;
mod common;
mod ethcall_detector;
mod onchain_owners;
#[expect(dead_code, reason = "not used until the token pre-processor analyses by simulation")]
mod simulation;

pub(crate) use common::{
    arbitrary_recipient, calculate_fee_bps, call_request, map_block_tag, ObservedTransfer,
};
pub use ethcall_detector::EthCallDetector;
pub use onchain_owners::OnChainTokenOwners;
