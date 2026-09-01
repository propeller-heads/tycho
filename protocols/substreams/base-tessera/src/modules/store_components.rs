use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::prelude::*;

use crate::common::pair_store_key;

/// Indexes discovered pairs by their contract address, so the tracked-contract predicate and
/// per-pair lookups (admin-mutation attribution, re-simulation marks) can resolve a storage
/// change back to its component.
#[substreams::handlers::store]
pub fn store_components(
    map: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    for tx_pc in map.tx_components {
        for pc in tx_pc.components {
            let pair = hex::decode(pc.id.trim_start_matches("0x")).unwrap_or_default();
            store.set_if_not_exists(0, pair_store_key(&pair), &pc);
        }
    }
}
