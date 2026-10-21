use crate::pb::cowamm::BlockPoolChanges;
use substreams::store::{StoreAddBigInt, StoreNew};
use tycho_substreams::prelude::*;

#[substreams::handlers::store]
pub fn store_balances(pool_balance_changes: BlockPoolChanges, store: StoreAddBigInt) {
    let balance_deltas = pool_balance_changes
        .block_balance_deltas
        .unwrap();
    //convert CowBalanceDeltas to normal BalanceDeltas
    let final_deltas = balance_deltas
        .balance_deltas
        .into_iter()
        .map(|delta| delta.into())
        .collect::<Vec<BalanceDelta>>();
    let block_balance_deltas = BlockBalanceDeltas { balance_deltas: final_deltas };
    tycho_substreams::balances::store_balance_changes(block_balance_deltas, store);
}
