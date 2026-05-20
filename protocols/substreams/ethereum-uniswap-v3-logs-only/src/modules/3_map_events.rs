use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use uniswap_v3_core::events::{Pool, TxRef};

use crate::pb::uniswap::v3::{events::PoolEvent, Events, Pool as ProtoPool};

#[substreams::handlers::map]
pub fn map_events(
    block: eth::Block,
    pools_store: StoreGetProto<ProtoPool>,
) -> Result<Events, anyhow::Error> {
    let mut pool_events: Vec<PoolEvent> = block
        .transaction_traces
        .into_iter()
        .filter(|tx| tx.status == 1)
        .flat_map(|tx| {
            let tx_ref = TxRef {
                hash: tx.hash.clone(),
                from: tx.from.clone(),
                to: tx.to.clone(),
                index: tx.index as u64,
            };
            let receipt = tx
                .receipt
                .as_ref()
                .expect("receipt missing");
            receipt
                .logs
                .iter()
                .filter_map(|log| {
                    let key = format!("{}:{}", "Pool", log.address.to_hex());
                    let proto_pool = pools_store.get_last(key)?;
                    let pool = Pool {
                        address: proto_pool.address.clone(),
                        token0: proto_pool.token0.clone(),
                        token1: proto_pool.token1.clone(),
                    };
                    let core_event = uniswap_v3_core::events::decode_log(log, &pool, &tx_ref)?;
                    Some(core_event.into())
                })
                .collect::<Vec<_>>()
        })
        .collect();

    pool_events.sort_unstable_by_key(|e| e.log_ordinal);
    Ok(Events { pool_events })
}
