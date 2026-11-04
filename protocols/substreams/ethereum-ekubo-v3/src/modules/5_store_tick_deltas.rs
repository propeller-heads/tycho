use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::TickDeltas;

/// Per tick, Core's net liquidity delta at `{pool}:{tick}` and its gross
/// liquidity (`liquidityNet`, the liquidity of all positions bounded by the
/// tick) at `{pool}:{tick}:gross`. A tick is initialized exactly while its
/// gross liquidity is non-zero, even when its net delta is zero.
///
/// Each tick delta adds both values, net first, so the store deltas come in
/// (net, gross) pairs, one pair per tick delta.
#[substreams::handlers::store]
pub fn store_tick_deltas(tick_deltas: TickDeltas, store: StoreAddBigInt) {
    tick_deltas
        .deltas
        .into_iter()
        .for_each(|delta| {
            let key = format!("{}:{}", delta.pool_id.to_hex(), delta.tick_index);
            store.add(
                delta.ordinal,
                &key,
                BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
            );
            store.add(
                delta.ordinal,
                format!("{key}:gross"),
                BigInt::from_signed_bytes_be(&delta.liquidity_gross_delta),
            );
        });
}
