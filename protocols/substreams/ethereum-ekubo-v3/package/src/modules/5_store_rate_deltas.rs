use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::RateDeltas;

#[substreams::handlers::store]
pub fn store_rate_deltas(rate_deltas: RateDeltas, store: StoreAddBigInt) {
    rate_deltas
        .deltas
        .into_iter()
        .for_each(|delta| {
            let pool_id = delta.pool_id.to_hex();
            let time = delta.time;
            let token = if delta.is_token1 { "1" } else { "0" };

            store.add(
                delta.ordinal,
                format!("{pool_id}:{token}:{time}"),
                BigInt::from_signed_bytes_be(&delta.rate_delta),
            );
        });
}
