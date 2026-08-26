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

            // Find connected blocks in sequence, one height at a time.
            let mut i = 1;
            while i < history.len() {
                let block = &history[i];
                // Skip duplicates or same-height forks already consumed at a taller height. The
                // input may contain overlapping blocks (e.g. when merging retained history with
                // current stream headers on reinit).
                if block.number >= current_number {
                    i += 1;
                    continue;
                }
                // If we find a gap in block numbers, stop building the chain
                if block.number != current_number - 1 {
                    break;
                }

                // Consider every candidate at this height together, not just the first one
                // encountered. A real hash match always wins: flashblock partials carry ephemeral
                // hashes (only the last partial of a block holds the sealed hash), so a
                // hash-matching full block must never be displaced by a same-height partial from
                // a losing fork. Only fall back to a partial when no candidate at this height
                // hash-matches — same height still means same canonical block.
                let height = block.number;
                let same_height_end = i + history[i..]
                    .iter()
                    .take_while(|b| b.number == height)
                    .count();
                let candidates = &history[i..same_height_end];
                let chosen = candidates
                    .iter()
                    .find(|b| b.hash == current_hash)
                    .or_else(|| {
                        let fallback = candidates
                            .iter()
                            .find(|b| b.is_partial());
                        if let Some(b) = fallback {
                            debug!(
                                number = b.number,
                                hash = ?b.hash,
                                expected_parent = ?current_hash,
                                "HistoryStitchHeightFallback"
                            );
                        }
                        fallback
                    });

                let Some(chosen) = chosen else { break };
                connected_chain.push(chosen.clone());
                current_hash = chosen.parent_hash.clone();
                current_number = chosen.number;
                i = same_height_end;
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
                    // A real hash match anywhere in history is always preferred: prefer walking
                    // down to it over stopping early at a same-height partial. Height is only a
                    // fallback for when the fork point is retained solely under an ephemeral
                    // mid-block partial hash, so it genuinely cannot be found by hash — that is
                    // the crash case this fallback exists for. The gate is defensive: it makes a
                    // real hash match win whenever the fork point is findable at all. BlockHistory
                    // keeps at most one entry per height, so there is no stale same-height sibling
                    // for it to guard against in practice.
                    let hash_findable = self.hash_in_history(&block.parent_hash);
                    // keep removing the head until the new block fits
                    loop {
                        let head = self
                            .history
                            .back()
                            .ok_or(BlockHistoryError::RevertPositionNotFound)?;

                        if head.hash == block.parent_hash {
                            break;
                        }
                        if !hash_findable && head.is_partial() && head.number + 1 == block.number {
                            debug!(
                                revert_number = block.number,
                                revert_parent = ?block.parent_hash,
                                fork_number = head.number,
                                fork_hash = ?head.hash,
                                "RevertForkPointHeightFallback"
                            );
                            break;
                        }
                        let reverted_block = self
                            .history
                            .pop_back()
                            .ok_or(BlockHistoryError::RevertPositionNotFound)?;
                        // record reverted blocks in cache
                        self.reverts
                            .push(reverted_block.hash.clone(), reverted_block);
                    }
                }
                // This mirrors the drain loop's two exit conditions above, so it can never fail
                // today — `push` never returns having stopped anywhere else. It is a
                // belt-and-braces check against future changes to that loop, not a live guard
                // against a stale same-height sibling: BlockHistory retains at most one entry per
                // height, so that state cannot arise.
                if let Some(latest) = self.latest() {
                    let connects = latest.hash == block.parent_hash ||
                        (!self.hash_in_history(&block.parent_hash) &&
                            latest.is_partial() &&
                            latest.number + 1 == block.number);
                    if !connects {
                        return Err(BlockHistoryError::DetachedBlock);
                    }
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
            } else if block.revert && self.partial_at_height(block.number) {
                // A revert to a block retained only as a mid-block partial: the revert carries
                // the sealed hash, which an ephemeral partial hash can never match. Same height
                // means same canonical block, so this is the expected forward update.
                debug!(
                    number = block.number,
                    hash = ?block.hash,
                    "RevertClassifiedByPartialHeight"
                );
                BlockPosition::NextExpected
            } else if block.is_partial() && !block.revert {
                // A non-revert partial block at or below the tip whose hash is not in history is a
                // superseded/delayed partial. Partials share a block number but carry ephemeral
                // hashes that are replaced as later partials of the same block arrive, so an
                // earlier partial (or one for a block that is no longer the tip) legitimately
                // cannot be found by hash. This is a catch-up, not a chain inconsistency.
                BlockPosition::Delayed
            } else if !block.revert && self.hash_in_history(&block.parent_hash) {
                // A non-revert block whose own hash is unknown but whose parent is a known block
                // in history is a competing branch at or below the tip (e.g. a sibling of the tip
                // observed without an explicit revert). We cannot tell from a single header which
                // sibling is canonical, so we do not flip the tip here. Classify it as Delayed:
                // if it is the losing fork it simply waits, and if it is canonical the next block
                // (number > tip) arrives as Advanced and triggers a block-history reinit that
                // rebuilds from the retained history merged with the synchronizers' converged
                // headers.
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

    fn partial_at_height(&self, number: u64) -> bool {
        self.history
            .iter()
            .any(|b| b.number == number && b.is_partial())
    }

    pub fn latest(&self) -> Option<&BlockHeader> {
        self.history.back()
    }

    pub fn oldest(&self) -> Option<&BlockHeader> {
        self.history.front()
    }

    /// Returns the retained block headers, oldest first.
    pub fn blocks(&self) -> impl Iterator<Item = &BlockHeader> {
        self.history.iter()
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
    fn test_new_tolerates_duplicate_blocks() {
        // Reinit seeds `new` with retained history plus current stream headers, so the same block
        // number can appear more than once. A duplicate must not truncate the connected chain.
        let mut blocks = generate_blocks(4, 5, None); // blocks 5,6,7,8
        blocks.push(blocks[2].clone()); // duplicate block 7

        let history = BlockHistory::new(blocks, 10).expect("failed to create history");

        // All four connected blocks are retained despite the duplicate.
        assert_eq!(history.history.len(), 4);
        assert_eq!(history.oldest().unwrap().number, 5);
        assert_eq!(history.latest().unwrap().number, 8);
    }

    #[test]
    fn test_new_stitches_partial_parent_by_height() {
        // Reinit merges retained history with stream headers. The child of a mid-block partial
        // links to the sealed block hash, which the partial's ephemeral hash can never match —
        // the chain must connect by height instead of dropping all ancestry.
        let mut blocks = generate_blocks(2, 7, None); // full blocks 7, 8
        blocks.push(partial_block(9, 2, int_hash(8))); // mid-block partial, ephemeral hash
        blocks.push(partial_block(10, 0, int_hash(9))); // child linking to the sealed hash of 9

        let history = BlockHistory::new(blocks, 15).expect("failed to create history");

        let retained: Vec<u64> = history
            .blocks()
            .map(|b| b.number)
            .collect();
        assert_eq!(retained, vec![7, 8, 9, 10], "ancestry must survive the ephemeral-hash break");
    }

    #[test]
    fn test_new_prefers_hash_match_over_partial_at_same_height() {
        // Regression: among several candidates at the same height, the connected-chain stitch
        // used to accept the first one that either hash-matched or was a partial. Since stream
        // headers are appended after retained history and the list is reversed, a same-height
        // partial from a Delayed synchronizer's losing fork was examined before — and beat —
        // the canonical hash-matching block, dropping the canonical block and all its ancestors.
        let mut retained = generate_blocks(2, 323, None); // full 323, full 324
        retained.push(partial_block(325, 2, int_hash(324))); // mid-block partial, ephemeral hash

        // Stream headers merged in on reinit: the tip advanced via a partial parented on the
        // sealed hash of 325, and a Delayed synchronizer still holds a losing-fork partial at
        // 324's height. Appended after retained history, it is examined first once reversed —
        // exactly the ordering that made the old first-match code pick it over canonical 324.
        let mut input = retained;
        input.push(partial_block(326, 0, int_hash(325)));
        input.push(partial_block(324, 1, random_hash()));

        let history = BlockHistory::new(input, 15).expect("failed to create history");

        let retained: Vec<u64> = history
            .blocks()
            .map(|b| b.number)
            .collect();
        assert_eq!(
            retained,
            vec![323, 324, 325, 326],
            "canonical 324 and its ancestor 323 must survive"
        );
        assert_eq!(
            history
                .blocks()
                .find(|b| b.number == 324)
                .unwrap()
                .hash,
            int_hash(324),
            "the canonical full block must win, not the losing-fork partial"
        );
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

    #[test]
    fn test_sibling_of_tip_is_delayed() {
        // A competing block arrives at the tip's height, sharing the tip's parent, with no
        // explicit revert (regression: this used to return UndeterminedBlockPosition and kill the
        // feed stream). We cannot know which sibling is canonical from one header, so it must
        // classify as Delayed rather than flip the tip; recovery to the canonical branch happens
        // via the Advanced -> reinit path once a higher block arrives.
        let blocks = generate_blocks(10, 0, None);
        let mut history = BlockHistory::new(blocks.clone(), 15).expect("failed to create history");

        // Tip is a block 10 built on block 9.
        let tip = BlockHeader {
            number: 10,
            hash: random_hash(),
            parent_hash: int_hash(9),
            ..Default::default()
        };
        history
            .push(tip.clone())
            .expect("push tip failed");

        // A sibling: same height, same parent, different hash, non-revert.
        let sibling = BlockHeader {
            number: 10,
            hash: random_hash(),
            parent_hash: int_hash(9),
            ..Default::default()
        };

        assert_eq!(
            history
                .determine_block_position(&sibling)
                .expect("should classify as delayed, not error"),
            BlockPosition::Delayed
        );

        // The tip is not disturbed by classification.
        history
            .push(sibling)
            .expect("push sibling failed");
        assert_eq!(history.latest().unwrap().hash, tip.hash);
    }

    #[test]
    fn test_detached_block_still_errors() {
        // A non-revert block whose parent is unknown to history remains a hard error.
        let blocks = generate_blocks(10, 0, None);
        let history = BlockHistory::new(blocks, 15).expect("failed to create history");

        let detached = BlockHeader {
            number: 9,
            hash: random_hash(),
            parent_hash: random_hash(),
            ..Default::default()
        };

        assert!(matches!(
            history.determine_block_position(&detached),
            Err(BlockHistoryError::UndeterminedBlockPosition)
        ));
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
    fn test_revert_to_block_retained_as_partial_is_next_expected() {
        // History retains block 9 only as a mid-block partial (ephemeral hash). A revert to
        // sealed 9 cannot be found by hash but is the same canonical block by height.
        let mut blocks = generate_blocks(2, 7, None); // full blocks 7, 8
        blocks.push(partial_block(9, 2, int_hash(8)));
        blocks.push(partial_block(10, 0, int_hash(9)));
        let history = BlockHistory::new(blocks, 15).unwrap();

        let revert = BlockHeader {
            number: 9,
            hash: int_hash(9),
            parent_hash: int_hash(8),
            revert: true,
            ..Default::default()
        };

        assert_eq!(
            history
                .determine_block_position(&revert)
                .expect("must classify, not error"),
            BlockPosition::NextExpected
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

    #[test]
    fn test_revert_drain_stops_at_partial_fork_point() {
        // The fork point (block 9) is retained as a mid-block partial. A revert to block 10
        // must drain down to it and re-push the reverted-to block, not exhaust the history.
        let mut blocks = generate_blocks(2, 7, None); // full blocks 7, 8
        blocks.push(partial_block(9, 2, int_hash(8)));
        blocks.push(partial_block(10, 0, int_hash(9)));
        let mut history = BlockHistory::new(blocks, 15).unwrap();

        let revert = BlockHeader {
            number: 10,
            hash: int_hash(10),
            parent_hash: int_hash(9),
            revert: true,
            ..Default::default()
        };

        history
            .push(revert)
            .expect("revert with a partial fork point must resolve");

        let retained: Vec<u64> = history
            .blocks()
            .map(|b| b.number)
            .collect();
        assert_eq!(retained, vec![7, 8, 9, 10]);
        let latest = history.latest().unwrap();
        assert!(latest.revert);
        assert!(!latest.is_partial());
    }
}
