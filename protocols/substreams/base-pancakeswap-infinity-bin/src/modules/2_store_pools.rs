use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::models::BlockEntityChanges;

use crate::{keys::pool_key, pb::pancakeswap::infinity::bin::Pool};

/// Stores every created pool so later modules can match events to known pools.
#[substreams::handlers::store]
pub fn store_pools(pools_created: BlockEntityChanges, store: StoreSetIfNotExistsProto<Pool>) {
    for change in pools_created.changes {
        for component_change in &change.component_changes {
            // The component id is the pool id's 0x-hex, as `1_map_pool_created` emits it.
            let pool_id = hex::decode(
                component_change
                    .id
                    .trim_start_matches("0x"),
            )
            .expect("component id is the pool id in hex");
            let pool: Pool = Pool {
                id: pool_id.clone(),
                currency0: component_change.tokens[0].clone(),
                currency1: component_change.tokens[1].clone(),
                created_tx_hash: change.tx.as_ref().unwrap().hash.clone(),
            };
            store.set_if_not_exists(0, pool_key(&pool_id), &pool);
        }
    }
}
