use std::{
    cmp::max,
    collections::{HashMap, HashSet},
    env,
    time::Duration,
};

use thiserror::Error;
use tokio::{sync::mpsc::Receiver, task::JoinHandle};
use tracing::{info, warn};
use tycho_common::{
    dto::{PaginationLimits, ProtocolSystemsRequestBody},
    models::{Chain, ExtractorIdentity},
};

use crate::{
    deltas::DeltasClient,
    feed::{
        component_tracker::ComponentFilter, synchronizer::ProtocolStateSynchronizer, BlockHeader,
        BlockSynchronizer, BlockSynchronizerError, FeedMessage,
    },
    rpc::{HttpRPCClientOptions, ProtocolSystemsParams, RPCClient},
    HttpRPCClient, WsDeltasClient,
};

#[derive(Error, Debug)]
pub enum StreamError {
    #[error("Error during stream set up: {0}")]
    SetUpError(String),

    #[error("WebSocket client connection error: {0}")]
    WebSocketConnectionError(String),

    #[error("BlockSynchronizer error: {0}")]
    BlockSynchronizerError(String),
}

#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum RetryConfiguration {
    Constant(ConstantRetryConfiguration),
}

impl RetryConfiguration {
    pub fn constant(max_attempts: u64, cooldown: Duration) -> Self {
        RetryConfiguration::Constant(ConstantRetryConfiguration { max_attempts, cooldown })
    }
}

#[derive(Clone, Debug)]
pub struct ConstantRetryConfiguration {
    max_attempts: u64,
    cooldown: Duration,
}

pub struct TychoStreamBuilder {
    tycho_url: String,
    chain: Chain,
    exchanges: HashMap<String, ComponentFilter>,
    blocklisted_ids: HashSet<String>,
    block_time: u64,
    timeout: u64,
    startup_timeout: Duration,
    max_missed_blocks: u64,
    state_sync_retry_config: RetryConfiguration,
    websockets_retry_config: RetryConfiguration,
    no_state: bool,
    auth_key: Option<String>,
    no_tls: bool,
    include_tvl: bool,
    compression: bool,
    partial_blocks: bool,
    max_messages: Option<usize>,
}

impl TychoStreamBuilder {
    /// Creates a new `TychoStreamBuilder` with the given Tycho URL and blockchain network.
    /// Initializes the builder with default values for block time and timeout based on the chain.
    pub fn new(tycho_url: &str, chain: Chain) -> Self {
        let (block_time, timeout, max_missed_blocks) = Self::default_timing(&chain);
        Self {
            tycho_url: tycho_url.to_string(),
            chain,
            exchanges: HashMap::new(),
            blocklisted_ids: HashSet::new(),
            block_time,
            timeout,
            startup_timeout: Duration::from_secs(block_time * max_missed_blocks),
            max_missed_blocks,
            state_sync_retry_config: RetryConfiguration::constant(
                32,
                Duration::from_secs(max(block_time / 4, 2)),
            ),
            websockets_retry_config: RetryConfiguration::constant(
                128,
                Duration::from_secs(max(block_time / 6, 1)),
            ),
            no_state: false,
            auth_key: None,
            no_tls: true,
            include_tvl: false,
            compression: true,
            partial_blocks: false,
            max_messages: None,
        }
    }

    /// Returns the default block_time, timeout and max_missed_blocks values for the given
    /// blockchain network.
    fn default_timing(chain: &Chain) -> (u64, u64, u64) {
        match chain {
            Chain::Ethereum => (12, 36, 50),
            Chain::Starknet => (2, 8, 50),
            Chain::ZkSync => (3, 12, 50),
            Chain::Arbitrum => (1, 2, 100), // Typically closer to 0.25s
            Chain::Base => (2, 12, 50),
            Chain::Bsc => (1, 12, 50),
            Chain::Unichain => (1, 10, 100),
            Chain::Polygon => (2, 12, 50), // ~2s block time
            Chain::Custom(cfg) => (cfg.block_time_secs, cfg.block_time_secs * 3, 50),
        }
    }

    /// Adds an exchange and its corresponding filter to the Tycho client.
    pub fn exchange(mut self, name: &str, filter: ComponentFilter) -> Self {
        self.exchanges
            .insert(name.to_string(), filter);
        self
    }

    /// Sets the block time for the Tycho client.
    pub fn block_time(mut self, block_time: u64) -> Self {
        self.block_time = block_time;
        self
    }

