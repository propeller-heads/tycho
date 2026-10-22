use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreGet, StoreGetString, StoreNew},
};
use tycho_substreams::prelude::*;

use crate::shares::{is_share_holder, share_balance_key};

/// Each SY's shares of each token it keeps in shares, from the same deltas `store_balances` adds,
/// so `map_protocol_changes` can revalue a holding when the token's ratio moves without the
/// holding changing.
#[substreams::handlers::store]
pub fn store_share_balances(
    deltas: BlockBalanceDeltas,
    holders: StoreGetString,
    store: StoreAddBigInt,
) {
    for delta in deltas.balance_deltas {
        let component_id =
            String::from_utf8(delta.component_id).expect("component id is not utf-8");
        if !is_share_holder(&holders, &component_id, &delta.token) {
            continue;
        }
        store.add(
            delta.ord,
            share_balance_key(&component_id, &delta.token),
            BigInt::from_signed_bytes_be(&delta.delta),
        );
    }
}
