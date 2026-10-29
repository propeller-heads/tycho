//! The full blocks the subscribers of an extractor received, so that a rebuilt extractor can replay
//! them without sending them again.
use std::collections::VecDeque;

use tycho_common::models::{blockchain::BlockAggregatedChanges, BlockHash};

/// What the runner does with a message of the extractor.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Send it to the subscribers.
    Send,
    /// Drop it: the subscribers received this block already.
    Skip,
    /// Send `ExtractorRestarted` before it: the replay does not continue what the subscribers
    /// received.
    Restart,
}

/// The number and hash of each full block sent to the subscribers of one extractor, and whether
/// a rebuilt extractor replays them.
///
/// A replay drops every message up to the newest block sent, after it checked the hash of each
/// full block. The subscribers then see no restart: the delta window rejects a full block it holds
/// already, and tycho-client ignores a partial it has. A replay that differs from what was sent (a
/// different hash, a revert, or a gap after the newest block sent) asks for a restart instead.
#[derive(Debug, Default)]
pub(crate) struct SentBlocks {
    /// Oldest first. Holds the blocks above the newest committed height a message reported, and
    /// always the newest block sent.
    full_blocks: VecDeque<(u64, BlockHash)>,
    /// The newest full block sent when the replay started; `None` when not replaying.
    replay_until: Option<u64>,
}

impl SentBlocks {
    /// Starts a replay up to the newest full block sent. Returns `false` when no block was sent,
    /// so the subscribers must get `ExtractorRestarted` instead.
    pub(crate) fn begin_replay(&mut self) -> bool {
        self.replay_until = self
            .full_blocks
            .back()
            .map(|(number, _)| *number);
        self.replay_until.is_some()
    }

    /// Forgets every block sent. Call it when the subscribers get `ExtractorRestarted`.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Returns what to do with `msg`, and ends the replay with its first message past the newest
    /// block sent.
    pub(crate) fn classify(&mut self, msg: &BlockAggregatedChanges) -> Delivery {
        let Some(replay_until) = self.replay_until else {
            return Delivery::Send;
        };
        if msg.revert {
            return Delivery::Restart;
        }
        if msg.block.number > replay_until {
            self.replay_until = None;
            let continues_sent_chain = self
                .full_blocks
                .back()
                .is_some_and(|(number, hash)| {
                    number + 1 == msg.block.number && *hash == msg.block.parent_hash
                });
            return if continues_sent_chain { Delivery::Send } else { Delivery::Restart };
        }
        if msg.partial_block_index.is_some() {
            return Delivery::Skip;
        }
        let sent_hash = self
            .full_blocks
            .iter()
            .find(|(number, _)| *number == msg.block.number)
            .map(|(_, hash)| hash);
        if sent_hash == Some(&msg.block.hash) {
            Delivery::Skip
        } else {
            Delivery::Restart
        }
    }

