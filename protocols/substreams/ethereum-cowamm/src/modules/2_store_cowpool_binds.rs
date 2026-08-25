use crate::pb::cowamm::{CowPoolBind, CowPoolBinds};
use prost::Message;
use substreams::store::{Appender, StoreAppend};

/// Hex-encodes the protobuf message so the append store can accumulate the per-pool
/// binding history as a `;`-delimited string; hex never collides with the delimiter.
pub(crate) fn encode_binding_change(bind: &CowPoolBind) -> String {
    hex::encode(bind.encode_to_vec())
}

#[substreams::handlers::store]
pub fn store_cowpool_binds(binds: CowPoolBinds, store: StoreAppend<String>) {
    for bind in binds.binds.iter() {
        // The history persists across blocks; map_cowpools reduces it to the bindings
        // that are still active when the pool is announced.
        let pool_key = hex::encode(&bind.address);
        store.append(bind.ordinal, pool_key, encode_binding_change(bind));
    }
}
