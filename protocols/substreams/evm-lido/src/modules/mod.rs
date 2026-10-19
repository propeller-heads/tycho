pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use store_storage_variables::store_storage_variables;
use substreams::hex;

#[path = "1_map_components.rs"]
mod map_protocol_components;

#[path = "2_store_storage_variables.rs"]
mod store_storage_variables;

#[path = "3_map_protocol_changes.rs"]
mod map_protocol_changes;

pub const ST_ETH_ADDRESS_PROXY: [u8; 20] = hex!("ae7ab96520de3a18e5e111b5eaab095312d7fe84");
pub const ST_ETH_ADDRESS_PROXY_COMPONENT_ID: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
pub const ST_ETH_ADDRESS_IMPL: [u8; 20] = hex!("17144556fd3424edc8fc8a4c940b2d04936d17eb");
pub const WST_ETH_ADDRESS: [u8; 20] = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0");
pub const WST_ETH_ADDRESS_COMPONENT_ID: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");
