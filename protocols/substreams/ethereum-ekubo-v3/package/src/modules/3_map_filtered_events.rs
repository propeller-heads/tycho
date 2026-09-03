use substreams::store::{StoreGet, StoreGetProto};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::{
    block_transaction_events::{transaction_events::pool_log::Event, TransactionEvents},
    BlockTransactionEvents, PoolDetails,
};

#[substreams::handlers::map]
fn map_filtered_events(
    block_tx_events: BlockTransactionEvents,
    pool_details_store: StoreGetProto<PoolDetails>,
) -> BlockTransactionEvents {
    BlockTransactionEvents {
        block_transaction_events: block_tx_events
            .block_transaction_events
            .into_iter()
            .filter_map(|tx_events| {
                let pool_logs: Vec<_> = tx_events
                    .pool_logs
                    .into_iter()
                    .filter(|log| {
                        match log
                            .event
                            .as_ref()
                            .expect("pool log should have an event")
                        {
                            Event::PoolInitialized(_) |
                            Event::PoolSnapshot(_) |
                            Event::VirtualExecution(_) |
                            Event::RateUpdated(_) => true,
                            Event::Swapped(_) | Event::PositionUpdated(_) => {
                                pool_details_store.has_last(log.pool_id.to_hex())
                            }
                        }
                    })
                    .collect();

                (!pool_logs.is_empty())
                    .then_some(TransactionEvents { transaction: tx_events.transaction, pool_logs })
            })
            .collect(),
        timestamp: block_tx_events.timestamp,
    }
}
