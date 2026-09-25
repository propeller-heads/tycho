//! One handler per file, numbered in manifest order.

#[path = "1_map_components.rs"]
mod map_components;

#[path = "2_store_components.rs"]
mod store_components;

#[path = "3_store_pairs.rs"]
mod store_pairs;

#[path = "4_store_treasury.rs"]
mod store_treasury;

#[path = "5_store_safety.rs"]
mod store_safety;

#[path = "6_map_relative_balances.rs"]
mod map_relative_balances;

#[path = "7_store_balances.rs"]
mod store_balances;

#[path = "8_map_protocol_changes.rs"]
mod map_protocol_changes;
