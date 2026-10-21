use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::models::BlockChanges;

use crate::pb::ekubo::PoolDetails;

// Since only the PoolInitialized event contains the complete pool key we need to store some info
// required when processing other events
#[substreams::handlers::store]
fn store_pool_details(changes: BlockChanges, store: StoreSetIfNotExistsProto<PoolDetails>) {
    changes
        .changes
        .into_iter()
        .flat_map(|c| c.component_changes.into_iter())
        .for_each(|component| {
            let attrs = component.static_att;

            let pool_details = PoolDetails {
                token0: attrs[0].value.clone(),
                token1: attrs[1].value.clone(),
                fee: u64::from_be_bytes(
                    attrs[2]
                        .value
                        .clone()
                        .try_into()
                        .unwrap(),
                ),
            };

            store.set_if_not_exists(0, component.id, &pool_details);
        });
}
