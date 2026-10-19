use substreams::store::{StoreAddBigInt, StoreNew};
use tycho_substreams::prelude::*;

#[substreams::handlers::store]
pub fn store_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}
