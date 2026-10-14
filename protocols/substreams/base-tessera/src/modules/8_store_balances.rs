use substreams::store::{StoreAddBigInt, StoreNew};
use tycho_substreams::prelude::*;

/// Aggregates relative balance deltas into absolute balances expected by the indexer.
#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}
