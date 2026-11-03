use substreams::prelude::*;
use tycho_substreams::prelude::*;

/// Records which components have been created within this run, so downstream modules
/// never emit changes for components outside the indexed window.
#[substreams::handlers::store]
pub fn store_components(
    components: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsInt64,
) {
    for tx_components in components.tx_components.iter() {
        for component in tx_components.components.iter() {
            store.set_if_not_exists(0, &component.id, &1);
        }
    }
}

pub fn component_created(store: &StoreGetInt64, component_id: &str) -> bool {
    store.get_last(component_id).is_some()
}
