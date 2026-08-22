use substreams::store::{StoreNew, StoreSet, StoreSetRaw};
use tycho_substreams::prelude::*;

/// Indexes each component's token list by component id, for the balance modules to look up.
#[substreams::handlers::store]
pub fn store_protocol_components(
    components: BlockTransactionProtocolComponents,
    store: StoreSetRaw,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            let tokens = serde_sibor::to_bytes(&component.tokens)
                .expect("serializing component tokens for the component store");
            store.set(0, component.id.clone(), &tokens);
        }
    }
}
