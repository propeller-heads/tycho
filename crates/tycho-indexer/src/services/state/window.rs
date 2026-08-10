//! Fixed-depth in-memory window of block deltas, one per extractor.
//!
//! The window retains roughly the last `W` blocks of [`BlockAggregatedChanges`] instead of
//! dropping blocks as soon as the database commits them. Blocks leave the window only through
//! [`DeltaWindow::insert`], which folds each evicted block into a [`FoldSink`] first — a block is
//! removed only after its fold succeeded, so no block's deltas can be lost between the window and
//! the long-lived store behind the sink.
//!
//! Retention rule: a block is evictable only when its number is at or below
//! `min(finalized, db_committed, tip - W, lowest_active_pin - 1)`, all subtractions saturating.
//!
//! - `finalized`: folded data must never be affected by a reorg; reverts purge unfolded window
//!   blocks only.
//! - `db_committed`: readers of the pending-deltas facade assume every uncommitted block is
//!   buffered; the database fallback path assumes every below-floor block is in the database.
//! - `tip - W`: the serving depth. `W = max(finality horizon, maximum version age served from
//!   memory) + margin` (~128 on Ethereum).
//! - pins: an in-flight database fill pins the floor so the pinned block and everything above it
//!   cannot be evicted underneath the fill (see [`FloorPin`]).

// Not yet constructed by production code; wired into `PendingDeltas` in a follow-up.
#![allow(dead_code)]

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
/// pin expired must discard its result (see [`DeltaWindow::publish_with_pin`]).
const PIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Receives blocks evicted from a [`DeltaWindow`].
pub(crate) trait FoldSink: Send + Sync {
    /// Merges one finalized, database-committed block into the long-lived store.
    ///
    /// Blocks arrive in ascending order. Delta values are absolute, so applying the same block
    /// twice must be a no-op for implementations that tag values with their block number —
    /// callers rely on this to retry a block whose fold previously failed. Implementations must
    /// be fast: folds run synchronously and delay state reads while they run.
    ///
    /// An error means the block was not (fully) applied; the caller keeps the block buffered and
    /// propagates the error.
    fn apply_folded(
        &self,
        extractor: &str,
        block: &BlockAggregatedChanges,
    ) -> Result<(), StorageError>;
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
/// deeper than the configured depth, and above every active pin.
///
/// The type is `Sync` by composition but not internally synchronized: methods rely on the caller
/// for exclusive access — one instance lives behind the per-extractor `Arc<Mutex<..>>` owned by
/// the pending-deltas facade.
///
/// Because committed blocks are retained, window contents and database rows overlap by up to
/// `depth` blocks. Readers that merge window deltas with database queries and assume the two are
/// disjoint (e.g. the new-components listing, which concatenates and counts both sides) must
/// bound window reads below by `db_committed + 1`, not by [`DeltaWindow::floor`].
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
    /// Pin expiry (see [`PIN_TIMEOUT`]). A field rather than the constant inline so tests can
    /// exercise expiry without waiting out the real timeout.
    pin_timeout: Duration,
}

