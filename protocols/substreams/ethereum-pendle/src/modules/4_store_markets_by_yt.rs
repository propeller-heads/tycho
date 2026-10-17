use substreams::store::{Appender, StoreAppend};
use tycho_substreams::prelude::*;

use crate::{consts::PENDLE_MARKET, keys::markets_by_yt_key};

/// Lists, per yield token, every market priced against it.
///
/// The yield token emits `NewInterestIndex` from an address that is not a component, so it needs
/// its own route back to the markets. One yield token can back several markets: a PT and its YT
/// are created once per SY and expiry, and every factory may open a market on them. On mainnet 21
/// PTs back two or three markets each, so a key holding one market would leave the others without
/// the index updates.
#[substreams::handlers::store]
pub fn store_markets_by_yt(
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
            let yt = component
                .get_attribute_value("yt_address")
                .unwrap_or_else(|| panic!("market {} has no yt_address", component.id));
            store.append(0, markets_by_yt_key(&yt), component.id.clone());
        }
    }
}
