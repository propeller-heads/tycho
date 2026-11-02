use crate::pb::nami::v4::{events::pool_event::Typ, Events, TickDelta, TickDeltas};

use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};

#[substreams::handlers::map]
pub fn map_ticks_changes(events: Events) -> TickDeltas {
    let deltas = events
        .pool_events
        .into_iter()
        .flat_map(|event| {
            let ticks = maybe_tick_deltas(event.typ.unwrap())?;
            let pool_address = event.pool_address;
            let ordinal = event.log_ordinal;
            let tx = event.transaction;

            Some(ticks.into_iter().map(
                move |(tick_index, liquidity_net_delta, liquidity_gross_delta)| TickDelta {
                    pool_address: pool_address.clone(),
                    tick_index,
                    liquidity_net_delta: liquidity_net_delta.to_signed_bytes_be(),
                    liquidity_gross_delta: liquidity_gross_delta.to_signed_bytes_be(),
                    ordinal,
                    transaction: tx.clone(),
                },
            ))
        })
        .flatten()
        .collect();

    TickDeltas { deltas }
}

/// Key under which a tick's net liquidity is summed.
pub fn net_liquidity_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("{}:{}:net", hex::encode(pool_address), tick_index)
}

/// Key under which a tick's gross liquidity is summed.
pub fn gross_liquidity_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("{}:{}:gross", hex::encode(pool_address), tick_index)
}

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    for delta in ticks_deltas.deltas {
        store.add(
            delta.ordinal,
            net_liquidity_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
        store.add(
            delta.ordinal,
            gross_liquidity_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_gross_delta),
        );
    }
}

/// Returns `(tick, net delta, gross delta)` for both ticks of a position change.
fn maybe_tick_deltas(ty: Typ) -> Option<[(i32, BigInt, BigInt); 2]> {
    Some(match ty {
        Typ::Mint(mint) => {
            let amount = BigInt::from_unsigned_bytes_be(&mint.amount);
            [
                (mint.tick_upper, amount.neg(), amount.clone()),
                (mint.tick_lower, amount.clone(), amount),
            ]
        }
        Typ::Burn(burn) => {
            let amount = BigInt::from_unsigned_bytes_be(&burn.amount);
            [
                (burn.tick_lower, amount.neg(), amount.neg()),
                (burn.tick_upper, amount.clone(), amount.neg()),
            ]
        }
        _ => return None,
    })
}
