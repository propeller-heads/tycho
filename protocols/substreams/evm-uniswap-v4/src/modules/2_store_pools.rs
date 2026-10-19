use std::str;

use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::models::BlockEntityChanges;

use crate::pb::uniswap::v4::Pool;

#[substreams::handlers::store]
pub fn store_pools(pools_created: BlockEntityChanges, store: StoreSetIfNotExistsProto<Pool>) {
    // Store pools. Required so the next maps can match any event to a known pool by their address

    for change in pools_created.changes {
        for component_change in &change.component_changes {
            let pool_address: &str = &component_change.id;
            let pool: Pool = Pool {
                id: hex::decode(pool_address.trim_start_matches("0x")).unwrap(),
                currency0: component_change.tokens[0].clone(),
                currency1: component_change.tokens[1].clone(),
                created_tx_hash: change.tx.as_ref().unwrap().hash.clone(),
            };
            store.set_if_not_exists(0, format!("{}:{}", "pool", pool_address), &pool);
        }
    }
}
