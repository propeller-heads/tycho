use crate::{common::*, config::DeploymentConfig, pb::tessera::v1::BlockStorageChanges};
use substreams::store::{StoreNew, StoreSet, StoreSetString};

/// Persist the custodian address across blocks; this store contains no token balances.
/// Consumers read the closing address with get_last and use old_value from the shared map
/// to recover the opening custodian when the same block contains a rotation.
#[substreams::handlers::store]
pub fn store_treasury(params: String, changes: BlockStorageChanges, store: StoreSetString) {
    let config = DeploymentConfig::parse(&params).expect("invalid deployment config");
    let treasury_slot = slot(config.treasury_slot);
    for tx in changes.transactions {
        for w in tx.writes {
            if w.address == config.tesseraswap && w.key == treasury_slot {
                store.set(w.ordinal, "treasury", &hex::encode(address(&w.new_value)));
            }
        }
    }
}
