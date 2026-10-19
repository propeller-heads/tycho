use std::str::FromStr;

use substreams::store::StoreAddBigInt;

use crate::pb::pancakeswap::v3::{
    events::{pool_event, PoolEvent},
    Events, TickDelta, TickDeltas,
};

use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreNew},
};

use anyhow::Ok;

#[substreams::handlers::map]
pub fn map_ticks_changes(events: Events) -> Result<TickDeltas, anyhow::Error> {
    let ticks_deltas = events
        .pool_events
        .into_iter()
        .flat_map(event_to_ticks_deltas)
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

fn event_to_ticks_deltas(event: PoolEvent) -> Vec<TickDelta> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Mint(mint) => {
            vec![
                TickDelta {
                    pool_address: hex::decode(&event.pool_address).unwrap(),
                    tick_index: mint.tick_lower,
                    liquidity_net_delta: BigInt::from_str(&mint.amount)
                        .unwrap()
                        .to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction.clone(),
                },
                TickDelta {
                    pool_address: hex::decode(&event.pool_address).unwrap(),
                    tick_index: mint.tick_upper,
                    liquidity_net_delta: BigInt::from_str(&mint.amount)
                        .unwrap()
                        .neg()
                        .to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction,
                },
            ]
        }
        pool_event::Type::Burn(burn) => vec![
            TickDelta {
                pool_address: hex::decode(&event.pool_address).unwrap(),
                tick_index: burn.tick_lower,
                liquidity_net_delta: BigInt::from_str(&burn.amount)
                    .unwrap()
                    .neg()
                    .to_signed_bytes_be(),
                ordinal: event.log_ordinal,
                transaction: event.transaction.clone(),
            },
            TickDelta {
                pool_address: hex::decode(&event.pool_address).unwrap(),
                tick_index: burn.tick_upper,
                liquidity_net_delta: BigInt::from_str(&burn.amount)
                    .unwrap()
                    .to_signed_bytes_be(),
                ordinal: event.log_ordinal,
                transaction: event.transaction,
            },
        ],
        _ => vec![],
    }
}
