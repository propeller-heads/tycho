use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{format_err, Context};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::Client;
use prost::Message;
use serde::Deserialize;
use tokio::{
    runtime::Handle,
    sync::{mpsc::Sender, oneshot, Mutex},
};
use tracing::info;
use tycho_common::{
    models::{Address, Chain, ExtractorIdentity, FinancialType, ImplementationType, ProtocolType},
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
            dci::DynamicContractIndexer, hooks::hooks_dci_builder::UniswapV4HookDCIBuilder,
        },
        post_processors::POST_PROCESSOR_REGISTRY,
        protocol_cache::ProtocolMemoryCache,
        protocol_extractor::{ExtractorPgGateway, ProtocolExtractor},
        runner::{compute_start_block, DCIPlugin, ExtractorRunner, SubscriptionsMap},
        DeltaCommand, ExtractionError, Extractor,
    },
    pb::sf::substreams::v1::Package,
    substreams::{stream::SubstreamsStream, SubstreamsEndpoint},
};

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

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DCIType {
    /// RPC DCI plugin — uses the RPC endpoint to fetch account data.
    #[serde(rename = "rpc")]
    RPC,
    /// UniswapV4Hooks DCI plugin — wraps RPC DCI and generates hook entry points for tracing.
    UniswapV4Hooks { pool_manager_address: String },
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ExtractorConfig {
    name: String,
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
    pub dci_plugin: Option<DCIType>,
    #[serde(default)]
    post_processor: Option<String>,
    #[serde(default)]
    pub(super) max_restarts: Option<u32>,
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
        max_restarts: Option<u32>,
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
            max_restarts,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Holds the config and all dependencies needed to build an extractor from scratch.
///
/// Designed for repeated use: each call to `build_runner` produces a fresh `ProtocolExtractor`
/// with a new `ReorgBuffer` and DCI plugin — suitable for restart after failure.
///
/// Reused across restarts:
/// - `protocol_cache`: populated once at construction; Arc-based so cloning is cheap and all runs
///   share the same live cache. The TTL mechanism refreshes stale entries automatically.
/// - `chain_state`: estimated once at construction (block number via RPC); `Copy` so each run gets
///   its own copy at no cost.
/// - `cached_gw`: each `build_runner` call creates a fresh instance via `new_instance()` (fresh
///   `open_tx` and LRU cache; shared write channel to `DBCacheWriteExecutor`).
/// - `token_pre_processor`, `rpc_client`: stateless RPC wrappers.
pub struct ExtractorFactory {
    pub(super) config: ExtractorConfig,
    endpoint_url: String,
    s3_bucket: Option<String>,
    token: String,
    chain: Chain,
    cached_gw: CachedGateway,
    token_pre_processor: EthereumTokenPreProcessor,
    rpc_client: EthereumRpcClient,
    database_insert_batch_size: usize,
    partial_blocks: bool,
    runtime_handle: Option<Handle>,
    protocol_cache: ProtocolMemoryCache,
    chain_state: ChainState,
}

impl ExtractorFactory {
    /// Creates the factory, making one RPC call to fetch the current block number and populating
    /// the protocol cache from the database.
    ///
    /// Both `chain_state` and `protocol_cache` are reused across all subsequent restarts.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        config: ExtractorConfig,
        endpoint_url: String,
        s3_bucket: Option<String>,
        token: String,
        chain: Chain,
        cached_gw: CachedGateway,
        token_pre_processor: EthereumTokenPreProcessor,
        rpc_client: EthereumRpcClient,
        database_insert_batch_size: usize,
        partial_blocks: bool,
        runtime_handle: Option<Handle>,
    ) -> Result<Self, ExtractionError> {
        let block_number = rpc_client
            .get_block_number()
            .await
            .map_err(|e| ExtractionError::Setup(format!("Failed to get block number: {e}")))?;
        let chain_state = ChainState::new(
            chrono::Local::now().naive_utc(),
            block_number,
            chain.block_time().ceil() as i64, // round up
        );

        let protocol_cache = ProtocolMemoryCache::new(
            config.chain,
            chrono::Duration::seconds(900),
            Arc::new(cached_gw.clone()),
        );
        protocol_cache.populate().await?;

        Ok(Self {
            config,
            endpoint_url,
            s3_bucket,
            token,
            chain,
            cached_gw,
            token_pre_processor,
            rpc_client,
            database_insert_batch_size,
            partial_blocks,
            runtime_handle,
            protocol_cache,
            chain_state,
        })
    }

    /// Builds a fresh, ready-to-run [`ExtractorRunner`].
    ///
    /// Creates a fresh gateway instance, constructs the DCI plugin if configured, and establishes
    /// the Substreams stream from the last committed block (or the config start block on first
    /// run). The protocol cache and chain state are reused from factory construction.
    pub async fn build_runner(
        &self,
        ws_subscriptions: Arc<Mutex<SubscriptionsMap>>,
        pending_deltas_tx: Option<Sender<DeltaCommand>>,
        stop_rx: oneshot::Receiver<()>,
    ) -> Result<ExtractorRunner, ExtractionError> {
        let fresh_gw = self.cached_gw.new_instance();

        // Protocol types from config.
        let protocol_types: HashMap<String, ProtocolType> = self
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

        // Storage gateway for this extractor.
        let gw = ExtractorPgGateway::new(
            &self.config.name,
            self.config.chain,
            self.config.sync_batch_size,
            fresh_gw.clone(),
        );

        // Optional post-processor.
        let post_processor = self
            .config
            .post_processor
            .as_deref()
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

        // Optional DCI plugin.
        let dci_plugin = match self.config.dci_plugin.as_ref() {
            None => None,
            Some(DCIType::RPC) => {
                let rpc_dci = Self::create_rpc_dci(
                    &self.rpc_client,
                    self.config.chain,
                    self.config.name.clone(),
                    &fresh_gw,
                )
                .await?;
                Some(DCIPlugin::Standard(rpc_dci))
            }
            Some(DCIType::UniswapV4Hooks { pool_manager_address }) => {
                // random address to deploy our mini router to
                let router_address = Address::from("0x2e234DAe75C793f67A35089C9d99245E1C58470b");
                let pool_manager = Address::from(pool_manager_address.as_str());

                let base_dci = Self::create_rpc_dci(
                    &self.rpc_client,
                    self.config.chain,
                    self.config.name.clone(),
                    &fresh_gw,
                )
                .await?;

                let mut hooks_dci = UniswapV4HookDCIBuilder::new(
                    base_dci,
                    &self.rpc_client,
                    router_address,
                    pool_manager,
                    fresh_gw.clone(),
                    self.config.chain,
                )
                .pause_after_retries(3)
                .max_retries(5)
                .build()?;

                hooks_dci.initialize().await?;
                Some(DCIPlugin::UniswapV4Hooks(Box::new(hooks_dci)))
            }
        };

        // Build the protocol extractor.
        let extractor = Arc::new(
            ProtocolExtractor::<ExtractorPgGateway, EthereumTokenPreProcessor, DCIPlugin<_>>::new(
                gw,
                self.database_insert_batch_size,
                &self.config.name,
                self.config.chain,
                self.chain_state,
                self.config.name.clone(),
                self.protocol_cache.clone(),
                protocol_types,
                self.token_pre_processor.clone(),
                post_processor,
                dci_plugin,
            )
            .await?,
        );

        // Ensure the spkg file is present (download from S3 if needed).
        ensure_spkg(&self.config.spkg, self.s3_bucket.as_deref()).await?;

        let content = std::fs::read(&self.config.spkg)
            .with_context(|| format_err!("read package from file '{}'", self.config.spkg))
            .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?;
        let spkg = Package::decode(content.as_ref())
            .context("decode spkg")
            .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?;

        let endpoint = Arc::new(
            SubstreamsEndpoint::new(&self.endpoint_url, Some(self.token.clone()))
                .await
                .map_err(|err| ExtractionError::SubstreamsError(err.to_string()))?,
        );

        // Determine the start block.
        //
        // We resume from (last_committed + 1) rather than using a cursor so that a restarted
        // extractor always replays at least from the last finalized block. The cursor is only
        // maintained inside SubstreamsStream for hot reconnections within a single run.
        let last_block = extractor
            .get_last_processed_block()
            .await;
        let start_block = compute_start_block(last_block.as_ref(), self.config.start_block)?;
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
            None, // No cursor on fresh start.
            Some(spkg),
            self.config.module_name.clone(),
            start_block,
            self.config.stop_block.unwrap_or(0) as u64,
            false, // final_block_only: not exposed in config, always false.
            extractor.get_id().to_string(),
            self.partial_blocks,
        );

        Ok(ExtractorRunner::new(
            extractor,
            stream,
            ws_subscriptions,
            pending_deltas_tx,
            stop_rx,
            self.runtime_handle.clone(),
            self.partial_blocks,
        ))
    }

    pub fn extractor_id(&self) -> ExtractorIdentity {
        ExtractorIdentity::new(self.chain, &self.config.name)
    }

    /// Creates a RPC-based `DynamicContractIndexer` with account extractor and tracer configured.
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
}

