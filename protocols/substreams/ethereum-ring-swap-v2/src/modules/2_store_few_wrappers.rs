use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsRaw};

use crate::store_key::StoreKey;
use tycho_substreams::prelude::*;

#[substreams::handlers::store]
pub fn store_few_wrappers(pools_created: BlockChanges, store: StoreSetIfNotExistsRaw) {
    for changes in pools_created.changes {
        for component in changes.component_changes {
            store_wrapper(&component, "underlying_token0", "fw_token0", &store);
            store_wrapper(&component, "underlying_token1", "fw_token1", &store);
        }
    }
}

fn store_wrapper(
    component: &ProtocolComponent,
    underlying_attribute: &str,
    wrapper_attribute: &str,
    store: &StoreSetIfNotExistsRaw,
) {
    let underlying = static_attribute(component, underlying_attribute);
    let wrapper = static_attribute(component, wrapper_attribute);
    store.set_if_not_exists(
        0,
        StoreKey::FewWrapper.get_unique_key(&hex::encode(underlying)),
        &wrapper,
    );
}

fn static_attribute<'a>(component: &'a ProtocolComponent, name: &str) -> &'a [u8] {
    component
        .static_att
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.value.as_slice())
        .unwrap_or_else(|| {
            panic!("Ring pool {} is missing the {} static attribute", component.id, name)
        })
}
