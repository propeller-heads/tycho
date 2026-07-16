use std::{collections::VecDeque, num::NonZeroUsize};

use lru::LruCache;
use thiserror::Error;
use tracing::{debug, error};
use tycho_common::{display::opt, Bytes};

use crate::feed::BlockHeader;

#[derive(Debug, Error)]
pub enum BlockHistoryError {
    #[error("Cache size cannot be 0")]
    InvalidCacheSize,
    #[error("History is empty")]
    EmptyHistory,
    #[error("Could not determine the block's position")]
    UndeterminedBlockPosition,
    #[error("Reverting block's insert position not found! History exceeded")]
    RevertPositionNotFound,
    #[error("Pushing a detached block is unsafe")]
    DetachedBlock,
    #[error("Expected latest block to be a partial block for NextPartial position")]
    ExpectedPartialBlock,
}

pub struct BlockHistory {
    history: VecDeque<BlockHeader>,
    reverts: LruCache<Bytes, BlockHeader>,
    size: usize,
}

#[derive(Debug, PartialEq)]
pub enum BlockPosition {
    /// The next expected block
    NextExpected,
    /// The next partial block
    NextPartial,
    /// The latest processed block
    Latest,
    /// A previously seen block
    Delayed,
    /// A detached block with a height above NextExpected
    Advanced,
}

/// BlockHistory
///
/// Provides lightweight validation and relative positioning of received block headers
/// emitted by StateSynchronizer structs.
impl BlockHistory {
    /// Create a new BlockHistory from a vector of headers.
    ///
    /// The latest block and all connected block preceeding it are added to the history.
    /// Detached blocks are skipped.
    pub fn new(mut history: Vec<BlockHeader>, size: usize) -> Result<Self, BlockHistoryError> {
        // sort history by block number in descending order
        history.sort_by_key(|h| h.number);
        history.reverse();

        // Start with the latest block and build connected chain
        let mut connected_chain = Vec::new();
        if let Some(latest) = history.first() {
            connected_chain.push(latest.clone());
            let mut current_hash = latest.parent_hash.clone();
            let mut current_number = latest.number;

            // Find connected blocks in sequence
            for block in history.iter().skip(1) {
                // If we find a gap in block numbers, stop building the chain
                if block.number != current_number - 1 {
                    break;
                }
                // Check hash connection (preceeding block is parent of current block)
                if block.hash == current_hash {
                    connected_chain.push(block.clone());
                    current_hash = block.parent_hash.clone();
                    current_number = block.number;
                }
            }
        }

        // Reverse to get oldest->newest order
        connected_chain.reverse();

        let cache_size = NonZeroUsize::new(size * 10).ok_or(BlockHistoryError::InvalidCacheSize)?;
        debug!(tip = opt(&connected_chain.last()), "InitBlockHistory");
        Ok(Self {
            history: VecDeque::from(connected_chain),
            size,
            reverts: LruCache::new(cache_size),
        })
    }

