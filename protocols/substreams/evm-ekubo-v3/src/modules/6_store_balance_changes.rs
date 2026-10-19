use substreams::store::{StoreAddBigInt, StoreNew};
use tycho_substreams::models::BlockBalanceDeltas;

#[substreams::handlers::store]
fn store_balance_changes(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}
