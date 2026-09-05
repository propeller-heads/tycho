use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw, StoreNew, StoreSet, StoreSetBigInt},
};
use substreams_ethereum::{pb::eth::v2 as eth, rpc::RpcBatch};
use tycho_substreams::prelude::*;

use crate::{
    abi::pendle_yield_token,
    consts::PENDLE_MARKET,
    keys::{market_by_yt_key, py_index_key},
    market_state::py_index_stored,
};

/// Tracks each market's `pyIndexStored`, the floor half of the live PY index.
///
/// `map_protocol_changes` needs the value on refresh blocks, where the yield token has emitted
/// nothing, so it cannot be read off this block's logs — it has to be carried.
///
/// New markets are seeded with an `eth_call`. `NewInterestIndex` fires only on PY mint, redeem
/// and interest collection, which can be weeks apart, so without a seed the floor would be
/// missing for the whole gap and the index would follow the SY rate alone — wrong for any market
/// whose SY rate has fallen since its last interaction.
#[substreams::handlers::store]
pub fn store_py_index(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    components_store: StoreGetRaw,
    store: StoreSetBigInt,
) {
    for (component_id, index) in seed_new_markets(&new_components) {
        store.set(0, py_index_key(&component_id), &index);
    }

    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(index) = py_index_stored(log) else { continue };
            let Some(component_id) = components_store.get_last(market_by_yt_key(&log.address))
            else {
                continue;
            };
            let component_id =
                String::from_utf8(component_id).expect("market id is not valid utf-8");
            store.set(log.ordinal, py_index_key(&component_id), &index);
        }
    }
}

/// Reads `pyIndexStored()` off the yield token of every market created in this block.
///
/// A yield token that will not answer is left unseeded rather than guessed at: the index then
/// falls back to the SY rate until the first `NewInterestIndex`.
fn seed_new_markets(components: &BlockTransactionProtocolComponents) -> Vec<(String, BigInt)> {
    let mut markets = Vec::new();
    for tx_components in &components.tx_components {
        for component in &tx_components.components {
            let is_market = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_MARKET);
            if !is_market {
                continue;
            }
            let yt = component
                .get_attribute_value("yt_address")
                .unwrap_or_else(|| panic!("market {} has no yt_address", component.id));
            markets.push((component.id.clone(), yt));
        }
    }
    if markets.is_empty() {
        return vec![];
    }

    let mut batch = RpcBatch::new();
    for (_, yt) in &markets {
        batch = batch.add(pendle_yield_token::functions::PyIndexStored {}, yt.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut seeds = Vec::new();
    for ((component_id, _), response) in markets
        .into_iter()
        .zip(responses.iter())
    {
        let Some(index) =
            RpcBatch::decode::<_, pendle_yield_token::functions::PyIndexStored>(response)
        else {
            continue;
        };
        seeds.push((component_id, index));
    }
    seeds
}
