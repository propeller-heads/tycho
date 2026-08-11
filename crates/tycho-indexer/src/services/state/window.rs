//! Fixed-depth in-memory window of block deltas, one per extractor.
//!
//! The window retains roughly the last `W` blocks of [`BlockAggregatedChanges`] instead of
//! dropping blocks as soon as the database commits them. Blocks leave the window only through
//! [`DeltaWindow::fold_and_evict`], which folds each evicted block into a [`FoldSink`] before
//! removing it, so no block's deltas can be lost between the window and the long-lived store
//! behind the sink.
//!
//! Retention rule: a block is evictable only when its number is at or below
//! `min(finalized, db_committed, tip - W)`, the subtraction saturating.
//!
//! ```text
//!  ◄── evicted (folded into the sink, oldest first)
//!    ┌──────────────────────┬─────────────────────┬────────────────┐
//!    │ committed, finalized │ finalized, not yet  │  unfinalized   │
//!    │  (kept for serving)  │     committed       │                │
//!    └──────────────────────┴─────────────────────┴────────────────┘
//!    ▲                      ▲                     ▲                ▲
//!  floor               db_committed           finalized          tip
//!    ◄───────────────────────── ≥ W blocks ───────────────────────►
//! ```
//!
//! `W` is the total window depth measured from the tip — it includes the unfinalized and
//! uncommitted blocks, it is not extra retention on top of them. When finality or the database
//! commit lag more than `W` blocks behind the tip, the watermark terms of the `min` govern and
//! the window grows beyond `W` (unfinalized and uncommitted blocks are never evicted).
//!
//! Folding is batched: [`DeltaWindow::fold_and_evict`] is a no-op until at least
//! `min_fold_batch` blocks are evictable, then folds all of them, so blocks are not folded one
//! by one at the chain tip rate. At steady state the window size therefore oscillates between
//! `W` and `W + min_fold_batch` blocks.
//!
//! - `finalized`: folded data must never be affected by a reorg; reverts purge unfolded window
//!   blocks only.
//! - `db_committed`: readers of the pending-deltas facade assume every uncommitted block is
//!   buffered; the database fallback path assumes every below-floor block is in the database.
//! - `tip - W`: the serving depth. `W = max(finality horizon, maximum version age served from
//!   memory) + margin` (~128 on Ethereum).
//!
//! Errors from [`DeltaWindow::insert`] and [`DeltaWindow::fold_and_evict`] are fatal: each one
//! means the window no longer matches the chain or the database, with no in-process recovery —
//! the caller must terminate the indexer process rather than keep serving.

// Not yet constructed by production code; wired into `PendingDeltas` in a follow-up.
#![allow(dead_code)]

use deepsize::DeepSizeOf;
use tycho_common::{
    models::blockchain::{Block, BlockAggregatedChanges},
    storage::StorageError,
};

use crate::extractor::reorg_buffer::{BlockNumberOrTimestamp, CommitStatus, ReorgBuffer};

/// Receives blocks evicted from a [`DeltaWindow`].
pub(crate) trait FoldSink: Send + Sync {
    /// Merges one finalized, database-committed block into the long-lived store.
    ///
    /// Blocks arrive in ascending order. Delta values are absolute, so applying the same block
    /// twice must be a no-op for implementations that tag values with their block number.
    /// Implementations must be fast: folds run synchronously and delay state reads while they
    /// run.
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
/// as soon as the database commits, here blocks are kept until they are finalized, committed, and
/// deeper than the configured depth.
///
/// The type is `Sync` by composition but not internally synchronized: methods rely on the caller
/// for exclusive access — one instance lives behind the per-extractor `Arc<Mutex<..>>` owned by
/// the pending-deltas facade. That lock is also the coordination point between eviction and the
/// database fill path: a fill must not publish state assembled from blocks that
/// [`DeltaWindow::fold_and_evict`] folded out from under it, so the facade serializes the two
/// behind the same lock (the fill path lands in a later story).
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
    /// Minimum number of evictable blocks required before a fold runs. Amortizes folding: with
    /// 1 every evictable block is folded as soon as possible; larger values trade `min_fold_batch`
    /// extra buffered blocks for folds that run `min_fold_batch` times less often.
    min_fold_batch: u64,
}

impl DeltaWindow {
    /// Creates an empty window with the given target retention depth `W` (at least 1 block) and
    /// fold batch size (at least 1, see [`DeltaWindow::fold_and_evict`]).
    pub(crate) fn new(extractor: String, depth: u64, min_fold_batch: u64) -> Self {
        assert!(depth >= 1, "DeltaWindow depth must be at least 1, got {depth}");
        assert!(
            min_fold_batch >= 1,
            "DeltaWindow fold batch must be at least 1, got {min_fold_batch}"
        );
        Self {
            extractor,
            buffer: ReorgBuffer::new(),
            depth,
            db_committed: None,
            finalized: None,
            min_fold_batch,
        }
    }