    /// Sets the timeout duration for network operations.
    pub fn timeout(mut self, timeout: u64) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn max_missed_blocks(mut self, max_missed_blocks: u64) -> Self {
        self.max_missed_blocks = max_missed_blocks;
        self
    }

    pub fn websockets_retry_config(mut self, retry_config: &RetryConfiguration) -> Self {
        self.websockets_retry_config = retry_config.clone();
        self.warn_on_potential_timing_issues();
        self
    }

    pub fn state_synchronizer_retry_config(mut self, retry_config: &RetryConfiguration) -> Self {
        self.state_sync_retry_config = retry_config.clone();
        self.warn_on_potential_timing_issues();
        self
    }

    fn warn_on_potential_timing_issues(&self) {
        let (RetryConfiguration::Constant(state_config), RetryConfiguration::Constant(ws_config)) =
            (&self.state_sync_retry_config, &self.websockets_retry_config);

        if ws_config.cooldown >= state_config.cooldown {
            warn!(
                "Websocket cooldown should be < than state syncronizer cooldown \
                to avoid spending retries due to disconnected websocket."
            )
        }
    }

    /// Configures the client to exclude state updates from the stream.
    pub fn no_state(mut self, no_state: bool) -> Self {
        self.no_state = no_state;
        self
    }

    /// Sets the API key for authenticating with the Tycho server.
    ///
    /// Optionally you can set the TYCHO_AUTH_TOKEN env var instead. Make sure to set no_tsl
    /// to false if you do this.
    pub fn auth_key(mut self, auth_key: Option<String>) -> Self {
        self.auth_key = auth_key;
        self.no_tls = false;
        self
    }

    /// Disables TLS/SSL for the connection, using `http` and `ws` protocols.
    pub fn no_tls(mut self, no_tls: bool) -> Self {
        self.no_tls = no_tls;
        self
    }

    /// Configures the client to include TVL in the stream.
    ///
    /// If set to true, this will increase start-up time due to additional requests.
    pub fn include_tvl(mut self, include_tvl: bool) -> Self {
        self.include_tvl = include_tvl;
        self
    }

    /// Disables compression for RPC and WebSocket communication.
    /// By default, messages are compressed using zstd.
    pub fn disable_compression(mut self) -> Self {
        self.compression = false;
        self
    }

    /// Enables the client to receive partial block updates (flashblocks).
    pub fn enable_partial_blocks(mut self) -> Self {
        self.partial_blocks = true;
        self
    }

    /// Stops the stream after emitting this many messages. Useful for testing or
    /// triggering a periodic restart after a fixed number of blocks.
    pub fn max_messages(mut self, n: usize) -> Self {
        self.max_messages = Some(n);
        self
    }

    /// Overrides the maximum number of retry attempts for state synchronizer startup.
    /// The retry cooldown is derived from the chain's block time and is not affected.
    pub fn max_retries(mut self, max_retries: u64) -> Self {
        let cooldown = match &self.state_sync_retry_config {
            RetryConfiguration::Constant(c) => c.cooldown,
        };
        self.state_sync_retry_config = RetryConfiguration::constant(max_retries, cooldown);
        self
    }

