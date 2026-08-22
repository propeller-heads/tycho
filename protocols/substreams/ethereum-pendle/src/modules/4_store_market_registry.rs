use substreams::{
    scalar::BigInt,
    store::{Appender, StoreAppend},
};
use tycho_substreams::prelude::*;

use crate::{
    consts::PENDLE_MARKET,
    keys::{contract_id, MARKET_REGISTRY},
    registry::MarketEntry,
};

/// Keeps every market ever created under one key, so the per-block refresh can enumerate them.
///
/// `store_protocol_components` answers "is this address one of ours?" — it is keyed by the
/// address a log arrives from. Refreshing the PY index asks the opposite question, on a block
/// where no Pendle log arrived at all, and a key-addressed store cannot answer it: `StoreGet`
/// exposes only `get_at` / `get_last` / `get_first`. Hence one key whose value is the set.
///
/// Only markets go in. The SYs behind them are derived from the live subset each block, since an
/// SY whose every market has expired needs no refresh.
#[substreams::handlers::store]
pub fn store_market_registry(
    components: BlockTransactionProtocolComponents,
    store: StoreAppend<String>,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            let is_market = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_MARKET);
            if !is_market {
                continue;
            }

            let sy = component
                .get_attribute_value("sy_address")
                .unwrap_or_else(|| panic!("market {} has no sy_address", component.id));
            let expiry = component
                .get_attribute_value("expiry")
                .unwrap_or_else(|| panic!("market {} has no expiry", component.id));
            let entry = MarketEntry {
                id: component.id.clone(),
                sy: contract_id(&sy),
                expiry: BigInt::from_signed_bytes_be(&expiry).to_u64(),
            };
            store.append(0, MARKET_REGISTRY, entry.encode());
        }
    }
}
