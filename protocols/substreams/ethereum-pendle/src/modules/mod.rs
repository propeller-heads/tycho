//! One substreams handler per file.
mod map_market_components;
mod map_protocol_changes;
mod map_protocol_components;
mod map_relative_component_balance;
mod store_balances;
mod store_protocol_components;
mod store_sy_seen;

pub use map_market_components::map_market_components;
pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use map_relative_component_balance::map_relative_component_balance;
pub use store_balances::store_balances;
pub use store_protocol_components::store_protocol_components;
pub use store_sy_seen::store_sy_seen;
