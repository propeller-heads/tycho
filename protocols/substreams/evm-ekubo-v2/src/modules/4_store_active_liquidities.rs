use substreams::{
    scalar::BigInt,
    store::{StoreSetSum, StoreSetSumBigInt},
};
use substreams_helper::hex::Hexable;

use crate::{pb::ekubo::LiquidityChanges, store::store_method_from_change_type};

#[substreams::handlers::store]
pub fn store_active_liquidities(liquidity_changes: LiquidityChanges, store: StoreSetSumBigInt) {
    liquidity_changes
        .changes
        .into_iter()
        .for_each(|changes| {
            store_method_from_change_type(changes.change_type())(
                &store,
                changes.ordinal,
                format!("pool:{}", changes.pool_id.to_hex()),
                BigInt::from_signed_bytes_be(&changes.value),
            );
        });
}
