use crate::pb::uniswap::v3::BalanceDeltas;
use num_bigint::Sign;
use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BalanceDeltas, store: StoreAddBigInt) {
    let mut deltas = balances_deltas.deltas.clone();

    deltas.sort_unstable_by_key(|delta| delta.ordinal);

    deltas.iter().for_each(|delta| {
        store.add(
            delta.ordinal,
            format!(
                "pool:{0}:token:{1}",
                hex::encode(&delta.pool_address),
                hex::encode(&delta.token_address)
            ),
            BigInt::from_bytes_le(
                if delta.sign { Sign::Plus } else { Sign::Minus },
                delta.amount.as_slice(),
            ),
        );
    });
}
