pub use map_pool_created::map_pools_created;
pub use map_protocol_changes::map_protocol_changes;
pub use store_pools::store_pools;
use substreams_ethereum::pb::eth::v2::TransactionTrace;

use crate::pb::uniswap::v3::Transaction;

#[path = "1_map_pool_created.rs"]
mod map_pool_created;

#[path = "2_store_pools.rs"]
mod store_pools;

#[path = "3_map_events.rs"]
mod map_events;

#[path = "4_map_and_store_balance_changes.rs"]
mod map_store_balance_changes;

#[path = "4_map_and_store_ticks.rs"]
mod map_store_ticks;

#[path = "4_map_and_store_liquidity.rs"]
mod map_store_liquidity;

#[path = "5_map_protocol_changes.rs"]
mod map_protocol_changes;

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

impl From<Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: Transaction) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index }
    }
}
