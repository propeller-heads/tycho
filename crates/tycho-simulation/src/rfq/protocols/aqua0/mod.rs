//! Aqua0: just-in-time liquidity on Uniswap V4 pools with Aqua0's hook. The feed publishes the
//! backend's sampled fills for every listed market; a binding quote reserves LP backing for one
//! exact-input fill and signs the hook data the existing V4 executor forwards.

mod client;
pub mod feed;
mod models;
mod source;
pub mod state;

pub use client::AQUA0_API_URL;

/// Protocol system stamped on every component this integration emits. tycho-execution resolves
/// the Aqua0 encoder by this string.
pub const PROTOCOL_SYSTEM: &str = "rfq:aqua0";

/// Component type stamped on every market this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "aqua0_jit_pool";
