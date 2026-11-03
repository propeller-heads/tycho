//! Helper-backed pAMM books, fetched through the shared snapshot-feed framework.
pub mod all_pairs;
mod client;
pub mod feed;
pub mod models;
mod source;
pub mod state;
mod tokens;

pub use feed::{ElfomofiFeed, ElfomofiFeedBuilder};
pub const PROTOCOL_SYSTEM: &str = "propamm:elfomofi";
pub(crate) const PROTOCOL_TYPE: &str = "elfomofi_pool";

#[cfg(test)]
mod tests;
