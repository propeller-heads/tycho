use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
    time::SystemTime,
};

use alloy::primitives::Address;
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use reqwest::Client;
use tokio::time::{interval, timeout, Duration};
use tracing::{error, info, warn};
use tycho_common::{
    models::{
        protocol::{GetAmountOutParams, ProtocolComponent, ProtocolComponentState},
        Chain,
    },
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::{
    rfq::{
        client::RFQClient,
        errors::RFQError,
        models::TimestampHeader,
        protocols::metric::models::{
            MetricBidAskResponse, MetricMetadata, PaginatedMetadataResponse,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
};

static METRIC_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

/// Page size for the paginated metadata endpoint. The API clamps `count` to `[1, 500]`.
const METADATA_PAGE_SIZE: u32 = 500;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricClient {
    chain: Chain,
    metadata_endpoint: String,
    // Prefix ending at /public/v1/evm/{chain_id}; pool-specific endpoints are derived from it.
    chain_endpoint: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    #[serde(skip_serializing, default)]
    api_key: Option<String>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl MetricClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:metric";

    pub fn new(
        chain: Chain,
        tokens: HashSet<Bytes>,
        tvl: f64,
        base_url: String,
        api_key: Option<String>,
        poll_time: Duration,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        let chain_id = chain_to_chain_id(chain)?;
        let base_url = base_url.trim_end_matches('/');
        let chain_endpoint = format!("{base_url}/public/v1/evm/{chain_id}");
        Ok(Self {
            chain,
            metadata_endpoint: format!("{chain_endpoint}/metadata"),
            chain_endpoint,
            tokens,
            tvl,
            api_key,
            poll_time,
            quote_timeout,
        })
    }

    fn http_client(&self) -> &Client {
        &METRIC_HTTP_CLIENT
    }

    pub fn create_component_with_state(
        &self,
        component_id: String,
        metadata: &MetricMetadata,
        bid_ask: &MetricBidAskResponse,
        tvl: f64,
    ) -> ComponentWithState {
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "metric_pool".to_string(),
            chain: self.chain,
            tokens: vec![metadata.token0.clone(), metadata.token1.clone()],
            contract_addresses: Vec::new(),
            static_attributes: HashMap::new(),
            ..Default::default()
        };

        let mut attributes = HashMap::new();

        let entries: [(&str, Vec<u8>); 6] = [
            ("bid_adj", bid_ask.bid_adj.as_bytes().to_vec()),
            ("ask_adj", bid_ask.ask_adj.as_bytes().to_vec()),
            (
                "total_token0_available",
                bid_ask
                    .total_token0_available
                    .clone()
                    .unwrap_or_default()
                    .into_bytes(),
            ),
            (
                "total_token1_available",
                bid_ask
                    .total_token1_available
                    .clone()
                    .unwrap_or_default()
                    .into_bytes(),
            ),
            (
                "server_ts",
                bid_ask
                    .server_ts
                    .to_string()
                    .into_bytes(),
            ),
            ("depth", serde_json::to_vec(&bid_ask.depth).unwrap_or_default()),
        ];

        for (key, bytes) in entries {
            attributes.insert(key.to_string(), bytes.into());
        }

        ComponentWithState {
            state: ProtocolComponentState::new(&component_id, attributes, HashMap::new()),
            component: protocol_component,
            component_tvl: Some(tvl),
            entrypoints: vec![],
        }
    }

    /// Fetches every configured pool by paging through the metadata endpoint until the API reports
    /// no next page.
    async fn fetch_metadata(&self) -> Result<Vec<MetricMetadata>, RFQError> {
        let mut pools = Vec::new();
        let mut offset: u64 = 0;

        loop {
            let response = self
                .http_client()
                .get(&self.metadata_endpoint)
                .header("accept", "application/json")
                .query(&[("count", METADATA_PAGE_SIZE.to_string()), ("offset", offset.to_string())])
                .send()
                .await
                .map_err(|e| {
                    RFQError::ConnectionError(format!("Failed to fetch Metric metadata: {e}"))
                })?;

            if !response.status().is_success() {
                return Err(RFQError::ConnectionError(format!(
                    "Metric metadata HTTP error {}: {}",
                    response.status(),
                    response
                        .text()
                        .await
                        .unwrap_or_default()
                )));
            }

            let page: PaginatedMetadataResponse = response.json().await.map_err(|e| {
                RFQError::ParsingError(format!("Failed to parse Metric metadata response: {e}"))
            })?;

            let page_len = page.data.len();
            pools.extend(page.data);

            // Stop when the API reports the last page, returns nothing, or fails to advance the
            // offset (defensive guard against an infinite loop).
            match page.next_offset {
                Some(next) if page_len > 0 && next > offset => offset = next,
                _ => break,
            }
        }

        Ok(pools)
    }

    async fn fetch_bid_ask(&self, pool: &Bytes) -> Result<MetricBidAskResponse, RFQError> {
        let endpoint =
            format!("{}/{}/bid_ask", self.chain_endpoint, bytes_to_address_string(pool)?);
        let mut request = self
            .http_client()
            .get(endpoint)
            .header("accept", "application/json");

        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }

        let response = timeout(self.quote_timeout, request.send())
            .await
            .map_err(|_| {
                RFQError::ConnectionError(format!(
                    "Metric bid/ask request timed out after {} seconds",
                    self.quote_timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                RFQError::ConnectionError(format!("Failed to fetch Metric bid/ask: {e}"))
            })?;

        if !response.status().is_success() {
            return Err(RFQError::ConnectionError(format!(
                "Metric bid/ask HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse Metric bid/ask response: {e}"))
        })
    }

    fn find_pool<'a>(
        &self,
        metadata: &'a [MetricMetadata],
        params: &GetAmountOutParams,
    ) -> Result<&'a MetricMetadata, RFQError> {
        metadata
            .iter()
            .find(|pool| {
                (params.token_in == pool.token0 && params.token_out == pool.token1) ||
                    (params.token_in == pool.token1 && params.token_out == pool.token0)
            })
            .ok_or_else(|| {
                RFQError::QuoteNotFound(format!(
                    "Metric pool not found for {} -> {}",
                    params.token_in, params.token_out
                ))
            })
    }
}

#[async_trait]
impl RFQClient for MetricClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_components: HashMap<String, ComponentWithState> = HashMap::new();
            let mut ticker = interval(client.poll_time);

            info!("Starting Metric polling every {} seconds", client.poll_time.as_secs());
            loop {
                ticker.tick().await;

                let metadata = match client.fetch_metadata().await {
                    Ok(metadata) => metadata,
                    Err(e) => {
                        error!("Failed to fetch Metric metadata: {}", e);
                        continue;
                    }
                };

                let mut new_components = HashMap::new();
                for pool in &metadata {
                    if !client.tokens.is_empty() &&
                        (!client.tokens.contains(&pool.token0) ||
                            !client.tokens.contains(&pool.token1))
                    {
                        continue;
                    }

                    // v1 metadata carries the fiat TVL directly, so no cross-pool price
                    // normalization is needed.
                    let tvl = pool.tvl_fiat.unwrap_or(0.0);
                    if tvl < client.tvl {
                        continue;
                    }

                    let bid_ask = match client.fetch_bid_ask(&pool.pool_address).await {
                        Ok(bid_ask) => bid_ask,
                        Err(e) => {
                            warn!(
                                "Failed to fetch Metric bid/ask for pool {}: {}",
                                pool.pool_address, e
                            );
                            continue;
                        }
                    };
                    if !bid_ask.is_quotable() {
                        continue;
                    }

                    let component_id = pool.pool_address.to_string();
                    new_components.insert(
                        component_id.clone(),
                        client.create_component_with_state(component_id, pool, &bid_ask, tvl),
                    );
                }

                let removed_components: HashMap<String, ProtocolComponent> = current_components
                    .iter()
                    .filter(|(id, _)| !new_components.contains_key(*id))
                    .map(|(id, component)| (id.clone(), component.component.clone()))
                    .collect();

                current_components = new_components.clone();
                let timestamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(|_| RFQError::ParsingError("SystemTime before UNIX EPOCH".to_string()))?
                    .as_secs();

                yield Ok(("metric".to_string(), StateSyncMessage {
                    header: TimestampHeader { timestamp },
                    snapshots: Snapshot { states: new_components, vm_storage: HashMap::new() },
                    deltas: None,
                    removed_components,
                }));
            }
        })
    }

    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let metadata = self.fetch_metadata().await?;
        // Validates that a pool exists for the requested pair.
        self.find_pool(&metadata, params)?;

        // The v1 heartbeat updates the oracle on-chain every block, so no signed oracle-update args
        // are relayed with the swap. The binding quote therefore carries no quote attributes.
        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: params.amount_in.clone(),
            amount_out: BigUint::default(),
            quote_attributes: HashMap::new(),
        })
    }
}

