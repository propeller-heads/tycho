use crate::pb::ramses::v3::{events::pool_event::Typ, Events, TickDelta, TickDeltas};

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

            Some(
                ticks
                    .into_iter()
                    .map(move |(tick_index, liquidity_net_delta)| TickDelta {
                        pool_address: pool_address.clone(),
                        tick_index,
                        liquidity_net_delta: liquidity_net_delta.to_signed_bytes_be(),
                        ordinal,
                        transaction: tx.clone(),
                    }),
            )
        })
        .flatten()
        .collect();

    TickDeltas { deltas }
}

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    for delta in ticks_deltas.deltas {
        store.add(
            delta.ordinal,
            format!("{}:{}", hex::encode(&delta.pool_address), delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
    }
}

fn maybe_tick_deltas(ty: Typ) -> Option<[(i32, BigInt); 2]> {
    Some(match ty {
        Typ::Mint(mint) => {
            let amount = BigInt::from_unsigned_bytes_be(&mint.amount);
            [(mint.tick_upper, amount.neg()), (mint.tick_lower, amount)]
        }
        Typ::Burn(burn) => {
            let amount = BigInt::from_unsigned_bytes_be(&burn.amount);
            [(burn.tick_lower, amount.neg()), (burn.tick_upper, amount)]
        }
        _ => return None,
    })
}
