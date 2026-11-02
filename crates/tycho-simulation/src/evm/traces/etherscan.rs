use std::{
    borrow::Cow,
    collections::BTreeMap,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy::{json_abi::JsonAbi, primitives::Address};
use alloy_chains;
use foundry_block_explorers::{
    contract::{ContractMetadata, Metadata},
    errors::EtherscanError,
    Client,
};
use futures::{
    stream::{Stream, StreamExt},
    task::{Context, Poll},
};

use crate::evm::traces::config::TraceConfig;

// Type alias to reduce complexity
type EtherscanFuture = Pin<
    Box<dyn futures::Future<Output = (Address, Result<ContractMetadata, EtherscanError>)> + Send>,
>;
use tokio::time::Interval;

/// An address identified by Etherscan
pub struct EtherscanAddress {
    /// The address
    pub address: Address,
    /// The label for the address
    pub label: Option<String>,
    /// The contract name
    pub contract: Option<String>,
    /// The ABI of the contract
    pub abi: Option<Cow<'static, JsonAbi>>,
}

/// Etherscan identifier for trace addresses
#[expect(
    clippy::partial_pub_fields,
    reason = "public API; making every field public or private is a breaking change"
)]
pub struct EtherscanIdentifier {
    /// The Etherscan client
    client: Arc<Client>,
    /// Tracks whether the API key was marked as invalid
    invalid_api_key: Arc<AtomicBool>,
    /// Cached contract metadata
    pub contracts: BTreeMap<Address, Metadata>,
}

impl EtherscanIdentifier {
    /// Creates a new Etherscan identifier
    pub fn new(
        config: &TraceConfig,
    ) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        // Don't use Etherscan if offline or no API key
        if config.offline || !config.can_use_etherscan() {
            return Ok(None);
        }

        let api_key = config
            .etherscan_api_key
            .as_ref()
            .unwrap();
        let client = Client::new(convert_chain(&config.chain), api_key.clone())?;

        // Use the client as-is (API URL configuration is handled by chain)

        Ok(Some(Self {
            client: Arc::new(client),
            invalid_api_key: Arc::new(AtomicBool::new(false)),
            contracts: BTreeMap::new(),
        }))
    }

    /// Identify addresses using Etherscan
    pub async fn identify_addresses(&mut self, addresses: &[Address]) -> Vec<EtherscanAddress> {
        if self
            .invalid_api_key
            .load(Ordering::Relaxed) ||
            addresses.is_empty()
        {
            return Vec::new();
        }

        tracing::trace!(target: "evm::traces::etherscan", "identify {} addresses", addresses.len());

        let mut identities = Vec::new();
        let mut fetcher = EtherscanFetcher::new(
            self.client.clone(),
            Duration::from_secs(1), // 1 second timeout for rate limiting
            5,                      // max 5 concurrent requests
            Arc::clone(&self.invalid_api_key),
        );

        for &address in addresses {
            if let Some(metadata) = self.contracts.get(&address) {
                identities.push(Self::identify_from_metadata(address, metadata));
            } else {
                fetcher.push(address);
            }
        }

        // Fetch unknown contracts
        let mut fetched_stream = Box::pin(fetcher);
        while let Some((address, metadata)) = fetched_stream.next().await {
            let identity = Self::identify_from_metadata(address, &metadata);
            self.contracts.insert(address, metadata);
            identities.push(identity);
        }

        identities
    }

    fn identify_from_metadata(address: Address, metadata: &Metadata) -> EtherscanAddress {
        let label = metadata.contract_name.clone();
        let abi = metadata.abi().ok().map(Cow::Owned);
        EtherscanAddress { address, label: Some(label.clone()), contract: Some(label), abi }
    }
}

/// Convert tycho chain to foundry chain
fn convert_chain(chain: &tycho_common::models::Chain) -> alloy_chains::Chain {
    use tycho_common::models::Chain;
    match chain {
        Chain::Ethereum => alloy_chains::Chain::from_id(1),
        Chain::Base => alloy_chains::Chain::from_id(8453),
        Chain::Arbitrum => alloy_chains::Chain::from_id(42161),
        Chain::Unichain => alloy_chains::Chain::from_id(1291), // Unichain testnet
        Chain::Starknet |
        Chain::ZkSync |
        Chain::Bsc |
        Chain::Polygon |
        Chain::Plasma |
        Chain::Robinhood |
        Chain::Arc |
        Chain::Custom(_) |
        _ => alloy_chains::Chain::from_id(1), // Default to mainnet
    }
}

/// Rate-limited Etherscan fetcher
struct EtherscanFetcher {
    client: Arc<Client>,
    timeout: Duration,
    backoff: Option<Interval>,
    concurrency: usize,
    queue: Vec<Address>,
    in_progress: futures::stream::FuturesUnordered<EtherscanFuture>,
    invalid_api_key: Arc<AtomicBool>,
}

impl EtherscanFetcher {
    fn new(
        client: Arc<Client>,
        timeout: Duration,
        concurrency: usize,
        invalid_api_key: Arc<AtomicBool>,
    ) -> Self {
        Self {
            client,
            timeout,
            backoff: None,
            concurrency,
            queue: Vec::new(),
            in_progress: futures::stream::FuturesUnordered::new(),
            invalid_api_key,
        }
    }

    fn push(&mut self, address: Address) {
        self.queue.push(address);
    }

    fn queue_next_reqs(&mut self) {
        while self.in_progress.len() < self.concurrency {
            let Some(addr) = self.queue.pop() else { break };
            let client = Arc::clone(&self.client);
            self.in_progress
                .push(Box::pin(async move {
                    tracing::trace!(target: "traces::etherscan", ?addr, "fetching info");
                    let res = client.contract_source_code(addr).await;
                    (addr, res)
                }));
        }
    }
}

impl Stream for EtherscanFetcher {
    type Item = (Address, Metadata);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        loop {
            // Handle backoff timer
            if let Some(mut backoff) = pin.backoff.take() {
                match backoff.poll_tick(cx) {
                    Poll::Pending => {
                        pin.backoff = Some(backoff);
                        return Poll::Pending;
                    }
                    Poll::Ready(_) => {
                        // Backoff completed, continue
                    }
                }
            }

            pin.queue_next_reqs();

            let mut made_progress = false;
            match pin.in_progress.poll_next_unpin(cx) {
                Poll::Pending => {}
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some((addr, res))) => {
                    made_progress = true;
                    match res {
                        Ok(mut metadata) => {
                            if let Some(item) = metadata.items.pop() {
                                return Poll::Ready(Some((addr, item)));
                            }
                        }
                        Err(EtherscanError::RateLimitExceeded) => {
                            tracing::warn!(target: "traces::etherscan", "rate limit exceeded");
                            pin.backoff = Some(tokio::time::interval(pin.timeout));
                            pin.queue.push(addr); // Retry
                        }
                        Err(EtherscanError::InvalidApiKey) => {
                            tracing::warn!(target: "traces::etherscan", "invalid api key");
                            pin.invalid_api_key
                                .store(true, Ordering::Relaxed);
                            return Poll::Ready(None);
                        }
                        Err(EtherscanError::BlockedByCloudflare) => {
                            tracing::warn!(target: "traces::etherscan", "blocked by cloudflare");
                            pin.invalid_api_key
                                .store(true, Ordering::Relaxed);
                            return Poll::Ready(None);
                        }
                        Err(err) => {
                            tracing::warn!(target: "traces::etherscan", "etherscan error: {:?}", err);
                        }
                    }
                }
            }

            if !made_progress {
                return Poll::Pending;
            }
        }
    }
}