fn chain_to_chain_id(chain: Chain) -> Result<u64, RFQError> {
    match chain {
        Chain::Ethereum => Ok(1),
        Chain::Base => Ok(8453),
        unsupported => Err(RFQError::FatalError(format!(
            "Metric does not support chain in this integration: {unsupported:?}"
        ))),
    }
}

fn bytes_to_address_string(address: &Bytes) -> Result<String, RFQError> {
    if address.len() != 20 {
        return Err(RFQError::InvalidInput(format!("Invalid EVM address length: {address}")));
    }
    Ok(Address::from_slice(address).to_checksum(None))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::rfq::protocols::metric::{client_builder::MetricClientBuilder, models::MetricDepth};

    fn client() -> MetricClient {
        MetricClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            "http://localhost:8080".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    fn live_client() -> MetricClient {
        let config = crate::rfq::constants::get_metric_config();
        MetricClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            config.base_url,
            config.api_key,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn metadata() -> MetricMetadata {
        MetricMetadata {
            pool_address: Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
            token0: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            token1: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            tvl_fiat: Some(3000.0),
        }
    }

    fn bid_ask() -> MetricBidAskResponse {
        MetricBidAskResponse {
            bid_adj: "55340232221128654848000".to_string(),
            ask_adj: "55358678965202364400000".to_string(),
            total_token0_available: Some("1000000000000000000".to_string()),
            total_token1_available: Some("3000000000".to_string()),
            server_ts: 1_770_053_095,
            depth: MetricDepth::default(),
        }
    }

    #[test]
    fn test_chain_to_chain_id() {
        assert_eq!(chain_to_chain_id(Chain::Ethereum).unwrap(), 1);
        assert_eq!(chain_to_chain_id(Chain::Base).unwrap(), 8453);
        assert!(chain_to_chain_id(Chain::Arbitrum).is_err());
    }

    #[test]
    fn test_endpoints_use_numeric_chain_id() {
        let client = MetricClient::new(
            Chain::Base,
            HashSet::new(),
            0.0,
            "https://api.metric.xyz".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(client.metadata_endpoint, "https://api.metric.xyz/public/v1/evm/8453/metadata");
        assert_eq!(client.chain_endpoint, "https://api.metric.xyz/public/v1/evm/8453");
    }

    #[test]
    fn test_builder_defaults_to_v1_base_url() {
        let client = MetricClientBuilder::new(Chain::Ethereum)
            .build()
            .unwrap();

        assert_eq!(client.metadata_endpoint, "https://api.metric.xyz/public/v1/evm/1/metadata");
    }

    #[test]
    fn test_component_attributes_round_trip_values() {
        let metadata = metadata();
        let component = client().create_component_with_state(
            metadata.pool_address.to_string(),
            &metadata,
            &bid_ask(),
            3000.0,
        );

        assert_eq!(component.component.protocol_system, MetricClient::PROTOCOL_SYSTEM);
        assert_eq!(
            component.component.tokens,
            vec![metadata.token0.clone(), metadata.token1.clone()]
        );
        assert!(component
            .component
            .static_attributes
            .is_empty());
        assert_eq!(component.component.id, metadata.pool_address.to_string());
        assert!(component
            .component
            .contract_addresses
            .is_empty());
        assert_eq!(
            String::from_utf8(component.state.attributes["bid_adj"].to_vec()).unwrap(),
            "55340232221128654848000"
        );
        assert_eq!(
            String::from_utf8(component.state.attributes["server_ts"].to_vec()).unwrap(),
            "1770053095"
        );
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API"]
    async fn test_live_metric_api_fetch_bid_ask_latest_fields() {
        let client = live_client();
        let metadata = client.fetch_metadata().await.unwrap();
        assert!(!metadata.is_empty());

        let mut last_error = None;
        let mut selected = None;
        for pool in &metadata {
            match client
                .fetch_bid_ask(&pool.pool_address)
                .await
            {
                Ok(bid_ask) => {
                    if bid_ask.is_quotable() &&
                        !bid_ask.depth.asks.is_empty() &&
                        !bid_ask.depth.bids.is_empty()
                    {
                        selected = Some((pool, bid_ask));
                        break;
                    }
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        let Some((_pool, bid_ask)) = selected else {
            panic!(
                "Metric live API returned no quotable bid_ask response with ask and bid depth across {} pools; last error: {:?}",
                metadata.len(),
                last_error
            );
        };

        let bid_price = bid_ask.bid_price().unwrap();
        let ask_price = bid_ask.ask_price().unwrap();
        assert!(bid_price.is_finite() && bid_price > 0.0);
        assert!(ask_price.is_finite() && ask_price >= bid_price);
        assert!(bid_ask.total_token0_available().is_ok());
        assert!(bid_ask.total_token1_available().is_ok());
        assert!(bid_ask.server_ts > 0);

        for bin in bid_ask
            .depth
            .asks
            .iter()
            .chain(bid_ask.depth.bids.iter())
            .take(6)
        {
            assert!(bin.price().unwrap().is_finite());
            assert!(bin.cumulative_volume().is_ok());
        }
    }
}
