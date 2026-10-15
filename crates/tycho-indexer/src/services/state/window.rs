//! Fixed-depth in-memory window of block deltas, one per extractor.
//!
//! The window retains roughly the last `W` blocks of [`BlockAggregatedChanges`] instead of
//! dropping blocks as soon as the database commits them. Blocks leave the window only through
//! [`DeltaWindow::fold_evictable`] and [`DeltaWindow::fold_committed`], which fold each evicted
//! block into a [`FoldSink`] before removing it, so no committed block's deltas can be lost
//! between the window and the long-lived store behind the sink.
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
//! `W` is the total window depth measured from the tip, not extra retention on top of the
//! unfinalized and uncommitted blocks. It is a floor, not a target: when finality or the
//! database commit lag more than `W` blocks behind the tip, the watermark terms of the `min`
//! govern and the window grows beyond `W`. `db_committed` advances in jumps of
//! `--database-insert-batch-size`, so a commit batch larger than `W` always binds.
//!
//! Folding is batched: [`DeltaWindow::fold_evictable`] is a no-op until at least
//! `min_fold_batch` blocks are evictable, then folds all of them. At steady state the window
//! size oscillates between `W` and `W + min_fold_batch` blocks.
//!
//! An error from [`DeltaWindow::insert`] or [`DeltaWindow::fold_evictable`] means the window no
//! longer matches the extractor's chain or the sink. The window cannot repair itself: the
//! blocks it is missing come only from the extractor, and clearing the window alone leaves a
//! gap of blocks that are neither in the database nor in memory. The pump therefore ends, and
//! with it the process. The one recovery is an extractor restart: the extractor replays every
//! block above its database cursor, so the pump folds the committed blocks with
//! [`DeltaWindow::fold_committed`], clears the window, and lets the replay refill it.
//!
//! Next to its blocks, the window keeps a [`RevertJournal`] of the revert messages it applied.
//! With both, [`DeltaWindow::catch_up_path`] tells a client that missed messages which ones to
//! apply to get from its last block to a block the window holds, reverts included.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use deepsize::DeepSizeOf;
use metrics::{gauge, histogram};
use tracing::{debug, trace, warn};
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges},
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Address, BlockHash,
    },
    storage::{BlockIdentifier, BlockOrTimestamp, StorageError, WriteTimestamp},
    Bytes,
};

use crate::{
    extractor::reorg_buffer::{BlockNumberOrTimestamp, CommitStatus, ReorgBuffer},
    services::state::revert_journal::RevertJournal,
};

/// Receives blocks evicted from a [`DeltaWindow`].
pub(crate) trait FoldSink: Send + Sync {
    /// Merges one finalized, database-committed block into the long-lived store.
    ///
    /// Blocks arrive in ascending order. Delta values are absolute, so applying the same block
    /// twice must be a no-op for implementations that tag values with their block. An error
    /// means the block was not fully applied.
    fn fold(&self, block: &BlockAggregatedChanges) -> Result<(), StorageError>;
}

/// Retention settings shared by every extractor's [`DeltaWindow`]. Both values are at least 1;
/// the CLI enforces this.
#[derive(Clone, Copy, Debug, DeepSizeOf)]
pub struct WindowConfig {
    /// Retention depth `W` in blocks, a lower bound on how many blocks the window keeps.
    pub depth: u64,
    /// Evictable blocks required before a fold runs. Only shapes fold cadence when `tip - depth`
    /// binds; a `--database-insert-batch-size` at or above it already groups the folds.
    pub min_fold_batch: usize,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { depth: 128, min_fold_batch: 1 }
    }
}

/// One empty window per extractor, keyed by extractor name, all with the same `config`. The
/// windows are shared: every holder of the map reads and writes the same windows.
pub(crate) fn new_windows<'a>(
    extractors: impl IntoIterator<Item = &'a str>,
    config: WindowConfig,
) -> HashMap<String, Arc<Mutex<DeltaWindow>>> {
    extractors
        .into_iter()
        .map(|extractor| {
            debug!(extractor, "Creating DeltaWindow");
            (
                extractor.to_string(),
                Arc::new(Mutex::new(DeltaWindow::new(extractor.to_string(), config))),
            )
        })
        .collect()
}

/// Drops every folded block: the sink for `ENTITY_CACHE_MODE=off`, which keeps no cache.
pub(crate) struct DiscardSink;

impl FoldSink for DiscardSink {
    fn fold(&self, _block: &BlockAggregatedChanges) -> Result<(), StorageError> {
        Ok(())
    }
}

/// Outcome of resolving a requested version against the window contents.
#[derive(Debug, PartialEq)]
pub(crate) enum WindowResolution {
    /// The version maps to a block currently held in the window.
    InWindow(Block),
    /// The version is older than the window floor.
    BelowFloor,
    /// The window holds no block: at startup, or after the extractor restarted.
    Empty,
    /// The version is a block hash the window does not hold.
    UnknownHash,
    /// The version is newer than the newest block this window has seen.
    AboveTip,
}

/// Why a window cannot serve the messages between two blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CatchUpUnavailable {
    /// The window holds no block: at startup, or right after the extractor restarted.
    EmptyWindow,
    /// `from` is older than the parent of the window's oldest block.
    BelowFloor,
    /// `from` is not a block the window holds, was orphaned by no revert the window still
    /// remembers, and is not the parent of the window's oldest block.
    UnknownHash,
    /// `to` is not a canonical block in the window.
    ToNotInWindow,
    /// The walk from `from` reaches the window's chain after `to`: the client received a revert
    /// the window has not applied yet.
    ToNotAhead,
}

/// The messages that take a client from one block to another, oldest first, or why a window
/// cannot serve them.
pub(crate) type CatchUpPath = Result<Vec<Arc<BlockAggregatedChanges>>, CatchUpUnavailable>;

/// One block's changes to a component, as captured from the window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComponentChange {
    /// Write timestamp of the block the change belongs to.
    pub at: WriteTimestamp,
    /// State delta of the block, if the block changed the component's state.
    pub delta: Option<ProtocolComponentStateDelta>,
    /// Token balances of the block, if the block changed the component's balances.
    pub balances: Option<HashMap<Bytes, ComponentBalance>>,
}

/// One block's changes to an account, as captured from the window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccountChange {
    /// Write timestamp of the block the change belongs to.
    pub at: WriteTimestamp,
    /// Account delta of the block, if the block changed the account.
    pub delta: Option<AccountDelta>,
    /// Token balances of the block, if the block changed the account's balances.
    pub balances: Option<HashMap<Address, AccountBalance>>,
}

/// Folds slower than this are logged at `warn`.
const SLOW_FOLD_THRESHOLD: Duration = Duration::from_millis(5);

/// Window of block deltas for one extractor.
///
/// Owns an RPC-side [`ReorgBuffer`] and decides retention: a block stays until it is finalized,
/// committed, and deeper than `config.depth`. Not internally synchronized; the owner holds it
/// behind a `Mutex`. Window and database overlap by up to `depth` blocks; readers that merge both
/// must start window reads at `db_committed + 1` (see [`DeltaWindow::uncommitted_blocks`]).
#[derive(DeepSizeOf)]
pub(crate) struct DeltaWindow {
    extractor: String,
    buffer: ReorgBuffer<Arc<BlockAggregatedChanges>>,
    config: WindowConfig,
    /// Highest `db_committed_block_height` seen on any inserted message. `None` until the first
    /// commit is observed; nothing is evictable before that.
    db_committed: Option<u64>,
    /// Highest `finalized_block_height` seen on any inserted message.
    finalized: Option<u64>,
    /// Moves on every insert (a block or a revert) and every clear. A fold does not move it: it
    /// runs under the same lock as the insert or the clear that makes it due. Two equal readings
    /// mean the window did not change in between.
    generation: u64,
    /// The revert messages applied to `buffer`, kept while their target can still anchor a
    /// catch-up path. Pruned on eviction and emptied on clear.
    journal: RevertJournal,
}

