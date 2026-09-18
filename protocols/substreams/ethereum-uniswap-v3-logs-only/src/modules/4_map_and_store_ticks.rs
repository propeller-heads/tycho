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

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    let mut deltas = ticks_deltas.deltas;

    deltas.sort_unstable_by_key(|delta| delta.ordinal);

    deltas.iter().for_each(|delta| {
        store.add(
            delta.ordinal,
            tick_store_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
    });
}

/// Builds the `store_ticks_liquidity` key that holds a tick's net liquidity.
///
/// The writer and any consumer joining the resulting store deltas back onto tick deltas must
/// derive the key identically, so both go through this function.
pub(crate) fn tick_store_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("pool:{}:tick:{}", hex::encode(pool_address), tick_index)
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

#[cfg(test)]
mod tests {
    use super::tick_store_key;

    #[test]
    fn tick_store_key_uses_unprefixed_hex() {
        // Pinned because the key is a contract between store_ticks_liquidity and the consumer
        // that joins its deltas back onto tick deltas. Note the address carries no `0x` prefix.
        assert_eq!(tick_store_key(&[0x0a, 0x0b], -100), "pool:0a0b:tick:-100");
    }
}
