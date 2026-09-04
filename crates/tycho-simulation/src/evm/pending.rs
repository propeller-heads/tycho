use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use thiserror::Error;
use tokio::sync::{mpsc::UnboundedReceiver, watch};
use tycho_client::feed::{synchronizer::Snapshot, BlockHeader, FeedMessage};
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, PendingBlock},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        Chain,
    },
    traits::TxDeltaIndexer,
    Bytes,
};

use crate::{
    evm::decoder::{StreamDecodeError, TychoStreamDecoder},
    protocol::models::Update,
};

/// An ephemeral [`Update`] tagged with a caller-supplied label.
///
/// The label is an opaque string chosen by the caller to distinguish parallel bundle evaluations
/// (e.g. bundle ID, strategy name). It is separate from `update.block_number_or_timestamp`,
/// which carries the target block the bundle was evaluated against.
pub struct PendingUpdate {
    pub label: String,
    pub update: Update,
}

#[derive(Debug, Error)]
pub enum PendingError {
    /// Returned when the parent of `pending.block()` has not been confirmed. Use
    /// [`subscribe_confirmed_block`](PendingBlockProcessor::subscribe_confirmed_block) to wait
    /// for the right block before calling.
    #[error("parent block {needed} not yet confirmed (current: {current})")]
    ParentNotYetConfirmed { needed: u64, current: u64 },
    #[error("decoder error: {0}")]
    Decoder(#[from] StreamDecodeError),
    #[error("indexer error for extractor '{extractor}': {message}")]
    Indexer { extractor: String, message: String },
}

/// Wires one or more [`TxDeltaIndexer`]s to an existing [`TychoStreamDecoder`], enabling
/// ephemeral simulation of candidate transaction bundles against the correct parent state
/// for a specific target block.
///
/// # Block targeting
///
/// Call [`subscribe_confirmed_block`](Self::subscribe_confirmed_block) to obtain a
/// [`watch::Receiver<u64>`] that fires on every confirmed block. Use it to wait for the
/// right parent before submitting a bundle:
///
/// ```no_run
/// # async fn example(
/// #     mut processor: tycho_simulation::evm::pending::PendingBlockProcessor,
/// #     pending_block: &tycho_common::models::blockchain::PendingBlock,
/// # ) {
/// processor
///     .subscribe_confirmed_block()
///     .wait_for(|&n| n >= pending_block.block().number - 1)
///     .await
///     .expect("stream closed");
/// let update = processor
///     .generate_pending_update(pending_block, "bundle-1".to_string())
///     .await
///     .expect("pending update failed");
/// # }
/// ```
///
/// # Concurrency
///
/// `PendingBlockProcessor` is intentionally **not** wrapped in a `Mutex` at construction
/// time. The confirmed stream forwards blocks via an unbounded channel — it never blocks
/// waiting for the consumer. Multiple callers can each hold a watch receiver and
/// independently decide when to acquire whatever external lock they use around
/// `generate_pending_update`.
pub struct PendingBlockProcessor {
    indexers: HashMap<String, Box<dyn TxDeltaIndexer>>,
    decoder: Arc<TychoStreamDecoder<BlockHeader>>,
    chain: Chain,
    /// Block number of the most recently confirmed block applied to `indexers`.
    current_confirmed_block: u64,
    /// Notified on every `advance_inner` call; drives `subscribe_confirmed_block`.
    confirmed_block_tx: watch::Sender<u64>,
    /// Confirmed blocks forwarded by the stream pipeline.
    block_rx: UnboundedReceiver<FeedMessage<BlockHeader>>,
}

impl PendingBlockProcessor {
    pub(crate) fn new(
        indexers: HashMap<String, Box<dyn TxDeltaIndexer>>,
        decoder: Arc<TychoStreamDecoder<BlockHeader>>,
        chain: Chain,
        block_rx: UnboundedReceiver<FeedMessage<BlockHeader>>,
    ) -> Self {
        let (confirmed_block_tx, _) = watch::channel(0u64);
        Self { indexers, decoder, chain, current_confirmed_block: 0, confirmed_block_tx, block_rx }
    }

    /// Returns a receiver that is notified with the latest confirmed block number every time
    /// a new block is applied.
    ///
    /// Typical usage: `.wait_for(|&n| n >= target_block - 1).await` before calling
    /// [`generate_pending_update`](Self::generate_pending_update).
    pub fn subscribe_confirmed_block(&self) -> watch::Receiver<u64> {
        self.confirmed_block_tx.subscribe()
    }

    /// Returns the block number of the last confirmed block applied to the indexers.
    pub fn current_confirmed_block(&self) -> u64 {
        self.current_confirmed_block
    }

