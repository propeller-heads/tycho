//! PancakeSwap Infinity `CLAlphaHook`, the hook Binance Alpha listings launch their CL pools
//! with: a per-pool start-time gate in front of an otherwise hookless pool.
pub mod hook_handler;
pub mod hook_handler_creator;

use alloy::primitives::U256;
use tycho_common::Bytes;

use crate::protocol::errors::InvalidSnapshotError;

/// State attribute holding the pool's `poolStartedTimestamp`, in seconds. Zero disables the pool.
pub const START_TIMESTAMP_ATTRIBUTE: &str = "alpha_start_timestamp";

/// Decodes the big-endian unsigned start time the indexer writes with `to_signed_bytes_be`.
///
/// The contract stores a `uint256`, and the first version lets the owner write any value, so a
/// time past `u64::MAX` saturates there: such a pool never opens, as on chain.
pub fn decode_start_timestamp(raw: &Bytes) -> Result<u64, InvalidSnapshotError> {
    let trimmed = raw
        .iter()
        .position(|byte| *byte != 0)
        .map_or(&raw[raw.len()..], |first| &raw[first..]);
    if trimmed.len() > 32 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{START_TIMESTAMP_ATTRIBUTE} is {} bytes (0x{}), wider than the uint256 it encodes",
            raw.len(),
            hex::encode(raw)
        )));
    }
    Ok(u64::try_from(U256::from_be_slice(trimmed)).unwrap_or(u64::MAX))
}
