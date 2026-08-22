use anyhow::Result;
use std::collections::HashSet;
use substreams::pb::substreams::StoreDeltas;
use tycho_substreams::prelude::*;

use crate::consts::PENDLE_SY;

/// Drops the repeated SY components, leaving one creation per SY.
///
/// A store delta exists only where `store_sy_seen` actually wrote, so the delta keys for this
/// block name the SYs seen for the first time — that rules out an SY re-emitted in a later
/// block. Within this block the delta cannot distinguish two markets sharing an SY, since both
/// carry the same first-seen key, so emitted ids are also tracked as the block is walked.
#[substreams::handlers::map]
pub fn map_protocol_components(
    market_components: BlockTransactionProtocolComponents,
    sy_seen_deltas: StoreDeltas,
) -> Result<BlockTransactionProtocolComponents> {
    let first_seen: HashSet<&str> = sy_seen_deltas
        .deltas
        .iter()
        .map(|delta| delta.key.as_str())
        .collect();

    let mut emitted_sy: HashSet<String> = HashSet::new();
    let mut tx_components = Vec::new();
    for tx in market_components.tx_components {
        let mut components = Vec::new();
        for component in tx.components {
            let is_sy = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_SY);
            if is_sy &&
                (!first_seen.contains(component.id.as_str()) ||
                    !emitted_sy.insert(component.id.clone()))
            {
                continue;
            }
            components.push(component);
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: tx.tx, components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}
