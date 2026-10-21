use substreams::prelude::*;
use tycho_substreams::prelude::*;

/// Aggregates the relative balances into absolute values in an additive store.
#[substreams::handlers::store]
pub fn store_component_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}
