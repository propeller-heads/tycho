//! In-memory serving of state requests.
//!
//! State responses are built as `cached base ⊕ window deltas up to the requested version`, where
//! `⊕` applies deltas on top of a base and the highest block wins for each value. The database is
//! read only for entities the cache has never seen.
//!
//! This module currently contains the block window ([`window`]) and the entity store ([`cache`])
//! it folds into. The database fill path and the request routing layer build on top of them.
//!
//! # Synchronization model
//!
//! Shared state: one [`window::DeltaWindow`] per extractor behind a mutex, and the
//! [`cache::EntityCache`] with one lock per entry, written only by folds and fills. The
//! correctness bar for every response: each returned value equals the entity's true value at the
//! requested version `V`.
//!
//! ## Reads vs folds
//!
//! A read captures, under the window lock, its patch — every delta in `(floor − 1, V]` for its
//! keys — strictly **before** reading any entry. Folding and eviction are atomic under the same
//! lock, so patch and entry bases always connect: a fold that lands in between only moves values
//! the patch already covers, and tag-guarded application (`delta block > tag`) makes the overlap
//! a no-op.
//!
//! **Read guard:** if any required value's tag (or entry height) exceeds `V` — a batched fold or
//! a concurrent fill advanced the entity past an older `V` — the request cannot be served from
//! the cache (it keeps no history) and routes to the fallback. That fallback must be the
//! latest-plus-window-patch read, not a plain versioned query: during commit lag the database
//! also has no rows for `(committed, V]`. The window-depth margin over the maximum served
//! version age makes this guard a rare tripwire, not a working path; it is counted when it
//! fires.
//!
//! ## Folds vs fills
//!
//! Folds apply only blocks at or below the db-committed watermark, and the watermark must be a
//! lagging observation of **acknowledged database flushes** — never ahead of what a concurrent
//! database read returns. Then every folded block's effects are contained in any fill's base, so
//! a fold that skipped an absent entity mid-fill loses nothing once the fill's entry lands. Two
//! further requirements close the remaining races:
//!
//! - **Honest provenance** (see the cache module doc): filled values are tagged with the block that
//!   actually wrote them, never a chain-global head stamp, so a fill can never mask a concurrently
//!   folded newer value on a shared account.
//! - **Publication validated against eviction:** between a fill's database snapshot and its
//!   publication, eviction may fold blocks the snapshot missed and drop them from the window. Entry
//!   insertion therefore happens in a critical section with the involved window lock(s),
//!   re-checking that no involved extractor's floor has passed its snapshot commit height;
//!   otherwise the fill is discarded and the request falls back.
//!
//! ## Reads vs fills
//!
//! Entries appear in the maps only complete — a fill builds the whole entry before inserting the
//! handle — so a hit is never partial, and per-value tag guards make concurrent (including
//! duplicate) publications converge. Fills publish only the reorg-safe database base: the window
//! top-up that completes a response to `V` is applied to a response-local copy and never
//! published, so a revert — which purges only unfolded window blocks — can never leave an
//! orphaned unfinalized value in the cache that equal-height canonical folds would then silently
//! skip. A reader whose `V` predates a concurrently published newer value is caught by the read
//! guard.
//!
//! ## Deletions
//!
//! Attribute and component deletions fold as guarded removals. Folding and eviction are atomic,
//! so every folded deletion lies below the window floor, and version resolution routes every `V`
//! below the floor to the versioned database fallback — a cache-path read can never observe an
//! entity as missing at a `V` that precedes its deletion.

pub(crate) mod cache;
pub(crate) mod window;
