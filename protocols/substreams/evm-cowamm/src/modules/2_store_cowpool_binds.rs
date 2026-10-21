use crate::pb::cowamm::CowPoolBinds;
use substreams::store::{Appender, StoreAppend};

#[substreams::handlers::store]
pub fn store_cowpool_binds(binds: CowPoolBinds, store: StoreAppend<String>) {
    for bind in binds.binds.iter() {
        let pool_key = hex::encode(&bind.address);
        // Format the bind as a JSON string, we use an AppendString store so that
        // the binds can persist across block state and we can create pools with the binds
        // in map_cowpools
        let bind_string = serde_json::json!({
            "address": hex::encode(&bind.address),
            "token": hex::encode(&bind.token),
            "weight": hex::encode(&bind.weight),
            "amount": hex::encode(&bind.amount),
            //store fields individually, reconstruct tx object in map cowpools
            //this information is useful for the deltas we want to create
            "from": hex::encode(&bind.tx.as_ref().unwrap().from),
            "to": hex::encode(&bind.tx.as_ref().unwrap().to),
            "hash": hex::encode(&bind.tx.as_ref().unwrap().hash),
            "index": hex::encode(bind.tx.clone().unwrap().index.to_le_bytes()),
            "ordinal": hex::encode(bind.ordinal.to_le_bytes()),
        })
        .to_string();
        store.append(0, pool_key, bind_string);
    }
}
