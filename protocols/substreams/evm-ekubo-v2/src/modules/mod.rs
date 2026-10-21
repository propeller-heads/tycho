use substreams_ethereum::pb::eth::v2::TransactionTrace;

use crate::pb::ekubo::Transaction;

#[path = "1_map_events.rs"]
mod map_events;

#[path = "2_map_components.rs"]
mod map_components;
#[path = "2_map_order_sale_rate_deltas.rs"]
mod map_order_sale_rate_deltas;
#[path = "2_map_sale_rate_changes.rs"]
mod map_sale_rate_changes;
#[path = "2_map_tick_deltas.rs"]
mod map_tick_deltas;
#[path = "2_store_active_ticks.rs"]
mod store_active_ticks;

#[path = "3_map_liquidity_changes.rs"]
mod map_liquidity_changes;
#[path = "3_store_active_sale_rates.rs"]
mod store_active_sale_rates;
#[path = "3_store_order_sale_rates.rs"]
mod store_order_sale_rates;
#[path = "3_store_pool_details.rs"]
mod store_pool_details;
#[path = "3_store_tick_liquidities.rs"]
mod store_tick_liquidities;

#[path = "4_map_balance_changes.rs"]
mod map_balance_changes;
#[path = "4_store_active_liquidities.rs"]
mod store_active_liquidities;

#[path = "5_store_balance_changes.rs"]
mod store_balance_changes;

#[path = "6_map_protocol_changes.rs"]
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
