use substreams::{
    scalar::BigInt,
    store::{StoreSetSum, StoreSetSumBigInt},
};
use substreams_helper::hex::Hexable;

use crate::{details_store::store_method_from_change_type, pb::ekubo::ActiveRateChanges};

#[substreams::handlers::store]
pub fn store_active_rates(active_rate_changes: ActiveRateChanges, store: StoreSetSumBigInt) {
    active_rate_changes
        .changes
        .into_iter()
        .for_each(|changes| {
            let pool_id = changes.pool_id.to_hex();

            let store_method = store_method_from_change_type(changes.change_type());

            store_method(
                &store,
                changes.ordinal,
                format!("{pool_id}:0"),
                BigInt::from_signed_bytes_be(&changes.token0_value),
            );
            store_method(
                &store,
                changes.ordinal,
                format!("{pool_id}:1"),
                BigInt::from_signed_bytes_be(&changes.token1_value),
            );
        });
}
