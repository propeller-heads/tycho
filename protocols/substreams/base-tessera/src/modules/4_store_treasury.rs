use crate::{common::*, config::DeploymentConfig};
use substreams::store::{StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth;

#[substreams::handlers::store]
pub fn store_treasury(params: String, block: eth::v2::Block, store: StoreSetString) {
    let config = DeploymentConfig::parse(&params).expect("invalid deployment config");
    for tx in block.transactions() {
        for w in committed_writes(tx) {
            if w.address == config.tesseraswap && w.key == slot(config.treasury_slot) {
                store.set(w.ordinal, "treasury", &hex::encode(address(&w.new_value)));
            }
        }
    }
}
