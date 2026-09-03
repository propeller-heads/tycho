use substreams_ethereum::pb::eth::v2::TransactionTrace;

use crate::pb::ekubo::Transaction;

// Stage 1
#[path = "1_map_events.rs"]
mod map_events;

// Stage 2
#[path = "2_map_components.rs"]
mod map_components;
#[path = "2_store_pool_details.rs"]
mod store_pool_details;

// Stage 3
#[path = "3_map_filtered_events.rs"]
mod map_filtered_events;

// Stage 4
#[path = "4_map_active_rate_changes.rs"]
mod map_active_rate_changes;
#[path = "4_map_rate_deltas.rs"]
mod map_rate_deltas;
#[path = "4_map_tick_deltas.rs"]
mod map_tick_deltas;
#[path = "4_store_active_ticks.rs"]
mod store_active_ticks;

// Stage 5
#[path = "5_map_active_liquidity_changes.rs"]
mod map_active_liquidity_changes;
#[path = "5_map_balance_changes.rs"]
mod map_balance_changes;
#[path = "5_store_active_rates.rs"]
mod store_active_rates;
#[path = "5_store_rate_deltas.rs"]
mod store_rate_deltas;
#[path = "5_store_tick_deltas.rs"]
mod store_tick_deltas;

// Stage 6
#[path = "6_store_active_liquidities.rs"]
mod store_active_liquidities;
#[path = "6_store_balance_changes.rs"]
mod store_balance_changes;

// Stage 7
#[path = "7_map_protocol_changes.rs"]
mod map_protocol_changes;

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
