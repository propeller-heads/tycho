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

/// Drops every folded block.
// Placeholder until the entity cache (ENG-6291) provides the real sink.
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
    /// The version is newer than the newest block this window has seen.
    AboveTip,
}

/// One block's changes to a component, as captured from the window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComponentChange {
    /// Block number the change belongs to.
    pub block: u64,
    /// State delta of the block, if the block changed the component's state.
    pub delta: Option<ProtocolComponentStateDelta>,
    /// Token balances of the block, if the block changed the component's balances.
    pub balances: Option<HashMap<Bytes, ComponentBalance>>,
}

/// One block's changes to an account, as captured from the window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccountChange {
    /// Block number the change belongs to.
    pub block: u64,
    /// Account delta of the block, if the block changed the account.
    pub delta: Option<AccountDelta>,
    /// Token balances of the block, if the block changed the account's balances.
    pub balances: Option<HashMap<Address, AccountBalance>>,
}

/// Window changes for a set of keys up to a version, ascending by block within each key.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct WindowPatch {
    /// Changes per component id.
    pub components: HashMap<String, Vec<ComponentChange>>,
    /// Changes per account address.
    pub accounts: HashMap<Bytes, Vec<AccountChange>>,
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
}

impl DeltaWindow {
    /// Creates an empty window.
    pub(crate) fn new(extractor: String, config: WindowConfig) -> Self {
        Self { extractor, buffer: ReorgBuffer::new(), config, db_committed: None, finalized: None }
    }

    /// Applies one full-block message to the window.
    ///
    /// Regular messages must extend the buffered chain; revert messages purge the abandoned
    /// blocks. No folding or eviction happens here — see [`DeltaWindow::fold_evictable`]. The
    /// caller filters partial-block messages.
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

    /// Empties the window and forgets both watermarks. The configuration stays. The next
    /// inserted block starts a new chain, whatever its parent.
    pub(crate) fn clear(&mut self) {
        self.buffer = ReorgBuffer::new();
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
}

// TODO: merge impl blocks back together once code is consumed and no longer dead
#[allow(dead_code)] // consumed by the state service, ENG-6293
impl DeltaWindow {
    /// The oldest block still held in the window, if any.
    pub(crate) fn floor(&self) -> Option<Block> {
        self.buffer
            .oldest()
            .map(|m| m.block.clone())
    }

    /// The newest block held in the window, if any.
    pub(crate) fn tip(&self) -> Option<Block> {
        self.buffer
            .newest()
            .map(|m| m.block.clone())
    }

    /// Every buffered change to `components` and `accounts` up to `upto`, ascending by block.
    /// Keys with no change are absent. An `upto` below the floor yields the floor block alone;
    /// resolve it first with [`DeltaWindow::resolve`] if that matters.
    pub(crate) fn capture_patch(
        &self,
        components: &[&str],
        accounts: &[Bytes],
        upto: Option<BlockNumberOrTimestamp>,
    ) -> Result<WindowPatch, StorageError> {
        let mut patch = WindowPatch::default();
        for entry in self.blocks(None, upto)? {
            let block = entry.block.number;
            for id in components {
                let delta = entry.state_deltas.get(*id).cloned();
                let balances = entry
                    .component_balances
                    .get(*id)
                    .cloned();
                if delta.is_some() || balances.is_some() {
                    patch
                        .components
                        .entry(id.to_string())
                        .or_default()
                        .push(ComponentChange { block, delta, balances });
                }
            }
            for address in accounts {
                let delta = entry
                    .account_deltas
                    .get(address)
                    .cloned();
                let balances = entry
                    .account_balances
                    .get(address)
                    .cloned();
                if delta.is_some() || balances.is_some() {
                    patch
                        .accounts
                        .entry(address.clone())
                        .or_default()
                        .push(AccountChange { block, delta, balances });
                }
            }
        }
        Ok(patch)
    }

