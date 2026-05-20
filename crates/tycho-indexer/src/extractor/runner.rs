use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use metrics::gauge;
use tokio::{
    runtime::Handle,
    sync::{
        mpsc::{self, error::SendError, Receiver, Sender},
        oneshot, Mutex,
    },
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges},
        ExtractorIdentity,
    },
    traits::AccountExtractor,
};
use tycho_ethereum::services::entrypoint_tracer::tracer::EVMEntrypointService;
use tycho_storage::postgres::cache::CachedGateway;

use crate::{
    extractor::{
        dynamic_contract_indexer::{
            dci::DynamicContractIndexer, hooks::hook_dci::UniswapV4HookDCI,
        },
        DeltaCommand, ExtractionError, Extractor, ExtractorExtension, ExtractorMsg,
    },
    pb::sf::substreams::rpc::v2::BlockScopedData,
    substreams::stream::{BlockResponse, SubstreamsStream},
};

/// Enum to handle both standard DCI and UniswapV4 Hook DCI
#[allow(clippy::large_enum_variant)]
pub(crate) enum DCIPlugin<AE: AccountExtractor + Send + Sync> {
    Standard(DynamicContractIndexer<AE, EVMEntrypointService, CachedGateway>),
    UniswapV4Hooks(Box<UniswapV4HookDCI<AE, EVMEntrypointService, CachedGateway>>),
}

#[async_trait]
impl<AE: AccountExtractor + Send + Sync> ExtractorExtension for DCIPlugin<AE> {
    async fn process_block_update(
        &mut self,
        block_changes: &mut crate::extractor::models::BlockChanges,
    ) -> Result<(), ExtractionError> {
        match self {
            DCIPlugin::Standard(dci) => {
                dci.process_block_update(block_changes)
                    .await
            }
            DCIPlugin::UniswapV4Hooks(hooks_dci) => {
                hooks_dci
                    .process_block_update(block_changes)
                    .await
            }
        }
    }

    async fn process_revert(
        &mut self,
        target_block: &tycho_common::models::BlockHash,
    ) -> Result<(), ExtractionError> {
        match self {
            DCIPlugin::Standard(dci) => dci.process_revert(target_block).await,
            DCIPlugin::UniswapV4Hooks(hooks_dci) => {
                hooks_dci
                    .process_revert(target_block)
                    .await
            }
        }
    }

    fn cache_size(&self) -> usize {
        match self {
            DCIPlugin::Standard(dci) => dci.cache_size(),
            DCIPlugin::UniswapV4Hooks(hooks_dci) => hooks_dci.cache_size(),
        }
    }

    fn emit_cache_metrics(&self, chain: &str, extractor: &str) {
        match self {
            DCIPlugin::Standard(dci) => dci.emit_cache_metrics(chain, extractor),
            DCIPlugin::UniswapV4Hooks(hooks_dci) => hooks_dci.emit_cache_metrics(chain, extractor),
        }
    }
}

pub enum ControlMessage {
    Stop,
    Subscribe(Sender<ExtractorMsg>),
}

/// A trait for a message sender that can be used to subscribe to messages.
///
/// Extracted out of [`ExtractorHandle`] to allow for easier testing.
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>>;
}

#[derive(Clone)]
pub struct ExtractorHandle {
    id: ExtractorIdentity,
    control_tx: Sender<ControlMessage>,
}

impl ExtractorHandle {
    pub fn new(id: ExtractorIdentity, control_tx: Sender<ControlMessage>) -> Self {
        Self { id, control_tx }
    }

    pub fn get_id(&self) -> ExtractorIdentity {
        self.id.clone()
    }

    #[instrument(skip(self))]
    pub async fn stop(&self) -> Result<(), ExtractionError> {
        self.control_tx
            .send(ControlMessage::Stop)
            .await
            .map_err(|err| ExtractionError::Unknown(err.to_string()))
    }
}

#[async_trait]
impl MessageSender for ExtractorHandle {
    #[instrument(skip(self))]
    async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
        let (tx, rx) = mpsc::channel(16);
        // Define a timeout duration
        let timeout_duration = std::time::Duration::from_secs(5); // 5 seconds timeout

        // Wrap the send operation with a timeout
        let send_result = tokio::time::timeout(
            timeout_duration,
            self.control_tx
                .send(ControlMessage::Subscribe(tx)),
        )
        .await;

        match send_result {
            Ok(Ok(())) => Ok(rx),
            Ok(Err(e)) => Err(e),
            // TODO: use a better error type that lets us return this as an error.
            Err(_) => panic!("Subscription timed out!"),
        }
    }
}

// Define the SubscriptionsMap type alias
pub(crate) type SubscriptionsMap = HashMap<u64, Sender<ExtractorMsg>>;