    /// Blocklist specific component IDs across all registered exchanges.
    ///
    /// Blocklisted components are never tracked, regardless of TVL or other
    /// filter criteria.
    pub fn blocklisted_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.blocklisted_ids.extend(ids);
        self
    }

    /// Builds and starts the Tycho client, connecting to the Tycho server and
    /// setting up the synchronization of exchange components.
    pub async fn build(
        self,
    ) -> Result<
        (JoinHandle<()>, Receiver<Result<FeedMessage<BlockHeader>, BlockSynchronizerError>>),
        StreamError,
    > {
        if self.exchanges.is_empty() {
            return Err(StreamError::SetUpError(
                "At least one exchange must be registered.".to_string(),
            ));
        }

        // Attempt to read the authentication key from the environment variable if not provided
        let auth_key = self
            .auth_key
            .clone()
            .or_else(|| env::var("TYCHO_AUTH_TOKEN").ok());

        info!("Running with version: {}", option_env!("CARGO_PKG_VERSION").unwrap_or("unknown"));

        // Determine the URLs based on the TLS setting
        let (tycho_ws_url, tycho_rpc_url) = if self.no_tls {
            info!("Using non-secure connection: ws:// and http://");
            let tycho_ws_url = format!("ws://{}", self.tycho_url);
            let tycho_rpc_url = format!("http://{}", self.tycho_url);
            (tycho_ws_url, tycho_rpc_url)
        } else {
            info!("Using secure connection: wss:// and https://");
            let tycho_ws_url = format!("wss://{}", self.tycho_url);
            let tycho_rpc_url = format!("https://{}", self.tycho_url);
            (tycho_ws_url, tycho_rpc_url)
        };

        // Initialize the WebSocket client
        let ws_client = match &self.websockets_retry_config {
            RetryConfiguration::Constant(config) => WsDeltasClient::new_with_reconnects(
                &tycho_ws_url,
                auth_key.as_deref(),
                config.max_attempts,
                config.cooldown,
            ),
        }
        .map_err(|e| StreamError::SetUpError(e.to_string()))?;
        let rpc_client = HttpRPCClient::new(
            &tycho_rpc_url,
            HttpRPCClientOptions::new()
                .with_auth_key(auth_key)
                .with_compression(self.compression),
        )
        .map_err(|e| StreamError::SetUpError(e.to_string()))?;
        let ws_jh = ws_client
            .connect()
            .await
            .map_err(|e| StreamError::WebSocketConnectionError(e.to_string()))?;

        // Create and configure the BlockSynchronizer
        let mut block_sync = BlockSynchronizer::new(
            Duration::from_secs(self.block_time),
            Duration::from_secs(self.timeout),
            self.max_missed_blocks,
        );
        if let Some(n) = self.max_messages {
            block_sync.max_messages(n);
        }

        let requested: HashSet<_> = self.exchanges.keys().cloned().collect();
        let info = ProtocolSystemsInfo::fetch(&rpc_client, self.chain, &requested).await;
        info.log_other_available();
        let dci_protocols = info.dci_protocols;

        // Register each exchange with the BlockSynchronizer
        for (name, filter) in self
            .exchanges
            .into_iter()
            .map(|(name, filter)| {
                let filter = if self.blocklisted_ids.is_empty() {
                    filter
                } else {
                    filter.blocklist(self.blocklisted_ids.iter().cloned())
                };
                (name, filter)
            })
        {
            info!("Registering exchange: {}", name);
            let id = ExtractorIdentity { chain: self.chain, name: name.clone() };
            let uses_dci = dci_protocols.contains(&name);
            let sync = match &self.state_sync_retry_config {
                RetryConfiguration::Constant(retry_config) => ProtocolStateSynchronizer::new(
                    id.clone(),
                    true,
                    filter,
                    retry_config.max_attempts,
                    retry_config.cooldown,
                    !self.no_state,
                    self.include_tvl,
                    self.compression,
                    rpc_client.clone(),
                    ws_client.clone(),
                    self.block_time + self.timeout,
                )
                .with_dci(uses_dci)
                .with_partial_blocks(self.partial_blocks),
            };
            block_sync = block_sync.register_synchronizer(id, sync);
        }

        // Start the BlockSynchronizer and monitor for disconnections
        let (sync_jh, rx) = block_sync
            .run()
            .await
            .map_err(|e| StreamError::BlockSynchronizerError(e.to_string()))?;

        // Monitor WebSocket and BlockSynchronizer futures
        let handle = tokio::spawn(async move {
            tokio::select! {
                res = ws_jh => {
                    let _ = res.map_err(|e| StreamError::WebSocketConnectionError(e.to_string()));
                }
                res = sync_jh => {
                    res.map_err(|e| StreamError::BlockSynchronizerError(e.to_string())).unwrap();
                }
            }
            if let Err(e) = ws_client.close().await {
                warn!(?e, "Failed to close WebSocket client");
            }
        });

        Ok((handle, rx))
    }
}

/// Result of fetching protocol systems: which protocols use DCI, and which
/// available protocols on the server were not requested by the client.
pub struct ProtocolSystemsInfo {
    pub dci_protocols: HashSet<String>,
    pub other_available: HashSet<String>,
}

