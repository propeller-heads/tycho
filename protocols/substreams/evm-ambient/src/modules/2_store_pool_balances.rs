use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};

use crate::pb::tycho::ambient::v1::BlockPoolChanges;

#[substreams::handlers::store]
pub fn store_pool_balances(changes: BlockPoolChanges, balance_store: StoreAddBigInt) {
    for balance_delta in changes.balance_deltas {
        let pool_hash_hex = hex::encode(&balance_delta.pool_hash);
        balance_store.add(
            balance_delta.ordinal,
            format!("{}:{}", pool_hash_hex, balance_delta.token_type),
            BigInt::from_signed_bytes_be(&balance_delta.token_delta),
        );
    }
}