pub struct ExtractorRunner {
    extractor: Arc<dyn Extractor>,
    substreams: SubstreamsStream,
    /// WS subscribers — managed by the supervisor, shared across restarts.
    ws_subscriptions: Arc<Mutex<SubscriptionsMap>>,
    /// Dedicated channel for PendingDeltasBuffer — survives restarts.
    pending_deltas_tx: Option<Sender<DeltaCommand>>,
    /// Oneshot stop signal from the supervisor.
    stop_rx: oneshot::Receiver<()>,
    /// Handle of the tokio runtime on which the extraction tasks will be run.
    /// If `None` the default runtime will be used.
    runtime_handle: Option<Handle>,
    partial_blocks: bool,
}

impl ExtractorRunner {
    pub fn new(
        extractor: Arc<dyn Extractor>,
        substreams: SubstreamsStream,
        ws_subscriptions: Arc<Mutex<SubscriptionsMap>>,
        pending_deltas_tx: Option<Sender<DeltaCommand>>,
        stop_rx: oneshot::Receiver<()>,
        runtime_handle: Option<Handle>,
        partial_blocks: bool,
    ) -> Self {
        ExtractorRunner {
            extractor,
            substreams,
            ws_subscriptions,
            pending_deltas_tx,
            stop_rx,
            runtime_handle,
            partial_blocks,
        }
    }

    pub fn run(mut self) -> JoinHandle<Result<(), ExtractionError>> {
        info!("Extractor {} started!", self.extractor.get_id());

        let runtime = self
            .runtime_handle
            .clone()
            .unwrap_or_else(|| Handle::current());

        runtime.spawn(async move {
            let id = self.extractor.get_id();
            // Track the number of partials received for the current block != partial_index.
            let mut partials_in_block: u32 = 0;
            loop {
                // this is the main info span of an extractor
                let loop_span = info_span!(
                    parent: None,  // don't attach this to the parent (builder) span to keep spans short
                    "extractor",
                    extractor_id = %id,
                    sf_trace_id = tracing::field::Empty,
                    block_number = tracing::field::Empty,
                    otel.status_code = tracing::field::Empty,
                );

                let should_continue = async {
                    tokio::select! {
                        _ = &mut self.stop_rx => {
                            warn!("Stop signal received; exiting!");
                            return Ok(false);
                        }
                        val = self.substreams.next().instrument(info_span!("substreams_waiting")) => {
                            match val {
                                None => {
                                    error!("stream ended");
                                    tracing::Span::current().record("otel.status_code", "error");
                                    return Err(ExtractionError::SubstreamsError(format!("{id}: stream ended")));
                                }
                                Some(Ok(BlockResponse::New(data))) => {
                                    let block_number = data.clock.as_ref().map(|v| v.number).unwrap_or(0);
                                    tracing::Span::current().record("block_number", block_number);
                                    gauge!(
                                        "extractor_current_block_number",
                                        "chain" => id.chain.to_string(),
                                        "extractor" => id.name.to_string()
                                    ).set(block_number as f64);

                                    if data.is_partial {
                                        partials_in_block += 1;
                                    }

                                    if data.is_last_partial == Some(true) || data.partial_index.is_none() {
                                        gauge!(
                                            "extractor_partials_per_block",
                                            "chain" => id.chain.to_string(),
                                            "extractor" => id.name.to_string()
                                        )
                                        .set(partials_in_block as f64);
                                        partials_in_block = 0;
                                    }

                                    // Start measuring block processing time
                                    let start_time = std::time::Instant::now();

                                    let msgs = Self::process_block_data(
                                        self.extractor.as_ref(),
                                        &data,
                                        self.partial_blocks,
                                    )
                                    .await
                                    .map_err(|err| {
                                        error!(error = %err, "Error while processing block data");
                                        tracing::Span::current().record("otel.status_code", "error");
                                        err
                                    })?;
                                    for msg in msgs {
                                        trace!("Propagating block data message.");
                                        Self::propagate_msg(
                                            &self.ws_subscriptions,
                                            self.pending_deltas_tx.as_ref(),
                                            msg,
                                        ).await;
                                    }

                                    let duration_ms = start_time.elapsed().as_millis() as f64;
                                    let block_type = match (data.is_partial, data.is_last_partial) {
                                        (false, _) => "full",
                                        (true, Some(true)) => "final_partial",
                                        (true, _) => "partial",
                                    };

                                    gauge!(
                                        "block_processing_time_ms",
                                        "chain" => id.chain.to_string(),
                                        "extractor" => id.name.to_string(),
                                        "block_type" => block_type
                                    ).set(duration_ms);
                                }
                                Some(Ok(BlockResponse::Undo(undo_signal))) => {
                                    partials_in_block = 0;
                                    info!(block=?&undo_signal.last_valid_block, "Revert requested!");
                                    match self.extractor.handle_revert(undo_signal.clone()).await {
                                        Ok(Some(msg)) => {
                                            trace!("Propagating block undo message.");
                                            Self::propagate_msg(
                                                &self.ws_subscriptions,
                                                self.pending_deltas_tx.as_ref(),
                                                msg,
                                            ).await;
                                        }
                                        Ok(None) => {
                                            trace!("No message to propagate.");
                                        }
                                        Err(err) => {
                                            error!(error = %err, "Error while processing revert!");
                                            tracing::Span::current().record("otel.status_code", "error");
                                            return Err(err);
                                        }
                                    }
                                }
                                Some(Ok(BlockResponse::Ended)) => {
                                    tracing::Span::current().record("otel.status_code", "ok");
                                    return Ok(false);
                                }
                                Some(Err(err)) => {
                                    error!(error = %err, "Stream terminated with error.");
                                    tracing::Span::current().record("otel.status_code", "error");
                                    return Err(ExtractionError::SubstreamsError(err.to_string()));
                                }
                            };
                        }
                    }

                    tracing::Span::current().record("otel.status_code", "ok");
                    Ok(true) // Continue the loop
                }
                    .instrument(loop_span)
                    .await?;

                if !should_continue {
                    break Ok(());
                }
            }
        })
    }

