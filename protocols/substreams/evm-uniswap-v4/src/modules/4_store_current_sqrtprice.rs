use substreams::{
    scalar::BigInt,
    store::{StoreSet, StoreSetBigInt},
};

use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events,
};

use substreams::store::StoreNew;

#[substreams::handlers::store]
pub fn store_pool_current_sqrt_price(events: Events, store: StoreSetBigInt) {
    events
        .pool_events
        .into_iter()
        .filter_map(event_to_current_sqrt_price)
        .for_each(|(pool, ordinal, new_tick_index)| {
            store.set(ordinal, format!("pool:{pool}"), &new_tick_index)
        });
}

fn event_to_current_sqrt_price(event: PoolEvent) -> Option<(String, u64, BigInt)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initialize) => Some((
            event.pool_id,
            event.log_ordinal,
            BigInt::try_from(&initialize.sqrt_price_x96)
                .expect("cannot convert sqrt_price to bigint"),
        )),
        pool_event::Type::Swap(swap) => Some((
            event.pool_id,
            event.log_ordinal,
            BigInt::try_from(&swap.sqrt_price_x96).expect("cannot convert sqrt_price to bigint"),
        )),
        _ => None,
    }
}