impl DeltaWindow {
    /// Creates an empty window.
    pub(crate) fn new(extractor: String, config: WindowConfig) -> Self {
        Self {
            extractor,
            buffer: ReorgBuffer::new(),
            config,
            db_committed: None,
            finalized: None,
            generation: 0,
            journal: RevertJournal::default(),
        }
    }

    /// The change counter; see the `generation` field.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Applies one full-block message to the window.
    ///
    /// Regular messages must extend the buffered chain; revert messages purge the abandoned
    /// blocks and are journaled with them. No folding or eviction happens here — see
    /// [`DeltaWindow::fold_evictable`]. The caller filters partial-block messages, reverts
    /// included: the window holds full blocks only.
    ///
    /// # Errors
    ///
    /// - `StorageError::Unexpected` when a regular message does not extend the buffered chain
    ///   (parent-hash mismatch), or when a revert would remove a block at or below `min(finalized,
    ///   db_committed)` — the database then holds rows from the abandoned branch, and persisted
    ///   state is never rolled back. The window no longer matches the extractor's chain.
    /// - `StorageError::NotFound` when a revert targets a hash that is not buffered. The window is
    ///   unchanged.
    pub(crate) fn insert(
        &mut self,
        message: &Arc<BlockAggregatedChanges>,
    ) -> Result<(), StorageError> {
        self.generation += 1;
        if message.revert {
            return self.revert_to(message);
        }
        self.buffer
            .insert_block(Arc::clone(message))?;
        self.finalized = Some(
            self.finalized
                .map_or(message.finalized_block_height, |f| f.max(message.finalized_block_height)),
        );
        if let Some(committed) = message.db_committed_block_height {
            self.db_committed = Some(
                self.db_committed
                    .map_or(committed, |c| c.max(committed)),
            );
        }
        Ok(())
    }

    /// Purges every block after the revert target and journals the revert with the blocks it
    /// orphaned. Errors when the purge would remove a block at or below `min(finalized,
    /// db_committed)`: the database may hold rows from the abandoned branch, and persisted state
    /// is never rolled back. Watermarks stay as they are.
    fn revert_to(&mut self, message: &Arc<BlockAggregatedChanges>) -> Result<(), StorageError> {
        let target = message.block.number;
        let irreversible = match (self.finalized, self.db_committed) {
            (Some(finalized), Some(committed)) => Some(finalized.min(committed)),
            (Some(finalized), None) => Some(finalized),
            (None, _) => None,
        };
        if irreversible.is_some_and(|height| target < height) {
            return Err(StorageError::Unexpected(format!(
                "Revert to block {target} would remove blocks at or below the irreversible height {}",
                irreversible.unwrap_or_default()
            )));
        }
        let orphaned = self
            .buffer
            .purge(message.block.hash.clone())?;
        self.journal
            .record(Arc::clone(message), &orphaned);
        Ok(())
    }

    /// Folds every evictable block into `sink`, oldest first, then evicts it.
    ///
    /// No-op while fewer than `min_fold_batch` blocks are evictable. A block is evicted only
    /// after its fold succeeds.
    ///
    /// # Errors
    ///
    /// The first [`FoldSink::fold`] error is returned after the folded prefix is evicted; the
    /// failing block stays in the window.
    pub(crate) fn fold_evictable(&mut self, sink: &dyn FoldSink) -> Result<(), StorageError> {
        let Some(bound) = self.eviction_bound() else {
            trace!(
                extractor = %self.extractor,
                finalized = ?self.finalized,
                db_committed = ?self.db_committed,
                tip = ?self.buffer.newest().map(|m| m.block.number),
                "Nothing evictable yet"
            );
            return Ok(());
        };
        let evictable = self
            .buffer
            .count_blocks_before(bound + 1);
        if evictable < self.config.min_fold_batch {
            return Ok(());
        }
        self.fold_and_evict(evictable, sink)
    }

    /// Folds every finalized, committed block into `sink` and evicts it. Depth and fold
    /// batching do not apply: everything at or below `min(finalized, db_committed)` is handed to
    /// the sink. Uncommitted blocks stay in the window.
    ///
    /// # Errors
    ///
    /// The first [`FoldSink::fold`] error is returned after the folded prefix is evicted; the
    /// failing block and everything above it stay in the window.
    pub(crate) fn fold_committed(&mut self, sink: &dyn FoldSink) -> Result<(), StorageError> {
        let (Some(finalized), Some(committed)) = (self.finalized, self.db_committed) else {
            return Ok(());
        };
        let count = self
            .buffer
            .count_blocks_before(finalized.min(committed) + 1);
        self.fold_and_evict(count, sink)
    }

    /// Publishes the window's block count and newest block for its extractor, as the gauges
    /// `delta_window_blocks` and `delta_window_tip_block`. An empty window reports 0 blocks and
    /// leaves the tip gauge at its last value.
    pub(crate) fn report_metrics(&self) {
        let blocks = gauge!("delta_window_blocks", "extractor" => self.extractor.clone());
        let (Some(oldest), Some(newest)) = (self.buffer.oldest(), self.buffer.newest()) else {
            blocks.set(0.0);
            return;
        };
        // Buffered blocks are contiguous, so the span from floor to tip is the count.
        blocks.set((newest.block.number - oldest.block.number + 1) as f64);
        gauge!("delta_window_tip_block", "extractor" => self.extractor.clone())
            .set(newest.block.number as f64);
    }

    /// Empties the window and its revert journal, and forgets both watermarks. The configuration
    /// stays. The next inserted block starts a new chain, whatever its parent.
    pub(crate) fn clear(&mut self) {
        self.generation += 1;
        self.buffer = ReorgBuffer::new();
        self.journal.clear();
        self.db_committed = None;
        self.finalized = None;
    }

    /// Folds the `count` oldest blocks into `sink` and evicts the folded prefix.
    fn fold_and_evict(&mut self, count: usize, sink: &dyn FoldSink) -> Result<(), StorageError> {
        let mut folded_upto = None;
        let mut outcome = Ok(());
        for block in self
            .buffer
            .get_block_range(None, None)?
            .take(count)
        {
            let started = Instant::now();
            let result = sink.fold(block);
            let elapsed = started.elapsed();
            histogram!("delta_window_fold_duration_ms", "extractor" => self.extractor.clone())
                .record(elapsed.as_secs_f64() * 1000.0);
            if elapsed > SLOW_FOLD_THRESHOLD {
                warn!(
                    extractor = %self.extractor,
                    block = block.block.number,
                    ?elapsed,
                    "Slow DeltaWindow fold"
                );
            }
            match result {
                Ok(()) => folded_upto = Some(block.block.number),
                Err(err) => {
                    outcome = Err(err);
                    break;
                }
            }
        }
        if let Some(height) = folded_upto {
            self.buffer
                .drain_blocks_until(height + 1)?;
            if let Some(oldest) = self.buffer.oldest() {
                self.journal.prune(oldest.block.number);
            }
        }
        outcome
    }

    /// Buffered blocks between two versions, ascending. Same bound semantics as
    /// [`ReorgBuffer::get_block_range`].
    pub(crate) fn blocks(
        &self,
        start: Option<BlockNumberOrTimestamp>,
        end: Option<BlockNumberOrTimestamp>,
    ) -> Result<impl Iterator<Item = &BlockAggregatedChanges>, StorageError> {
        Ok(self
            .buffer
            .get_block_range(start, end)?
            .map(Arc::as_ref))
    }

    /// Buffered blocks the database does not hold yet: everything above `db_committed`.
    /// Readers that merge window data with database rows must use this rather than
    /// `blocks(None, ..)`, or they count retained committed blocks twice.
    pub(crate) fn uncommitted_blocks(
        &self,
    ) -> Result<impl Iterator<Item = &BlockAggregatedChanges>, StorageError> {
        let committed = self.db_committed;
        Ok(self
            .blocks(None, None)?
            .skip_while(move |b| committed.is_some_and(|c| b.block.number <= c)))
    }

