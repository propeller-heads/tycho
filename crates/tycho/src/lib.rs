//! # tycho
//!
//! Meta-crate that re-exports a verified-compatible set of Tycho ecosystem crates.
//!
//! Instead of depending on each crate individually and risking version drift, add
//! a single dependency:
//!
//! ```toml
//! [dependencies]
//! tycho = "0.1"
//! ```
//!
//! Then import via the re-exported modules:
//!
//! ```rust,ignore
//! use tycho::client::feed::ClientBuilder;
//! use tycho::common::models::Chain;
//! use tycho::simulation::evm::stream::ProtocolStreamBuilder;
//! use tycho::execution::encoding::models::Solution;
//! ```
//!
//! ## Feature flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `evm`   | yes     | EVM simulation and execution support |
//! | `book-feeds` | no | Off-chain book feeds: the RFQ and pAMM providers (implies `evm`) |

pub use tycho_client as client;
pub use tycho_common as common;
#[cfg(feature = "evm")]
pub use tycho_execution as execution;
#[cfg(feature = "evm")]
pub use tycho_simulation as simulation;