    /// Advances each registered indexer by applying one confirmed block.
    ///
    /// Only needed when using the processor standalone (without
    /// [`ProtocolStreamBuilder::build_with_pending`](crate::evm::stream::ProtocolStreamBuilder::build_with_pending)).
    /// When using `build_with_pending`, confirmed blocks are forwarded automatically.
    pub fn advance(&mut self, msg: &FeedMessage<BlockHeader>) -> Result<(), PendingError> {
        self.advance_inner(msg)
    }

    /// Simulates `pending` against the confirmed parent of `pending.block()`.
    ///
    /// Drains any confirmed blocks that have arrived since the last call, then immediately
    /// checks whether `pending.block().number - 1` is available. If not, returns
    /// [`PendingError::ParentNotYetConfirmed`] — **no blocking**. Use
    /// [`subscribe_confirmed_block`](Self::subscribe_confirmed_block) to wait for the right
    /// block before calling.
    ///
    /// Neither the indexers' internal state nor the decoder's confirmed pool states are
    /// mutated. Calling this twice with the same arguments returns identical results.
    ///
    /// # Parameters
    /// * `pending` — the in-flight block: the block being built, the candidate bundle in execution
    ///   order (failed transactions are skipped), and post-execution account state for the accounts
    ///   it touched. The returned deltas use this block's number and timestamp.
    /// * `label` — opaque caller-supplied tag stamped onto the returned [`PendingUpdate`]. Use it
    ///   to associate the result with a specific bundle or evaluation context.
    pub async fn generate_pending_update(
        &mut self,
        pending: &PendingBlock,
        label: String,
    ) -> Result<PendingUpdate, PendingError> {
        // Drain any confirmed blocks that have arrived since our last call.
        while let Ok(msg) = self.block_rx.try_recv() {
            self.advance_inner(&msg)?;
        }

        let target_block = pending.block();
        let parent = target_block.number.saturating_sub(1);
        if self.current_confirmed_block < parent {
            return Err(PendingError::ParentNotYetConfirmed {
                needed: parent,
                current: self.current_confirmed_block,
            });
        }
        let target_header = BlockHeader::from(target_block);

        let mut pending_deltas: HashMap<String, BlockAggregatedChanges> = HashMap::new();
        for (extractor, indexer) in &self.indexers {
            let changes = indexer.generate_deltas(pending);
            pending_deltas.insert(extractor.clone(), changes);
        }

        let update = self
            .decoder
            .apply_deltas_ephemeral(&pending_deltas, target_header)
            .await?;
        Ok(PendingUpdate { label, update })
    }

