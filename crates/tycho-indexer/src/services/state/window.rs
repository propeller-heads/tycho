//! Fixed-depth in-memory window of block deltas, one per extractor.
//!
//! The window retains roughly the last `W` blocks of [`BlockAggregatedChanges`] instead of
//! dropping blocks as soon as the database commits them. Blocks leave the window only through
//! [`DeltaWindow::insert`], which folds each evicted block into a [`FoldSink`] first — eviction
//! and fold are a single operation, so no block's deltas can be lost between the window and the
//! long-lived store behind the sink.
//!
//! Retention rule: a block is evictable only when its number is at or below
//! `min(finalized, db_committed, tip - W, lowest_active_pin)`.
//!
//! - `finalized`: folded data must never be affected by a reorg; reverts purge unfolded window
//!   blocks only.
//! - `db_committed`: readers of the pending-deltas facade assume every uncommitted block is
//!   buffered; the database fallback path assumes every below-floor block is in the database.
//! - `tip - W`: the serving depth. `W = max(finality_horizon, max_version_age × blocks_per_min) +
//!   margin` (~128 on Ethereum).
//! - pins: an in-flight database fill pins the floor so the blocks it needs for top-up cannot be
//!   evicted underneath it (see [`FloorPin`]).

// Not yet constructed by production code; wired into `PendingDeltas` in a follow-up. The
// `unused_variables` allow covers parameters of the not-yet-implemented method bodies.
#![allow(dead_code)]
#![allow(unused_variables)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use deepsize::DeepSizeOf;
use tycho_common::{
    models::blockchain::{Block, BlockAggregatedChanges},
    storage::StorageError,
};

use crate::extractor::reorg_buffer::{BlockNumberOrTimestamp, CommitStatus, ReorgBuffer};

/// How long a floor pin protects the window before eviction may pass it.
///
/// Expiry is lazy: it is checked when eviction computes its bound, not by a timer. A fill whose
/// pin expired must discard its result (see [`FloorPin::is_valid`]).
const PIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Receives blocks evicted from a [`DeltaWindow`].
///
/// `apply_folded` is called while the caller holds the per-extractor window lock, before the
/// block is removed from the window. A concurrent reader therefore never observes a block that is
/// absent from both the window and the sink's store. Implementations must be fast: every facade
/// read on this extractor waits while a fold runs.
pub(crate) trait FoldSink: Send + Sync {
    /// Merges one finalized, database-committed block into the long-lived store.
    ///
    /// Blocks arrive in ascending order. Delta values are absolute, so applying the same block
    /// twice must be a no-op for implementations that tag values with their block number.
    fn apply_folded(&self, extractor: &str, block: &BlockAggregatedChanges);
}

/// Outcome of resolving a requested version against the window contents.
#[derive(Debug)]
pub(crate) enum WindowResolution {
    /// The version maps to a block currently held in the window.
    InWindow(Block),
    /// The version is older than the window floor; the database fallback path serves it.
    BelowFloor,
    /// The version is newer than the newest block this window has seen.
    AboveTip,
}

/// Fixed-depth window of block deltas for a single extractor.
///
/// Wraps the extractor's [`ReorgBuffer`] and owns the retention decision: today the buffer drains
/// as soon as the database commits, here blocks are kept until they are finalized, committed,
/// deeper than the configured depth, and not covered by a pin.
///
/// Not `Sync`: one instance lives behind the per-extractor `Arc<Mutex<..>>` owned by the
/// pending-deltas facade, and all methods run under that lock.
pub(crate) struct DeltaWindow {
    extractor: String,
    buffer: ReorgBuffer<BlockAggregatedChanges>,
    /// Target retention depth `W` in blocks.
    depth: u64,
    /// Highest `db_committed_block_height` seen on any inserted message. `None` until the first
    /// commit is observed; nothing is evictable before that.
    db_committed: Option<u64>,
    /// Highest `finalized_block_height` seen on any inserted message.
    finalized: Option<u64>,
    /// Active floor pins. Shared with issued [`FloorPin`] handles so release-on-drop does not
    /// need the window lock.
    pins: Arc<Mutex<PinRegistry>>,
    pin_timeout: Duration,
}

