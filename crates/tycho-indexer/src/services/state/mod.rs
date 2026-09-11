//! In-memory serving of state requests.
//!
//! State responses are built as `cached base ⊕ window deltas up to the requested version`, where
//! `⊕` applies deltas on top of a base and the highest block wins for each value. The database is
//! read only for entities the cache has never seen.
//!
//! This module currently contains the block window ([`window`]). The entity cache, the database
//! fill path, and the request routing layer build on top of it.

pub(crate) mod window;