    fn advance_inner(&mut self, msg: &FeedMessage<BlockHeader>) -> Result<(), PendingError> {
        let msg_block = msg
            .state_msgs
            .values()
            .map(|s| s.header.number)
            .max()
            .unwrap_or(0);

        for (extractor, state_msg) in &msg.state_msgs {
            let Some(indexer) = self.indexers.get_mut(extractor) else {
                continue;
            };

            if !state_msg.snapshots.states.is_empty() {
                let block_changes = snapshot_to_block_changes(
                    extractor,
                    &state_msg.snapshots,
                    &state_msg.header,
                    self.chain,
                );
                indexer
                    .apply_block(&block_changes)
                    .map_err(|e| PendingError::Indexer {
                        extractor: extractor.clone(),
                        message: format!("{e:#}"),
                    })?;
            }

            if let Some(deltas) = &state_msg.deltas {
                indexer
                    .apply_block(deltas)
                    .map_err(|e| PendingError::Indexer {
                        extractor: extractor.clone(),
                        message: format!("{e:#}"),
                    })?;
            }
        }

        if msg_block > self.current_confirmed_block {
            self.current_confirmed_block = msg_block;
            // Receivers that have been dropped are silently ignored.
            let _ = self.confirmed_block_tx.send(msg_block);
        }
        Ok(())
    }
}

/// Converts a startup snapshot into a `BlockAggregatedChanges` suitable for
/// [`TxDeltaIndexer::apply_block`].
fn snapshot_to_block_changes(
    extractor: &str,
    snapshot: &Snapshot,
    header: &BlockHeader,
    chain: Chain,
) -> BlockAggregatedChanges {
    let ts = chrono::DateTime::from_timestamp(header.timestamp as i64, 0)
        .unwrap_or_default()
        .naive_utc();
    let block = Block {
        number: header.number,
        chain,
        hash: header.hash.clone(),
        parent_hash: header.parent_hash.clone(),
        ts,
    };

    let mut new_protocol_components: HashMap<String, ProtocolComponent> = HashMap::new();
    let mut state_deltas: HashMap<String, ProtocolComponentStateDelta> = HashMap::new();
    let mut component_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> = HashMap::new();

    for (id, comp_with_state) in &snapshot.states {
        new_protocol_components.insert(id.clone(), comp_with_state.component.clone());

        state_deltas.insert(
            id.clone(),
            ProtocolComponentStateDelta {
                component_id: id.clone(),
                updated_attributes: comp_with_state.state.attributes.clone(),
                deleted_attributes: HashSet::new(),
                created_attributes: HashSet::new(),
            },
        );

        let token_balances: HashMap<Bytes, ComponentBalance> = comp_with_state
            .state
            .balances
            .iter()
            .map(|(token, balance)| {
                (
                    token.clone(),
                    ComponentBalance {
                        token: token.clone(),
                        balance: balance.clone(),
                        balance_float: 0.0,
                        modify_tx: Bytes::default(),
                        component_id: id.clone(),
                    },
                )
            })
            .collect();
        component_balances.insert(id.clone(), token_balances);
    }

    BlockAggregatedChanges {
        extractor: extractor.to_string(),
        chain,
        block,
        finalized_block_height: header.number,
        new_protocol_components,
        state_deltas,
        component_balances,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tycho_common::models::blockchain::{Block, PendingBlock};

    use super::*;

    /// Records the block it was handed, so a test can assert what the processor passed down.
    struct RecordingIndexer {
        seen: Arc<Mutex<Vec<Block>>>,
    }

    impl TxDeltaIndexer for RecordingIndexer {
        fn apply_block(&mut self, _block: &BlockAggregatedChanges) -> anyhow::Result<()> {
            Ok(())
        }

        fn generate_deltas(&self, pending: &PendingBlock) -> BlockAggregatedChanges {
            self.seen
                .lock()
                .unwrap()
                .push(pending.block().clone());
            BlockAggregatedChanges::default()
        }
    }

    fn target_block(number: u64, timestamp: i64) -> Block {
        Block {
            number,
            chain: Chain::Ethereum,
            hash: Bytes::from([1u8; 32]),
            parent_hash: Bytes::from([2u8; 32]),
            ts: chrono::DateTime::from_timestamp(timestamp, 0)
                .unwrap()
                .naive_utc(),
        }
    }

    fn processor(seen: Arc<Mutex<Vec<Block>>>) -> PendingBlockProcessor {
        let indexers: HashMap<String, Box<dyn TxDeltaIndexer>> =
            HashMap::from([("fluid".to_string(), Box::new(RecordingIndexer { seen }) as _)]);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        PendingBlockProcessor::new(
            indexers,
            Arc::new(TychoStreamDecoder::<BlockHeader>::new(Chain::Ethereum)),
            Chain::Ethereum,
            rx,
        )
    }

    #[tokio::test]
    async fn test_indexer_receives_the_callers_target_block() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pending_processor = processor(seen.clone());
        // Parent is block 0, which the processor considers confirmed from the start.
        let block = target_block(1, 1_759_842_947);

        pending_processor
            .generate_pending_update(
                &PendingBlock::new(block.clone(), vec![], HashMap::new()),
                "bundle-1".to_string(),
            )
            .await
            .expect("pending update failed");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            [block],
            "The indexer must be handed the caller's block, not one derived from the parent."
        );
    }

    /// A confirmed block carrying no snapshots or deltas: it only advances the tip.
    fn confirmed_at(number: u64) -> FeedMessage<BlockHeader> {
        FeedMessage {
            state_msgs: HashMap::from([(
                "fluid".to_string(),
                tycho_client::feed::synchronizer::StateSyncMessage {
                    header: BlockHeader { number, ..Default::default() },
                    ..Default::default()
                },
            )]),
            sync_states: HashMap::new(),
        }
    }

    /// The header stamped onto every delta must come from the pending block too, not from the
    /// confirmed tip. Priced against a tip of 5, a target of 3 tells the two apart.
    #[tokio::test]
    async fn test_stamped_header_comes_from_the_pending_block() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pending_processor = processor(seen);
        pending_processor
            .advance(&confirmed_at(5))
            .expect("advance failed");

        let update = pending_processor
            .generate_pending_update(
                &PendingBlock::new(target_block(3, 1_759_842_947), vec![], HashMap::new()),
                "bundle-1".to_string(),
            )
            .await
            .expect("pending update failed");

        assert_eq!(
            update.update.block_number_or_timestamp, 3,
            "The update must be stamped with the pending block, not the confirmed tip."
        );
    }

    #[tokio::test]
    async fn test_parent_guard_reads_the_pending_blocks_number() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pending_processor = processor(seen.clone());
        let block = target_block(23_526_115, 1_759_842_947);

        let result = pending_processor
            .generate_pending_update(
                &PendingBlock::new(block, vec![], HashMap::new()),
                "bundle-1".to_string(),
            )
            .await;

        match result {
            Err(PendingError::ParentNotYetConfirmed { needed, current }) => {
                assert_eq!(needed, 23_526_114);
                assert_eq!(current, 0);
            }
            Err(other) => panic!("expected ParentNotYetConfirmed, got {other:?}"),
            Ok(_) => panic!("expected ParentNotYetConfirmed, got a successful update"),
        }
        assert!(
            seen.lock().unwrap().is_empty(),
            "No indexer should run when the parent is not confirmed."
        );
    }
}
