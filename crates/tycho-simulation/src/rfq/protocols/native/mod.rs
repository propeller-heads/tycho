mod client;
pub mod feed;
mod models;
mod source;
pub mod state;

/// Protocol system stamped on every component this integration emits.
pub const PROTOCOL_SYSTEM: &str = "rfq:native";
