use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::prelude::*;

#[substreams::handlers::store]
pub fn store_components(
    map: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    for tx in map.tx_components {
        for c in tx.components {
            store.set_if_not_exists(0, &c.id, &c);
        }
    }
}