    /// Commit status of `version`.
    ///
    /// `Unseen` above the tip. `Committed` for versions at or below `db_committed`, or below the
    /// floor when no committed block is in the window. `Uncommitted` otherwise. `None` on an
    /// empty window.
    pub(crate) fn commit_status(&self, version: BlockNumberOrTimestamp) -> Option<CommitStatus> {
        let oldest = &self.buffer.oldest()?.block;
        let tip = &self.buffer.newest()?.block;
        if version.greater_than(tip) {
            return Some(CommitStatus::Unseen);
        }
        let committed = match self.committed_block() {
            Some(block) => !version.greater_than(block),
            // No committed block is in the window: everything below the floor counts as
            // committed.
            None => version.less_than(oldest),
        };
        Some(if committed { CommitStatus::Committed } else { CommitStatus::Uncommitted })
    }

    /// The buffered block at `db_committed`, if the window still holds it.
    fn committed_block(&self) -> Option<&Block> {
        self.buffer
            .block_at(self.db_committed?)
            .map(|m| &m.block)
    }

    /// Highest block number that may be folded-and-evicted. `None` when the window is empty or
    /// no database commit has been observed yet.
    fn eviction_bound(&self) -> Option<u64> {
        let tip = self.buffer.newest()?.block.number;
        let finalized = self.finalized?;
        let db_committed = self.db_committed?;
        Some(
            finalized
                .min(db_committed)
                .min(tip.saturating_sub(self.config.depth)),
        )
    }

    /// The oldest block still held in the window, if any.
    #[cfg(test)]
    pub(crate) fn floor(&self) -> Option<Block> {
        self.buffer
            .oldest()
            .map(|m| m.block.clone())
    }

    /// The newest block held in the window, if any.
    #[cfg(test)]
    pub(crate) fn tip(&self) -> Option<Block> {
        self.buffer
            .newest()
            .map(|m| m.block.clone())
    }

    /// The blocks up to `upto`, ascending. Only the `Arc`s are copied, so a reader holds the window
    /// lock just for this and scans the blocks with [`account_changes`] or [`component_changes`]
    /// after releasing it. An `upto` below the floor yields the floor block alone; resolve it
    /// first with [`DeltaWindow::resolve`] if that matters.
    ///
    /// # Errors
    ///
    /// `StorageError::NotFound` when the window is empty.
    pub(crate) fn blocks_upto(
        &self,
        upto: u64,
    ) -> Result<Vec<Arc<BlockAggregatedChanges>>, StorageError> {
        Ok(self
            .buffer
            .get_block_range(None, Some(BlockNumberOrTimestamp::Number(upto)))?
            .cloned()
            .collect())
    }

    /// The messages a client must apply, in order, to get from block `from` to block `to`: the
    /// same full-block and revert messages the extractor broadcast. Only the `Arc`s are copied,
    /// so a caller holds the window lock just for this and builds its response after releasing
    /// it.
    ///
    /// | `from`                                  | Path                                        |
    /// |-----------------------------------------|---------------------------------------------|
    /// | A block the window holds                | The blocks in `(from, to]`                  |
    /// | Orphaned by a journaled revert          | That revert, then the path from its target  |
    /// | The parent of the window's oldest block | The blocks from the oldest one up to `to`   |
    ///
    /// A revert target that was orphaned in turn continues through the revert that orphaned it.
    /// The path is empty when `from` is `to`, and holds only reverts when the walk from `from`
    /// ends at `to`. `from_number` is the number of `from`; it only tells
    /// [`CatchUpUnavailable::BelowFloor`] apart from [`CatchUpUnavailable::UnknownHash`].
    ///
    /// # Errors
    ///
    /// The checks run in this order:
    /// 1. [`CatchUpUnavailable::EmptyWindow`] when the window holds no block.
    /// 2. [`CatchUpUnavailable::ToNotInWindow`] when the window does not hold `to`.
    /// 3. [`CatchUpUnavailable::BelowFloor`] when the walk from `from` gets stuck and `from_number`
    ///    is below the parent of the oldest block, else [`CatchUpUnavailable::UnknownHash`].
    /// 4. [`CatchUpUnavailable::ToNotAhead`] when the walk reaches the window's chain after `to`.
    pub(crate) fn catch_up_path(
        &self,
        from: &BlockHash,
        from_number: u64,
        to: &BlockHash,
    ) -> CatchUpPath {
        let Some(oldest) = self.buffer.oldest() else {
            return Err(CatchUpUnavailable::EmptyWindow);
        };
        let Some(to_position) = self.position(to) else {
            return Err(CatchUpUnavailable::ToNotInWindow);
        };
        let mut path = Vec::new();
        let mut current = from;
        // Position of the first held block the path continues with. Each revert walked through
        // targets a lower block than the one it orphaned, so the walk ends.
        let continue_at = loop {
            if let Some(position) = self.position(current) {
                break position + 1;
            }
            if current == &oldest.block.parent_hash {
                break 0;
            }
            let Some(revert) = self.journal.orphaned_by(current) else {
                return Err(if from_number.saturating_add(1) < oldest.block.number {
                    CatchUpUnavailable::BelowFloor
                } else {
                    CatchUpUnavailable::UnknownHash
                });
            };
            path.push(Arc::clone(revert));
            current = &revert.block.hash;
        };
        if continue_at > to_position + 1 {
            return Err(CatchUpUnavailable::ToNotAhead);
        }
        path.extend(
            self.buffer
                .iter()
                .skip(continue_at)
                .take(to_position + 1 - continue_at)
                .cloned(),
        );
        Ok(path)
    }

    /// Position of the held block `hash`, counted from the oldest block.
    fn position(&self, hash: &BlockHash) -> Option<usize> {
        // Clients resume close to the tip, so the scan starts there.
        self.buffer
            .iter()
            .rposition(|message| &message.block.hash == hash)
    }

    /// Resolves a requested version to a servable window block, from memory only.
    ///
    /// | Version                                    | Resolution        |
    /// |--------------------------------------------|-------------------|
    /// | Timestamp newer than the tip (the default) | `InWindow(tip)`   |
    /// | Number, hash or timestamp in the window    | `InWindow(block)` |
    /// | Number or timestamp below the floor        | `BelowFloor`      |
    /// | Number above the tip                       | `AboveTip`        |
    /// | Hash the window does not hold              | `BelowFloor`      |
    /// | `Latest`                                   | `InWindow(tip)`   |
    ///
    /// A timestamp between two buffered blocks rounds up to the first block whose timestamp is
    /// not older than the request, like [`ReorgBuffer::get_block_range`]. An unknown hash reports
    /// `BelowFloor` because only the database can tell an old hash from one that never existed.
    /// An empty window reports `BelowFloor` for every version.
    pub(crate) fn resolve(&self, version: &BlockOrTimestamp) -> WindowResolution {
        let (Some(oldest), Some(tip)) = (self.buffer.oldest(), self.buffer.newest()) else {
            return WindowResolution::Empty;
        };
        let version = match version {
            BlockOrTimestamp::Block(BlockIdentifier::Hash(hash)) => {
                return self
                    .blocks(None, None)
                    .ok()
                    .and_then(|mut blocks| blocks.find(|b| &b.block.hash == hash))
                    .map_or(WindowResolution::UnknownHash, |b| {
                        WindowResolution::InWindow(b.block.clone())
                    });
            }
            BlockOrTimestamp::Block(BlockIdentifier::Latest(_)) => {
                return WindowResolution::InWindow(tip.block.clone());
            }
            BlockOrTimestamp::Block(BlockIdentifier::Number((_, number))) => {
                let Ok(number) = u64::try_from(*number) else {
                    return WindowResolution::BelowFloor;
                };
                BlockNumberOrTimestamp::Number(number)
            }
            BlockOrTimestamp::Timestamp(ts) => BlockNumberOrTimestamp::Timestamp(*ts),
        };
        if version.less_than(&oldest.block) {
            return WindowResolution::BelowFloor;
        }
        if version.greater_than(&tip.block) {
            return match version {
                BlockNumberOrTimestamp::Number(_) => WindowResolution::AboveTip,
                BlockNumberOrTimestamp::Timestamp(_) => {
                    WindowResolution::InWindow(tip.block.clone())
                }
            };
        }
        let block = self
            .blocks(None, None)
            .ok()
            .and_then(|mut blocks| blocks.find(|b| !version.greater_than(&b.block)))
            .map(|b| b.block.clone());
        match block {
            Some(block) => WindowResolution::InWindow(block),
            None => WindowResolution::AboveTip,
        }
    }
}

