use substreams::{
    scalar::BigInt,
    store::{StoreSetSum, StoreSetSumBigInt},
};
use substreams_helper::hex::Hexable;

use crate::{pb::ekubo::SaleRateChanges, store::store_method_from_change_type};

#[substreams::handlers::store]
pub fn store_active_sale_rates(sale_rate_changes: SaleRateChanges, store: StoreSetSumBigInt) {
    sale_rate_changes
        .changes
        .into_iter()
        .for_each(|changes| {
            let pool_id = changes.pool_id.to_hex();

            let store_method = store_method_from_change_type(changes.change_type());

            store_method(
                &store,
                changes.ordinal,
                format!("pool:{pool_id}:token0"),
                BigInt::from_signed_bytes_be(&changes.token0_value),
            );
            store_method(
                &store,
                changes.ordinal,
                format!("pool:{pool_id}:token1"),
                BigInt::from_signed_bytes_be(&changes.token1_value),
            );
        });
}
