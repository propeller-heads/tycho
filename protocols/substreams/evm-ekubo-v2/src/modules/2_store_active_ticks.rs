use substreams::store::{StoreSet, StoreSetInt64};

use substreams::store::StoreNew;
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::pool_log::Event, BlockTransactionEvents,
};

#[substreams::handlers::store]
pub fn store_active_ticks(block_tx_events: BlockTransactionEvents, store: StoreSetInt64) {
    block_tx_events
        .block_transaction_events
        .into_iter()
        .flat_map(|tx_events| tx_events.pool_logs)
        .filter_map(|log| {
            maybe_tick(log.event.unwrap()).map(|tick| (log.pool_id.to_hex(), log.ordinal, tick))
        })
        .for_each(|(pool, ordinal, new_tick_index)| {
            store.set(ordinal, format!("pool:{pool}"), &new_tick_index.into())
        });
}

fn maybe_tick(ev: Event) -> Option<i32> {
    match ev {
        Event::PoolInitialized(pool_initialized) => Some(pool_initialized.tick),
        Event::Swapped(swapped) => Some(swapped.tick_after),
        _ => None,
    }
}