async fn ensure_spkg(spkg_path: &str, s3_bucket: Option<&str>) -> Result<(), ExtractionError> {
    if !Path::new(spkg_path).exists() {
        download_file_from_s3(
            s3_bucket.ok_or_else(|| {
                ExtractionError::Setup(format!("Missing spkg and s3 bucket config for {spkg_path}"))
            })?,
            spkg_path,
            Path::new(spkg_path),
        )
        .await
        .map_err(|e| {
            ExtractionError::Setup(format!("Failed to download {spkg_path} from s3. {e}"))
        })?;
    }
    Ok(())
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

    let data = resp
        .body
        .collect()
        .await
        .with_context(|| format!("Failed to read S3 response body for {key}"))?;

    if let Some(parent) = download_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directories for {parent:?}"))?;
    }

    std::fs::write(download_path, data.into_bytes())
        .with_context(|| format!("Failed to write {download_path:?}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(config.name, "uniswap_v2");
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

        assert_eq!(config.name, "uniswap_v3");
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

        assert_eq!(config.name, "uniswap_v4");
        assert_eq!(config.chain, Chain::Ethereum);
        assert_eq!(config.sync_batch_size, 1000);
        assert_eq!(config.start_block, 21688329);
        assert_eq!(config.protocol_types.len(), 1);
        assert_eq!(config.protocol_types[0].name, "uniswap_v4_pool");

        let dci_plugin = config
            .dci_plugin
            .expect("Expected dci_plugin to be set");
        match dci_plugin {
            DCIType::UniswapV4Hooks { pool_manager_address } => {
                assert_eq!(pool_manager_address, "0x000000000004444c5dc75cB358380D2e3dE08A90");
            }
            _ => panic!("Expected UniswapV4Hooks DCI plugin but got RPC"),
        }
    }
}
