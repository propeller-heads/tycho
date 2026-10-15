//! What the server's delta window must answer when a client asks to catch up over a gap.

use std::sync::Arc;

use super::generator::{History, HistoryEvent, RestartKind};
use crate::models::{
    blockchain::{Block, BlockAggregatedChanges},
    BlockHash,
};

/// Where a client's view of a history breaks. Both fields index [`History::events`], which holds
/// the restarts as well, not [`History::messages`]; both must point at a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// The last message the client applied before its subscription ended. The client's head
    /// after it is the catch-up's `from`.
    pub last_applied: usize,
    /// The first message of the client's new subscription. The catch-up's `to` is its parent, or
    /// its target when it is a revert.
    pub first_live: usize,
}

/// What a catch-up must do for a [`Gap`].
#[derive(Debug, Clone, PartialEq)]
pub enum CatchUpSpec {
    /// The server must answer with exactly these messages, oldest first. Empty when `from` is
    /// `to`.
    Possible(Vec<Arc<BlockAggregatedChanges>>),
    /// No path can be proven; the client must resync from a snapshot.
    Fallback(FallbackReason),
}

/// Why a catch-up must fall back to a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FallbackReason {
    /// An upgraded extractor restarted inside the gap.
    FingerprintChanged,
    /// The server's window holds no block when the client resubscribes.
    EmptyWindow,
    /// `from` is older than the parent of the window's oldest block.
    BelowFloor,
    /// `from` is not held by the window, was orphaned by no revert the window still remembers,
    /// and is not the parent of the window's oldest block.
    UnknownHash,
    /// `to` is not a canonical block in the window.
    ToNotInWindow,
    /// `to` lies behind the canonical block the walk from `from` reaches: the client received a
    /// revert the window has not applied yet.
    ToNotAhead,
}

/// Says whether a client that missed `gap` must be able to catch up, and with which messages.
///
/// The server's window is rebuilt from the history's messages before `gap.first_live`, as
/// `DeltaWindow` keeps them with `config.window_depth` and a fold batch of 1:
/// - A block stays until it is at or below `min(finalized, db_committed, tip - window_depth)`, from
///   the highest watermarks the blocks carried. Nothing is evicted before the first commit.
/// - A revert purges the blocks above its target and leaves the watermarks alone.
/// - A restart clears the window.
/// - Every revert is journaled with the hashes of the blocks it orphaned. An entry is kept while
///   its target is no lower than the parent of the window's oldest block, by number (`target + 1 >=
///   oldest`), so a revert to that parent still anchors a path. A clear drops every entry.
///
/// `from` is the block of `gap.last_applied`; `to` is the parent of `gap.first_live`, or its
/// target when it is a revert. The rules, in order:
/// 1. An upgraded restart inside the gap: [`FallbackReason::FingerprintChanged`].
/// 2. An empty window: [`FallbackReason::EmptyWindow`].
/// 3. `to` not held by the window: [`FallbackReason::ToNotInWindow`].
/// 4. Walk from `from` to a canonical block: through the journaled revert that orphaned it, then
///    from that revert's target, until reaching a block the window holds or the parent of its
///    oldest block. A walk that gets stuck is [`FallbackReason::BelowFloor`] when `from` is older
///    than that parent, else [`FallbackReason::UnknownHash`].
/// 5. A walk that ends past `to` is [`FallbackReason::ToNotAhead`]. Otherwise the path is the
///    reverts walked through, then the window's blocks after the walk's end up to `to`.
///
/// # Panics
///
/// When `gap` does not lie inside the history, `gap.last_applied` or `gap.first_live` is not a
/// message, or the history's messages do not form a chain the window accepts.
pub fn catch_up_spec(history: &History, gap: Gap) -> CatchUpSpec {
    let Gap { last_applied, first_live } = gap;
    assert!(
        last_applied < first_live && first_live < history.events.len(),
        "gap {gap:?} does not lie inside a history of {} events",
        history.events.len()
    );
    let HistoryEvent::Message(last) = &history.events[last_applied] else {
        panic!("event {last_applied} is not a message the client could apply");
    };
    let HistoryEvent::Message(live) = &history.events[first_live] else {
        panic!("event {first_live} is not a message a subscription could start with");
    };

    for event in &history.events[last_applied + 1..first_live] {
        match event {
            HistoryEvent::ExtractorRestarted { last_committed: _, kind: RestartKind::Upgraded } => {
                return CatchUpSpec::Fallback(FallbackReason::FingerprintChanged);
            }
            HistoryEvent::ExtractorRestarted {
                last_committed: _,
                kind: RestartKind::Unchanged,
            } |
            HistoryEvent::Message(_) => {}
        }
    }

    let mut window = Window::new(history.config.window_depth);
    for event in &history.events[..first_live] {
        window.apply(event);
    }
    let to = if live.revert { &live.block.hash } else { &live.block.parent_hash };
    window.path(&last.block, to)
}

