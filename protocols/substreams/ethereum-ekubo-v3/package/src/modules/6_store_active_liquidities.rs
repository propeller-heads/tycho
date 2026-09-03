use substreams::{
    scalar::BigInt,
    store::{StoreSetSum, StoreSetSumBigInt},
};
use substreams_helper::hex::Hexable;

use crate::{details_store::store_method_from_change_type, pb::ekubo::ActiveLiquidityChanges};

#[substreams::handlers::store]
pub fn store_active_liquidities(
    active_liquidity_changes: ActiveLiquidityChanges,
    store: StoreSetSumBigInt,
) {
    active_liquidity_changes
        .changes
        .into_iter()
        .for_each(|changes| {
            store_method_from_change_type(changes.change_type())(
                &store,
                changes.ordinal,
                changes.pool_id.to_hex(),
                BigInt::from_signed_bytes_be(&changes.value),
            );
        });
}
