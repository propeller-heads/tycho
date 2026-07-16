use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::models::BlockEntityChanges;

use crate::pb::ramses::v3::Pool;

#[substreams::handlers::store]
pub fn store_pools(pools_created: BlockEntityChanges, store: StoreSetIfNotExistsProto<Pool>) {
    // Store pools. Required so the next maps can match any event to a known pool by their address

    for change in pools_created.changes {
        for component_change in change.component_changes {
            let [token0, token1] = component_change
                .tokens
                .try_into()
                .expect("exactly two tokens");

            let pool_address = component_change
                .id
                .trim_start_matches("0x");

            store.set_if_not_exists(0, pool_address, &Pool { token0, token1 });
        }
    }
}