/// One revert the server's window applied.
#[derive(Debug)]
pub(super) struct JournalEntry {
    pub(super) revert: Arc<BlockAggregatedChanges>,
    /// Hashes of the blocks the revert removed from the window.
    orphaned: Vec<BlockHash>,
}

/// The server's delta window for one extractor, with its revert journal.
#[derive(Debug)]
pub(super) struct Window {
    pub(super) depth: u64,
    /// Contiguous canonical blocks, oldest first.
    pub(super) blocks: Vec<Arc<BlockAggregatedChanges>>,
    pub(super) finalized: Option<u64>,
    pub(super) committed: Option<u64>,
    /// Reverts applied, oldest first.
    pub(super) journal: Vec<JournalEntry>,
}

impl Window {
    pub(super) fn new(depth: u64) -> Self {
        Self { depth, blocks: Vec::new(), finalized: None, committed: None, journal: Vec::new() }
    }

    pub(super) fn apply(&mut self, event: &HistoryEvent) {
        match event {
            HistoryEvent::Message(message) if message.revert => self.revert(message),
            HistoryEvent::Message(message) => self.insert(message),
            HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => self.clear(),
        }
    }

    fn insert(&mut self, message: &Arc<BlockAggregatedChanges>) {
        if let Some(tip) = self.blocks.last() {
            assert_eq!(
                tip.block.hash, message.block.parent_hash,
                "block {} does not extend the window's tip {}",
                message.block.number, tip.block.number
            );
        }
        self.blocks.push(Arc::clone(message));
        let finalized = message.finalized_block_height;
        self.finalized = Some(
            self.finalized
                .map_or(finalized, |held| held.max(finalized)),
        );
        if let Some(committed) = message.db_committed_block_height {
            self.committed = Some(
                self.committed
                    .map_or(committed, |held| held.max(committed)),
            );
        }
        self.evict();
    }

    fn revert(&mut self, message: &Arc<BlockAggregatedChanges>) {
        let target = &message.block;
        let irreversible = match (self.finalized, self.committed) {
            (Some(finalized), Some(committed)) => Some(finalized.min(committed)),
            (Some(finalized), None) => Some(finalized),
            (None, _) => None,
        };
        assert!(
            irreversible.is_none_or(|height| target.number >= height),
            "revert to block {} below the irreversible height {irreversible:?}",
            target.number
        );
        let position = self
            .position(&target.hash)
            .unwrap_or_else(|| {
                panic!("revert to block {} the window does not hold", target.number)
            });
        let orphaned = self
            .blocks
            .split_off(position + 1)
            .into_iter()
            .map(|orphan| orphan.block.hash.clone())
            .collect();
        self.journal
            .push(JournalEntry { revert: Arc::clone(message), orphaned });
        self.evict();
    }

    fn clear(&mut self) {
        self.blocks.clear();
        self.journal.clear();
        self.finalized = None;
        self.committed = None;
    }