impl DeltaWindow {
    pub(crate) fn new(extractor: String, depth: u64) -> Self {
        Self {
            extractor,
            buffer: ReorgBuffer::new(),
            depth,
            db_committed: None,
            finalized: None,
            pins: Arc::new(Mutex::new(PinRegistry::default())),
            pin_timeout: PIN_TIMEOUT,
        }
    }

    /// Applies one full-block message to the window, folding-and-evicting as one operation.
    ///
    /// Evicted blocks are handed to `sink` before removal and never returned to the caller, so
    /// the fold-before-evict invariant cannot be skipped.
    pub(crate) fn insert(
        &mut self,
        message: &BlockAggregatedChanges,
        sink: &dyn FoldSink,
    ) -> Result<(), StorageError> {
        // Step 1 — revert message (`message.revert == true`):
        //
        // If the purge target would remove any block at or below
        // `min(self.finalized, self.db_committed)`, the window would silently diverge from the
        // database. Log at error level and `std::process::abort()` — an explicit abort, not a
        // panic: the deltas pump unwraps inside a spawned task, and a panic there kills only the
        // pump, leaving the server serving a frozen buffer.
        //
        // Otherwise `self.buffer.purge(message.block.hash)`. Purged blocks are unfolded by
        // construction (folds are gated on finality) and are discarded. Reverts carry
        // `db_committed_block_height: None`; watermarks stay as they are.
        //
        // Step 2 — regular message:
        //
        // `self.buffer.insert_block(message.clone())` (parent-hash chain enforced there), then
        // raise `self.finalized` / `self.db_committed` monotonically from the message.
        //
        // Step 3 — fold and evict:
        //
        // Drain the buffer up to `self.eviction_bound()`; for every drained block in ascending
        // order call `sink.apply_folded(&self.extractor, &block)`, timing each call into a
        // `delta_window_fold_duration` histogram (label: extractor) and logging a warning above a
        // slow-fold threshold. The fold runs under the caller's lock — that latency is visible to
        // every reader of this extractor.
        todo!("fold-then-evict insert")
    }

    /// The oldest block number still held in the window, if any.
    pub(crate) fn floor(&self) -> Option<u64> {
        // Needs a front accessor on `ReorgBuffer` (only `get_most_recent_block` exists today).
        todo!("expose the buffer's oldest block")
    }

    /// The newest block seen by this window, if any.
    pub(crate) fn tip(&self) -> Option<Block> {
        self.buffer.get_most_recent_block()
    }

    /// Resolves a requested version to a servable window block.
    ///
    /// The default request ("now", a timestamp newer than the tip) clamps to the tip. Versions
    /// below the floor report [`WindowResolution::BelowFloor`] and are served by the database
    /// fallback path; versions above the tip report [`WindowResolution::AboveTip`], which callers
    /// map to the same not-found error produced today. No database lookup is involved.
    pub(crate) fn resolve(&self, version: BlockNumberOrTimestamp) -> WindowResolution {
        // 1. Empty window -> BelowFloor (fallback path).
        // 2. Timestamp newer than tip -> InWindow(tip)  [clamp preserves today's semantics].
        // 3. Number/timestamp within [floor, tip] -> InWindow(matching block).
        // 4. Number below floor -> BelowFloor; number above tip -> AboveTip.
        todo!("resolve against buffered blocks")
    }

    /// Commit status of `version` derived from the database-commit watermark.
    ///
    /// Deliberately not `ReorgBuffer::get_commit_status`: that reports any version at or below
    /// the oldest buffered block as `Committed`, which is off by one today (the oldest buffered
    /// block is `db_committed + 1`) and would be off by the whole window depth once committed
    /// blocks are retained.
    pub(crate) fn commit_status(&self, version: BlockNumberOrTimestamp) -> Option<CommitStatus> {
        // None while the window is empty (mirrors today's "no finality found" default).
        // - version <= self.db_committed            -> Committed
        // - version <= tip                          -> Uncommitted
        // - otherwise                               -> Unseen
        // Timestamp versions compare against buffered block timestamps.
        todo!("watermark-based commit status")
    }

