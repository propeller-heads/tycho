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
//! `W` is a floor on retention, not a target. `PendingDeltas` receives `db_committed` in jumps of
//! `--database-insert-batch-size`, so on chains where that batch exceeds `W` (Arbitrum at 1000,
//! BSC and Polygon at 512 today) the `db_committed` term always binds and `W` never does.
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
//! An error from [`DeltaWindow::insert`] or [`DeltaWindow::fold_and_evict`] means this window
//! no longer matches the chain or the database and cannot be repaired in place. The caller
//! resets the extractor's window, the same recovery the supervisor triggers through
//! `ExtractorRestarted`, and keeps serving the other extractors.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use deepsize::DeepSizeOf;
use metrics::histogram;
use tracing::{trace, warn};
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges},
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Address,
    },
    storage::StorageError,
    Bytes,
};

use crate::extractor::reorg_buffer::{BlockNumberOrTimestamp, CommitStatus, ReorgBuffer};

/// Receives blocks evicted from a [`DeltaWindow`].
pub(crate) trait FoldSink: Send + Sync {
    /// Merges one finalized, database-committed block into the long-lived store.
    ///
    /// Blocks arrive in ascending order. Delta values are absolute, so applying the same block
    /// twice must be a no-op for implementations that tag values with their block. An error
    /// means the block was not fully applied.
    fn fold(&self, block: &BlockAggregatedChanges) -> Result<(), StorageError>;
}

/// Retention settings shared by every extractor's [`DeltaWindow`].
#[derive(Clone, Copy, Debug, DeepSizeOf)]
pub struct WindowConfig {
    /// Target retention depth `W` in blocks.
    pub depth: u64,
    /// Evictable blocks required before a fold runs.
    pub min_fold_batch: usize,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { depth: 128, min_fold_batch: 1 }
    }
}

/// Drops every folded block. Stands in for the entity cache until ENG-6291 lands.
pub(crate) struct DiscardSink;

impl FoldSink for DiscardSink {
    fn fold(&self, _block: &BlockAggregatedChanges) -> Result<(), StorageError> {
        Ok(())
    }
}

/// Outcome of resolving a requested version against the window contents.
#[allow(dead_code)] // consumed by the state service, ENG-6293
#[derive(Debug, PartialEq)]
pub(crate) enum WindowResolution {
    /// The version maps to a block currently held in the window.
    InWindow(Block),
    /// The version is older than the window floor; the database fallback path serves it.
    BelowFloor,
    /// The version is newer than the newest block this window has seen.
    AboveTip,
}

/// One block's changes to a component, as captured from the window.
#[allow(dead_code)] // consumed by the state service, ENG-6293
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComponentChange {
    pub block: u64,
    pub state: Option<ProtocolComponentStateDelta>,
    pub balances: Option<HashMap<Bytes, ComponentBalance>>,
}

/// One block's changes to an account, as captured from the window.
#[allow(dead_code)] // consumed by the state service, ENG-6293
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccountChange {
    pub block: u64,
    pub delta: Option<AccountDelta>,
    pub token_balances: Option<HashMap<Address, AccountBalance>>,
}

/// Window changes for a set of keys up to a version, ascending by block within each key.
#[allow(dead_code)] // consumed by the state service, ENG-6293
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct WindowPatch {
    pub components: HashMap<String, Vec<ComponentChange>>,
    pub accounts: HashMap<Bytes, Vec<AccountChange>>,
}

/// Folds run under the facade lock every reader contends on; anything slower than this is
/// logged. The entity cache design expects well under a millisecond.
const SLOW_FOLD: Duration = Duration::from_millis(5);

/// Fixed-depth window of block deltas for a single extractor.
///
/// Wraps the extractor's [`ReorgBuffer`] and owns the retention decision: today the buffer drains
/// as soon as the database commits, here blocks are kept until they are finalized, committed, and
/// deeper than the configured depth.
///
/// The type is `Sync` by composition but not internally synchronized: one instance lives behind
/// the per-extractor `Arc<Mutex<..>>` owned by the pending-deltas facade, and every method relies
/// on that lock for exclusive access.
///
/// The wrapped buffer is the RPC-side instance. It never calls
/// `ReorgBuffer::drain_into_committing`, so its committing section stays empty and this window's
/// retention is the only retention on it. Blocks leave only through
/// [`DeltaWindow::fold_and_evict`].
///
/// Because committed blocks are retained, window contents and database rows overlap by up to
/// `depth` blocks. Readers that merge window deltas with database queries and assume the two are
/// disjoint (e.g. the new-components listing, which concatenates and counts both sides) must
/// bound window reads below by `db_committed + 1`, not by [`DeltaWindow::floor`].
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
}