    /// Processes block-scoped data from the stream: always sends the input to the extractor,
    /// then optionally adds a partial copy of the message (for full blocks with partials enabled)
    /// and/or the result of collect_and_process_full_block (for final partials).
    #[instrument(skip_all, fields(partial_blocks_enabled, is_partial = data.is_partial))]
    async fn process_block_data(
        extractor: &dyn Extractor,
        data: &BlockScopedData,
        partial_blocks_enabled: bool,
    ) -> Result<Vec<ExtractorMsg>, ExtractionError> {
        let mut msgs = Vec::new();

        match extractor
            .handle_tick_scoped_data(data.clone())
            .await
        {
            Ok(Some(msg)) => {
                if partial_blocks_enabled && !data.is_partial {
                    // Full block and partial blocks enabled: add a partial copy of the message
                    msgs.push(Self::as_partial_message(&msg));
                }
                msgs.push(msg);
            }
            Ok(None) => {
                trace!("No message to propagate.");
            }
            Err(e) => return Err(e),
        }

        let is_final_partial = data.is_partial && data.is_last_partial == Some(true);
        if partial_blocks_enabled && is_final_partial {
            // Final partial: Create full block message from cached partials
            match extractor
                .collect_and_process_full_block(
                    data.cursor.clone(),
                    data.final_block_height,
                    data.clock.clone(),
                )
                .await
            {
                Ok(Some(msg)) => msgs.push(msg),
                Ok(None) => {
                    trace!("No message to propagate.");
                }
                Err(e) => return Err(e),
            }
        }

        Ok(msgs)
    }

    /// Returns a copy of the message with partial_block_index set to Some(0).
    fn as_partial_message(msg: &ExtractorMsg) -> ExtractorMsg {
        let mut copy: BlockAggregatedChanges = (**msg).clone();
        copy.partial_block_index = Some(0);
        Arc::new(copy)
    }

    // TODO: add message tracing_id to the log
    #[instrument(skip_all, fields(subscriber_count))]
    async fn propagate_msg(
        subscribers: &Arc<Mutex<SubscriptionsMap>>,
        pending_deltas_tx: Option<&Sender<DeltaCommand>>,
        message: ExtractorMsg,
    ) {
        trace!(msg = %message, "Propagating message to subscribers.");

        if let Some(tx) = pending_deltas_tx {
            if let Err(err) = tx.send(DeltaCommand::Block(message.clone())).await {
                error!(error = %err, "Failed to send to PendingDeltas channel");
            }
        }

        // TODO: rename variable here instead
        let arced_message = message;

        let mut to_remove = Vec::new();

        // Lock the subscribers HashMap for exclusive access
        let mut subscribers = subscribers.lock().await;
        tracing::Span::current().record("subscriber_count", subscribers.len());

        for (counter, sender) in subscribers.iter_mut() {
            match sender.send(arced_message.clone()).await {
                Ok(_) => {
                    // Message sent successfully
                    trace!(subscriber_id = %counter, "Message sent successfully.");
                }
                Err(err) => {
                    // Receiver has been dropped, mark for removal
                    to_remove.push(*counter);
                    error!(error = %err, counter, "Error while sending message to subscriber");
                }
            }
        }

        // Remove inactive subscribers
        for counter in to_remove {
            subscribers.remove(&counter);
            debug!("Subscriber {} has been dropped", counter);
        }
    }
}

