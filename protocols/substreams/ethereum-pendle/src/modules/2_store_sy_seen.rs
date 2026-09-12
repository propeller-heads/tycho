use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString};
use tycho_substreams::prelude::*;

use crate::consts::PENDLE_SY;

/// Records the first block in which each SY was referenced by a market.
///
/// One SY backs every market on the same asset across expiries, so `map_market_components`
/// re-emits it for each. `set_if_not_exists` writes only on first sight, which means the store's
/// deltas for a block name exactly the SYs that are genuinely new — that is what
/// `map_protocol_components` filters on.
#[substreams::handlers::store]
pub fn store_sy_seen(
    components: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsString,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            let is_sy = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_SY);
            if !is_sy {
                continue;
            }
            store.set_if_not_exists(0, &component.id, &component.id);
        }
    }
}
