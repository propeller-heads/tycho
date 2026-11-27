//! In-memory serving of state requests.
//!
//! This module holds the block window ([`window`]), the entity cache ([`cache`]) it folds into,
//! the catch-up that brings the cache up to date from the database ([`catch_up`]), the service that
//! answers state requests from the windows and the cache ([`service`]), and the views those answers
//! are written from ([`view`]). Locking is described in the [`cache`] module doc; the read order
//! in the [`service`] module doc.
//!
//! # How a state request is served
//!
//! The cache is built at startup by [`cache::EntityCache::from_snapshot`] when the mode is not
//! `off`. The windows fold into it, and catch-ups write into it. In `serve`, a response is
//! `cached entry ⊕ window changes up to the requested version`, where `⊕` applies each change only
//! when it is newer than the value it writes. Folds use the same rule. An entity the cache and the
//! window do not hold does not exist.
//!
//! The cache keeps no history. A version it cannot rebuild — below the window, or older than a
//! cached value — is answered by the database path the handlers used before the cache existed.
//! Deletions folded into the cache are always below the window floor by then, so a below-window
//! read never misses them.

pub mod cache;
pub mod catch_up;
pub mod service;
pub mod shadow;
pub use cache::view;
pub mod window;

/// Which path answers `/contract_state` and `/protocol_state`. Set per deployment with
/// `ENTITY_CACHE_MODE`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum EntityCacheMode {
    /// No cache is loaded; every request reads the database.
    #[default]
    Off,
    /// The cache is loaded at startup and the windows fold into it. Clients get the database
    /// answer. On a sample of requests (`--entity-cache-shadow-sample-rate`), the cache path also
    /// runs and the two answers are compared; see [`shadow`].
    Shadow,
    /// The cache is loaded at startup and the windows fold into it. Clients get the cache answer;
    /// versions the cache cannot rebuild are answered from the database.
    Serve,
}
