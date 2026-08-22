use anyhow::Result;
use substreams::store::{StoreGet, StoreGetRaw};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

/// Balance tracking is not implemented yet; it lands with the reserve accounting.
#[substreams::handlers::map]
pub fn map_relative_component_balance(
    _block: eth::Block,
    _store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    Ok(BlockBalanceDeltas { balance_deltas: vec![] })
}
