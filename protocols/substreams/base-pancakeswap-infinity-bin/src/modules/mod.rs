use substreams_ethereum::pb::eth::v2::TransactionTrace;

use crate::pb::pancakeswap::infinity::bin::Transaction;

#[path = "1_map_pool_created.rs"]
pub mod map_pool_created;

#[path = "2_store_pools.rs"]
pub mod store_pools;

#[path = "3_map_events.rs"]
pub mod map_events;

#[path = "4_store_active_id.rs"]
pub mod store_active_id;

#[path = "4_map_tree_changes.rs"]
pub mod map_tree_changes;

#[path = "5_store_bin_trees.rs"]
pub mod store_bin_trees;

#[path = "6_map_bin_changes.rs"]
pub mod map_bin_changes;

#[path = "6_map_store_balance_changes.rs"]
pub mod map_store_balance_changes;

#[path = "7_map_protocol_changes.rs"]
pub mod map_protocol_changes;

impl From<TransactionTrace> for Transaction {
    fn from(value: TransactionTrace) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index.into() }
    }
}

impl From<&TransactionTrace> for Transaction {
    fn from(value: &TransactionTrace) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index.into(),
        }
    }
}

impl From<Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: Transaction) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index }
    }
}

impl From<&Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: &Transaction) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index,
        }
    }
}
