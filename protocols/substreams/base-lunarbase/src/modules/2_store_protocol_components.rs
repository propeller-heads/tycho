use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::prelude as tycho;

#[substreams::handlers::store]
pub fn store_protocol_components(
    components: tycho::BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsProto<tycho::ProtocolComponent>,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            store.set_if_not_exists(0, component_key(&component.id), &component);
        }
    }
}

fn component_key(component_id: &str) -> String {
    format!("component:{component_id}")
}