    /// Pins the current floor and returns the pinned height with a release-on-drop handle.
    ///
    /// While the pin is active (not dropped, not expired) eviction never passes the pinned
    /// height, so window deltas above it stay available for a fill's top-up.
    pub(crate) fn pin_floor(&mut self) -> (u64, FloorPin) {
        // 1. Read the current floor (or the next insert height for an empty window).
        // 2. Register (id, floor, Instant::now()) in `self.pins`.
        // 3. Return the height and a `FloorPin { id, registry: Arc::downgrade(&self.pins) }`.
        todo!("register pin")
    }

    /// Highest block number that may be folded-and-evicted, if any.
    fn eviction_bound(&self) -> Option<u64> {
        // min(finalized, db_committed, tip - depth, lowest active pin), where:
        // - `None` finalized/db_committed/tip means nothing is evictable yet;
        // - pins past `self.pin_timeout` are lazily marked invalid here (with a warning log and a
        //   counter) and excluded from the bound — see `PinRegistry::lowest_active`.
        todo!("compute eviction bound")
    }
}

impl DeepSizeOf for DeltaWindow {
    fn deep_size_of_children(&self, context: &mut deepsize::Context) -> usize {
        // The buffered blocks dominate; pin bookkeeping is transient and excluded.
        self.extractor
            .deep_size_of_children(context) +
            self.buffer
                .deep_size_of_children(context)
    }
}

/// Release-on-drop handle for a pinned window floor.
///
/// Dropping the handle releases the pin. Expiry is lazy (checked by eviction), so holding an
/// expired pin does not keep blocks alive — the owner must re-check [`FloorPin::is_valid`]
/// immediately before using data the pin was meant to protect, and discard its work if invalid.
pub(crate) struct FloorPin {
    id: u64,
    height: u64,
    registry: Weak<Mutex<PinRegistry>>,
}

impl FloorPin {
    /// The floor height this pin protects.
    pub(crate) fn height(&self) -> u64 {
        self.height
    }

    /// Whether the pin still guarantees that window blocks above its height are retained.
    ///
    /// Answered from the pin registry alone (own mutex, no window lock), so calling this at
    /// publish time cannot deadlock against an in-progress insert. Returns `false` once eviction
    /// has passed this pin after its timeout expired, or once the window is gone.
    pub(crate) fn is_valid(&self) -> bool {
        // Upgrade the registry weak ref; look up `self.id`; valid iff present and not marked
        // invalidated by a lazy-expiry sweep.
        todo!("registry lookup")
    }
}

impl Drop for FloorPin {
    fn drop(&mut self) {
        // Remove `self.id` from the registry if it still exists. Infallible and lock-ordered:
        // only the registry mutex is taken, never the window lock.
    }
}

/// Bookkeeping for active floor pins, shared between the window and issued handles.
#[derive(Default)]
struct PinRegistry {
    next_id: u64,
    entries: HashMap<u64, PinEntry>,
}

impl PinRegistry {
    /// Lowest pinned height among valid pins, lazily invalidating expired ones.
    fn lowest_active(&mut self, timeout: Duration) -> Option<u64> {
        // For each entry: if `created_at.elapsed() > timeout`, set `invalidated`, warn, and skip.
        // Return the minimum height of the remaining valid entries.
        todo!("lazy expiry sweep")
    }
}

struct PinEntry {
    height: u64,
    created_at: Instant,
    /// Set when an eviction sweep passed this pin after expiry; `FloorPin::is_valid` then
    /// reports `false` so the pin's owner discards its in-flight work.
    invalidated: bool,
}
