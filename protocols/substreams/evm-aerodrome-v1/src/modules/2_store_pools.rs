use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};

use tycho_substreams::prelude::*;

#[substreams::handlers::store]
pub fn store_pools(
    pools_created: BlockChanges,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    for change in pools_created.changes {
        for new_protocol_component in change.component_changes {
            store.set_if_not_exists(0, &new_protocol_component.id, &new_protocol_component);
        }
    }
}