    /// Add the block as next block.
    ///
    /// May error if the block does not fit the tip of the chain, or if history is empty and the
    /// block is a revert.
    pub fn push(&mut self, block: BlockHeader) -> Result<(), BlockHistoryError> {
        let pos = self.determine_block_position(&block)?;
        match pos {
            BlockPosition::NextExpected => {
                // if the block is NextExpected, but does not fit on top of the latest
                // block (via parent hash) -> we are dealing with a
                // revert.
                if block.revert {
                    // keep removing the head until the new block fits
                    loop {
                        let head = self
                            .history
                            .back()
                            .ok_or(BlockHistoryError::RevertPositionNotFound)?;

                        if head.hash == block.parent_hash {
                            break;
                        } else {
                            let reverted_block = self
                                .history
                                .pop_back()
                                .ok_or(BlockHistoryError::RevertPositionNotFound)?;
                            // record reverted blocks in cache
                            self.reverts
                                .push(reverted_block.hash.clone(), reverted_block);
                        }
                    }
                }
                // Final sanity check against things going awfully wrong.
                if let Some(true) = self
                    .latest()
                    .map(|b| b.hash != block.parent_hash)
                {
                    return Err(BlockHistoryError::DetachedBlock);
                }
                // Push new block to history, marking it as latest.
                debug!(
                    tip = ?block.parent_hash,
                    "BlockHistoryUpdate"
                );
                self.history.push_back(block);
                if self.history.len() > self.size {
                    self.history.pop_front();
                }
                Ok(())
            }
            BlockPosition::NextPartial => {
                // Pop the latest partial block and add the new one instead.
                // This is because they are not connected to each other using parent hashes, so
                // managing them would add unnecessary complexity.
                let latest = self
                    .history
                    .back()
                    .ok_or(BlockHistoryError::EmptyHistory)?;

                // Safety check: the latest block must be a partial block. If it's not, something
                // went wrong in determine_block_position or there's an unexpected state.
                if !latest.is_partial() {
                    error!(
                        latest_block = ?latest,
                        incoming_block = ?block,
                        "NextPartial returned but latest block is not a partial"
                    );
                    return Err(BlockHistoryError::ExpectedPartialBlock);
                }

                debug!(
                    tip = ?block.parent_hash,
                    "BlockHistoryPartialUpdate"
                );
                self.history.pop_back();
                self.history.push_back(block);
                Ok(())
            }
            BlockPosition::Latest => {
                // Partial revert always points to the latest partial block. Only 1 partial block is
                // kept at the tip so we just pop it.
                if block.revert {
                    let latest = self
                        .history
                        .back()
                        .ok_or(BlockHistoryError::EmptyHistory)?;
                    if latest.is_partial() {
                        let reverted = self
                            .history
                            .pop_back()
                            .ok_or(BlockHistoryError::RevertPositionNotFound)?;
                        self.reverts
                            .push(reverted.hash.clone(), reverted);
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Determines the blocks position relative to current history.
    ///
    /// If there is no history we'll return an error here. This will also error if we
    /// have a single block and we encounter a revert as it will be impossible to
    /// find the fork block.
    pub fn determine_block_position(
        &self,
        block: &BlockHeader,
    ) -> Result<BlockPosition, BlockHistoryError> {
        let latest = self
            .latest()
            .ok_or(BlockHistoryError::EmptyHistory)?;

        Ok(if block.parent_hash == latest.hash {
            // if the block is the next expected block.
            BlockPosition::NextExpected
        } else if block.number == latest.number && block.is_partial() {
            // For a partial block at the same height, determine its position relative to latest.
            // If the latest is also a partial block, we can compare their partial indices.
            // If the latest is a full block, any partial block at the same height is considered
            // delayed.
            match (latest.partial_block_index, block.partial_block_index) {
                (Some(latest_idx), Some(incoming_idx)) if incoming_idx > latest_idx => {
                    BlockPosition::NextPartial
                }
                (Some(latest_idx), Some(incoming_idx)) if incoming_idx == latest_idx => {
                    BlockPosition::Latest
                }
                _ => BlockPosition::Delayed,
            }
        } else if (block.hash == latest.hash) & !block.revert {
            // if the block is the latest block and it is not a revert.
            BlockPosition::Latest
        } else if self.reverts.contains(&block.hash) {
            // if the block is still on an already reverted branch.
            BlockPosition::Delayed
        } else if block.number <= latest.number {
            // block is potentially delayed or reverted.

            let oldest = self
                .oldest()
                .ok_or(BlockHistoryError::EmptyHistory)?;

            if block.number < oldest.number {
                // if this block is older than the oldest block in our history it means it is
                // delayed.
                BlockPosition::Delayed
            } else if self.hash_in_history(&block.hash) {
                // if this block is in our history
                if block.revert {
                    // if it is a revert, that is a expected forward update.
                    BlockPosition::NextExpected
                } else {
                    // if this is not a revert it means this block is delayed.
                    BlockPosition::Delayed
                }
            } else if block.is_partial() && !block.revert {
                // A non-revert partial block at or below the tip whose hash is not in history is a
                // superseded/delayed partial. Partials share a block number but carry ephemeral
                // hashes that are replaced as later partials of the same block arrive, so an
                // earlier partial (or one for a block that is no longer the tip) legitimately
                // cannot be found by hash. This is a catch-up, not a chain inconsistency.
                BlockPosition::Delayed
            } else {
                // anything else raises e.g. a completely detached, revert=false block
                let history = &self.history;
                let is_revert = block.revert;
                error!(?history, ?block, ?is_revert, "Could not determine history");
                Err(BlockHistoryError::UndeterminedBlockPosition)?
            }
        } else {
            // otherwise the block is advanced.
            BlockPosition::Advanced
        })
    }

    fn hash_in_history(&self, h: &Bytes) -> bool {
        self.history
            .iter()
            .any(|b| &b.hash == h)
    }

    pub fn latest(&self) -> Option<&BlockHeader> {
        self.history.back()
    }

    pub fn oldest(&self) -> Option<&BlockHeader> {
        self.history.front()
    }
}

#[cfg(test)]
mod test {
    use rand::Rng;
    use rstest::rstest;

    use super::*;

    fn random_hash() -> Bytes {
        let mut rng = rand::thread_rng();

        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes[..]);

        Bytes::from(bytes)
    }

    fn int_hash(no: u64) -> Bytes {
        Bytes::from(no.to_be_bytes())
    }

    fn generate_blocks(n: usize, start_n: u64, parent: Option<Bytes>) -> Vec<BlockHeader> {
        let mut blocks = Vec::with_capacity(n);
        let mut parent_hash = parent.unwrap_or_else(random_hash);
        for i in start_n..start_n + n as u64 {
            let hash = int_hash(i);
            blocks.push(BlockHeader {
                number: i,
                hash: hash.clone(),
                parent_hash,
                revert: false,
                ..Default::default()
            });
            parent_hash = hash;
        }
        blocks
    }

    #[test]
    fn test_push() {
        let start_blocks = generate_blocks(1, 0, None);
        let new_block = BlockHeader {
            number: 1,
            hash: random_hash(),
            parent_hash: int_hash(0),
            ..Default::default()
        };
        let mut history =
            BlockHistory::new(start_blocks.clone(), 2).expect("block history creation failed");

        history
            .push(new_block.clone())
            .expect("push failed");

        let hist: Vec<_> = history
            .history
            .iter()
            .cloned()
            .collect();
        assert_eq!(hist, vec![start_blocks[0].clone(), new_block]);
    }

    #[test]
    fn test_size_limit() {
        let blocks = generate_blocks(3, 0, None);
        let mut history =
            BlockHistory::new(blocks[0..2].to_vec(), 2).expect("failed to create history");

        history
            .push(blocks[2].clone())
            .expect("push failed");

        assert_eq!(history.history.len(), 2);
    }

    #[test]
    fn test_push_revert_push() {
        let blocks = generate_blocks(5, 0, None);
        let mut history = BlockHistory::new(blocks.clone(), 5).expect("failed to create history");
        let revert_block = BlockHeader {
            number: 2,
            hash: int_hash(2),
            parent_hash: int_hash(1),
            revert: true,
            ..Default::default()
        };
        let new_block = BlockHeader {
            number: 3,
            hash: random_hash(),
            parent_hash: int_hash(2),
            ..Default::default()
        };
        let mut exp_history: Vec<_> = blocks[0..3]
            .iter()
            .cloned()
            .chain([new_block.clone()])
            .collect();
        exp_history[2].revert = true;

        history
            .push(revert_block.clone())
            .expect("push failed");
        history
            .push(new_block)
            .expect("push failed");

        assert_eq!(history.history, exp_history);
        assert!(history.reverts.contains(&int_hash(3)));
        assert!(history.reverts.contains(&int_hash(4)));
    }

    #[test]
    fn test_push_detached_block() {
        let blocks = generate_blocks(3, 0, None);
        let mut history = BlockHistory::new(blocks.clone(), 5).expect("failed to create history");
        let detached = BlockHeader {
            number: 2,
            hash: int_hash(2),
            parent_hash: random_hash(),
            revert: true,
            ..Default::default()
        };

        assert!(history.push(detached).is_err());
    }

    #[test]
    fn test_new_block_history_filters_disconnected() {
        // Create a valid chain of 5 blocks starting from block 5
        let mut blocks = generate_blocks(5, 5, None);

        // Add some disconnected blocks
        blocks.push(BlockHeader {
            number: 2,
            hash: random_hash(),
            parent_hash: random_hash(),
            ..Default::default()
        });
        blocks.push(BlockHeader {
            number: 4,
            hash: random_hash(),
            parent_hash: random_hash(),
            ..Default::default()
        });

        let history = BlockHistory::new(blocks, 10).expect("failed to create history");

        // Should only contain the original 5 connected blocks
        assert_eq!(history.history.len(), 5);
        // Verify chain connectivity
        let blocks: Vec<_> = history.history.iter().collect();
        for pair in blocks.windows(2) {
            assert_eq!(pair[0].number + 1, pair[1].number);
            assert_eq!(pair[0].hash, pair[1].parent_hash);
        }
    }

    #[rstest]
    #[case::next_expected(15, 14, false, BlockPosition::NextExpected)]
    #[case::latest(14, 13, false, BlockPosition::Latest)]
    #[case::advanced(16, 15, false, BlockPosition::Advanced)]
    #[case::delayed_in_history(12, 11, false, BlockPosition::Delayed)]
    #[case::revert_is_next_expected(14, 13, true, BlockPosition::NextExpected)]
    #[case::delayed_before_history(1, 0, false, BlockPosition::Delayed)]
    fn test_determine_position(
        #[case] number: u64,
        #[case] parent_number: u64,
        #[case] revert: bool,
        #[case] expected: BlockPosition,
    ) {
        // History contains blocks 5-14
        let start_blocks = generate_blocks(10, 5, None);
        let history = BlockHistory::new(start_blocks, 20).expect("failed to create history");

        let block = BlockHeader {
            number,
            hash: int_hash(number),
            parent_hash: int_hash(parent_number),
            revert,
            ..Default::default()
        };

        let result = history
            .determine_block_position(&block)
            .expect("failed to determine position");

        assert_eq!(result, expected);
    }

    #[test]
    fn test_determine_position_reverted_branch() {
        let start_blocks = generate_blocks(10, 0, None);
        let mut history = BlockHistory::new(start_blocks, 15).expect("failed to create history");
        // Revert blocks 8-9, add new block 8
        history
            .push(BlockHeader {
                number: 7,
                hash: int_hash(7),
                parent_hash: int_hash(6),
                revert: true,
                ..Default::default()
            })
            .unwrap();
        history
            .push(BlockHeader {
                number: 8,
                hash: random_hash(),
                parent_hash: int_hash(7),
                ..Default::default()
            })
            .unwrap();

        // Block from old branch should be delayed
        let old_branch_block = BlockHeader {
            number: 9,
            hash: int_hash(9),
            parent_hash: int_hash(8),
            ..Default::default()
        };

        let result = history
            .determine_block_position(&old_branch_block)
            .expect("failed to determine position");

        assert_eq!(result, BlockPosition::Delayed);
    }

    // ==================== Partial Block Tests ====================

    /// Creates a partial block with an ephemeral hash encoding (block_number, partial_idx).
    fn partial_block(number: u64, partial_idx: u32, parent_hash: Bytes) -> BlockHeader {
        let hash = Bytes::from(
            [number.to_be_bytes().as_slice(), partial_idx.to_be_bytes().as_slice()].concat(),
        );
        BlockHeader {
            number,
            hash,
            parent_hash,
            partial_block_index: Some(partial_idx),
            ..Default::default()
        }
    }

    /// Creates history with full blocks 0..(block_num-1) and partials 0..=partial_idx for
    /// block_num.
    fn history_with_partial(block_num: u64, partial_idx: u32) -> (BlockHistory, Bytes) {
        let full_blocks = generate_blocks(block_num as usize, 0, None);
        let parent_hash = full_blocks
            .last()
            .map(|b| b.hash.clone())
            .unwrap_or_else(random_hash);
        let mut history = BlockHistory::new(full_blocks, 20).unwrap();

        for idx in 0..=partial_idx {
            history
                .push(partial_block(block_num, idx, parent_hash.clone()))
                .unwrap();
        }
        (history, parent_hash)
    }

    #[rstest]
    #[case::next_partial_after_partial_0(0, 1, BlockPosition::NextPartial)]
    #[case::next_partial_with_skip(2, 5, BlockPosition::NextPartial)]
    #[case::duplicate_partial_is_latest(3, 3, BlockPosition::Latest)]
    #[case::earlier_partial_delayed(3, 1, BlockPosition::Delayed)]
    #[case::first_partial_delayed(3, 0, BlockPosition::Delayed)]
    fn test_determine_position_partial_ordering(
        #[case] history_partial_idx: u32,
        #[case] incoming_partial_idx: u32,
        #[case] expected: BlockPosition,
    ) {
        let block_num = 10u64;
        let (history, parent_hash) = history_with_partial(block_num, history_partial_idx);

        let incoming = partial_block(block_num, incoming_partial_idx, parent_hash);

        assert_eq!(
            history
                .determine_block_position(&incoming)
                .unwrap(),
            expected
        );
    }

    #[rstest]
    #[case::first_partial_for_new_block_is_next_expected(10, 10, 0, BlockPosition::NextExpected)]
    #[case::partial_after_full_block_same_number_is_delayed(11, 10, 0, BlockPosition::Delayed)]
    fn test_determine_position_partial_edge_cases(
        #[case] history_len: usize,
        #[case] incoming_block_num: u64,
        #[case] incoming_partial_idx: u32,
        #[case] expected: BlockPosition,
    ) {
        let blocks = generate_blocks(history_len, 0, None);
        let history = BlockHistory::new(blocks.clone(), 20).unwrap();
        let parent_hash = blocks
            .get(incoming_block_num.saturating_sub(1) as usize)
            .map(|b| b.hash.clone())
            .unwrap_or_else(random_hash);

        let incoming = partial_block(incoming_block_num, incoming_partial_idx, parent_hash);

        assert_eq!(
            history
                .determine_block_position(&incoming)
                .unwrap(),
            expected
        );
    }

    #[test]
    fn test_partial_below_tip_is_delayed() {
        // History tip has advanced to the next block's first partial, while an earlier partial of
        // the previous block arrives late from a catching-up synchronizer. It must classify as
        // Delayed, not error (regression: this used to return UndeterminedBlockPosition and kill
        // the feed stream).
        let full_blocks = generate_blocks(10, 0, None);
        let parent_hash = full_blocks.last().unwrap().hash.clone();
        let mut history = BlockHistory::new(full_blocks, 20).unwrap();

        // Advance the tip: block 10 partial 11, then block 11 partial 1.
        history
            .push(partial_block(10, 11, parent_hash.clone()))
            .unwrap();
        let tip_10 = history.latest().unwrap().hash.clone();
        history
            .push(partial_block(11, 1, tip_10))
            .unwrap();

        // A delayed earlier partial of block 10 arrives.
        let late_partial = partial_block(10, 6, parent_hash);

        assert_eq!(
            history
                .determine_block_position(&late_partial)
                .expect("should classify as delayed, not error"),
            BlockPosition::Delayed
        );
    }

    #[test]
    fn test_partial_block_lifecycle() {
        let blocks = generate_blocks(10, 0, None);
        let parent_hash = blocks.last().unwrap().hash.clone();
        let mut history = BlockHistory::new(blocks, 20).unwrap();

        // Phase 1: Sequential partials replace the previous
        history
            .push(partial_block(10, 0, parent_hash.clone()))
            .unwrap();
        assert_eq!(
            history
                .latest()
                .unwrap()
                .partial_block_index,
            Some(0)
        );
        assert_eq!(history.history.len(), 11);

        history
            .push(partial_block(10, 1, parent_hash.clone()))
            .unwrap();
        assert_eq!(
            history
                .latest()
                .unwrap()
                .partial_block_index,
            Some(1)
        );
        assert_eq!(history.history.len(), 11); // Replaced, not added

        let p3 = partial_block(10, 3, parent_hash.clone());
        history.push(p3.clone()).unwrap();
        assert_eq!(
            history
                .latest()
                .unwrap()
                .partial_block_index,
            Some(3)
        );
        assert_eq!(history.latest().unwrap().hash, p3.hash);

        // Phase 2: Out-of-order partial is no-op
        history
            .push(partial_block(10, 1, parent_hash.clone()))
            .unwrap();
        assert_eq!(
            history
                .latest()
                .unwrap()
                .partial_block_index,
            Some(3)
        );

        // Phase 3: Revert invalidates partials
        let revert = BlockHeader {
            number: 9,
            hash: int_hash(9),
            parent_hash: int_hash(8),
            revert: true,
            ..Default::default()
        };
        history.push(revert).unwrap();
        assert_eq!(history.latest().unwrap().number, 9);
        assert!(history
            .latest()
            .unwrap()
            .partial_block_index
            .is_none());

        let reverted_hash =
            Bytes::from([10u64.to_be_bytes().as_slice(), 3u32.to_be_bytes().as_slice()].concat());
        assert!(history.reverts.contains(&reverted_hash));

        // Phase 4: Continue with new partials on new fork
        let new_p0 = partial_block(10, 0, int_hash(9));
        history.push(new_p0.clone()).unwrap();
        assert_eq!(history.latest().unwrap().number, 10);
        assert_eq!(
            history
                .latest()
                .unwrap()
                .partial_block_index,
            Some(0)
        );
        assert_eq!(history.latest().unwrap().hash, new_p0.hash);
    }

    #[test]
    fn test_partial_block_revert_reverts_to_last_full_block() {
        // We keep at most one partial at the tip; partial revert pops that one block.
        let blocks = generate_blocks(10, 0, None);
        let parent_hash = blocks.last().unwrap().hash.clone();
        let mut history = BlockHistory::new(blocks.clone(), 20).unwrap();

        let partial_1 = partial_block(10, 1, parent_hash.clone());
        history.push(partial_1.clone()).unwrap();

        let partial_revert = BlockHeader {
            number: 10,
            hash: partial_1.hash.clone(),
            parent_hash: parent_hash.clone(),
            revert: true,
            partial_block_index: Some(1),
            ..Default::default()
        };

        history.push(partial_revert).unwrap();

        let latest = history.latest().unwrap();
        assert_eq!(latest.number, 9);
        assert!(!latest.is_partial());
        assert_eq!(latest.hash, blocks[9].hash);
        assert!(history
            .reverts
            .contains(&partial_1.hash));
    }
}
