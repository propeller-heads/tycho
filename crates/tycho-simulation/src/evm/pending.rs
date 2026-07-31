use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use thiserror::Error;
use tokio::sync::{mpsc::UnboundedReceiver, watch};
use tycho_client::feed::{synchronizer::Snapshot, BlockHeader, FeedMessage};
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, TxInput},
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
    /// Returned when `generate_pending_update` is called before the parent block of
    /// `target_block` has been confirmed. Use
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
/// #     mut pending: tycho_simulation::evm::pending::PendingBlockProcessor,
/// #     txs: &[tycho_common::models::blockchain::TxInput],
/// #     target_header: tycho_client::feed::BlockHeader,
/// # ) {
/// pending
///     .subscribe_confirmed_block()
///     .wait_for(|&n| n >= target_header.number - 1)
///     .await
///     .expect("stream closed");
/// let update = pending
///     .generate_pending_update(txs, target_header, "bundle-1".to_string())
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

    /// Simulates `txs` against the confirmed parent state of `target_block` and returns an
    /// ephemeral [`Update`].
    ///
    /// Drains any confirmed blocks that have arrived since the last call, then immediately
    /// checks whether the parent block (`target_block - 1`) is available. If not, returns
    /// [`PendingError::ParentNotYetConfirmed`] — **no blocking**. Use
    /// [`subscribe_confirmed_block`](Self::subscribe_confirmed_block) to wait for the right
    /// block before calling.
    ///
    /// Neither the indexers' internal state nor the decoder's confirmed pool states are
    /// mutated. Calling this twice with the same arguments returns identical results.
    ///
    /// # Parameters
    /// * `txs` — candidate bundle in execution order; failed transactions are skipped.
    /// * `target_header` — header of the block being built. Its `number` is used for the
    ///   parent-block guard; the full header is forwarded to `apply_deltas_ephemeral` so that block
    ///   number and timestamp are injected into each state delta.
    /// * `label` — opaque caller-supplied tag stamped onto the returned [`PendingUpdate`]. Use it
    ///   to associate the result with a specific bundle or evaluation context.
    pub async fn generate_pending_update(
        &mut self,
        txs: &[TxInput],
        target_header: BlockHeader,
        label: String,
    ) -> Result<PendingUpdate, PendingError> {
        // Drain any confirmed blocks that have arrived since our last call.
        while let Ok(msg) = self.block_rx.try_recv() {
            self.advance_inner(&msg)?;
        }

        let parent = target_header.number.saturating_sub(1);
        if self.current_confirmed_block < parent {
            return Err(PendingError::ParentNotYetConfirmed {
                needed: parent,
                current: self.current_confirmed_block,
            });
        }

        let mut pending_deltas: HashMap<String, BlockAggregatedChanges> = HashMap::new();
        for (extractor, indexer) in &mut self.indexers {
            let changes = indexer.generate_deltas(txs);
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
