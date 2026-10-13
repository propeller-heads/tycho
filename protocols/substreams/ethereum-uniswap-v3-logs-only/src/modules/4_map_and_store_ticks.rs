use std::str::FromStr;

use substreams::store::StoreAddBigInt;

use crate::pb::uniswap::v3::{
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

pub fn tick_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("pool:{0}:tick:{1}", hex::encode(pool_address), tick_index)
}

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    let mut deltas = ticks_deltas.deltas;

    deltas.sort_unstable_by_key(|delta| delta.ordinal);

    deltas.iter().for_each(|delta| {
        store.add(
            delta.ordinal,
            tick_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
    });
}

/// Tracks each tick's gross liquidity, the sum of the positions bounded by it.
///
/// A tick stays initialized while its gross liquidity is positive, even when the positions
/// starting and ending at it cancel out to zero net liquidity. The pool still stops a swap step
/// at such a tick, which changes how the fee rounds.
#[substreams::handlers::store]
pub fn store_ticks_gross_liquidity(events: Events, store: StoreAddBigInt) {
    for event in events.pool_events {
        let (tick_lower, tick_upper, delta) = match event.r#type.as_ref().unwrap() {
            pool_event::Type::Mint(mint) => {
                (mint.tick_lower, mint.tick_upper, BigInt::from_str(&mint.amount).unwrap())
            }
            pool_event::Type::Burn(burn) => (
                burn.tick_lower,
                burn.tick_upper,
                BigInt::from_str(&burn.amount)
                    .unwrap()
                    .neg(),
            ),
            _ => continue,
        };
        let pool_address = hex::decode(&event.pool_address).unwrap();
        for tick_index in [tick_lower, tick_upper] {
            store.add(event.log_ordinal, tick_key(&pool_address, tick_index), &delta);
        }
    }
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
