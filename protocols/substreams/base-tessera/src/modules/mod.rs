//! One handler per file, numbered in manifest order.

#[path = "1_map_components.rs"]
mod map_components;

#[path = "2_store_components.rs"]
mod store_components;

#[path = "3_store_pairs.rs"]
mod store_pairs;

#[path = "4_map_storage_changes.rs"]
pub(crate) mod map_storage_changes;

#[path = "5_store_treasury.rs"]
mod store_treasury;

#[path = "6_store_safety.rs"]
mod store_safety;

#[path = "7_map_relative_balances.rs"]
mod map_relative_balances;

#[path = "8_store_balances.rs"]
mod store_balances;

#[path = "9_map_protocol_changes.rs"]
mod map_protocol_changes;