    /// Resolves a requested version to a servable window block.
    ///
    /// A timestamp newer than the tip clamps to the tip. A timestamp between two buffered blocks
    /// rounds up to the first block whose timestamp is not older than the request, like
    /// [`ReorgBuffer::get_block_range`]. Versions below the floor report
    /// [`WindowResolution::BelowFloor`]; block numbers above the tip report
    /// [`WindowResolution::AboveTip`].
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

    fn revert_msg(number: u64) -> BlockAggregatedChanges {
        BlockAggregatedChanges { revert: true, ..msg(number, 0, None) }
    }

    fn with_component_delta(
        mut m: BlockAggregatedChanges,
        id: &str,
        x: u64,
    ) -> BlockAggregatedChanges {
        m.state_deltas
            .insert(id.to_string(), testing::state_delta(id, x));
        m
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
            .all(|c| c.delta.is_some() && c.balances.is_none()));
    }

    #[test]
    fn arc_same_timestamp_blocks_keep_number_order_for_latest_snapshot() {
        let timestamp = "2020-01-01T00:00:00"
            .parse::<NaiveDateTime>()
            .unwrap();
        let mut w = window(128, 1);
        for number in 40..=42 {
            put(&mut w, with_component_delta(arc_msg(number, timestamp), "c1", number)).unwrap();
        }

        let patch = w
            .capture_patch(&["c1"], &[], None)
            .unwrap();
        let blocks = patch.components["c1"]
            .iter()
            .map(|change| change.block)
            .collect::<Vec<_>>();
        let latest = w.tip().unwrap();

        assert_eq!(blocks, vec![40, 41, 42]);
        assert_eq!(latest.number, 42);
        assert_eq!(latest.chain, Chain::Arc);
        assert_eq!(
            w.resolve(BlockNumberOrTimestamp::Number(42)),
            WindowResolution::InWindow(latest.clone())
        );
        assert_eq!(
            w.resolve(BlockNumberOrTimestamp::Timestamp(timestamp + chrono::Duration::seconds(1))),
            WindowResolution::InWindow(latest)
        );
    }

    #[test]
    fn capture_patch_captures_balance_only_changes() {
        let address = Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        let mut w = window(128, 1);
        put(&mut w, with_component_balance(msg(1, 0, None), "c1")).unwrap();
        put(&mut w, with_account_balance(msg(2, 0, None), &address)).unwrap();

        let patch = w
            .capture_patch(&["c1"], std::slice::from_ref(&address), None)
            .unwrap();

        assert_eq!(
            patch.components["c1"],
            vec![ComponentChange { block: 1, delta: None, balances: Some(HashMap::new()) }]
        );
        assert_eq!(
            patch.accounts[&address],
            vec![AccountChange { block: 2, delta: None, balances: Some(HashMap::new()) }]
        );
    }

    #[test]
    fn capture_patch_below_the_floor_yields_the_floor_block_alone() {
        let mut w = window(128, 1);
        for n in 5..=7u64 {
            put(&mut w, with_component_delta(msg(n, 0, None), "c1", n)).unwrap();
        }

        let patch = w
            .capture_patch(&["c1"], &[], Some(BlockNumberOrTimestamp::Number(2)))
            .unwrap();

        let blocks: Vec<u64> = patch.components["c1"]
            .iter()
            .map(|c| c.block)
            .collect();
        assert_eq!(blocks, vec![5]);
    }

    #[test]
    fn capture_patch_on_an_empty_window_is_empty() {
        let patch = window(128, 1)
            .capture_patch(&["c1"], &[], None)
            .unwrap();
        assert!(patch.components.is_empty() && patch.accounts.is_empty());
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

    #[rstest]
    #[case::number_in_window(
        BlockNumberOrTimestamp::Number(5),
        WindowResolution::InWindow(testing::block(5))
    )]
    #[case::number_below_floor(BlockNumberOrTimestamp::Number(0), WindowResolution::BelowFloor)]
    #[case::number_above_tip(BlockNumberOrTimestamp::Number(11), WindowResolution::AboveTip)]
    #[case::timestamp_at_block(
        BlockNumberOrTimestamp::Timestamp(testing::block(5).ts),
        WindowResolution::InWindow(testing::block(5))
    )]
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
}
