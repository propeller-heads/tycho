use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{format_err, Context, Result};
use async_trait::async_trait;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::Client;
use metrics::gauge;
use prost::Message;
use serde::Deserialize;
use tokio::{
    runtime::Handle,
    sync::{
        mpsc::{self, error::SendError, Receiver, Sender},
        Mutex,
    },
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};
use tycho_common::{
    models::{
        blockchain::BlockAggregatedChanges, Address, Chain, ExtractorIdentity, FinancialType,
        ImplementationType, ProtocolType,
    },
    traits::AccountExtractor,
    Bytes,
};
use tycho_ethereum::{
    rpc::EthereumRpcClient,
    services::{
        account_extractor::EVMAccountExtractor, entrypoint_tracer::tracer::EVMEntrypointService,
        token_pre_processor::EthereumTokenPreProcessor,
    },
};
use tycho_storage::postgres::cache::CachedGateway;

use crate::{
    extractor::{
        chain_state::ChainState,
        dynamic_contract_indexer::{
            dci::DynamicContractIndexer,
            hooks::{hook_dci::UniswapV4HookDCI, hooks_dci_builder::UniswapV4HookDCIBuilder},
        },
        post_processors::POST_PROCESSOR_REGISTRY,
        protocol_cache::ProtocolMemoryCache,
        protocol_extractor::{ExtractorPgGateway, ProtocolExtractor},
        ExtractionError, Extractor, ExtractorExtension, ExtractorMsg,
    },
    pb::sf::substreams::{rpc::v2::BlockScopedData, v1::Package},
    substreams::{
        stream::{BlockResponse, SubstreamsStream},
        SubstreamsEndpoint,
    },
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

/// A trait for a message sender that can be used to subscribe to messages
///
/// Extracted out of the [ExtractorHandle] to allow for easier testing
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
    fn new(id: ExtractorIdentity, control_tx: Sender<ControlMessage>) -> Self {
        Self { id, control_tx }
    }

    pub fn get_id(&self) -> ExtractorIdentity {
        self.id.clone()
    }

    #[instrument(skip(self))]
    pub async fn stop(&self) -> Result<(), ExtractionError> {
        // TODO: send a oneshot along here and wait for it
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
            // TODO: use a better error type that let's us return this as an error.
            Err(_) => panic!("Subscription timed out!"),
        }
    }
}

// Define the SubscriptionsMap type alias
type SubscriptionsMap = HashMap<u64, Sender<ExtractorMsg>>;

pub struct ExtractorRunner {
    extractor: Arc<dyn Extractor>,
    substreams: SubstreamsStream,
    subscriptions: Arc<Mutex<SubscriptionsMap>>,
    next_subscriber_id: u64,
    control_rx: Receiver<ControlMessage>,
    /// Handle of the tokio runtime on which the extraction tasks will be run.
    /// If 'None' the default runtime will be used.
    runtime_handle: Option<Handle>,
    partial_blocks: bool,
}

