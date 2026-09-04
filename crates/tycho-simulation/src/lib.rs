//! Tycho Simulation: a decentralized exchange simulation library
//!
//! This library allows to simulate trades against a wide range
//! of different protocols, including uniswap-v2 and uniswap-v3.
//! It allows to simulate chained trades over different venues
//! together to exploit price differences by using token prices
//! calculated from the protocol's state.

extern crate core;

// Reexports
pub use tycho_client;
pub use tycho_common;
#[deprecated(
    since = "0.252.0",
    note = "Use `tycho_simulation::tycho_common` instead of `tycho_simulation::tycho_core`."
)]
pub mod tycho_core {
    pub use tycho_common::*;
}
#[cfg(feature = "evm")]
pub use revm;
pub use tycho_ethereum;

#[cfg(feature = "evm")]
pub mod evm;
#[cfg(feature = "price-level-stream")]
pub mod price_level_stream;
pub mod protocol;
#[cfg(feature = "rfq")]
pub mod rfq;
pub mod serde_helpers;
pub mod utils;