// `component_changes` and `account_changes` are kept parallel on purpose: one generic over the key,
// delta and balance types is harder to read than the two bodies. Every `changes_*` test runs both
// on the same blocks, so a fix to one that misses the other fails.

/// Every change in `blocks` to the components `ids`, in block order. Ids with no change are absent.
pub(crate) fn component_changes(
    blocks: &[Arc<BlockAggregatedChanges>],
    ids: &[&str],
) -> HashMap<String, Vec<ComponentChange>> {
    // The requested ids as a set. Each block is scanned by its own changed keys, which are
    // usually far fewer than a full page of ids, and each key is checked against this set.
    let requested: HashSet<&str> = ids.iter().copied().collect();

    let mut changes: HashMap<String, Vec<ComponentChange>> = HashMap::new();
    // Reused across blocks: the requested ids the current block touches. An id present in
    // several of the block's maps is collected once, so the block yields one change per id.
    let mut touched: HashSet<&str> = HashSet::new();
    for entry in blocks {
        // Collect the requested ids this block changed the state or balances of.
        touched.clear();
        touched.extend(
            entry
                .state_deltas
                .keys()
                .chain(entry.component_balances.keys())
                .map(String::as_str)
                .filter(|id| requested.contains(id)),
        );

        // Record one change per touched id, stamped with the block.
        let at = WriteTimestamp::from(&entry.block);
        for &id in &touched {
            let delta = entry.state_deltas.get(id).cloned();
            let balances = entry
                .component_balances
                .get(id)
                .cloned();
            changes
                .entry(id.to_string())
                .or_default()
                .push(ComponentChange { at, delta, balances });
        }
    }
    changes
}

/// Every change in `blocks` to the accounts `addresses`, in block order. Addresses with no change
/// are absent.
pub(crate) fn account_changes(
    blocks: &[Arc<BlockAggregatedChanges>],
    addresses: &[Bytes],
) -> HashMap<Bytes, Vec<AccountChange>> {
    // The requested addresses as a set. Each block is scanned by its own changed keys, which
    // are usually far fewer than a full page of addresses, and each key is checked against
    // this set.
    let requested: HashSet<&Bytes> = addresses.iter().collect();

    let mut changes: HashMap<Bytes, Vec<AccountChange>> = HashMap::new();
    // Reused across blocks: the requested addresses the current block touches. An address
    // present in both of the block's maps is collected once, so the block yields one change
    // per address.
    let mut touched: HashSet<&Bytes> = HashSet::new();
    for entry in blocks {
        // Collect the requested addresses this block changed the state or token balances of.
        touched.clear();
        touched.extend(
            entry
                .account_deltas
                .keys()
                .chain(entry.account_balances.keys())
                .filter(|address| requested.contains(address)),
        );

        // Record one change per touched address, stamped with the block.
        let at = WriteTimestamp::from(&entry.block);
        for &address in &touched {
            let delta = entry
                .account_deltas
                .get(address)
                .cloned();
            let balances = entry
                .account_balances
                .get(address)
                .cloned();
            changes
                .entry(address.clone())
                .or_default()
                .push(AccountChange { at, delta, balances });
        }
    }
    changes
}

#[cfg(test)]
mod test {
    use std::{ops::RangeInclusive, str::FromStr};

    use chrono::NaiveDateTime;
    use rstest::rstest;
    use tycho_common::models::{Chain, ChangeType};

    use super::*;
    use crate::{extractor::models::fixtures, testing};

    const EXTRACTOR: &str = "ex";

    fn msg(number: u64, finalized: u64, committed: Option<u64>) -> BlockAggregatedChanges {
        testing::aggregated_changes(EXTRACTOR, number, finalized, committed)
    }

    fn arc_msg(number: u64, timestamp: NaiveDateTime) -> BlockAggregatedChanges {
        let mut message = msg(number, 0, None);
        message.chain = Chain::Arc;
        message.block.chain = Chain::Arc;
        message.block.ts = timestamp;
        message
    }

    fn at(number: u64) -> WriteTimestamp {
        WriteTimestamp::from(&testing::block(number))
    }

    fn revert_msg(number: u64) -> BlockAggregatedChanges {
        BlockAggregatedChanges { revert: true, ..msg(number, 0, None) }
    }

    #[test]
    fn generation_moves_on_every_change() {
        let mut w =
            DeltaWindow::new(EXTRACTOR.to_string(), WindowConfig { depth: 1, min_fold_batch: 1 });
        let mut seen = vec![w.generation()];

        w.insert(&Arc::new(msg(1, 1, Some(1))))
            .unwrap();
        seen.push(w.generation());
        w.insert(&Arc::new(msg(2, 1, Some(1))))
            .unwrap();
        seen.push(w.generation());
        w.insert(&Arc::new(msg(3, 2, Some(2))))
            .unwrap();
        seen.push(w.generation());
        w.insert(&Arc::new(revert_msg(3)))
            .unwrap();
        seen.push(w.generation());
        w.clear();
        seen.push(w.generation());

        let mut distinct = seen.clone();
        distinct.dedup();
        assert_eq!(distinct, seen, "every change must move the generation: {seen:?}");
    }

    fn with_component_balance(mut m: BlockAggregatedChanges, id: &str) -> BlockAggregatedChanges {
        m.component_balances
            .insert(id.to_string(), HashMap::new());
        m
    }

    fn with_account_delta(
        mut m: BlockAggregatedChanges,
        address: &Bytes,
        x: u64,
    ) -> BlockAggregatedChanges {
        m.account_deltas.insert(
            address.clone(),
            AccountDelta::new(
                Chain::Ethereum,
                address.clone(),
                fixtures::optional_slots([(1, x)]),
                None,
                None,
                ChangeType::Update,
            ),
        );
        m
    }

    fn with_account_balance(
        mut m: BlockAggregatedChanges,
        address: &Bytes,
    ) -> BlockAggregatedChanges {
        m.account_balances
            .insert(address.clone(), HashMap::new());
        m
    }

    fn window(depth: u64, min_fold_batch: usize) -> DeltaWindow {
        DeltaWindow::new(EXTRACTOR.to_string(), WindowConfig { depth, min_fold_batch })
    }

    fn put(w: &mut DeltaWindow, m: BlockAggregatedChanges) -> Result<(), StorageError> {
        w.insert(&Arc::new(m))
    }

    fn fill(
        w: &mut DeltaWindow,
        range: RangeInclusive<u64>,
        finalized: u64,
        committed: Option<u64>,
    ) {
        for n in range {
            put(w, msg(n, finalized, committed)).unwrap();
        }
    }

