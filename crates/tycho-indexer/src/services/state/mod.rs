//! In-memory serving of state requests.
//!
//! State responses are built as `cached base ⊕ window deltas up to the requested version`, where
//! `⊕` applies deltas on top of a base and the highest block wins for each value. The database is
//! read only for entities the cache has never seen.
//!
//! This module currently contains the block window ([`window`]) and the entity store ([`cache`])
//! it folds into. The database fill path and the request routing layer build on top of them.
//!
//! # Synchronization
//!
//! The cache is written by the startup load — which runs before anything else — and then only
//! by folds. Reads and folds are sequential: a fold takes the write side of one lock and applies
//! a whole block atomically, reads take the read side. Folds are fast, so waiting is fine, and a
//! read never sees half a block.
//!
//! A read collects the window patch for its keys first, then reads the entries, and applies only
//! the patch changes that are newer than each value's tag. If an entry has already moved past
//! the requested version (folding is batched), that request is served the way uncommitted
//! versions are served today — latest from the DB plus the buffered window changes — because the
//! cache keeps no history.
//!
//! Versions below the window go to the versioned DB path. Deletions folded into the cache are
//! always below the window floor by then, so a below-window read never misses them.

pub(crate) mod cache;
pub(crate) mod window;
