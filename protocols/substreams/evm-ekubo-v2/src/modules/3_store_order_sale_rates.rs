use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::OrderSaleRateDeltas;

#[substreams::handlers::store]
pub fn store_order_sale_rates(order_sale_rate_deltas: OrderSaleRateDeltas, store: StoreAddBigInt) {
    order_sale_rate_deltas
        .deltas
        .into_iter()
        .for_each(|delta| {
            let pool_id = delta.pool_id.to_hex();
            let time = delta.time;
            let token = if delta.is_token1 { "token1" } else { "token0" };

            store.add(
                delta.ordinal,
                format!("pool:{pool_id}:{token}:time:{time}:"),
                BigInt::from_signed_bytes_be(&delta.sale_rate_delta),
            );
        });
}
