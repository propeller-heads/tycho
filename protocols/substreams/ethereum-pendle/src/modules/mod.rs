//! One substreams handler per file, prefixed with its stage in the module graph.
//!
//! A module at stage N reads only from stages below it, so modules sharing a stage are
//! independent and run in parallel. The manifest lists them in the same order.

// Stage 1
#[path = "1_map_market_components.rs"]
mod map_market_components;

// Stage 2
#[path = "2_store_sy_seen.rs"]
mod store_sy_seen;

// Stage 3
#[path = "3_map_protocol_components.rs"]
mod map_protocol_components;

// Stage 4
#[path = "4_store_market_registry.rs"]
mod store_market_registry;
#[path = "4_store_protocol_components.rs"]
mod store_protocol_components;

// Stage 5
#[path = "5_map_relative_component_balance.rs"]
mod map_relative_component_balance;
#[path = "5_map_reserve_deltas.rs"]
mod map_reserve_deltas;
#[path = "5_store_py_index.rs"]
mod store_py_index;

// Stage 6
#[path = "6_store_balances.rs"]
mod store_balances;
#[path = "6_store_market_reserves.rs"]
mod store_market_reserves;

// Stage 7
#[path = "7_map_protocol_changes.rs"]
mod map_protocol_changes;

pub use map_market_components::map_market_components;
pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use map_relative_component_balance::map_relative_component_balance;
pub use map_reserve_deltas::map_reserve_deltas;
pub use store_balances::store_balances;
pub use store_market_registry::store_market_registry;
pub use store_market_reserves::store_market_reserves;
pub use store_protocol_components::store_protocol_components;
pub use store_py_index::store_py_index;
pub use store_sy_seen::store_sy_seen;