    /// Evicts every block at or below `min(finalized, committed, tip - depth)`, then forgets the
    /// reverts whose target is below the parent of the oldest block left.
    fn evict(&mut self) {
        let (Some(tip), Some(finalized), Some(committed)) =
            (self.blocks.last(), self.finalized, self.committed)
        else {
            return;
        };
        let bound = finalized.min(committed).min(
            tip.block
                .number
                .saturating_sub(self.depth),
        );
        let evicted = self
            .blocks
            .iter()
            .take_while(|block| block.block.number <= bound)
            .count();
        self.blocks.drain(..evicted);
        if let Some(oldest) = self.blocks.first() {
            let oldest = oldest.block.number;
            self.journal
                .retain(|entry| entry.revert.block.number + 1 >= oldest);
        }
    }

    pub(super) fn position(&self, hash: &BlockHash) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| &block.block.hash == hash)
    }

    /// The newest journaled revert that orphaned `hash`.
    fn orphaned_by(&self, hash: &BlockHash) -> Option<&JournalEntry> {
        self.journal
            .iter()
            .rev()
            .find(|entry| entry.orphaned.contains(hash))
    }

    /// The messages that take a client from `from` to the block `to`; see [`catch_up_spec`].
    fn path(&self, from: &Block, to: &BlockHash) -> CatchUpSpec {
        let Some(oldest) = self.blocks.first() else {
            return CatchUpSpec::Fallback(FallbackReason::EmptyWindow);
        };
        let Some(to_position) = self.position(to) else {
            return CatchUpSpec::Fallback(FallbackReason::ToNotInWindow);
        };
        let mut path = Vec::new();
        let mut current = from.hash.clone();
        // Index of the first window block the path continues with.
        let continue_at = loop {
            if let Some(position) = self.position(&current) {
                break position + 1;
            }
            if current == oldest.block.parent_hash {
                break 0;
            }
            let Some(entry) = self.orphaned_by(&current) else {
                let reason = if from.number + 1 < oldest.block.number {
                    FallbackReason::BelowFloor
                } else {
                    FallbackReason::UnknownHash
                };
                return CatchUpSpec::Fallback(reason);
            };
            path.push(Arc::clone(&entry.revert));
            current = entry.revert.block.hash.clone();
        };
        if continue_at > to_position + 1 {
            return CatchUpSpec::Fallback(FallbackReason::ToNotAhead);
        }
        path.extend(
            self.blocks[continue_at..=to_position]
                .iter()
                .cloned(),
        );
        CatchUpSpec::Possible(path)
    }
}

#[cfg(test)]
mod test {
    use rstest::rstest;

    use super::*;
    use crate::testing::delta_history::fixtures::*;