impl DeltaWindow {
    /// Creates an empty window.
    pub(crate) fn new(extractor: String, config: WindowConfig) -> Self {
        Self { extractor, buffer: ReorgBuffer::new(), config, db_committed: None, finalized: None }
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
    /// Every error leaves the window unusable (see the module doc):
    ///
    /// - `StorageError::Unexpected` when a regular message does not extend the buffered chain
    ///   (parent-hash mismatch), or when a revert would remove a block at or below `min(finalized,
    ///   db_committed)` — the database then holds rows from the abandoned branch, and persisted
    ///   state is never rolled back.
    /// - `StorageError::NotFound` when a revert targets a hash that is not buffered.
    pub(crate) fn insert(
        &mut self,
        message: &Arc<BlockAggregatedChanges>,
    ) -> Result<(), StorageError> {
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

    /// Purges every block after the revert target. Errors when the purge would remove a block at
    /// or below `min(finalized, db_committed)`: the database may hold rows from the abandoned
    /// branch, and persisted state is never rolled back. Watermarks stay as they are.
    fn revert_to(&mut self, message: &BlockAggregatedChanges) -> Result<(), StorageError> {
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
        self.buffer
            .purge(message.block.hash.clone())?;
        Ok(())
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
    /// Any error from [`FoldSink::fold`] is propagated after evicting the successfully
    /// folded prefix. A fold error leaves the failing block buffered for the caller to decide on
    /// (see the module doc).
    pub(crate) fn fold_and_evict(&mut self, sink: &dyn FoldSink) -> Result<(), StorageError> {
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
        self.fold_oldest(evictable, sink)
    }

    /// Folds every finalized, committed block into `sink`, then empties the window.
    ///
    /// Depth and fold batching do not apply: everything at or below `min(finalized,
    /// db_committed)` is handed to the sink, so the store behind it misses nothing the window
    /// held. Uncommitted blocks are dropped; the extractor re-sends them.
    ///
    /// # Errors
    ///
    /// The first [`FoldSink::fold`] error is returned after the window is emptied; the
    /// blocks after the failing one were not folded.
    pub(crate) fn reset(&mut self, sink: &dyn FoldSink) -> Result<(), StorageError> {
        let outcome = match (self.finalized, self.db_committed) {
            (Some(finalized), Some(committed)) => {
                let count = self
                    .buffer
                    .count_blocks_before(finalized.min(committed) + 1);
                self.fold_oldest(count, sink)
            }
            _ => Ok(()),
        };
        self.buffer = ReorgBuffer::new();
        self.db_committed = None;
        self.finalized = None;
        outcome
    }

    /// Folds the `count` oldest blocks into `sink` and evicts the folded prefix.
    fn fold_oldest(&mut self, count: usize, sink: &dyn FoldSink) -> Result<(), StorageError> {
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
            if elapsed > SLOW_FOLD {
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
        }
        outcome
    }

    /// The oldest block number still held in the window, if any.
    #[allow(dead_code)] // consumed by the state service, ENG-6293
    pub(crate) fn floor(&self) -> Option<u64> {
        self.buffer
            .oldest()
            .map(|m| m.block.number)
    }

    /// Collects every buffered change for the given keys up to `upto`, ascending by block. This
    /// is the filtering the facade applies in place, returned as data so a caller can lay it
    /// over a cache entry. The caller resolves `upto` with [`DeltaWindow::resolve`] first; a
    /// version below the floor never reaches here.
    #[allow(dead_code)] // consumed by the state service, ENG-6293
    pub(crate) fn capture_patch(
        &self,
        components: &[&str],
        accounts: &[Bytes],
        upto: Option<BlockNumberOrTimestamp>,
    ) -> Result<WindowPatch, StorageError> {
        let mut patch = WindowPatch::default();
        for entry in self
            .buffer
            .get_block_range(None, upto)?
        {
            let block = entry.block.number;
            for id in components {
                let state = entry.state_deltas.get(*id).cloned();
                let balances = entry
                    .component_balances
                    .get(*id)
                    .cloned();
                if state.is_some() || balances.is_some() {
                    patch
                        .components
                        .entry(id.to_string())
                        .or_default()
                        .push(ComponentChange { block, state, balances });
                }
            }
            for address in accounts {
                let delta = entry
                    .account_deltas
                    .get(address)
                    .cloned();
                let token_balances = entry
                    .account_balances
                    .get(address)
                    .cloned();
                if delta.is_some() || token_balances.is_some() {
                    patch
                        .accounts
                        .entry(address.clone())
                        .or_default()
                        .push(AccountChange { block, delta, token_balances });
                }
            }
        }
        Ok(patch)
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

    /// The newest block seen by this window, if any.
    #[allow(dead_code)] // consumed by the state service, ENG-6293
    pub(crate) fn tip(&self) -> Option<Block> {
        self.buffer
            .newest()
            .map(|m| m.block.clone())
    }

    /// Resolves a requested version to a servable window block.
    ///
    /// The default request ("now", a timestamp newer than the tip) clamps to the tip. A
    /// timestamp between two buffered blocks rounds up to the first block whose timestamp is not
    /// older than the request, matching the buffer's range lookup today. Versions below the
    /// floor report [`WindowResolution::BelowFloor`] and are served by the database fallback
    /// path; versions above the tip report [`WindowResolution::AboveTip`]. No database lookup is
    /// involved.
    #[allow(dead_code)] // consumed by the state service, ENG-6293
    pub(crate) fn resolve(&self, version: BlockNumberOrTimestamp) -> WindowResolution {
        let (Some(oldest), Some(tip)) = (self.buffer.oldest(), self.buffer.newest()) else {
            return WindowResolution::BelowFloor;
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
            .buffer
            .get_block_range(None, None)
            .ok()
            .and_then(|mut blocks| blocks.find(|b| !version.greater_than(&b.block)))
            .map(|b| b.block.clone());
        match block {
            Some(block) => WindowResolution::InWindow(block),
            None => WindowResolution::AboveTip,
        }
    }

    /// Commit status of `version` derived from the database-commit watermark.
    ///
    /// Deliberately not `ReorgBuffer::get_commit_status`: that reports any version at or below
    /// the oldest buffered block as `Committed`, which is off by one today (the oldest buffered
    /// block is `db_committed + 1`) and would be off by the whole window depth once committed
    /// blocks are retained.
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
    /// no database commit has been observed yet; the caller logs both watermarks and the tip so
    /// the two cases are distinguishable.
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
}

#[cfg(test)]
mod test {
    use std::{collections::HashSet, ops::RangeInclusive, str::FromStr};

    use rstest::rstest;
    use tycho_common::models::{Chain, ChangeType};

    use super::*;
    use crate::{extractor::models::fixtures, testing};

    const EXTRACTOR: &str = "ex";

    fn msg(number: u64, finalized: u64, committed: Option<u64>) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            extractor: EXTRACTOR.to_string(),
            block: testing::block(number),
            finalized_block_height: finalized,
            db_committed_block_height: committed,
            ..Default::default()
        }
    }

    fn revert_to(number: u64) -> BlockAggregatedChanges {
        BlockAggregatedChanges { revert: true, ..msg(number, 0, None) }
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

    #[test]
    fn insert_rejects_a_block_that_does_not_extend_the_chain() {
        let mut w = window(3, 1);
        put(&mut w, msg(1, 0, None)).unwrap();

        let res = put(&mut w, msg(3, 0, None));

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(w.tip().map(|b| b.number), Some(1));
    }

    fn with_component_delta(
        mut m: BlockAggregatedChanges,
        id: &str,
        x: u64,
    ) -> BlockAggregatedChanges {
        m.state_deltas.insert(
            id.to_string(),
            ProtocolComponentStateDelta {
                component_id: id.to_string(),
                updated_attributes: HashMap::from([("x".to_string(), Bytes::from(x))]),
                deleted_attributes: HashSet::new(),
                ..Default::default()
            },
        );
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

    #[test]
    fn capture_patch_returns_exactly_the_changes_up_to_the_version_in_order() {
        let address = Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        let mut w = window(128, 1);
        for n in 1..=6u64 {
            let mut m = msg(n, 0, None);
            if n % 2 == 0 {
                m = with_component_delta(m, "c1", n);
            }
            if n == 3 || n == 5 {
                m = with_account_delta(m, &address, n);
            }
            put(&mut w, m).unwrap();
        }

        let patch = w
            .capture_patch(
                &["c1", "absent"],
                std::slice::from_ref(&address),
                Some(BlockNumberOrTimestamp::Number(5)),
            )
            .unwrap();

        let component_blocks: Vec<u64> = patch.components["c1"]
            .iter()
            .map(|c| c.block)
            .collect();
        assert_eq!(component_blocks, vec![2, 4]);
        assert!(!patch.components.contains_key("absent"));
        let account_blocks: Vec<u64> = patch.accounts[&address]
            .iter()
            .map(|c| c.block)
            .collect();
        assert_eq!(account_blocks, vec![3, 5]);
        assert!(patch.components["c1"]
            .iter()
            .all(|c| c.state.is_some() && c.balances.is_none()));
    }

    #[test]
    fn capture_patch_on_an_empty_window_is_empty() {
        let patch = window(128, 1)
            .capture_patch(&["c1"], &[], None)
            .unwrap();
        assert!(patch.components.is_empty() && patch.accounts.is_empty());
    }

    fn numbers<'a>(blocks: impl Iterator<Item = &'a BlockAggregatedChanges>) -> Vec<u64> {
        blocks.map(|b| b.block.number).collect()
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
    fn blocks_delegates_to_the_buffer_range() {
        let mut w = window(128, 1);
        fill(&mut w, 1..=10, 10, Some(6));

        let upto_five = w
            .blocks(None, Some(BlockNumberOrTimestamp::Number(5)))
            .unwrap();
        assert_eq!(numbers(upto_five), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn window_config_defaults_match_the_epic() {
        let config = WindowConfig::default();
        assert_eq!((config.depth, config.min_fold_batch), (128, 1));
    }

    #[derive(Default)]
    struct RecordingSink {
        folded: std::sync::Mutex<Vec<u64>>,
        fail_at: Option<u64>,
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

    fn buffered(w: &DeltaWindow) -> Vec<u64> {
        w.buffer
            .get_block_range(None, None)
            .unwrap()
            .map(|b| b.block.number)
            .collect()
    }

    #[test]
    fn fold_and_evict_folds_in_order_then_evicts() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        let sink = RecordingSink::default();

        w.fold_and_evict(&sink).unwrap();

        assert_eq!(*sink.folded.lock().unwrap(), (1..=7).collect::<Vec<_>>());
        assert_eq!(buffered(&w), vec![8, 9, 10]);
    }

    #[test]
    fn nothing_above_the_bound_is_folded() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 5, Some(10));
        let sink = RecordingSink::default();

        w.fold_and_evict(&sink).unwrap();

        assert_eq!(*sink.folded.lock().unwrap(), (1..=5).collect::<Vec<_>>());
        assert_eq!(w.floor(), Some(6));
    }

    #[test]
    fn fold_is_a_no_op_below_the_batch_size() {
        let mut w = window(3, 5);
        fill(&mut w, 1..=5, 5, Some(5));
        let sink = RecordingSink::default();

        w.fold_and_evict(&sink).unwrap();

        assert!(sink.folded.lock().unwrap().is_empty());
        assert_eq!(w.floor(), Some(1));
    }

    #[test]
    fn fold_is_a_no_op_before_the_first_commit() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, None);
        let sink = RecordingSink::default();

        w.fold_and_evict(&sink).unwrap();

        assert!(sink.folded.lock().unwrap().is_empty());
        assert_eq!(w.floor(), Some(1));
    }

    #[test]
    fn a_fold_error_evicts_only_the_folded_prefix() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        let sink = RecordingSink { fail_at: Some(4), ..Default::default() };

        let res = w.fold_and_evict(&sink);

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(*sink.folded.lock().unwrap(), vec![1, 2, 3]);
        assert_eq!(w.floor(), Some(4));
    }

    #[test]
    fn no_block_is_lost_between_window_and_sink() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));
        let sink = RecordingSink::default();

