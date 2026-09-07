pub mod feed;
pub mod models;
mod source;
pub mod state;

/// Protocol system stamped on every component this integration emits. The `rfq:` prefix is
/// on-the-wire identity (executor registry, component ids) and predates Metric becoming a pAMM.
pub const PROTOCOL_SYSTEM: &str = "rfq:metric";
