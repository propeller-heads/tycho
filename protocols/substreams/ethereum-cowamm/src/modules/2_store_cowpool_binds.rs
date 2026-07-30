use crate::pb::cowamm::CowPoolBinds;
use anyhow::{Context, Result};
use substreams::store::{Appender, StoreAppend};

pub(crate) fn serialize_binding_change(bind: &crate::pb::cowamm::CowPoolBind) -> Result<String> {
    let tx = bind
        .tx
        .as_ref()
        .context("CowAMM binding change is missing transaction context")?;
    Ok(serde_json::json!({
        "address": hex::encode(&bind.address),
        "token": hex::encode(&bind.token),
        "weight": hex::encode(&bind.weight),
        "amount": hex::encode(&bind.amount),
        "from": hex::encode(&tx.from),
        "to": hex::encode(&tx.to),
        "hash": hex::encode(&tx.hash),
        "index": hex::encode(tx.index.to_le_bytes()),
        "ordinal": hex::encode(bind.ordinal.to_le_bytes()),
        "change_type": bind.change_type,
    })
    .to_string())
}

#[substreams::handlers::store]
pub fn store_cowpool_binds(binds: CowPoolBinds, store: StoreAppend<String>) {
    for bind in binds.binds.iter() {
        let pool_key = hex::encode(&bind.address);
        // Format the bind as a JSON string, we use an AppendString store so that
        // the binds can persist across block state and we can create pools with the binds
        // in map_cowpools.
        // Serialization only fails when the map module emitted a bind without its
        // transaction, which is a bug; skipping the change would silently corrupt the
        // active-binding history, so fail loudly instead.
        let bind_string = serialize_binding_change(bind)
            .expect("map_cowpool_binds emitted a binding change without its transaction");
        store.append(bind.ordinal, pool_key, bind_string);
    }
}
