use substreams::store::{StoreAddBigInt, StoreNew};
use tycho_substreams::prelude::*;

/// Accumulates the relative reserve deltas into each market's running `totalPt` and `totalSy`.
///
/// This mirrors `store_balances` but is a separate store: reserves are not the market's ERC-20
/// balances. `totalSy` excludes donated excess that `skim()` later sweeps, so the two diverge,
/// and the balance channel has to carry the real balances for the indexer to reconcile them.
#[substreams::handlers::store]
pub fn store_market_reserves(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}
