mod client;
pub mod feed;
mod models;
mod source;
pub mod state;

/// Protocol system stamped on every component this integration emits.
pub const PROTOCOL_SYSTEM: &str = "rfq:native";

/// Component type stamped on every pair this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "native_relay_pool";
