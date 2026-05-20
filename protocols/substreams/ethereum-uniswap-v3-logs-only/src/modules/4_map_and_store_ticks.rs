use crate::pb::uniswap::v3::{Events, TickDelta, TickDeltas};
use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};

use uniswap_v3_core::events::PoolEvent as CorePoolEvent;

#[substreams::handlers::map]
pub fn map_ticks_changes(events: Events) -> Result<TickDeltas, anyhow::Error> {
    let ticks_deltas = events
        .pool_events
        .into_iter()
        .flat_map(|proto_event| {
            let ordinal = proto_event.log_ordinal;
            let transaction = proto_event.transaction.clone();
            let core_event = CorePoolEvent::from(&proto_event);
            uniswap_v3_core::ticks::event_to_tick_deltas(&core_event)
                .into_iter()
                .map(move |d| TickDelta { ordinal, transaction: transaction.clone(), ..d.into() })
        })
        .collect();

    Ok(TickDeltas { deltas: ticks_deltas })
}

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    let mut deltas = ticks_deltas.deltas;

    deltas.sort_unstable_by_key(|delta| delta.ordinal);

    deltas.iter().for_each(|delta| {
        store.add(
            delta.ordinal,
            format!("pool:{0}:tick:{1}", hex::encode(&delta.pool_address), delta.tick_index,),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
    });
}
