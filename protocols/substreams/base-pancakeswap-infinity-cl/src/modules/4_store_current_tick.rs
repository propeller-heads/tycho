use substreams::store::{StoreNew, StoreSet, StoreSetInt64};

use crate::pb::pancakeswap::infinity::cl::{
    events::{pool_event, PoolEvent},
    Events,
};

/// Tracks the current tick per pool (`pool:{id}`) from `Initialize` and `Swap`.
///
/// Ported from `ethereum-uniswap-v4/shared/src/modules/4_store_current_tick.rs`.
#[substreams::handlers::store]
pub fn store_pool_current_tick(events: Events, store: StoreSetInt64) {
    events
        .pool_events
        .into_iter()
        .filter_map(event_to_current_tick)
        .for_each(|(pool, ordinal, new_tick_index)| {
            store.set(ordinal, format!("pool:{pool}"), &new_tick_index.into())
        });
}

fn event_to_current_tick(event: PoolEvent) -> Option<(String, u64, i32)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initialize) => {
            Some((event.pool_id, event.log_ordinal, initialize.tick))
        }
        pool_event::Type::Swap(swap) => Some((event.pool_id, event.log_ordinal, swap.tick)),
        pool_event::Type::ModifyLiquidity(_) |
        pool_event::Type::Donate(_) |
        pool_event::Type::ProtocolFeeUpdated(_) => None,
    }
}
