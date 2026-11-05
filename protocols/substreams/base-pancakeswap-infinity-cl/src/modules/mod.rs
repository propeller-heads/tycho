use crate::pb::pancakeswap::infinity::cl::Transaction;

#[path = "1_map_pool_created.rs"]
pub mod map_pool_created;

#[path = "2_store_pools.rs"]
pub mod store_pools;

#[path = "3_map_events.rs"]
pub mod map_events;

#[path = "4_store_current_tick.rs"]
pub mod store_current_tick;

#[path = "4_store_current_sqrtprice.rs"]
pub mod store_current_sqrtprice;

#[path = "5_map_store_balance_changes.rs"]
pub mod map_store_balance_changes;

#[path = "5_map_store_ticks.rs"]
pub mod map_store_ticks;

#[path = "5_map_store_liquidity.rs"]
pub mod map_store_liquidity;

#[path = "6_map_protocol_changes.rs"]
pub mod map_protocol_changes;

pub mod uni_math;

pancakeswap_infinity_substreams::transaction_conversions!(Transaction);