    fn numbers<'a>(blocks: impl Iterator<Item = &'a BlockAggregatedChanges>) -> Vec<u64> {
        blocks.map(|b| b.block.number).collect()
    }

    fn buffered(w: &DeltaWindow) -> Vec<u64> {
        numbers(w.blocks(None, None).unwrap())
    }

    fn floor_number(w: &DeltaWindow) -> Option<u64> {
        w.floor().map(|b| b.number)
    }

    fn tip_number(w: &DeltaWindow) -> Option<u64> {
        w.tip().map(|b| b.number)
    }

    #[derive(Default)]
    struct RecordingSink {
        folded: std::sync::Mutex<Vec<u64>>,
        fail_at: Option<u64>,
    }

    impl RecordingSink {
        fn folded(&self) -> Vec<u64> {
            self.folded.lock().unwrap().clone()
        }
    }

    impl FoldSink for RecordingSink {
        fn fold(&self, block: &BlockAggregatedChanges) -> Result<(), StorageError> {
            let number = block.block.number;
            if self.fail_at == Some(number) {
                return Err(StorageError::Unexpected(format!("fold failed at {number}")));
            }
            self.folded.lock().unwrap().push(number);
            Ok(())
        }
    }

    #[test]
    fn insert_rejects_a_block_that_does_not_extend_the_chain() {
        let mut w = window(3, 1);
        put(&mut w, msg(1, 0, None)).unwrap();

        let res = put(&mut w, msg(3, 0, None));

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(tip_number(&w), Some(1));
    }

    #[test]
    fn changes_are_exactly_those_up_to_the_version_in_order() {
        let address = Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        let mut w = window(128, 1);
        for n in 1..=6u64 {
            let mut m = msg(n, 0, None);
            if n % 2 == 0 {
                m = testing::with_state_delta(m, "c1", n);
            }
            if n == 3 || n == 5 {
                m = with_account_delta(m, &address, n);
            }
            put(&mut w, m).unwrap();
        }

        let components = component_changes(&w.blocks_upto(5).unwrap(), &["c1", "absent"]);
        let accounts = account_changes(&w.blocks_upto(5).unwrap(), std::slice::from_ref(&address));

        let component_blocks: Vec<WriteTimestamp> = components["c1"]
            .iter()
            .map(|c| c.at)
            .collect();
        assert_eq!(component_blocks, vec![at(2), at(4)]);
        assert!(!components.contains_key("absent"));
        let account_blocks: Vec<WriteTimestamp> = accounts[&address]
            .iter()
            .map(|c| c.at)
            .collect();
        assert_eq!(account_blocks, vec![at(3), at(5)]);
        assert!(components["c1"]
            .iter()
            .all(|c| c.delta.is_some() && c.balances.is_none()));
    }

    #[test]
    fn arc_same_timestamp_blocks_keep_number_order_for_latest_snapshot() {
        let timestamp = "2020-01-01T00:00:00"
            .parse::<NaiveDateTime>()
            .unwrap();
        let mut w = window(128, 1);
        for number in 40..=42 {
            put(&mut w, testing::with_state_delta(arc_msg(number, timestamp), "c1", number))
                .unwrap();
        }

        let changes = component_changes(&w.blocks_upto(42).unwrap(), &["c1"]);
        let blocks = changes["c1"]
            .iter()
            .map(|change| {
                change
                    .delta
                    .as_ref()
                    .unwrap()
                    .updated_attributes["x"]
                    .clone()
            })
            .collect::<Vec<_>>();
        let latest = w.tip().unwrap();

        assert_eq!(
            blocks,
            [40u64, 41, 42]
                .map(Bytes::from)
                .to_vec()
        );
        assert_eq!(latest.number, 42);
        assert_eq!(latest.chain, Chain::Arc);
        assert_eq!(
            w.resolve(&BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Arc, 42)))),
            WindowResolution::InWindow(latest.clone())
        );
        assert_eq!(
            w.resolve(&BlockOrTimestamp::Timestamp(timestamp + chrono::Duration::seconds(1))),
            WindowResolution::InWindow(latest)
        );
    }

    #[test]
    fn changes_include_balance_only_changes() {
        let address = Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        let mut w = window(128, 1);
        put(&mut w, with_component_balance(msg(1, 0, None), "c1")).unwrap();
        put(&mut w, with_account_balance(msg(2, 0, None), &address)).unwrap();

        let components = component_changes(&w.blocks_upto(2).unwrap(), &["c1"]);
        let accounts = account_changes(&w.blocks_upto(2).unwrap(), std::slice::from_ref(&address));

        assert_eq!(
            components["c1"],
            vec![ComponentChange {
                at: WriteTimestamp::from(&testing::block(1)),
                delta: None,
                balances: Some(HashMap::new())
            }]
        );
        assert_eq!(
            accounts[&address],
            vec![AccountChange {
                at: WriteTimestamp::from(&testing::block(2)),
                delta: None,
                balances: Some(HashMap::new())
            }]
        );
    }

    #[test]
    fn changes_hold_one_change_per_key_and_block() {
        let address = Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        let m = with_account_balance(
            with_account_delta(
                testing::with_state_delta(with_component_balance(msg(1, 0, None), "c1"), "c1", 1),
                &address,
                1,
            ),
            &address,
        );
        let mut w = window(128, 1);
        put(&mut w, m).unwrap();

        let components = component_changes(&w.blocks_upto(1).unwrap(), &["c1"]);
        let accounts = account_changes(&w.blocks_upto(1).unwrap(), std::slice::from_ref(&address));

        let [component] = components["c1"].as_slice() else {
            panic!("expected one component change, got {:?}", components["c1"]);
        };
        assert!(component.delta.is_some() && component.balances.is_some());
        let [account] = accounts[&address].as_slice() else {
            panic!("expected one account change, got {:?}", accounts[&address]);
        };
        assert!(account.delta.is_some() && account.balances.is_some());
    }

    #[test]
    fn changes_below_the_floor_are_the_floor_block_alone() {
        let mut w = window(128, 1);
        for n in 5..=7u64 {
            put(&mut w, testing::with_state_delta(msg(n, 0, None), "c1", n)).unwrap();
        }

        let changes = component_changes(&w.blocks_upto(2).unwrap(), &["c1"]);

        let blocks: Vec<WriteTimestamp> = changes["c1"]
            .iter()
            .map(|c| c.at)
            .collect();
        assert_eq!(blocks, vec![at(5)]);
    }

    #[test]
    fn blocks_upto_on_an_empty_window_are_not_found() {
        let w = window(128, 1);

        assert!(matches!(w.blocks_upto(1), Err(StorageError::NotFound(..))));
    }

    #[test]
    fn uncommitted_blocks_start_above_the_commit_watermark() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=10, 10, Some(6));

        assert_eq!(numbers(w.uncommitted_blocks().unwrap()), vec![7, 8, 9, 10]);
    }

    #[test]
    fn uncommitted_blocks_are_all_blocks_before_the_first_commit() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=3, 3, None);

        assert_eq!(numbers(w.uncommitted_blocks().unwrap()), vec![1, 2, 3]);
    }

    #[test]
    fn uncommitted_blocks_are_empty_when_everything_is_committed() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=3, 3, Some(3));

        assert!(numbers(w.uncommitted_blocks().unwrap()).is_empty());
    }

    #[test]
    fn uncommitted_blocks_are_empty_on_an_empty_window() {
        assert!(numbers(
            window(128, 1)
                .uncommitted_blocks()
                .unwrap()
        )
        .is_empty());
    }

    #[test]
    fn fold_evictable_folds_in_order_then_evicts() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        let sink = RecordingSink::default();

        w.fold_evictable(&sink).unwrap();

        assert_eq!(sink.folded(), (1..=7).collect::<Vec<_>>());
        assert_eq!(buffered(&w), vec![8, 9, 10]);
    }

    #[rstest]
    #[case::depth_binds(10, Some(10), 7)]
    #[case::finalized_binds(5, Some(10), 5)]
    #[case::committed_binds(10, Some(4), 4)]
    #[case::no_commit_yet(10, None, 0)]
    fn fold_evictable_stops_at_the_smallest_bound(
        #[case] finalized: u64,
        #[case] committed: Option<u64>,
        #[case] folded_upto: u64,
    ) {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, finalized, committed);
        let sink = RecordingSink::default();

        w.fold_evictable(&sink).unwrap();

        assert_eq!(sink.folded(), (1..=folded_upto).collect::<Vec<_>>());
        assert_eq!(floor_number(&w), Some(folded_upto + 1));
    }

    #[test]
    fn nothing_is_folded_on_a_chain_shorter_than_the_depth() {
        let mut w = window(20, 1);
        fill(&mut w, 1..=5, 5, Some(5));
        let sink = RecordingSink::default();

        w.fold_evictable(&sink).unwrap();

        assert!(sink.folded().is_empty());
        assert_eq!(floor_number(&w), Some(1));
    }

    #[test]
    fn fold_evictable_on_an_empty_window_is_a_no_op() {
        let sink = RecordingSink::default();

        window(3, 1)
            .fold_evictable(&sink)
            .unwrap();

        assert!(sink.folded().is_empty());
    }

    #[test]
    fn fold_evictable_is_a_no_op_below_the_batch_size() {
        let mut w = window(3, 5);
        fill(&mut w, 1..=5, 5, Some(5));
        let sink = RecordingSink::default();

        w.fold_evictable(&sink).unwrap();

        assert!(sink.folded().is_empty());
        assert_eq!(floor_number(&w), Some(1));
    }

    #[test]
    fn a_fold_error_evicts_only_the_folded_prefix() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        let sink = RecordingSink { fail_at: Some(4), ..Default::default() };

        let res = w.fold_evictable(&sink);

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(sink.folded(), vec![1, 2, 3]);
        assert_eq!(floor_number(&w), Some(4));
    }

    // `metrics::with_local_recorder` takes a sync closure; the window is sync, so no runtime.
    #[test]
    fn fold_duration_is_recorded_per_extractor() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        let sink = RecordingSink::default();

        metrics::with_local_recorder(&recorder, || w.fold_evictable(&sink).unwrap());

        let recorded = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(key, _, _, value)| {
                key.key().name() == "delta_window_fold_duration_ms" &&
                    key.key()
                        .labels()
                        .any(|l| l.key() == "extractor" && l.value() == EXTRACTOR) &&
                    matches!(value, DebugValue::Histogram(samples) if samples.len() == 7)
            });
        assert!(recorded, "one histogram sample per folded block, labelled by extractor");
    }

    /// Gauge values recorded for `EXTRACTOR`, by metric name.
    fn extractor_gauges(
        snapshotter: &metrics_util::debugging::Snapshotter,
    ) -> HashMap<String, f64> {
        use metrics_util::debugging::DebugValue;

        let mut gauges = HashMap::new();
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let ours = key
                .key()
                .labels()
                .any(|l| l.key() == "extractor" && l.value() == EXTRACTOR);
            if let (true, DebugValue::Gauge(v)) = (ours, value) {
                gauges.insert(key.key().name().to_string(), v.0);
            }
        }
        gauges
    }

    #[test]
    fn report_metrics_publishes_the_block_count_and_tip() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        w.fold_evictable(&RecordingSink::default())
            .unwrap();

        metrics::with_local_recorder(&recorder, || w.report_metrics());

        // bound = min(finalized 10, committed 10, tip 10 - 3) = 7, so blocks 8..=10 stay
        let gauges = extractor_gauges(&snapshotter);
        assert_eq!(gauges.get("delta_window_blocks"), Some(&3.0));
        assert_eq!(gauges.get("delta_window_tip_block"), Some(&10.0));
    }

    #[test]
    fn report_metrics_on_an_empty_window_reports_zero_blocks_and_no_tip() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let w = window(3, 1);

        metrics::with_local_recorder(&recorder, || w.report_metrics());

        let gauges = extractor_gauges(&snapshotter);
        assert_eq!(gauges.get("delta_window_blocks"), Some(&0.0));
        assert_eq!(gauges.get("delta_window_tip_block"), None);
    }

    #[test]
    fn fold_committed_folds_every_committed_block_and_keeps_the_rest() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 8, Some(10));
        let sink = RecordingSink::default();

        w.fold_committed(&sink).unwrap();

        assert_eq!(sink.folded(), (1..=8).collect::<Vec<_>>());
        assert_eq!(buffered(&w), vec![9, 10]);
    }

    #[test]
    fn fold_committed_before_the_first_commit_folds_nothing() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 5, None);
        let sink = RecordingSink::default();

        w.fold_committed(&sink).unwrap();

        assert!(sink.folded().is_empty());
        assert_eq!(buffered(&w), (1..=5).collect::<Vec<_>>());
    }

    #[test]
    fn fold_committed_keeps_the_failing_block_and_everything_above_it() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 5, Some(5));
        let sink = RecordingSink { fail_at: Some(3), ..Default::default() };

        let res = w.fold_committed(&sink);

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(sink.folded(), vec![1, 2]);
        assert_eq!(buffered(&w), vec![3, 4, 5]);
    }

    #[test]
    fn clear_empties_the_window_and_accepts_any_next_block() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 5, Some(5));

        w.clear();

        assert!(buffered(&w).is_empty());
        assert_eq!(w.commit_status(BlockNumberOrTimestamp::Number(1)), None);
        put(&mut w, msg(9, 9, Some(9))).unwrap();
        assert_eq!(buffered(&w), vec![9]);
    }

    #[rstest]
    #[case::at_committed(BlockNumberOrTimestamp::Number(6), Some(CommitStatus::Committed))]
    #[case::above_committed(BlockNumberOrTimestamp::Number(7), Some(CommitStatus::Uncommitted))]
    #[case::at_tip(BlockNumberOrTimestamp::Number(10), Some(CommitStatus::Uncommitted))]
    #[case::above_tip(BlockNumberOrTimestamp::Number(11), Some(CommitStatus::Unseen))]
    #[case::ts_at_committed(
        BlockNumberOrTimestamp::Timestamp(testing::block(6).ts),
        Some(CommitStatus::Committed)
    )]
    #[case::ts_above_committed(
        BlockNumberOrTimestamp::Timestamp(testing::block(7).ts),
        Some(CommitStatus::Uncommitted)
    )]
    #[case::ts_after_tip(
        BlockNumberOrTimestamp::Timestamp(testing::block(10).ts + chrono::Duration::seconds(1)),
        Some(CommitStatus::Unseen)
    )]
    fn commit_status_follows_the_watermark(
        #[case] version: BlockNumberOrTimestamp,
        #[case] expected: Option<CommitStatus>,
    ) {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 8, Some(6));

        assert_eq!(w.commit_status(version), expected);
    }

    #[test]
    fn the_oldest_buffered_block_is_not_committed() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 0, None);

        assert_eq!(
            w.commit_status(BlockNumberOrTimestamp::Number(1)),
            Some(CommitStatus::Uncommitted)
        );
        assert_eq!(
            w.commit_status(BlockNumberOrTimestamp::Number(0)),
            Some(CommitStatus::Committed)
        );
    }

    #[test]
    fn a_committed_height_below_the_window_marks_only_below_floor_versions_committed() {
        let mut w = window(3, 1);
        fill(&mut w, 5..=10, 10, Some(3));

        assert_eq!(
            w.commit_status(BlockNumberOrTimestamp::Number(4)),
            Some(CommitStatus::Committed)
        );
        assert_eq!(
            w.commit_status(BlockNumberOrTimestamp::Number(5)),
            Some(CommitStatus::Uncommitted)
        );
    }

    #[test]
    fn commit_status_is_none_on_an_empty_window() {
        assert_eq!(window(3, 1).commit_status(BlockNumberOrTimestamp::Number(1)), None);
    }

    #[test]
    fn floor_is_the_oldest_buffered_block() {
        let mut w = window(3, 1);
        assert_eq!(w.floor(), None);
        fill(&mut w, 4..=6, 0, None);
        assert_eq!(w.floor(), Some(testing::block(4)));
    }

    fn number(n: i64) -> BlockOrTimestamp {
        BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, n)))
    }

    #[rstest]
    #[case::number_in_window(number(5), WindowResolution::InWindow(testing::block(5)))]
    #[case::number_below_floor(number(0), WindowResolution::BelowFloor)]
    #[case::negative_number(number(-1), WindowResolution::BelowFloor)]
    #[case::number_above_tip(number(11), WindowResolution::AboveTip)]
    #[case::hash_in_window(
        BlockOrTimestamp::Block(BlockIdentifier::Hash(testing::block(5).hash)),
        WindowResolution::InWindow(testing::block(5))
    )]
    #[case::hash_not_in_window(
        BlockOrTimestamp::Block(BlockIdentifier::Hash(testing::block(11).hash)),
        WindowResolution::UnknownHash
    )]
    #[case::latest(
        BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)),
        WindowResolution::InWindow(testing::block(10))
    )]
    #[case::timestamp_at_block(
        BlockOrTimestamp::Timestamp(testing::block(5).ts),
        WindowResolution::InWindow(testing::block(5))
    )]
    #[case::timestamp_rounds_up(
        BlockOrTimestamp::Timestamp(testing::block(5).ts + chrono::Duration::seconds(1)),
        WindowResolution::InWindow(testing::block(6))
    )]
    #[case::timestamp_after_tip_clamps(
        BlockOrTimestamp::Timestamp(testing::block(10).ts + chrono::Duration::hours(1)),
        WindowResolution::InWindow(testing::block(10))
    )]
    #[case::timestamp_before_floor(
        BlockOrTimestamp::Timestamp(testing::block(1).ts - chrono::Duration::seconds(1)),
        WindowResolution::BelowFloor
    )]
    fn resolve_maps_versions_onto_the_window(
        #[case] version: BlockOrTimestamp,
        #[case] expected: WindowResolution,
    ) {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(5));

        assert_eq!(w.resolve(&version), expected);
    }

    #[rstest]
    #[case::number(number(1))]
    #[case::latest(BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)))]
    fn resolve_on_an_empty_window_is_empty(#[case] version: BlockOrTimestamp) {
        assert_eq!(window(3, 1).resolve(&version), WindowResolution::Empty);
    }

    #[test]
    fn revert_above_the_irreversible_height_purges_the_abandoned_blocks() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 3, Some(3));

        put(&mut w, revert_msg(3)).unwrap();

        assert_eq!(tip_number(&w), Some(3));
        assert_eq!(
            w.commit_status(BlockNumberOrTimestamp::Number(3)),
            Some(CommitStatus::Committed)
        );
    }

    #[test]
    fn revert_below_the_irreversible_height_is_an_error() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 3, Some(3));

        let res = put(&mut w, revert_msg(2));

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(tip_number(&w), Some(5));
    }

    #[test]
    fn revert_uses_finalized_alone_before_the_first_commit() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 3, None);

        assert!(put(&mut w, revert_msg(2)).is_err());
        assert!(put(&mut w, revert_msg(3)).is_ok());
    }

    #[test]
    fn revert_to_an_unknown_hash_is_not_found() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 1, Some(1));
        let mut unknown = revert_msg(4);
        unknown.block.hash = Bytes::from(99u64).lpad(32, 0);

        let res = put(&mut w, unknown);

        assert!(matches!(res, Err(StorageError::NotFound(_, _))));
    }

    #[test]
    fn watermarks_only_rise() {
        let mut w = window(3, 1);
        put(&mut w, msg(1, 0, None)).unwrap();
        put(&mut w, msg(2, 2, Some(2))).unwrap();
        put(&mut w, msg(3, 0, None)).unwrap();

        // `finalized` stayed at 2: a revert below it is still rejected.
        assert!(put(&mut w, revert_msg(1)).is_err());
        // `db_committed` stayed at 2.
        assert_eq!(
            w.commit_status(BlockNumberOrTimestamp::Number(2)),
            Some(CommitStatus::Committed)
        );
    }

    fn hash(number: u64) -> Bytes {
        testing::block(number).hash
    }

    /// Hash of block `number` on fork `fork`. Fork 0 is the chain `testing::block` builds, so
    /// `fork` starts at 1.
    fn fork_hash(fork: u64, number: u64) -> Bytes {
        Bytes::from(fork * 1_000 + number).lpad(32, 0)
    }

    /// Block `number` on fork `fork`, a child of `parent`.
    fn fork_msg(fork: u64, number: u64, parent: Bytes) -> BlockAggregatedChanges {
        let mut m = msg(number, 0, None);
        m.block.hash = fork_hash(fork, number);
        m.block.parent_hash = parent;
        m
    }

    /// Inserts `m` and folds what became evictable, as the pump does after every message.
    fn pump(w: &mut DeltaWindow, m: BlockAggregatedChanges) {
        put(w, m).unwrap();
        w.fold_evictable(&DiscardSink).unwrap();
    }

    /// `(number, hash, revert)` of every message on a catch-up path, in order.
    fn steps(path: &[Arc<BlockAggregatedChanges>]) -> Vec<(u64, Bytes, bool)> {
        path.iter()
            .map(|m| (m.block.number, m.block.hash.clone(), m.revert))
            .collect()
    }

    fn block_step(number: u64, hash: Bytes) -> (u64, Bytes, bool) {
        (number, hash, false)
    }

    fn revert_step(number: u64, hash: Bytes) -> (u64, Bytes, bool) {
        (number, hash, true)
    }

    #[test]
    fn catch_up_from_a_canonical_block_is_the_blocks_after_it() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=5, 0, None);

        let path = w
            .catch_up_path(&hash(2), 2, &hash(4))
            .unwrap();

        assert_eq!(steps(&path), vec![block_step(3, hash(3)), block_step(4, hash(4))]);
    }

    #[test]
    fn catch_up_to_the_client_block_is_an_empty_path() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=5, 0, None);

        assert_eq!(w.catch_up_path(&hash(3), 3, &hash(3)), Ok(Vec::new()));
    }

    #[test]
    fn catch_up_from_an_orphaned_block_starts_with_its_revert() {
        // The client has 4; the chain reverts to 2 and grows 3', 4', 5' on fork 1.
        let mut w = window(128, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();
        put(&mut w, fork_msg(1, 4, fork_hash(1, 3))).unwrap();
        put(&mut w, fork_msg(1, 5, fork_hash(1, 4))).unwrap();

        let path = w
            .catch_up_path(&hash(4), 4, &fork_hash(1, 5))
            .unwrap();

        assert_eq!(
            steps(&path),
            vec![
                revert_step(2, hash(2)),
                block_step(3, fork_hash(1, 3)),
                block_step(4, fork_hash(1, 4)),
                block_step(5, fork_hash(1, 5)),
            ]
        );
    }

    #[test]
    fn catch_up_to_the_target_of_the_client_revert_is_the_revert_alone() {
        // The client has 4 and its new subscription starts with 3': it needs the revert to 2 alone,
        // also once the window holds 3'.
        let mut w = window(128, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();

        let path = w
            .catch_up_path(&hash(4), 4, &hash(2))
            .unwrap();

        assert_eq!(steps(&path), vec![revert_step(2, hash(2))]);
    }

    #[test]
    fn catch_up_follows_nested_reverts_to_the_canonical_chain() {
        // The client has 4. The chain reverts to 2 and grows 3', 4' on fork 1, then reverts to 1,
        // orphaning 2 as well, and grows 2'', 3'' on fork 2.
        let mut w = window(128, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();
        put(&mut w, fork_msg(1, 4, fork_hash(1, 3))).unwrap();
        put(&mut w, revert_msg(1)).unwrap();
        put(&mut w, fork_msg(2, 2, hash(1))).unwrap();
        put(&mut w, fork_msg(2, 3, fork_hash(2, 2))).unwrap();

        let path = w
            .catch_up_path(&hash(4), 4, &fork_hash(2, 3))
            .unwrap();

        assert_eq!(
            steps(&path),
            vec![
                revert_step(2, hash(2)),
                revert_step(1, hash(1)),
                block_step(2, fork_hash(2, 2)),
                block_step(3, fork_hash(2, 3)),
            ]
        );
    }

    #[test]
    fn catch_up_from_a_block_orphaned_twice_goes_through_the_newest_revert() {
        // The chain leaves 3, 4 for 3', returns to 3, 4 with the same hashes and grows 5, then
        // leaves them again for 2''. The two reverts to 2 restore different states, so each
        // carries a marker.
        let mut w = window(128, 1);
        fill(&mut w, 1..=4, 0, None);
        let first_revert_to_2 = Arc::new(testing::with_state_delta(revert_msg(2), "c1", 4));
        w.insert(&first_revert_to_2).unwrap();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();
        let second_revert_to_2 = Arc::new(testing::with_state_delta(revert_msg(2), "c1", 3));
        w.insert(&second_revert_to_2).unwrap();
        let readopted: Vec<_> = (3..=5)
            .map(|n| Arc::new(msg(n, 0, None)))
            .collect();
        for block in &readopted {
            w.insert(block).unwrap();
        }

        assert_eq!(w.catch_up_path(&hash(4), 4, &hash(5)), Ok(vec![readopted[2].clone()]));
        assert_eq!(
            w.catch_up_path(&fork_hash(1, 3), 3, &hash(5)),
            Ok([vec![second_revert_to_2], readopted].concat())
        );

        let revert_to_1 = Arc::new(revert_msg(1));
        w.insert(&revert_to_1).unwrap();
        let two_on_fork_2 = Arc::new(fork_msg(2, 2, hash(1)));
        w.insert(&two_on_fork_2).unwrap();

        // Block 4 was orphaned by the first revert to 2 and by the revert to 1. Going through
        // the older one would also end right, in one more step: [revert to 2, revert to 1, 2''].
        assert_eq!(
            w.catch_up_path(&hash(4), 4, &fork_hash(2, 2)),
            Ok(vec![revert_to_1, two_on_fork_2])
        );
    }

    #[test]
    fn catch_up_from_the_parent_of_the_oldest_block_starts_at_the_oldest_block() {
        // After a restart the window refills from the block above the last committed one, the
        // block a client may have stopped at.
        let mut w = window(128, 1);
        fill(&mut w, 5..=8, 0, None);

        let path = w
            .catch_up_path(&hash(4), 4, &hash(7))
            .unwrap();

        assert_eq!(
            steps(&path),
            vec![block_step(5, hash(5)), block_step(6, hash(6)), block_step(7, hash(7))]
        );
    }

    #[test]
    fn catch_up_from_an_unknown_hash_is_unavailable() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=5, 0, None);
        let unknown = Bytes::from(99u64).lpad(32, 0);

        assert_eq!(w.catch_up_path(&unknown, 3, &hash(5)), Err(CatchUpUnavailable::UnknownHash));
    }

    #[test]
    fn catch_up_from_below_the_floor_is_unavailable() {
        let mut w = window(128, 1);
        fill(&mut w, 5..=8, 0, None);

        assert_eq!(w.catch_up_path(&hash(3), 3, &hash(8)), Err(CatchUpUnavailable::BelowFloor));
        // A block 4 on another fork is at the height of the oldest block's parent, not below it.
        assert_eq!(
            w.catch_up_path(&fork_hash(1, 4), 4, &hash(8)),
            Err(CatchUpUnavailable::UnknownHash)
        );
    }

    #[test]
    fn catch_up_to_a_block_outside_the_window_is_unavailable() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=5, 0, None);

        assert_eq!(w.catch_up_path(&hash(2), 2, &hash(6)), Err(CatchUpUnavailable::ToNotInWindow));
    }

    #[test]
    fn catch_up_to_a_block_outside_the_window_is_checked_before_from() {
        let mut w = window(128, 1);
        fill(&mut w, 5..=8, 0, None);

        assert_eq!(w.catch_up_path(&hash(2), 2, &hash(9)), Err(CatchUpUnavailable::ToNotInWindow));
    }

    #[test]
    fn catch_up_to_an_ancestor_of_from_is_unavailable() {
        // The client saw a revert to 2 that the window has not applied yet.
        let mut w = window(128, 1);
        fill(&mut w, 1..=5, 0, None);

        assert_eq!(w.catch_up_path(&hash(4), 4, &hash(2)), Err(CatchUpUnavailable::ToNotAhead));
    }

    #[test]
    fn catch_up_through_a_revert_that_lands_past_to_is_unavailable() {
        // The client has 4. The window reverted to 2; the client saw a revert to 1 the window has
        // not applied yet.
        let mut w = window(128, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();

        assert_eq!(w.catch_up_path(&hash(4), 4, &hash(1)), Err(CatchUpUnavailable::ToNotAhead));
    }

    #[test]
    fn catch_up_on_an_empty_window_is_unavailable() {
        let w = window(128, 1);

        assert_eq!(w.catch_up_path(&hash(2), 2, &hash(3)), Err(CatchUpUnavailable::EmptyWindow));
    }

    #[rstest]
    #[case::target_held(1, None)]
    #[case::target_is_the_parent_of_the_oldest_block(2, None)]
    #[case::target_below_the_parent_of_the_oldest_block(3, Some(CatchUpUnavailable::UnknownHash))]
    fn catch_up_goes_through_a_revert_while_its_target_anchors_a_path(
        #[case] committed: u64,
        #[case] expected: Option<CatchUpUnavailable>,
    ) {
        // The client has 4. The chain reverts to 2 and grows 3'..=7' on fork 1, whose watermarks
        // evict every block up to `committed`.
        let mut w = window(2, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        let mut parent = hash(2);
        for n in 3..=7 {
            let mut m = fork_msg(1, n, parent);
            m.finalized_block_height = committed;
            m.db_committed_block_height = Some(committed);
            parent = m.block.hash.clone();
            pump(&mut w, m);
        }
        assert_eq!(floor_number(&w), Some(committed + 1));

        let path = w.catch_up_path(&hash(4), 4, &fork_hash(1, 6));

        let expected = match expected {
            Some(reason) => Err(reason),
            None => Ok(std::iter::once(revert_step(2, hash(2)))
                .chain((3..=6).map(|n| block_step(n, fork_hash(1, n))))
                .collect()),
        };
        assert_eq!(path.map(|path| steps(&path)), expected);
    }

    #[test]
    fn eviction_forgets_a_revert_once_its_target_is_below_the_parent_of_the_oldest_block() {
        let mut w = window(2, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        assert_eq!(w.journal.targets(), vec![2]);

        // Blocks 3'..=5' carry watermarks that evict one more block each:
        // (block, finalized and committed height, oldest block left, reverts still journaled).
        let mut parent = hash(2);
        for (n, watermark, oldest, expected) in
            [(3, 1, 2, vec![2]), (4, 2, 3, vec![2]), (5, 3, 4, vec![])]
        {
            let mut m = fork_msg(1, n, parent);
            m.finalized_block_height = watermark;
            m.db_committed_block_height = Some(watermark);
            parent = m.block.hash.clone();

            pump(&mut w, m);

            assert_eq!(floor_number(&w), Some(oldest), "after block {n}'");
            assert_eq!(w.journal.targets(), expected, "after block {n}'");
        }
    }

    #[test]
    fn a_revert_whose_target_was_evicted_is_forgotten() {
        // The client has 10. The chain reverts to 2 and grows 3'..=12' on fork 1; committing them
        // evicts every block up to 10', the revert target among them. 10 is not below the floor,
        // so only the journal could have led back from it.
        let mut w = window(2, 1);
        fill(&mut w, 1..=10, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        let mut parent = hash(2);
        for n in 3..=12 {
            let mut m = fork_msg(1, n, parent);
            m.finalized_block_height = n - 1;
            m.db_committed_block_height = Some(n - 1);
            parent = m.block.hash.clone();
            pump(&mut w, m);
        }

        assert_eq!(
            w.catch_up_path(&hash(10), 10, &fork_hash(1, 12)),
            Err(CatchUpUnavailable::UnknownHash)
        );
    }

    #[test]
    fn clear_forgets_the_journaled_reverts() {
        // Had the journal survived the clear, its revert to 2 would lead from 4 to the parent of
        // the refilled window's oldest block.
        let mut w = window(128, 1);
        fill(&mut w, 1..=4, 0, None);
        put(&mut w, revert_msg(2)).unwrap();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();

        w.clear();
        put(&mut w, fork_msg(1, 3, hash(2))).unwrap();
        put(&mut w, fork_msg(1, 4, fork_hash(1, 3))).unwrap();

        assert_eq!(
            w.catch_up_path(&hash(4), 4, &fork_hash(1, 4)),
            Err(CatchUpUnavailable::UnknownHash)
        );
    }
}
