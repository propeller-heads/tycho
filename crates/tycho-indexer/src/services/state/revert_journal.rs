//! The revert messages a [`DeltaWindow`](super::window::DeltaWindow) applied.
//!
//! A purge removes the orphaned blocks from the window, so the window alone cannot tell a client
//! that stopped on an orphaned block how to get back to the canonical chain. The journal keeps
//! every revert message the window applied, with the hashes of the blocks it orphaned: a client
//! whose last block is one of them resumes with that revert message, then from its target.

use std::sync::Arc;

use deepsize::DeepSizeOf;
use tycho_common::models::{blockchain::BlockAggregatedChanges, BlockHash};

/// One revert the window applied.
#[derive(Debug, DeepSizeOf)]
struct JournalEntry {
    /// The revert message as the extractor broadcast it: its block is the revert target, and its
    /// deltas restore the target's state.
    message: Arc<BlockAggregatedChanges>,
    /// Hashes of the blocks the revert removed from the window.
    orphaned: Vec<BlockHash>,
}

/// The revert messages a window applied, oldest first.
///
/// Entries leave once their target can no longer anchor a path through the window
/// ([`RevertJournal::prune`]), and all at once when the window is cleared.
#[derive(Debug, Default, DeepSizeOf)]
pub(crate) struct RevertJournal {
    entries: Vec<JournalEntry>,
}

impl RevertJournal {
    /// Records a revert message and the blocks it removed from the window. Keeps the message's
    /// `Arc` and the orphaned blocks' hashes, not the blocks.
    pub(crate) fn record(
        &mut self,
        message: Arc<BlockAggregatedChanges>,
        orphaned: &[Arc<BlockAggregatedChanges>],
    ) {
        let orphaned = orphaned
            .iter()
            .map(|block| block.block.hash.clone())
            .collect();
        self.entries
            .push(JournalEntry { message, orphaned });
    }

    /// Forgets the reverts whose target is below the parent of the window's oldest block, whose
    /// number is `oldest`: the window no longer holds the canonical blocks that follow such a
    /// target. A revert to that parent stays, since a path can start right after it.
    pub(crate) fn prune(&mut self, oldest: u64) {
        // A later revert can target a lower block than an earlier one, so the entries are not
        // ordered by target and the stale ones are not all at the front.
        self.entries
            .retain(|entry| entry.message.block.number + 1 >= oldest);
    }

    /// Forgets every revert.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// The newest revert message that orphaned the block `hash`, if the journal still holds one.
    pub(crate) fn orphaned_by(&self, hash: &BlockHash) -> Option<&Arc<BlockAggregatedChanges>> {
        // A chain can return to a branch it left, so a block can be orphaned, re-applied and
        // orphaned again; the newest revert is the one that left the block's latest copy.
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.orphaned.contains(hash))
            .map(|entry| &entry.message)
    }

    /// The target numbers of the journaled reverts, oldest revert first.
    #[cfg(test)]
    pub(crate) fn targets(&self) -> Vec<u64> {
        self.entries
            .iter()
            .map(|entry| entry.message.block.number)
            .collect()
    }
}

#[cfg(test)]
mod test {
    use tycho_common::Bytes;

    use super::*;
    use crate::testing;

    fn block(number: u64) -> Arc<BlockAggregatedChanges> {
        Arc::new(testing::aggregated_changes("ex", number, 0, None))
    }

    fn revert(number: u64) -> Arc<BlockAggregatedChanges> {
        Arc::new(BlockAggregatedChanges { revert: true, ..(*block(number)).clone() })
    }

    fn hash(number: u64) -> Bytes {
        testing::block(number).hash
    }

    #[test]
    fn orphaned_by_finds_the_revert_that_removed_the_block() {
        let mut journal = RevertJournal::default();
        journal.record(revert(2), &[block(3), block(4)]);

        assert_eq!(journal.orphaned_by(&hash(4)), Some(&revert(2)));
        assert_eq!(journal.orphaned_by(&hash(2)), None);
        assert_eq!(journal.orphaned_by(&hash(5)), None);
    }

    #[test]
    fn orphaned_by_prefers_the_newest_revert() {
        // Block 4 was orphaned by a revert to 3, re-applied, then orphaned by a revert to 2.
        let mut journal = RevertJournal::default();
        journal.record(revert(3), &[block(4)]);
        journal.record(revert(2), &[block(3), block(4)]);

        assert_eq!(
            journal
                .orphaned_by(&hash(4))
                .map(|message| message.block.number),
            Some(2)
        );
    }

    #[test]
    fn prune_keeps_a_revert_to_the_parent_of_the_oldest_block() {
        let mut journal = RevertJournal::default();
        // Out of target order, as nested reverts leave them.
        for target in [6, 4, 5, 3] {
            journal.record(revert(target), &[block(target + 1)]);
        }

        journal.prune(5);

        assert_eq!(journal.targets(), vec![6, 4, 5]);
    }

    #[test]
    fn clear_forgets_every_revert() {
        let mut journal = RevertJournal::default();
        journal.record(revert(2), &[block(3)]);

        journal.clear();

        assert_eq!(journal.orphaned_by(&hash(3)), None);
        assert!(journal.targets().is_empty());
    }
}