impl ExtractorRunner {
    pub fn new(
        extractor: Arc<dyn Extractor>,
        substreams: SubstreamsStream,
        subscriptions: Arc<Mutex<SubscriptionsMap>>,
        control_rx: Receiver<ControlMessage>,
        runtime_handle: Option<Handle>,
        partial_blocks: bool,
    ) -> Self {
        ExtractorRunner {
            extractor,
            substreams,
            subscriptions,
            next_subscriber_id: 0,
            control_rx,
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
                        Some(ctrl) = self.control_rx.recv() => {
                            match ctrl {
                                ControlMessage::Stop => {
                                    warn!("Stop signal received; exiting!");
                                    return Ok(false);
                                },
                                ControlMessage::Subscribe(sender) => {
                                    self.subscribe(sender).await;
                                },
                            }
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
                                        Self::propagate_msg(&self.subscriptions, msg).await
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
                                    info!(block=?&undo_signal.last_valid_block,  "Revert requested!");
                                    match self.extractor.handle_revert(undo_signal.clone()).await {
                                        Ok(Some(msg)) => {
                                            trace!("Propagating block undo message.");
                                            Self::propagate_msg(&self.subscriptions, msg).await
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

    #[instrument(skip_all)]
    async fn subscribe(&mut self, sender: Sender<ExtractorMsg>) {
        let subscriber_id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        tracing::Span::current().record("subscriber_id", subscriber_id);
        info!(?subscriber_id, "New subscription");
        self.subscriptions
            .lock()
            .await
            .insert(subscriber_id, sender);
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
    async fn propagate_msg(subscribers: &Arc<Mutex<SubscriptionsMap>>, message: ExtractorMsg) {
        trace!(msg = %message, "Propagating message to subscribers.");
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

#[derive(Debug, Deserialize, Clone)]
pub struct ProtocolTypeConfig {
    name: String,
    financial_type: FinancialType,
}

impl ProtocolTypeConfig {
    pub fn new(name: String, financial_type: FinancialType) -> Self {
        Self { name, financial_type }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ExtractorConfig {
    name: String,
    #[serde(deserialize_with = "deserialize_chain")]
    chain: Chain,
    implementation_type: ImplementationType,
    sync_batch_size: usize,
    start_block: i64,
    stop_block: Option<i64>,
    protocol_types: Vec<ProtocolTypeConfig>,
    spkg: String,
    module_name: String,
    #[serde(default)]
    pub initialized_accounts: Vec<Bytes>,
    #[serde(default)]
    pub initialized_accounts_block: u64,
    #[serde(default)]
    pub post_processor: Option<String>,
    #[serde(default)]
    pub dci_plugin: Option<DCIType>,
}

impl ExtractorConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        chain: Chain,
        implementation_type: ImplementationType,
        sync_batch_size: usize,
        start_block: i64,
        stop_block: Option<i64>,
        protocol_types: Vec<ProtocolTypeConfig>,
        spkg: String,
        module_name: String,
        initialized_accounts: Vec<Bytes>,
        initialized_accounts_block: u64,
        post_processor: Option<String>,
        dci_plugin: Option<DCIType>,
    ) -> Self {
        Self {
            name,
            chain,
            implementation_type,
            sync_batch_size,
            start_block,
            stop_block,
            protocol_types,
            spkg,
            module_name,
            initialized_accounts,
            initialized_accounts_block,
            post_processor,
            dci_plugin,
        }
    }

    /// The chain this extractor runs on, as resolved from config against the custom-chain registry
    /// at parse time.
    pub fn chain(&self) -> Chain {
        self.chain
    }
}

/// Deserializes a chain from a bare name. Built-in names map to their variant; any other name is
/// resolved via [`Chain::custom`] against the process-wide custom-chain registry, so an unknown
/// name fails here. The registry must be installed (from `chains.yaml`) before parsing.
fn deserialize_chain<'de, D>(deserializer: D) -> Result<Chain, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let name = String::deserialize(deserializer)?;
    match Chain::builtin_from_str(&name) {
        Some(chain) => Ok(chain),
        None => Chain::custom(&name).map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DCIType {
    /// RPC DCI plugin - uses the RPC endpoint to fetch the account data
    #[serde(rename = "rpc")]
    RPC,
    /// UniswapV4Hooks DCI plugin - wrapper for the RPC DCI plugin that generates hook entrypoints
    /// for tracing
    UniswapV4Hooks { pool_manager_address: String },
}

pub struct ExtractorBuilder {
    config: ExtractorConfig,
    endpoint_url: String,
    s3_bucket: Option<String>,
    token: String,
    extractor: Option<Arc<dyn Extractor>>,
    database_insert_batch_size: Option<usize>,
    final_block_only: bool,
    partial_blocks: bool,
    /// Handle of the tokio runtime on which the extraction tasks will be run.
    /// If 'None' the default runtime will be used.
    runtime_handle: Option<Handle>,
}

impl ExtractorBuilder {
    pub fn new(
        config: &ExtractorConfig,
        endpoint_url: &str,
        s3_bucket: Option<&str>,
        substreams_api_token: &str,
    ) -> Self {
        Self {
            config: config.clone(),
            endpoint_url: endpoint_url.to_owned(),
            s3_bucket: s3_bucket.map(ToString::to_string),
            token: substreams_api_token.to_string(),
            extractor: None,
            database_insert_batch_size: None,
            final_block_only: false,
            partial_blocks: false,
            runtime_handle: None,
        }
    }

    /// Set the substreams endpoint url
    pub fn endpoint_url(mut self, val: &str) -> Self {
        val.clone_into(&mut self.endpoint_url);
        self
    }

    pub fn module_name(mut self, val: &str) -> Self {
        val.clone_into(&mut self.config.module_name);
        self
    }

    pub fn start_block(mut self, val: i64) -> Self {
        self.config.start_block = val;
        self
    }

    pub fn token(mut self, val: &str) -> Self {
        val.clone_into(&mut self.token);
        self
    }

    pub fn only_final_blocks(mut self) -> Self {
        self.final_block_only = true;
        self
    }

    pub fn set_runtime(mut self, runtime: Handle) -> Self {
        self.runtime_handle = Some(runtime);
        self
    }

    pub fn partial_blocks(mut self, val: bool) -> Self {
        self.partial_blocks = val;
        self
    }

    /// Set the global database insert batch size
    pub fn database_insert_batch_size(mut self, database_insert_batch_size: usize) -> Self {
        self.database_insert_batch_size = Some(database_insert_batch_size);
        self
    }

    #[cfg(test)]
    pub fn set_extractor(mut self, val: Arc<dyn Extractor>) -> Self {
        self.extractor = Some(val);
        self
    }

    async fn ensure_spkg(&self) -> Result<(), ExtractionError> {
        // Pull spkg from s3 and copy it at `spkg_path`
        if !Path::new(&self.config.spkg).exists() {
            download_file_from_s3(
                self.s3_bucket.as_ref().ok_or_else(|| {
                    ExtractionError::Setup(format!(
                        "Missing spkg and s3 bucket config for {}",
                        self.config.spkg
                    ))
                })?,
                &self.config.spkg,
                Path::new(&self.config.spkg),
            )
            .await
            .map_err(|e| {
                ExtractionError::Setup(format!(
                    "Failed to download {} from s3. {}",
                    self.config.spkg, e
                ))
            })?;
        }
        Ok(())
    }

    /// Creates a rpc DynamicContractIndexer with account extractor and tracer configured
    async fn create_rpc_dci(
        rpc_client: &EthereumRpcClient,
        chain: Chain,
        extractor_name: String,
        cached_gw: &CachedGateway,
    ) -> Result<
        DynamicContractIndexer<EVMAccountExtractor, EVMEntrypointService, CachedGateway>,
        ExtractionError,
    > {
        let account_extractor = EVMAccountExtractor::new(rpc_client, chain);

        // Tracer uses dedicated TRACE_RPC_URL if available, and falls back to the main
        // rpc client otherwise.
        let tracer_rpc_client = if let Ok(tracer_rpc_url) = std::env::var("TRACE_RPC_URL") {
            EthereumRpcClient::new(&tracer_rpc_url).map_err(|err| {
                ExtractionError::Setup(format!(
                    "Failed to create RPC client for {tracer_rpc_url}: {err}"
                ))
            })?
        } else {
            rpc_client.clone()
        };

        let max_retries = std::env::var("TRACE_MAX_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);

        let retry_delay_ms = std::env::var("TRACE_RETRY_DELAY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);

        let tracer =
            EVMEntrypointService::new_with_config(&tracer_rpc_client, max_retries, retry_delay_ms);

        let mut rpc_dci = DynamicContractIndexer::new(
            chain,
            extractor_name,
            cached_gw.clone(),
            account_extractor,
            tracer,
        );
        rpc_dci.initialize().await?;

        Ok(rpc_dci)
    }

    pub async fn build(
        mut self,
        chain_state: ChainState,
        cached_gw: &CachedGateway,
        token_pre_processor: &EthereumTokenPreProcessor,
        protocol_cache: &ProtocolMemoryCache,
        rpc_client: &EthereumRpcClient,
    ) -> Result<Self, ExtractionError> {
        let protocol_types = self
            .config
            .protocol_types
            .iter()
            .map(|pt| {
                (
                    pt.name.clone(),
                    ProtocolType::new(
                        pt.name.clone(),
                        pt.financial_type.clone(),
                        None,
                        self.config.implementation_type.clone(),
                    ),
                )
            })
            .collect();

        let gw = ExtractorPgGateway::new(
            &self.config.name,
            self.config.chain,
            self.config.sync_batch_size,
            cached_gw.clone(),
        );

        let post_processor = self
            .config
            .post_processor
            .as_ref()
            .map(|name| {
                POST_PROCESSOR_REGISTRY
                    .get(name)
                    .cloned()
                    .ok_or_else(|| {
                        ExtractionError::Setup(format!(
                            "Post processor '{name}' not found in registry"
                        ))
                    })
            })
            .transpose()?;

        let dci_plugin = if let Some(ref dci_type) = self.config.dci_plugin {
            Some(match dci_type {
                DCIType::RPC => {
                    let rpc_dci = Self::create_rpc_dci(
                        rpc_client,
                        self.config.chain,
                        self.config.name.clone(),
                        cached_gw,
                    )
                    .await?;

                    DCIPlugin::Standard(rpc_dci)
                }
                DCIType::UniswapV4Hooks { pool_manager_address } => {
                    // random address to deploy our mini router to
                    let router_address =
                        Address::from("0x2e234DAe75C793f67A35089C9d99245E1C58470b");
                    let pool_manager = Address::from(pool_manager_address.as_str());

                    let base_dci = Self::create_rpc_dci(
                        rpc_client,
                        self.config.chain,
                        self.config.name.clone(),
                        cached_gw,
                    )
                    .await?;

                    let mut hooks_dci = UniswapV4HookDCIBuilder::new(
                        base_dci,
                        rpc_client,
                        router_address,
                        pool_manager,
                        cached_gw.clone(),
                        self.config.chain,
                    )
                    .pause_after_retries(3)
                    .max_retries(5)
                    .build()?;

                    hooks_dci.initialize().await?;
                    DCIPlugin::UniswapV4Hooks(Box::new(hooks_dci))
                }
            })
        } else {
            None
        };

        let database_insert_batch_size = self
            .database_insert_batch_size
            .unwrap_or_default();

        self.extractor = Some(Arc::new(
            ProtocolExtractor::<ExtractorPgGateway, EthereumTokenPreProcessor, DCIPlugin<_>>::new(
                gw,
                database_insert_batch_size,
                &self.config.name,
                self.config.chain,
                chain_state,
                self.config.name.clone(),
                protocol_cache.clone(),
                protocol_types,
                token_pre_processor.clone(),
                post_processor,
                dci_plugin,
            )
            .await?,
        ));

        Ok(self)
    }

    /// Converts this builder into a ready-to-run ExtractorRunner and its associated handle.
    ///
    /// This method completes the extractor setup process by:
    /// - Ensuring the Substreams package (.spkg) file is available, downloading from S3 if
    ///   necessary
    /// - Creating a Substreams endpoint connection with authentication
    /// - Setting up the data stream with the configured module, block range, and cursor
    /// - Initializing control channels for managing the extractor lifecycle
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - `ExtractorRunner`: The main component that processes blockchain data from the stream
    /// - `ExtractorHandle`: A control interface for stopping the extractor and subscribing to its
    ///   output
    ///
    /// # Errors
    ///
    /// Returns `ExtractionError` if:
    /// - The extractor was not properly configured
    /// - The Substreams package file cannot be accessed or downloaded
    /// - The Substreams endpoint connection cannot be established
    /// - Package decoding fails due to corrupted or invalid data
    #[instrument(name = "extractor_runner_build", skip(self), fields(extractor_id))]
    pub async fn into_runner(self) -> Result<(ExtractorRunner, ExtractorHandle), ExtractionError> {
        let extractor = self
            .extractor
            .clone()
            .expect("Extractor not set");
        let extractor_id = extractor.get_id();

        tracing::Span::current().record("id", format!("{extractor_id}"));

        self.ensure_spkg().await?;

        let content = std::fs::read(&self.config.spkg)
            .context(format_err!("read package from file '{}'", self.config.spkg))
            .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?;
        let spkg = Package::decode(content.as_ref())
            .context("decode command")
            .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?;
        let endpoint = Arc::new(
            SubstreamsEndpoint::new(&self.endpoint_url, Some(self.token))
                .await
                .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?,
        );

        // Determine the start block for the Substreams stream.
        //
        // We never pass a cursor on fresh start (process restart). Instead, we
        // resume from the block after the last one committed to DB. This is safe
        // because we only commit finalized block -1 to the DB. So we know last committed block + 1
        // is finalized.
        let last_block = extractor
            .get_last_processed_block()
            .await;
        // `None` means no blocks have been committed for this protocol yet (fresh
        // indexing), so fall back to the configured start block.
        let start_block = last_block
            .as_ref()
            .map(|b| {
                let next = b
                    .number
                    .checked_add(1)
                    .ok_or_else(|| ExtractionError::Setup("block number overflow".to_string()))?;
                i64::try_from(next)
                    .map_err(|_| ExtractionError::Setup("block number exceeds i64".to_string()))
            })
            .transpose()?
            .unwrap_or(self.config.start_block);

        if let Some(block) = &last_block {
            info!(
                start_block,
                last_committed_block = block.number,
                config_start_block = self.config.start_block,
                "Fresh start: resuming from block after last committed"
            );
        }

        let stream = SubstreamsStream::new(
            endpoint,
            None, // No cursor on fresh start; stream tracks cursor for hot reconnections
            Some(spkg),
            self.config.module_name,
            start_block,
            self.config.stop_block.unwrap_or(0) as u64,
            self.final_block_only,
            extractor_id.to_string(),
            self.partial_blocks,
        );

        let (ctrl_tx, ctrl_rx) = mpsc::channel(128);
        let runner = ExtractorRunner::new(
            extractor,
            stream,
            Arc::new(Mutex::new(HashMap::new())),
            ctrl_rx,
            self.runtime_handle,
            self.partial_blocks,
        );

        Ok((runner, ExtractorHandle::new(extractor_id, ctrl_tx)))
    }
}

async fn download_file_from_s3(
    bucket: &str,
    key: &str,
    download_path: &Path,
) -> anyhow::Result<()> {
    info!("Downloading file from s3: {}/{} to {:?}", bucket, key, download_path);

    let region_provider = RegionProviderChain::default_provider().or_else("eu-central-1");

    let config = aws_config::from_env()
        .region(region_provider)
        .load()
        .await;

    let client = Client::new(&config);

    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await?;

    let data = resp.body.collect().await.unwrap();

    // Ensure the directory exists
    if let Some(parent) = download_path.parent() {
        std::fs::create_dir_all(parent)
            .context(format!("Failed to create directories for {parent:?}"))?;
    }

    std::fs::write(download_path, data.into_bytes()).unwrap();

    Ok(())
}

#[cfg(test)]
mod test {
    use tycho_common::models::blockchain::BlockAggregatedChanges;

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

    #[test]
    fn test_extractor_config_without_dci_plugin() {
        let yaml = r#"
name: uniswap_v2
chain: ethereum
implementation_type: Custom
sync_batch_size: 1000
start_block: 10008300
protocol_types:
  - name: uniswap_v2_pool
    financial_type: Swap
spkg: substreams/ethereum-uniswap-v2/ethereum-uniswap-v2-v0.3.0.spkg
module_name: map_pool_events
"#;

        let config: ExtractorConfig =
            serde_yaml::from_str(yaml).expect("Failed to deserialize YAML");

        // Verify basic fields
        assert_eq!(config.name, "uniswap_v2");

        // Verify DCI plugin is None (optional field)
        assert!(config.dci_plugin.is_none());
    }

    #[test]
    fn test_dci_extractor_config() {
        let yaml = r#"
name: uniswap_v3
chain: ethereum
implementation_type: Custom
sync_batch_size: 1000
start_block: 12369621
protocol_types:
  - name: uniswap_v3_pool
    financial_type: Swap
spkg: substreams/ethereum-uniswap-v3/ethereum-uniswap-v3-logs-only-0.1.1.spkg
module_name: map_protocol_changes
dci_plugin:
  type: rpc
"#;

        let config: ExtractorConfig =
            serde_yaml::from_str(yaml).expect("Failed to deserialize YAML");

        // Verify basic fields
        assert_eq!(config.name, "uniswap_v3");

        // Verify DCI plugin is RPC
        assert!(
            matches!(config.dci_plugin, Some(DCIType::RPC)),
            "Expected RPC DCI plugin but got {:?}",
            config.dci_plugin
        );
    }

    #[test]
    fn test_uniswap_v4_hooks_dci_extractor_config() {
        let yaml = r#"
name: uniswap_v4
chain: ethereum
implementation_type: Custom
sync_batch_size: 1000
start_block: 21688329
protocol_types:
  - name: uniswap_v4_pool
    financial_type: Swap
spkg: substreams/ethereum-uniswap-v4/ethereum-uniswap-v4-v0.2.1.spkg
module_name: map_protocol_changes
dci_plugin:
  type: uniswap_v4_hooks
  router_address: "0x2e234DAe75C793f67A35089C9d99245E1C58470b"
  pool_manager_address: "0x000000000004444c5dc75cB358380D2e3dE08A90"
"#;

        let config: ExtractorConfig =
            serde_yaml::from_str(yaml).expect("Failed to deserialize YAML");

        // Verify basic fields
        assert_eq!(config.name, "uniswap_v4");
        assert_eq!(config.chain, Chain::Ethereum);
        assert_eq!(config.sync_batch_size, 1000);
        assert_eq!(config.start_block, 21688329);

        // Verify protocol types
        assert_eq!(config.protocol_types.len(), 1);
        assert_eq!(config.protocol_types[0].name, "uniswap_v4_pool");

        // Verify DCI plugin configuration
        let dci_plugin = config
            .dci_plugin
            .expect("Expected dci_plugin to be set");
        match dci_plugin {
            DCIType::UniswapV4Hooks { pool_manager_address } => {
                assert_eq!(pool_manager_address, "0x000000000004444c5dc75cB358380D2e3dE08A90");
            }
            _ => {
                panic!("Expected UniswapV4Hooks DCI plugin but got RPC");
            }
        }
    }

    fn one_msg() -> ExtractorMsg {
        Arc::new(BlockAggregatedChanges::default())
    }

    #[tokio::test]
    async fn test_process_block_data_partial_blocks_disabled() {
        // When partial_blocks is false: handle_tick_scoped_data is called with data as-is;
        // collect_and_process_full_block is not called. One message from handle_tick_scoped_data.
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
        // When partial_blocks is true and is_last_partial == true: handle_tick_scoped_data with
        // data, then collect_and_process_full_block. Two messages (one from each).
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
        // When partial_blocks is true and message is full block: handle_tick_scoped_data
        // receives data as-is; runner adds a partial copy of the returned message.
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
        // When partial_blocks is true and message is a non-final partial: only
        // handle_tick_scoped_data; collect_and_process_full_block is not called. One message.
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

    #[tokio::test]
    async fn test_extractor_runner_builder_fresh_start_no_db_state() {
        // No DB state: get_last_processed_block returns None, so the stream
        // starts from the config start_block with no cursor.
        let mut mock_extractor = MockExtractor::new();
        mock_extractor
            .expect_get_last_processed_block()
            .returning(|| None);
        mock_extractor
            .expect_get_id()
            .returning(ExtractorIdentity::default);

        // Build the ExtractorRunnerBuilder
        let extractor = Arc::new(mock_extractor);
        let builder = ExtractorBuilder::new(
            &ExtractorConfig {
                name: "test_module".to_owned(),
                implementation_type: ImplementationType::Vm,
                protocol_types: vec![ProtocolTypeConfig {
                    name: "test_module_pool".to_owned(),
                    financial_type: FinancialType::Swap,
                }],
                spkg: "./test/spkg/substreams-ethereum-quickstart-v1.0.0.spkg".to_owned(),
                module_name: "test_module".to_owned(),
                ..Default::default()
            },
            "https://mainnet.eth.streamingfast.io",
            None,
            "test_token",
        )
        .token("test_token")
        .set_extractor(extractor);

        // Run the builder
        let (runner, _handle) = builder.into_runner().await.unwrap();

        // Wait for the handle to complete
        match runner.run().await {
            Ok(_) => {
                info!("ExtractorRunnerBuilder completed successfully");
            }
            Err(err) => {
                error!(error = %err, "ExtractorRunnerBuilder failed");
                panic!("ExtractorRunnerBuilder failed");
            }
        }
    }

    #[tokio::test]
    async fn test_start_block_no_db_state() {
        use crate::substreams::mock::start_mock_substreams;

        let (captured, addr) = start_mock_substreams().await;

        let mut mock_extractor = MockExtractor::new();
        mock_extractor
            .expect_get_last_processed_block()
            .returning(|| None);
        mock_extractor
            .expect_get_id()
            .returning(ExtractorIdentity::default);

        let extractor = Arc::new(mock_extractor);
        let builder = ExtractorBuilder::new(
            &ExtractorConfig {
                name: "test_module".to_owned(),
                implementation_type: ImplementationType::Vm,
                protocol_types: vec![ProtocolTypeConfig {
                    name: "test_module_pool".to_owned(),
                    financial_type: FinancialType::Swap,
                }],
                spkg: "./test/spkg/substreams-ethereum-quickstart-v1.0.0.spkg".to_owned(),
                module_name: "test_module".to_owned(),
                start_block: 42,
                ..Default::default()
            },
            &format!("http://{addr}"),
            None,
            "test_token",
        )
        .token("test_token")
        .set_extractor(extractor);

        let (runner, _handle) = builder.into_runner().await.unwrap();
        let handle = runner.run();
        handle.await.unwrap().unwrap();

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one gRPC request");
        assert_eq!(requests[0].start_block_num, 42);
        assert!(requests[0].start_cursor.is_empty(), "fresh start should have no cursor");
    }

    #[tokio::test]
    async fn test_start_block_with_db_state() {
        use chrono::NaiveDateTime;
        use tycho_common::models::blockchain::Block;

        use crate::substreams::mock::start_mock_substreams;

        let (captured, addr) = start_mock_substreams().await;

        let mut mock_extractor = MockExtractor::new();
        mock_extractor
            .expect_get_last_processed_block()
            .returning(|| {
                Some(Block::new(
                    1000,
                    Chain::Ethereum,
                    vec![0x01].into(),
                    vec![0x00].into(),
                    NaiveDateTime::default(),
                ))
            });
        mock_extractor
            .expect_get_id()
            .returning(ExtractorIdentity::default);

        let extractor = Arc::new(mock_extractor);
        let builder = ExtractorBuilder::new(
            &ExtractorConfig {
                name: "test_module".to_owned(),
                implementation_type: ImplementationType::Vm,
                protocol_types: vec![ProtocolTypeConfig {
                    name: "test_module_pool".to_owned(),
                    financial_type: FinancialType::Swap,
                }],
                spkg: "./test/spkg/substreams-ethereum-quickstart-v1.0.0.spkg".to_owned(),
                module_name: "test_module".to_owned(),
                start_block: 500,
                ..Default::default()
            },
            &format!("http://{addr}"),
            None,
            "test_token",
        )
        .token("test_token")
        .set_extractor(extractor);

        let (runner, _handle) = builder.into_runner().await.unwrap();
        let handle = runner.run();
        handle.await.unwrap().unwrap();

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one gRPC request");
        assert_eq!(
            requests[0].start_block_num, 1001,
            "should use last_committed + 1, not config's start_block"
        );
        assert!(requests[0].start_cursor.is_empty(), "fresh start should have no cursor");
    }
}
