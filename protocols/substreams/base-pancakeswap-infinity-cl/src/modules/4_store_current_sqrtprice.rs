use substreams::{
    scalar::BigInt,
    store::{StoreNew, StoreSet, StoreSetBigInt},
};

use crate::pb::pancakeswap::infinity::cl::{
    events::{pool_event, PoolEvent},
    Events,
};

/// Tracks the current sqrt price per pool (`pool:{id}`) from `Initialize` and `Swap`.
///
/// Ported from `ethereum-uniswap-v4/shared/src/modules/4_store_current_sqrtprice.rs`.
#[substreams::handlers::store]
pub fn store_pool_current_sqrt_price(events: Events, store: StoreSetBigInt) {
    events
        .pool_events
        .into_iter()
        .filter_map(event_to_current_sqrt_price)
        .for_each(|(pool, ordinal, sqrt_price)| {
            store.set(ordinal, format!("pool:{pool}"), &sqrt_price)
        });
}

fn event_to_current_sqrt_price(event: PoolEvent) -> Option<(String, u64, BigInt)> {
    let sqrt_price = match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initialize) => &initialize.sqrt_price_x96,
        pool_event::Type::Swap(swap) => &swap.sqrt_price_x96,
        pool_event::Type::ModifyLiquidity(_) |
        pool_event::Type::Donate(_) |
        pool_event::Type::ProtocolFeeUpdated(_) => return None,
    };
    let sqrt_price = BigInt::try_from(sqrt_price).expect("cannot convert sqrt_price to bigint");

    Some((event.pool_id, event.log_ordinal, sqrt_price))
}
