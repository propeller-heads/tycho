pub(crate) mod client;
pub mod feed;
pub(crate) mod models;
mod source;
pub mod state;

/// Protocol system stamped on every component this integration emits.
pub const PROTOCOL_SYSTEM: &str = "rfq:bebop";