impl DeltaWindow {
    /// Creates an empty window with the given target retention depth `W` (at least 1 block).
    pub(crate) fn new(extractor: String, depth: u64) -> Self {
        assert!(depth >= 1, "DeltaWindow depth must be at least 1, got {depth}");
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
    /// the fold-before-evict invariant cannot be skipped. Folds run while the caller holds
    /// exclusive access: every facade read on this extractor waits while a fold runs, which is
    /// why fold duration is metered (step 3).
    ///
    /// The message must be a full-block message; the caller filters partial-block messages.
    ///
    /// # Errors
    ///
    /// - `StorageError::Unexpected` when the message does not extend the buffered chain
    ///   (parent-hash mismatch).
    /// - `StorageError::NotFound` when a revert targets a hash that is not buffered.
    /// - Any error returned by [`FoldSink::apply_folded`]; the failed block and everything above it
    ///   stay buffered, and absolute delta values make the retry refold a no-op.
    #[allow(unused_variables)]
    pub(crate) fn insert(
        &mut self,
        message: &BlockAggregatedChanges,
        sink: &dyn FoldSink,
    ) -> Result<(), StorageError> {
        // Step 1 — revert message (`message.revert == true`):
        //
        // If the purge target would remove any block at or below
        // `min(self.finalized, self.db_committed)`, the database holds rows from the abandoned
        // branch and persisted state is never rolled back, so there is no in-process recovery.
        // Log at error level and `std::process::abort()`. Returning an error would also end the
        // process (the pump unwraps, the join chain fails the server task, main exits), but only
        // after a multi-hop supervision chain during which the server keeps serving from a
        // buffer that diverged from the database. Abort stops serving immediately and does not
        // depend on `panic = "unwind"` or the join wiring staying as it is.
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
        // Let `bound = self.eviction_bound()`. Skip this step when `bound` is `None` or below
        // the oldest buffered block number — the steady state right after startup, where
        // `ReorgBuffer::drain_blocks_until` would error because the target is not buffered.
        //
        // Fold before evicting: for each buffered block up to `bound` in ascending order call
        // `sink.apply_folded(&self.extractor, &block)`, timing each call into a
        // `delta_window_fold_duration` histogram (label: extractor) and logging a warning above
        // a slow-fold threshold. On a fold error stop folding and evict only the successfully
        // folded prefix before returning the error.
        //
        // Evict with `self.buffer.drain_blocks_until(h + 1)` where `h` is the highest folded
        // block: the retention bound is inclusive while `drain_blocks_until` is exclusive, and
        // parent-hash chaining keeps buffered numbers contiguous, so `h + 1` is buffered
        // whenever `h < tip` (guaranteed by `bound <= tip - depth` with `depth >= 1`).
        todo!("fold-then-evict insert")
    }

    /// The oldest block number still held in the window, if any.
    pub(crate) fn floor(&self) -> Option<u64> {
        // Needs a front accessor on `ReorgBuffer` (`get_block_range(None, None)` can reach the
        // front element, but a dedicated accessor avoids building an iterator for one block).
        todo!("expose the buffer's oldest block")
    }

    /// The newest block seen by this window, if any.
    pub(crate) fn tip(&self) -> Option<Block> {
        self.buffer.get_most_recent_block()
    }

    /// Resolves a requested version to a servable window block.
    ///
    /// The default request ("now", a timestamp newer than the tip) clamps to the tip. A
    /// timestamp between two buffered blocks rounds up to the first block whose timestamp is not
    /// older than the request, matching the buffer's range lookup today. Versions below the
    /// floor report [`WindowResolution::BelowFloor`] and are served by the database fallback
    /// path; versions above the tip report [`WindowResolution::AboveTip`]. No database lookup is
    /// involved.
    #[allow(unused_variables)]
    pub(crate) fn resolve(&self, version: BlockNumberOrTimestamp) -> WindowResolution {
        // 1. Empty window -> BelowFloor (fallback path).
        // 2. Timestamp newer than tip -> InWindow(tip)  [clamp preserves today's semantics].
        // 3. Number/timestamp within [floor, tip] -> InWindow(matching block; timestamps round up).
        // 4. Number below floor -> BelowFloor; number above tip -> AboveTip. BelowFloor is a
        //    deliberate fix, not a preservation: today a below-floor end version falls through to
        //    the front block of the buffer and serves deltas newer than requested.
        todo!("resolve against buffered blocks")
    }

    /// Commit status of `version` derived from the database-commit watermark.
    ///
    /// Deliberately not `ReorgBuffer::get_commit_status`: that reports any version at or below
    /// the oldest buffered block as `Committed`, which is off by one today (the oldest buffered
    /// block is `db_committed + 1`) and would be off by the whole window depth once committed
    /// blocks are retained.
    #[allow(unused_variables)]
    pub(crate) fn commit_status(&self, version: BlockNumberOrTimestamp) -> Option<CommitStatus> {
        // None while the window is empty (mirrors today's "no finality found" default).
        // - version <= self.db_committed            -> Committed
        // - version <= tip                          -> Uncommitted
        // - otherwise                               -> Unseen
        // Timestamp versions compare against buffered block timestamps.
        todo!("watermark-based commit status")
    }

    /// Pins the current floor and returns a release-on-drop handle.
    ///
    /// While the pin is active (not dropped, not expired) eviction never removes the pinned
    /// block or anything above it, so window deltas from the pinned height upward stay available
    /// for a fill's top-up. The pinned height is [`FloorPin::height`].
    ///
    /// On an empty window the pin registers at `db_committed + 1` (the next height that can
    /// enter the window), or at 0 before any commit is observed — nothing is evictable in either
    /// case, so every block inserted after the pin is protected.
    pub(crate) fn pin_floor(&mut self) -> FloorPin {
        // 1. Compute the pin height: current floor, else `db_committed + 1`, else 0.
        // 2. Register (id, height, Instant::now()) in `self.pins`.
        // 3. Return `FloorPin { id, height, registry: Arc::downgrade(&self.pins) }`.
        todo!("register pin")
    }

    /// Runs `publish` only if `pin` is still active, serialized against fold-and-evict.
    ///
    /// [`FloorPin::is_valid`] alone is advisory: expiry is checked lazily by eviction, so a pin
    /// can be observed valid and then swept by an insert before the observer acts on the answer.
    /// This method closes that race — it requires the same exclusive window access as
    /// [`DeltaWindow::insert`], so no eviction can run between the validity check and `publish`.
    /// Returns `None` without running `publish` when the pin is no longer registered; the caller
    /// must then discard the work the pin was protecting.
    #[allow(unused_variables)]
    pub(crate) fn publish_with_pin<T>(
        &mut self,
        pin: &FloorPin,
        publish: impl FnOnce() -> T,
    ) -> Option<T> {
        // 1. Look up `pin.id` in `self.pins`; absent (dropped or swept after expiry) -> None.
        // 2. Present -> run `publish` and return `Some` of its result. Exclusive access is held for
        //    the duration, so `publish` must be fast for the same reason folds must be.
        todo!("validate pin and publish atomically")
    }

    /// Highest block number that may be folded-and-evicted, if any.
    fn eviction_bound(&self) -> Option<u64> {
        // min(finalized, db_committed, tip - depth, lowest_active_pin - 1), where:
        // - `None` finalized/db_committed/tip means nothing is evictable yet;
        // - both subtractions saturate at 0: a chain shorter than `depth` evicts nothing through
        //   the depth term, and a pin at height 0 blocks all eviction above genesis;
        // - `db_committed <= finalized` holds by construction today (message aggregation rejects
        //   the opposite), so the finalized term is belt-and-braces against a future change to the
        //   commit trigger;
        // - pins past `self.pin_timeout` are removed (with a warning log and a counter) and
        //   excluded from the bound — see `PinRegistry::lowest_active`.
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
/// Dropping the handle releases the pin. Expiry is lazy (an eviction sweep removes expired
/// pins), so holding an expired pin does not keep blocks alive — the owner must publish through
/// [`DeltaWindow::publish_with_pin`], which re-validates the pin atomically with eviction.
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

    /// Whether the pin still guarantees that the pinned height and above are retained.
    ///
    /// Advisory fast-path answered from the pin registry alone (own mutex, no window lock), so a
    /// fill can abandon doomed work early without contending on the window. The answer can go
    /// stale immediately after returning `true`; the authoritative check is
    /// [`DeltaWindow::publish_with_pin`]. Returns `false` once an eviction sweep removed the pin
    /// after its timeout expired, or once the window is gone.
    pub(crate) fn is_valid(&self) -> bool {
        // Upgrade the registry weak ref; the pin is valid iff `self.id` is still registered.
        todo!("registry lookup")
    }
}

impl Drop for FloorPin {
    fn drop(&mut self) {
        // Lock-ordered: only the registry mutex is taken, never the window lock, so a pin can be
        // dropped while an insert runs without deadlock. A poisoned registry is skipped — the
        // leaked entry only delays eviction until the expiry sweep collects it.
        if let Some(registry) = self.registry.upgrade() {
            if let Ok(mut registry) = registry.lock() {
                registry.entries.remove(&self.id);
            }
        }
    }
}

/// Bookkeeping for active floor pins, shared between the window and issued handles.
#[derive(Default)]
struct PinRegistry {
    next_id: u64,
    entries: HashMap<u64, PinEntry>,
}

impl PinRegistry {
    /// Lowest pinned height among active pins, removing expired ones.
    ///
    /// Removal is the lazy-expiry sweep: a pin past `timeout` is dropped from the registry here
    /// (with a warning log and a counter), which is what makes [`FloorPin::is_valid`] report
    /// `false` afterwards. An expired pin on a window that never evicts keeps reporting valid
    /// until a sweep runs — sound, because nothing has been evicted from under it.
    #[allow(unused_variables)]
    fn lowest_active(&mut self, timeout: Duration) -> Option<u64> {
        // Remove every entry with `created_at.elapsed() > timeout`; return the minimum height of
        // the remaining entries.
        todo!("lazy expiry sweep")
    }
}

struct PinEntry {
    height: u64,
    created_at: Instant,
}
