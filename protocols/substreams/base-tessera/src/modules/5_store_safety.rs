use crate::{common::*, config::DeploymentConfig};
use substreams::store::{StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth;

/// Persist fee-tag-0 and epoch signals even when the affected pair has no price update.
#[substreams::handlers::store]
pub fn store_safety(
    params: String,
    block: eth::v2::Block,
    pairs: StoreGetString,
    store: StoreSetString,
) {
    let config = DeploymentConfig::parse(&params).expect("invalid deployment config");
    let known: std::collections::HashSet<_> = crate::common::pairs(&pairs, "pairs")
        .into_iter()
        .collect();
    let fee_key = fee_tag_zero_slot();
    for tx in block.transactions() {
        for w in committed_writes(tx) {
            if known.contains(&id(&w.address)) && w.key == slot(config.pair_write_helper_slot) {
                store.set(
                    w.ordinal,
                    format!("helper:{}", id(&w.address)),
                    &hex::encode(address(&w.new_value)),
                );
            }
            // Remember this single candidate slot even before helper assignment.
            // Otherwise an already-configured helper could be adopted with a nonzero
            // tag-0 fee and look safe. No other candidate storage/code is indexed.
            if w.key == fee_key.as_slice() {
                store.set(w.ordinal, format!("fee:{}", id(&w.address)), &hex::encode(&w.new_value));
            }
            if w.address == config.tesseraswap && w.key == slot(0) {
                store.set(w.ordinal, "engine", &hex::encode(address(&w.new_value)));
            }
        }
    }
}
