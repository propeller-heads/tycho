use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::TickDeltas;

#[substreams::handlers::store]
pub fn store_tick_liquidities(tick_deltas: TickDeltas, store: StoreAddBigInt) {
    tick_deltas
        .deltas
        .into_iter()
        .for_each(|delta| {
            store.add(
                delta.ordinal,
                format!("pool:{}:tick:{}", delta.pool_id.to_hex(), delta.tick_index),
                BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
            );
        });
}
