use serde::Deserialize;
use substreams::{
    scalar::BigInt,
    store::{StoreNew, StoreSet, StoreSetBigInt},
};
use substreams_ethereum::pb::eth::v2::{self as eth};

use super::alpha::{decode_start_update, parse_hooks, start_key};

/// `hooks`: the `CLAlphaHook` addresses, hex without `0x`, comma-separated.
#[derive(Debug, Deserialize)]
pub(crate) struct HookParams {
    pub(crate) hooks: String,
}

/// `poolStartedTimestamp[id]` per pool (`start:0x<pool id>`), from every `PoolStartedAtUpdated`
/// a listed hook emits, written at the log's ordinal.
///
/// Keyed by pool id whether or not the pool exists yet: the first hook version sets the start
/// time in a transaction of its own before the pool is initialized, so the value has to be on
/// hand when `map_alpha_pools_created` sees the `Initialize`.
#[substreams::handlers::store]
pub fn store_alpha_start_times(params: String, block: eth::Block, store: StoreSetBigInt) {
    let params: HookParams = serde_qs::from_str(&params)
        .unwrap_or_else(|err| panic!("invalid store_alpha_start_times params {params:?}: {err}"));
    let hooks = parse_hooks(&params.hooks).unwrap_or_else(|err| panic!("invalid hook list: {err}"));

    for tx in block
        .transaction_traces
        .iter()
        .filter(|tx| tx.status == 1)
    {
        let Some(receipt) = tx.receipt.as_ref() else { continue };
        for log in &receipt.logs {
            if let Some((pool_id, start)) = decode_start_update(log, &hooks) {
                store.set(
                    log.ordinal,
                    start_key(&pool_id),
                    &BigInt::from_unsigned_bytes_be(&start),
                );
            }
        }
    }
}