    /// Applies one full-block message to the window.
    ///
    /// Regular messages must extend the buffered chain; revert messages purge the abandoned
    /// blocks. No folding or eviction happens here — see [`DeltaWindow::fold_and_evict`].
    ///
    /// The message must be a full-block message; the caller filters partial-block messages.
    ///
    /// # Errors
    ///
    /// Every error is fatal (see the module doc):
    ///
    /// - `StorageError::Unexpected` when a regular message does not extend the buffered chain
    ///   (parent-hash mismatch), or when a revert would remove a block at or below `min(finalized,
    ///   db_committed)` — the database then holds rows from the abandoned branch, and persisted
    ///   state is never rolled back.
    /// - `StorageError::NotFound` when a revert targets a hash that is not buffered.
    #[allow(unused_variables)]
    pub(crate) fn insert(&mut self, message: &BlockAggregatedChanges) -> Result<(), StorageError> {
        // Revert message (`message.revert == true`): error if the purge target would remove any
        // block at or below `min(self.finalized, self.db_committed)` (see Errors), otherwise
        // `self.buffer.purge(message.block.hash)`. Purged blocks are unfolded by construction
        // (folding is gated on the same watermarks) and are discarded. Reverts carry
        // `db_committed_block_height: None`; watermarks stay as they are.
        //
        // Regular message: `self.buffer.insert_block(message.clone())` (parent-hash chain
        // enforced there), then raise `self.finalized` / `self.db_committed` monotonically from
        // the message.
        todo!("insert or purge")
    }

    /// Folds every evictable block into `sink`, then removes it from the window.
    ///
    /// Folding is batched: when fewer than `min_fold_batch` blocks are evictable the call is a
    /// no-op, otherwise the whole batch is folded. Fold and eviction are a single operation per
    /// block: a block is removed only after its fold succeeded, and evicted blocks are never
    /// returned to the caller, so no block's deltas can be lost between the window and the store
    /// behind the sink. Folds run while the caller holds exclusive access: every facade read on
    /// this extractor waits while a fold runs, which is why fold duration is metered.
    ///
    /// # Errors
    ///
    /// Any error from [`FoldSink::apply_folded`] is propagated after evicting the successfully
    /// folded prefix. Fold errors are treated as fatal until their failure modes are better
    /// understood (see the module doc).
    #[allow(unused_variables)]
    pub(crate) fn fold_and_evict(&mut self, sink: &dyn FoldSink) -> Result<(), StorageError> {
        // Let `bound = self.eviction_bound()` and count the evictable blocks:
        // `self.buffer.count_blocks_before(bound + 1)`, 0 when `bound` is `None`. Return `Ok`
        // when the count is below `self.min_fold_batch` — this also covers a bound below the
        // oldest buffered block (count 0), the steady state right after startup, where
        // `ReorgBuffer::drain_blocks_until` would error because the target is not buffered.
        //
        // For each buffered block up to `bound` in ascending order call
        // `sink.apply_folded(&self.extractor, &block)`, timing each call into a
        // `delta_window_fold_duration` histogram (label: extractor) and logging a warning above
        // a slow-fold threshold. On a fold error stop folding.
        //
        // Evict with `self.buffer.drain_blocks_until(h + 1)` where `h` is the highest
        // successfully folded block: the retention bound is inclusive while `drain_blocks_until`
        // is exclusive, and parent-hash chaining keeps buffered numbers contiguous, so `h + 1`
        // is buffered whenever `h < tip` (guaranteed by `bound <= tip - depth` with
        // `depth >= 1`).
        todo!("fold then evict")
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

    /// Highest block number that may be folded-and-evicted, if any.
    fn eviction_bound(&self) -> Option<u64> {
        // min(finalized, db_committed, tip - depth), where:
        // - `None` finalized/db_committed/tip means nothing is evictable yet;
        // - the subtraction saturates at 0: a chain shorter than `depth` evicts nothing through the
        //   depth term;
        // - `db_committed <= finalized` holds by construction today (message aggregation rejects
        //   the opposite), so the finalized term is belt-and-braces against a future change to the
        //   commit trigger.
        todo!("compute eviction bound")
    }
}

impl DeepSizeOf for DeltaWindow {
    fn deep_size_of_children(&self, context: &mut deepsize::Context) -> usize {
        // The buffered blocks dominate.
        self.extractor
            .deep_size_of_children(context) +
            self.buffer
                .deep_size_of_children(context)
    }
}
