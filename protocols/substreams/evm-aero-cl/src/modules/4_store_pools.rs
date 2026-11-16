use crate::pb::tycho::evm::aerodrome::Pool;
use std::str;
use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::prelude::BlockChanges;

#[substreams::handlers::store]
pub fn store_pools(pools_created: BlockChanges, store: StoreSetIfNotExistsProto<Pool>) {
    for change in pools_created.changes {
        for component_change in &change.component_changes {
            let pool_address: &str = &component_change.id;
            let pool: Pool = Pool {
                address: hex::decode(pool_address.trim_start_matches("0x")).unwrap(),
                token0: component_change.tokens[0].clone(),
                token1: component_change.tokens[1].clone(),
                created_tx_hash: change.tx.as_ref().unwrap().hash.clone(),
            };
            store.set_if_not_exists(0, format!("{}:{}", "Pool", pool_address), &pool);
        }
    }
}
