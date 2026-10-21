use std::str;

use substreams::{
    scalar::BigInt,
    store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto},
};
use tycho_substreams::models::BlockChanges;

use crate::pb::pancakeswap::v3::Pool;

#[substreams::handlers::store]
pub fn store_pools(pools_created: BlockChanges, store: StoreSetIfNotExistsProto<Pool>) {
    // Store pools. Required so the next maps can match any event to a known pool by their address

    for change in pools_created.changes {
        for component_change in &change.component_changes {
            let pool_address: &str = &component_change.id;
            let pool: Pool = Pool {
                address: hex::decode(pool_address.trim_start_matches("0x")).unwrap(),
                token0: component_change.tokens[0].clone(),
                token1: component_change.tokens[1].clone(),
                created_tx_hash: change.tx.as_ref().unwrap().hash.clone(),
                fee: BigInt::from_signed_bytes_be(
                    &component_change
                        .static_att
                        .iter()
                        .find(|attr| attr.name == "fee")
                        .expect("every pool should have fee as static attribute")
                        .value,
                )
                .to_u64(),
            };
            store.set_if_not_exists(0, format!("{}:{}", "Pool", pool_address), &pool);
        }
    }
}
