use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::TickDeltas;

/// Core's per-tick `liquidityNet`: the liquidity of all positions bounded by the
/// tick. A tick is initialized exactly while it is non-zero, even when the
/// tick's net liquidity delta is zero. Adds once per tick delta so that its
/// store deltas line up with `store_tick_deltas`.
#[substreams::handlers::store]
pub fn store_tick_liquidity_gross(tick_deltas: TickDeltas, store: StoreAddBigInt) {
    tick_deltas
        .deltas
        .into_iter()
        .for_each(|delta| {
            store.add(
                delta.ordinal,
                format!("{}:{}", delta.pool_id.to_hex(), delta.tick_index),
                BigInt::from_signed_bytes_be(&delta.liquidity_gross_delta),
            );
        });
}