/// Returns the block number to start streaming from.
///
/// If a block has already been committed to the DB, resumes from the next one.
/// Otherwise falls back to `config_start_block`.
pub(crate) fn compute_start_block(
    last_block: Option<&Block>,
    config_start_block: i64,
) -> Result<i64, ExtractionError> {
    match last_block {
        None => Ok(config_start_block),
        Some(block) => {
            let next = block
                .number
                .checked_add(1)
                .ok_or_else(|| ExtractionError::Setup("block number overflow".to_string()))?;
            i64::try_from(next)
                .map_err(|_| ExtractionError::Setup("block number exceeds i64".to_string()))
        }
    }
}

#[cfg(test)]
mod test {
    use tycho_common::models::{blockchain::BlockAggregatedChanges, Chain};

    use super::*;
    use crate::{extractor::MockExtractor, pb::sf::substreams::v1::Clock};

    /// Builds minimal BlockScopedData for runner message-selection tests.
    fn make_block_scoped_data(
        is_partial: bool,
        partial_index: Option<u32>,
        is_last_partial: Option<bool>,
    ) -> BlockScopedData {
        BlockScopedData {
            output: None,
            clock: None,
            cursor: String::new(),
            final_block_height: 0,
            debug_map_outputs: vec![],
            debug_store_outputs: vec![],
            attestation: String::new(),
            is_partial,
            partial_index,
            is_last_partial,
        }
    }

    fn one_msg() -> ExtractorMsg {
        Arc::new(BlockAggregatedChanges::default())
    }

    #[tokio::test]
    async fn test_process_block_data_partial_blocks_disabled() {
        let data = make_block_scoped_data(false, None, None);
        let mut mock = MockExtractor::new();
        mock.expect_handle_tick_scoped_data()
            .once()
            .returning(|inp: BlockScopedData| {
                assert!(!inp.is_partial, "data must be sent as full block");
                Ok(Some(one_msg()))
            });
        let extractor: Arc<dyn Extractor> = Arc::new(mock);

        let msgs = ExtractorRunner::process_block_data(extractor.as_ref(), &data, false)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn test_process_block_data_final_partial() {
        let data = make_block_scoped_data(true, Some(2), Some(true));
        let mut mock = MockExtractor::new();
        mock.expect_handle_tick_scoped_data()
            .once()
            .returning(|inp: BlockScopedData| {
                assert_eq!(inp.partial_index, Some(2));
                assert_eq!(inp.is_last_partial, Some(true));
                Ok(Some(one_msg()))
            });
        mock.expect_collect_and_process_full_block()
            .once()
            .returning(|_cursor: String, _final_block_height: u64, _clock: Option<Clock>| {
                Ok(Some(one_msg()))
            });
        let extractor: Arc<dyn Extractor> = Arc::new(mock);

        let msgs = ExtractorRunner::process_block_data(extractor.as_ref(), &data, true)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn test_process_block_data_full_block() {
        let data = make_block_scoped_data(false, None, None);
        let mut mock = MockExtractor::new();
        mock.expect_handle_tick_scoped_data()
            .once()
            .returning(|inp: BlockScopedData| {
                assert!(!inp.is_partial, "data is sent as full block");
                Ok(Some(one_msg()))
            });
        let extractor: Arc<dyn Extractor> = Arc::new(mock);

        let msgs = ExtractorRunner::process_block_data(extractor.as_ref(), &data, true)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].partial_block_index, Some(0));
        assert!(msgs[1].partial_block_index.is_none());
    }

    #[tokio::test]
    async fn test_process_block_data_middle_partial() {
        let data = make_block_scoped_data(true, Some(1), Some(false));
        let mut mock = MockExtractor::new();
        mock.expect_handle_tick_scoped_data()
            .once()
            .returning(|inp: BlockScopedData| {
                assert_eq!(inp.partial_index, Some(1));
                assert_eq!(inp.is_last_partial, Some(false));
                Ok(Some(one_msg()))
            });
        let extractor: Arc<dyn Extractor> = Arc::new(mock);

        let msgs = ExtractorRunner::process_block_data(extractor.as_ref(), &data, true)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn test_compute_start_block_no_db_state() {
        // No committed block: fall back to the config start block.
        assert_eq!(compute_start_block(None, 42), Ok(42));
    }

    #[test]
    fn test_compute_start_block_with_db_state() {
        use chrono::NaiveDateTime;

        let block = Block::new(
            1000,
            Chain::Ethereum,
            vec![0x01].into(),
            vec![0x00].into(),
            NaiveDateTime::default(),
        );
        // Should resume from last_committed + 1, ignoring config start block.
        assert_eq!(compute_start_block(Some(&block), 500), Ok(1001));
    }
}