        w.fold_and_evict(&sink).unwrap();

        let mut all = sink.folded.lock().unwrap().clone();
        all.extend(buffered(&w));
        assert_eq!(all, (1..=10).collect::<Vec<_>>());
    }

    #[test]
    fn reset_folds_committed_blocks_beyond_the_depth_then_empties_the_window() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 8, Some(10));
        let sink = RecordingSink::default();

        w.reset(&sink).unwrap();

        assert_eq!(*sink.folded.lock().unwrap(), (1..=8).collect::<Vec<_>>());
        assert!(buffered(&w).is_empty());
        assert_eq!(w.commit_status(BlockNumberOrTimestamp::Number(1)), None);
    }

    #[test]
    fn reset_before_the_first_commit_folds_nothing() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 5, None);
        let sink = RecordingSink::default();

        w.reset(&sink).unwrap();

        assert!(sink.folded.lock().unwrap().is_empty());
        assert!(buffered(&w).is_empty());
    }

    #[test]
    fn reset_empties_the_window_even_when_a_fold_fails() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 5, Some(5));
        let sink = RecordingSink { fail_at: Some(3), ..Default::default() };

        let res = w.reset(&sink);

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(*sink.folded.lock().unwrap(), vec![1, 2]);
        assert!(buffered(&w).is_empty());
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

        metrics::with_local_recorder(&recorder, || w.fold_and_evict(&sink).unwrap());

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

    #[test]
    fn discard_sink_accepts_everything() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(10));

        w.fold_and_evict(&DiscardSink).unwrap();

        assert_eq!(w.floor(), Some(8));
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
        assert_eq!(w.floor(), Some(4));
    }

    #[rstest]
    #[case::number_in_window(
        BlockNumberOrTimestamp::Number(5),
        WindowResolution::InWindow(testing::block(5))
    )]
    #[case::number_below_floor(BlockNumberOrTimestamp::Number(0), WindowResolution::BelowFloor)]
    #[case::number_above_tip(BlockNumberOrTimestamp::Number(11), WindowResolution::AboveTip)]
    #[case::timestamp_rounds_up(
        BlockNumberOrTimestamp::Timestamp(testing::block(5).ts + chrono::Duration::seconds(1)),
        WindowResolution::InWindow(testing::block(6))
    )]
    #[case::timestamp_after_tip_clamps(
        BlockNumberOrTimestamp::Timestamp(testing::block(10).ts + chrono::Duration::hours(1)),
        WindowResolution::InWindow(testing::block(10))
    )]
    #[case::timestamp_before_floor(
        BlockNumberOrTimestamp::Timestamp(testing::block(1).ts - chrono::Duration::seconds(1)),
        WindowResolution::BelowFloor
    )]
    fn resolve_maps_versions_onto_the_window(
        #[case] version: BlockNumberOrTimestamp,
        #[case] expected: WindowResolution,
    ) {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, 10, Some(5));

        assert_eq!(w.resolve(version), expected);
    }

    #[test]
    fn resolve_on_an_empty_window_is_below_floor() {
        assert_eq!(
            window(3, 1).resolve(BlockNumberOrTimestamp::Number(1)),
            WindowResolution::BelowFloor
        );
    }

    #[test]
    fn revert_above_the_irreversible_height_purges_the_abandoned_blocks() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 3, Some(3));

        put(&mut w, revert_to(3)).unwrap();

        assert_eq!(w.tip().map(|b| b.number), Some(3));
        assert_eq!(w.finalized, Some(3));
        assert_eq!(w.db_committed, Some(3));
    }

    #[test]
    fn revert_below_the_irreversible_height_is_an_error() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 3, Some(3));

        let res = put(&mut w, revert_to(2));

        assert!(matches!(res, Err(StorageError::Unexpected(_))));
        assert_eq!(w.tip().map(|b| b.number), Some(5));
    }

    #[test]
    fn revert_uses_finalized_alone_before_the_first_commit() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 3, None);

        assert!(put(&mut w, revert_to(2)).is_err());
        assert!(put(&mut w, revert_to(3)).is_ok());
    }

    #[test]
    fn revert_to_an_unknown_hash_is_not_found() {
        let mut w = window(3, 1);
        fill(&mut w, 1..=5, 1, Some(1));
        let mut unknown = revert_to(4);
        unknown.block.hash = Bytes::from(99u64).lpad(32, 0);

        let res = put(&mut w, unknown);

        assert!(matches!(res, Err(StorageError::NotFound(_, _))));
    }

    #[rstest]
    #[case::depth_binds(10, Some(10), Some(7))]
    #[case::finalized_binds(5, Some(10), Some(5))]
    #[case::committed_binds(10, Some(4), Some(4))]
    #[case::no_commit_yet(10, None, None)]
    fn eviction_bound_is_the_smallest_term(
        #[case] finalized: u64,
        #[case] committed: Option<u64>,
        #[case] expected: Option<u64>,
    ) {
        let mut w = window(3, 1);
        fill(&mut w, 1..=10, finalized, committed);

        assert_eq!(w.eviction_bound(), expected);
    }

    #[test]
    fn eviction_bound_saturates_on_a_chain_shorter_than_the_depth() {
        let mut w = window(20, 1);
        fill(&mut w, 1..=5, 5, Some(5));

        assert_eq!(w.eviction_bound(), Some(0));
    }

    #[test]
    fn eviction_bound_is_none_on_an_empty_window() {
        assert_eq!(window(3, 1).eviction_bound(), None);
    }

    #[test]
    fn watermarks_only_rise() {
        let mut w = window(3, 1);
        put(&mut w, msg(1, 0, None)).unwrap();
        put(&mut w, msg(2, 1, Some(0))).unwrap();
        put(&mut w, msg(3, 0, None)).unwrap();

        assert_eq!(w.finalized, Some(1));
        assert_eq!(w.db_committed, Some(0));
    }
}