impl ProtocolSystemsInfo {
    /// Fetches protocol systems from the server and classifies them: which use DCI,
    /// and which are available but not in `requested_exchanges`.
    pub async fn fetch(
        rpc_client: &HttpRPCClient,
        chain: Chain,
        requested_exchanges: &HashSet<String>,
    ) -> Self {
        let page_size =
            ProtocolSystemsRequestBody::effective_max_page_size(rpc_client.compression());
        let params = ProtocolSystemsParams::new(chain).with_pagination(0, page_size);
        let response = rpc_client
            .get_protocol_systems(params)
            .await
            .map_err(|e| {
                warn!(
                    "Failed to fetch protocol systems: {e}. Skipping protocol availability check."
                );
                e
            })
            .ok();

        let Some(response) = response else {
            return Self { dci_protocols: HashSet::new(), other_available: HashSet::new() };
        };

        if response.total() > page_size {
            warn!(
                "Server has {} protocol systems but only {} were fetched (page_size={page_size}). \
                 Availability info may be incomplete.",
                response.total(),
                response.data().protocol_systems().len(),
            );
        }

        let available: HashSet<_> = response
            .data()
            .protocol_systems()
            .iter()
            .cloned()
            .collect();
        let other_available = available
            .difference(requested_exchanges)
            .cloned()
            .collect();
        let mut dci_protocols: HashSet<String> = response
            .data()
            .dci_protocols()
            .iter()
            .cloned()
            .collect();

        // TODO(ENG-5302): Remove this fallback once all environments serve
        // the `dci_protocols` field. Old servers omit the field, which
        // deserialises as empty — causing clients to skip entrypoint
        // fetches for DCI protocols.
        if dci_protocols.is_empty() {
            const LEGACY_DCI: &[&str] = &[
                "uniswap_v4_hooks",
                "vm:curve",
                "vm:balancer_v2",
                "vm:balancer_v3",
                "fluid_v1",
                "erc4626",
            ];
            for name in requested_exchanges {
                if LEGACY_DCI.contains(&name.as_str()) {
                    dci_protocols.insert(name.clone());
                }
            }
        }

        Self { dci_protocols, other_available }
    }

    /// Logs the protocols available on the server that the client didn't subscribe to.
    pub fn log_other_available(&self) {
        if !self.other_available.is_empty() {
            let names: Vec<_> = self
                .other_available
                .iter()
                .cloned()
                .collect();
            info!("Other available protocols: {}", names.join(", "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_configuration_constant() {
        let config = RetryConfiguration::constant(5, Duration::from_secs(10));
        match config {
            RetryConfiguration::Constant(c) => {
                assert_eq!(c.max_attempts, 5);
                assert_eq!(c.cooldown, Duration::from_secs(10));
            }
        }
    }

    #[test]
    fn test_stream_builder_retry_configs() {
        let mut builder = TychoStreamBuilder::new("localhost:4242", Chain::Ethereum);
        let ws_config = RetryConfiguration::constant(10, Duration::from_secs(2));
        let state_config = RetryConfiguration::constant(20, Duration::from_secs(5));

        builder = builder
            .websockets_retry_config(&ws_config)
            .state_synchronizer_retry_config(&state_config);

        // Verify configs are stored correctly by checking they match expected values
        match (&builder.websockets_retry_config, &builder.state_sync_retry_config) {
            (RetryConfiguration::Constant(ws), RetryConfiguration::Constant(state)) => {
                assert_eq!(ws.max_attempts, 10);
                assert_eq!(ws.cooldown, Duration::from_secs(2));
                assert_eq!(state.max_attempts, 20);
                assert_eq!(state.cooldown, Duration::from_secs(5));
            }
        }
    }

    #[test]
    fn test_default_stream_builder() {
        let builder = TychoStreamBuilder::new("localhost:4242", Chain::Ethereum);
        assert!(builder.compression, "Compression should be enabled by default.");
        assert!(!builder.partial_blocks, "partial_blocks should be disabled by default.");
    }

    #[tokio::test]
    async fn test_no_exchanges() {
        let receiver = TychoStreamBuilder::new("localhost:4242", Chain::Ethereum)
            .auth_key(Some("my_api_key".into()))
            .build()
            .await;
        assert!(receiver.is_err(), "Client should fail to build when no exchanges are registered.");
    }

    #[ignore = "require tycho gateway"]
    #[tokio::test]
    async fn test_simple_build() {
        let token = env::var("TYCHO_AUTH_TOKEN").unwrap();
        let receiver = TychoStreamBuilder::new("tycho-beta.propellerheads.xyz", Chain::Ethereum)
            .exchange("uniswap_v2", ComponentFilter::with_tvl_range(100.0, 100.0))
            .auth_key(Some(token))
            .build()
            .await;

        dbg!(&receiver);

        assert!(receiver.is_ok(), "Client should build successfully with exchanges registered.");
    }
}
