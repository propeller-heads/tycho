use anyhow::Result;
use substreams::store::{StoreGet, StoreGetRaw};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

use crate::{
    keys::{contract_id, market_tokens_key},
    market_state::reserve_delta,
};

/// Emits the per-event changes to every indexed market's PT and SY reserves.
///
/// The deltas are relative and are accumulated by `store_market_reserves`; `map_protocol_changes`
/// turns the accumulated totals into the `total_pt` / `total_sy` attributes. Ordinals come from
/// the log, which makes them strictly increasing per market and token within a block, as the
/// aggregation requires.
#[substreams::handlers::map]
pub fn map_reserve_deltas(block: eth::Block, store: StoreGetRaw) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(delta) = reserve_delta(log) else { continue };
            let component_id = contract_id(&log.address);
            let Some(roles) = store.get_last(market_tokens_key(&component_id)) else { continue };
            let roles: Vec<Vec<u8>> = serde_sibor::from_bytes(&roles)
                .expect("deserializing market token roles from the component store");
            let [sy, pt, _yt] = roles.as_slice() else {
                panic!("market {component_id} stored {} roles, expected 3", roles.len())
            };

            for (token, amount) in [(pt, delta.pt), (sy, delta.sy)] {
                balance_deltas.push(BalanceDelta {
                    ord: log.ordinal,
                    tx: Some(tx.into()),
                    token: token.clone(),
                    delta: amount.to_signed_bytes_be(),
                    component_id: component_id.as_bytes().to_vec(),
                });
            }
        }
    }
    Ok(BlockBalanceDeltas { balance_deltas })
}
