use std::collections::HashSet;

use anyhow::Result;
use substreams::store::{StoreGet, StoreGetRaw};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

use crate::{
    deltas::in_transaction_order,
    keys::{contract_id, market_tokens_key},
    market_state::reserve_delta,
    sy::supply_seed,
    transfers::{new_sy_components, supply_changes},
};

/// Emits the per-event changes to the reserves quotes rest on: every market's PT and SY reserves,
/// and every SY's supply.
///
/// The deltas are relative and are accumulated by `store_market_reserves`; `map_protocol_changes`
/// turns the accumulated totals into the `total_pt` / `total_sy` attributes of a market and the
/// `sy_total_supply` attribute of an SY. A market's ordinals come from the log, which makes them
/// strictly increasing per market and token within a block, as the aggregation requires.
///
/// An SY's supply is seeded once, from the `totalSupply()` read at its creation, and moved
/// afterwards by its own mints and burns, so it costs no read after creation. SY shares do not
/// rebase and every mint and burn emits a `Transfer`, which is what makes the running total exact.
/// The seed is read at the end of the creation block, so that block's own mints and burns are
/// already in it and are skipped.
#[substreams::handlers::map]
pub fn map_reserve_deltas(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
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

    let created = new_sy_components(&new_components);
    let seeded: HashSet<&str> = created
        .iter()
        .map(|(_, component)| component.id.as_str())
        .collect();
    for change in supply_changes(&block, &store) {
        if seeded.contains(change.sy_id.as_str()) {
            continue;
        }
        balance_deltas.push(BalanceDelta {
            ord: change.log.ordinal,
            tx: Some(change.tx.into()),
            token: change.log.address.clone(),
            delta: change.amount.to_signed_bytes_be(),
            component_id: change.sy_id.into_bytes(),
        });
    }
    for (tx, component) in created {
        let Some(supply) = supply_seed(component) else {
            panic!("SY {} was created without its supply seed", component.id)
        };
        balance_deltas.push(BalanceDelta {
            ord: 0,
            tx: Some(tx.clone()),
            token: hex::decode(component.id.trim_start_matches("0x"))
                .expect("SY component id is not hex"),
            delta: supply.to_signed_bytes_be(),
            component_id: component.id.clone().into_bytes(),
        });
    }
    Ok(in_transaction_order(balance_deltas))
}