    #[test]
    fn spec_from_a_canonical_block_is_the_canonical_blocks_after_it() {
        let blocks: Vec<_> = (1..=5)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let history = history(messages(&blocks), blocks.clone(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::Possible(blocks[2..4].to_vec()));
    }

    #[test]
    fn spec_from_an_orphaned_block_goes_through_the_revert() {
        // The client has 4, the chain reverts to 2 and grows 3', 4', 5', 6' on branch 1.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let revert = revert_to(2, 0, 0);
        let new_branch = vec![
            full_block(3, 1, 0),
            full_block(4, 1, 1),
            full_block(5, 1, 1),
            full_block(6, 1, 1),
        ];
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(revert.clone()));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 8 });

        let expected =
            vec![revert, new_branch[0].clone(), new_branch[1].clone(), new_branch[2].clone()];
        assert_eq!(spec, CatchUpSpec::Possible(expected));
    }

    #[test]
    fn spec_follows_nested_reverts_to_the_canonical_chain() {
        // The client has 4. The chain reverts to 2 and grows 3', 4' on branch 1, then reverts
        // to 1, orphaning 2 as well, and grows 2'', 3'', 4'' on branch 2.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let (first_revert, second_revert) = (revert_to(2, 0, 0), revert_to(1, 0, 0));
        let new_branch = vec![full_block(2, 2, 0), full_block(3, 2, 2), full_block(4, 2, 2)];
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(first_revert.clone()));
        events.extend(messages(&[full_block(3, 1, 0), full_block(4, 1, 1)]));
        events.push(HistoryEvent::Message(second_revert.clone()));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..1], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 10 });

        let expected =
            vec![first_revert, second_revert, new_branch[0].clone(), new_branch[1].clone()];
        assert_eq!(spec, CatchUpSpec::Possible(expected));
    }

    #[test]
    fn spec_to_the_client_block_is_an_empty_path() {
        // The client has 2 and misses 3 and 4; its new subscription starts with a revert to 2.
        let blocks: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks);
        events.push(HistoryEvent::Message(revert_to(2, 0, 0)));
        let history = history(events, blocks[..2].to_vec(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::Possible(Vec::new()));
    }

    #[test]
    fn spec_to_behind_the_client_block_falls_back() {
        // The client has 4; its new subscription starts with a revert to 2 the window has not
        // applied yet.
        let blocks: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks);
        events.push(HistoryEvent::Message(revert_to(2, 0, 0)));
        let history = history(events, blocks[..2].to_vec(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::ToNotAhead));
    }

    #[test]
    fn spec_to_a_block_the_window_lacks_falls_back() {
        // The client's first live block is 7, whose parent the window has not received yet.
        let mut blocks: Vec<_> = (1..=5)
            .map(|n| full_block(n, 0, 0))
            .collect();
        blocks.push(full_block(7, 0, 0));
        let history = history(messages(&blocks), blocks.clone(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 5 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::ToNotInWindow));
    }

    #[test]
    fn spec_right_after_a_restart_falls_back_on_the_empty_window() {
        let blocks: Vec<_> = (1..=3)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks);
        events.push(restarted(&blocks[1], RestartKind::Unchanged));
        events.push(HistoryEvent::Message(blocks[2].clone()));
        let history = history(events, blocks, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::EmptyWindow));
    }

    #[test]
    fn spec_an_upgraded_restart_in_the_gap_falls_back() {
        let blocks: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks[..3]);
        events.push(restarted(&blocks[1], RestartKind::Upgraded));
        events.extend(messages(&blocks[2..]));
        let history = history(events, blocks, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 2, first_live: 5 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::FingerprintChanged));
    }

    #[test]
    fn spec_a_client_at_the_last_committed_block_resumes_after_a_restart() {
        let blocks: Vec<_> = (1..=5)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks[..3]);
        events.push(restarted(&blocks[1], RestartKind::Unchanged));
        events.extend(messages(&blocks[2..]));
        let history = history(events, blocks.clone(), 16);

        // The client applied block 2, the extractor's last committed block.
        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 6 });

        assert_eq!(spec, CatchUpSpec::Possible(blocks[2..4].to_vec()));
    }

    #[test]
    fn spec_a_reorg_during_downtime_falls_back() {
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let new_branch = vec![full_block(3, 1, 0), full_block(4, 1, 1), full_block(5, 1, 1)];
        let mut events = messages(&old_branch);
        events.push(restarted(&old_branch[1], RestartKind::Unchanged));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        // The client applied 4 on the abandoned branch; no revert message ever orphaned it.
        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 7 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::UnknownHash));
    }

    #[test]
    fn spec_forgets_the_reverts_on_a_restart() {
        // Had the journal survived the restart, its revert to 2 would lead from 4 to the parent
        // of the refilled window's oldest block.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let new_branch = vec![full_block(3, 1, 0), full_block(4, 1, 1), full_block(5, 1, 1)];
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(revert_to(2, 0, 0)));
        events.push(HistoryEvent::Message(new_branch[0].clone()));
        events.push(restarted(&old_branch[1], RestartKind::Unchanged));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 9 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::UnknownHash));
    }

    #[rstest]
    #[case::target_held(1, None)]
    #[case::target_is_the_parent_of_the_oldest_block(2, None)]
    #[case::target_below_the_parent_of_the_oldest_block(3, Some(FallbackReason::UnknownHash))]
    fn spec_forgets_a_revert_once_its_target_cannot_anchor_a_path(
        #[case] committed: u64,
        #[case] expected: Option<FallbackReason>,
    ) {
        // The client has 4. The chain reverts to 2 and grows 3'..=7' on branch 1, whose
        // watermarks evict every block up to `committed`. Whether the window still holds block 2
        // or block 2 is the parent of its oldest block, the path is the same.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let revert = revert_to(2, 0, 0);
        let new_branch: Vec<_> = (3..=7)
            .map(|n| {
                Arc::new(BlockAggregatedChanges {
                    finalized_block_height: committed,
                    db_committed_block_height: Some(committed),
                    ..(*full_block(n, 1, if n == 3 { 0 } else { 1 })).clone()
                })
            })
            .collect();
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(revert.clone()));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 2);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 9 });

        let expected = match expected {
            Some(reason) => CatchUpSpec::Fallback(reason),
            None => CatchUpSpec::Possible([&[revert][..], &new_branch[..4]].concat()),
        };
        assert_eq!(spec, expected);
    }

    #[test]
    fn spec_an_evicted_from_falls_back_below_floor() {
        let blocks: Vec<_> = (1..=10).map(committed_block).collect();
        let history = history(messages(&blocks), blocks, 2);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 9 });

        assert_eq!(spec, CatchUpSpec::Fallback(FallbackReason::BelowFloor));
    }

    #[rstest]
    #[case::depth_binds(10, Some(10), 8)]
    #[case::finalized_binds(5, Some(10), 5)]
    #[case::committed_binds(10, Some(4), 4)]
    #[case::nothing_is_evicted_before_the_first_commit(10, None, 1)]
    fn spec_keeps_a_block_until_it_is_finalized_committed_and_deeper_than_the_window(
        #[case] finalized: u64,
        #[case] committed: Option<u64>,
        #[case] oldest_from: u64,
    ) {
        // Blocks 1..=10 carry the same watermarks; the window is 2 blocks deep. The client's
        // new subscription starts at block 11.
        let blocks: Vec<_> = (1..=11)
            .map(|n| block_with_watermarks(n, finalized, committed))
            .collect();
        let history = history(messages(&blocks), blocks.clone(), 2);
        let from = |number: u64| Gap { last_applied: number as usize - 1, first_live: 10 };

        assert_eq!(
            catch_up_spec(&history, from(oldest_from)),
            CatchUpSpec::Possible(blocks[oldest_from as usize..10].to_vec())
        );
        if oldest_from > 1 {
            assert_eq!(
                catch_up_spec(&history, from(oldest_from - 1)),
                CatchUpSpec::Fallback(FallbackReason::BelowFloor)
            );
        }
    }

    #[test]
    fn spec_a_revert_leaves_the_watermarks_alone() {
        // Blocks 1..=6 let the window evict block 1. A revert to 5 carrying high watermarks would
        // evict up to 3 if the window applied them.
        let blocks: Vec<_> = (1..=6)
            .map(|n| block_with_watermarks(n, 1, Some(1)))
            .collect();
        let revert = Arc::new(BlockAggregatedChanges {
            finalized_block_height: 100,
            db_committed_block_height: Some(100),
            ..(*revert_to(5, 0, 0)).clone()
        });
        let mut events = messages(&blocks);
        events.push(HistoryEvent::Message(revert));
        events.push(HistoryEvent::Message(full_block(6, 1, 0)));
        let history = history(events, blocks.clone(), 2);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 7 });

        assert_eq!(spec, CatchUpSpec::Possible(blocks[2..5].to_vec()));
    }
}
