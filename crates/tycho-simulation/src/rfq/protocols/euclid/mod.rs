//! Euclid Protocol: a cross-chain liquidity protocol whose pools and orderbook live on its own
//! app chain. Euclid's RFQ gateway publishes that depth as indicative price levels over a
//! WebSocket, and a binding quote settles atomically on the taker's chain, from pre-positioned
//! maker inventory, as a maker-signed 1inch limit-order-protocol v3 RFQ order.

mod client;
pub mod feed;
mod models;
mod source;
pub mod state;

pub use feed::EuclidBookAssemblerBuilder;
pub use models::{EuclidBook, EuclidPrices};
pub use source::EuclidBookAssembler;

/// Protocol system stamped on every component this integration emits.
pub const PROTOCOL_SYSTEM: &str = "rfq:euclid";

/// Component type stamped on every pair this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "euclid_pool";