    /// Records `msg` after the runner sent it.
    pub(crate) fn record(&mut self, msg: &BlockAggregatedChanges) {
        if msg.revert {
            while self
                .full_blocks
                .back()
                .is_some_and(|(number, _)| *number > msg.block.number)
            {
                self.full_blocks.pop_back();
            }
            return;
        }
        if msg.partial_block_index.is_some() {
            return;
        }
        self.full_blocks
            .push_back((msg.block.number, msg.block.hash.clone()));
        // A replay starts above the database cursor, which is at or above every committed height
        // a message reported.
        let Some(committed) = msg.db_committed_block_height else {
            return;
        };
        while self.full_blocks.len() > 1 &&
            self.full_blocks
                .front()
                .is_some_and(|(number, _)| *number <= committed)
        {
            self.full_blocks.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use tycho_common::Bytes;

    use super::*;
    use crate::testing;

    fn full(number: u64) -> BlockAggregatedChanges {
        BlockAggregatedChanges { block: testing::block(number), ..Default::default() }
    }

    fn partial(number: u64, index: u32) -> BlockAggregatedChanges {
        BlockAggregatedChanges { partial_block_index: Some(index), ..full(number) }
    }

    fn other_hash(number: u64) -> BlockAggregatedChanges {
        let mut msg = full(number);
        msg.block.hash = Bytes::from(0xff_u8).lpad(32, 0);
        msg
    }

    fn revert_to(number: u64) -> BlockAggregatedChanges {
        BlockAggregatedChanges { revert: true, ..full(number) }
    }

    fn sent(numbers: std::ops::RangeInclusive<u64>) -> SentBlocks {
        let mut sent = SentBlocks::default();
        for number in numbers {
            sent.record(&full(number));
        }
        sent
    }

    #[test]
    fn test_messages_are_sent_while_no_replay_runs() {
        let mut sent = sent(1..=3);

        assert_eq!(sent.classify(&full(2)), Delivery::Send);
        assert_eq!(sent.classify(&revert_to(1)), Delivery::Send);
    }

    #[test]
    fn test_replay_skips_the_sent_blocks_and_sends_the_first_block_that_continues_them() {
        let mut sent = sent(1..=3);
        assert!(sent.begin_replay());

        let deliveries: Vec<Delivery> = [full(2), partial(3, 0), full(3), partial(4, 0), full(4)]
            .iter()
            .map(|msg| sent.classify(msg))
            .collect();

        assert_eq!(
            deliveries,
            [Delivery::Skip, Delivery::Skip, Delivery::Skip, Delivery::Send, Delivery::Send]
        );
    }

    #[test]
    fn test_replay_of_a_block_with_another_hash_asks_for_a_restart() {
        let mut sent = sent(1..=3);
        assert!(sent.begin_replay());

        assert_eq!(sent.classify(&full(2)), Delivery::Skip);
        assert_eq!(sent.classify(&other_hash(3)), Delivery::Restart);
    }

    #[test]
    fn test_revert_during_a_replay_asks_for_a_restart() {
        let mut sent = sent(1..=3);
        assert!(sent.begin_replay());

        assert_eq!(sent.classify(&revert_to(2)), Delivery::Restart);
    }

    #[test]
    fn test_replay_that_does_not_continue_the_newest_sent_block_asks_for_a_restart() {
        let mut sent = sent(1..=3);
        assert!(sent.begin_replay());

        assert_eq!(sent.classify(&full(5)), Delivery::Restart);
        assert_eq!(sent.classify(&full(6)), Delivery::Send, "the replay ended");
    }

    #[test]
    fn test_replay_of_a_block_below_the_record_asks_for_a_restart() {
        let mut sent = SentBlocks::default();
        for number in 1..=3 {
            sent.record(&BlockAggregatedChanges {
                db_committed_block_height: Some(2),
                ..full(number)
            });
        }
        assert!(sent.begin_replay());

        assert_eq!(sent.classify(&full(2)), Delivery::Restart);
    }

    #[test]
    fn test_record_keeps_the_newest_block_above_the_committed_height() {
        let mut sent = sent(1..=2);
        sent.record(&BlockAggregatedChanges { db_committed_block_height: Some(5), ..full(3) });
        assert!(sent.begin_replay());

        assert_eq!(sent.classify(&full(3)), Delivery::Skip);
        assert_eq!(sent.classify(&full(4)), Delivery::Send);
    }

    #[test]
    fn test_record_of_a_revert_forgets_the_reverted_blocks() {
        let mut sent = sent(1..=3);
        sent.record(&revert_to(1));
        assert!(sent.begin_replay());

        assert_eq!(sent.classify(&full(1)), Delivery::Skip);
        assert_eq!(sent.classify(&full(2)), Delivery::Send);
    }

    #[test]
    fn test_no_replay_without_a_sent_full_block() {
        let mut sent = SentBlocks::default();
        sent.record(&partial(1, 0));

        assert!(!sent.begin_replay());
        assert_eq!(sent.classify(&full(1)), Delivery::Send);
    }

    #[test]
    fn test_clear_ends_a_replay() {
        let mut sent = sent(1..=3);
        assert!(sent.begin_replay());

        sent.clear();

        assert!(!sent.begin_replay());
        assert_eq!(sent.classify(&full(2)), Delivery::Send);
    }
}
