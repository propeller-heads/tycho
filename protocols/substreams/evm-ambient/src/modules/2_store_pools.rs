use substreams::store::{StoreNew, StoreSet, StoreSetProto};
use tycho_substreams::models::ProtocolComponent;

use crate::pb::tycho::ambient::v1::BlockPoolChanges;

#[substreams::handlers::store]
pub fn store_pools(changes: BlockPoolChanges, component_store: StoreSetProto<ProtocolComponent>) {
    for component in changes.new_components {
        let protocol_component: ProtocolComponent = component.into();
        component_store.set(0, protocol_component.id.clone(), &protocol_component);
    }
}
