use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    time::SystemTime,
};

use alloy::primitives::{utils::keccak256, Address, U256};
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use reqwest::Client;
use tokio::time::{interval, timeout, Duration};
use tracing::{error, info, warn};
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::{
    evm::protocol::u256_num::biguint_to_u256,
    rfq::{
        client::RFQClient,
        errors::RFQError,
        models::TimestampHeader,
        protocols::metric::models::{
            MetricBidAskResponse, MetricMetadata, MetricQuoteRequest, MetricQuoteResponse,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    tycho_common::dto::{ProtocolComponent, ResponseProtocolState},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MetricClient {
    chain: Chain,
    metadata_endpoint: String,
    // Prefix ending at /{chain}; pool-specific endpoints are derived from it.
    chain_endpoint: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_tokens: HashSet<Bytes>,
    #[serde(skip_serializing, default)]
    secret_key: Option<String>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl MetricClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:metric";
    const U128_MAX: &'static str = "340282366920938463463374607431768211455";

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        tokens: HashSet<Bytes>,
        tvl: f64,
        quote_tokens: HashSet<Bytes>,
        base_url: String,
        secret_key: Option<String>,
        poll_time: Duration,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        let chain_path = chain_to_metric_path(chain)?;
        let base_url = base_url.trim_end_matches('/');
        let chain_endpoint = format!("{base_url}/{chain_path}");
        Ok(Self {
            chain,
            metadata_endpoint: format!("{chain_endpoint}/metadata"),
            chain_endpoint,
            tokens,
            tvl,
            quote_tokens,
            secret_key,
            poll_time,
            quote_timeout,
        })
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
            chain: self.chain.into(),
            tokens: vec![metadata.token0.clone(), metadata.token1.clone()],
            contract_ids: vec![
                metadata.pool_address.clone(),
                metadata.price_provider_address.clone(),
                metadata.quoter_address.clone(),
            ],
            ..Default::default()
        };

        let mut attributes = HashMap::new();
        attributes.insert("pair".to_string(), metadata.pair.as_bytes().to_vec().into());
        attributes.insert("pool_address".to_string(), metadata.pool_address.clone());
        attributes
            .insert("price_provider_address".to_string(), metadata.price_provider_address.clone());
        attributes.insert("quoter_address".to_string(), metadata.quoter_address.clone());
        attributes.insert(
            "bid_adj".to_string(),
            bid_ask
                .bid_adj
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "ask_adj".to_string(),
            bid_ask
                .ask_adj
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "total_token0_available".to_string(),
            bid_ask
                .total_token0_available
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "total_token1_available".to_string(),
            bid_ask
                .total_token1_available
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "latest_block".to_string(),
            bid_ask
                .latest_block
                .to_string()
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "block_ts".to_string(),
            bid_ask
                .block_ts
                .to_string()
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "server_ts".to_string(),
            bid_ask
                .server_ts
                .to_string()
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "quote_expiration".to_string(),
            bid_ask
                .quote_expiration
                .to_string()
                .as_bytes()
                .to_vec()
                .into(),
        );
        attributes.insert(
            "depth".to_string(),
            serde_json::to_vec(&bid_ask.depth)
                .unwrap_or_default()
                .into(),
        );

        if let Some(cex_step) = metadata.cex_step {
            attributes.insert(
                "cex_step".to_string(),
                cex_step
                    .to_string()
                    .as_bytes()
                    .to_vec()
                    .into(),
            );
        }
        if let Some(dex_step) = metadata.dex_step {
            attributes.insert(
                "dex_step".to_string(),
                dex_step
                    .to_string()
                    .as_bytes()
                    .to_vec()
                    .into(),
            );
        }

        ComponentWithState {
            state: ResponseProtocolState {
                component_id: component_id.clone(),
                attributes,
                balances: HashMap::new(),
            },
            component: protocol_component,
            component_tvl: Some(tvl),
            entrypoints: vec![],
        }
    }

    async fn fetch_metadata(&self) -> Result<Vec<MetricMetadata>, RFQError> {
        let response = Client::new()
            .get(&self.metadata_endpoint)
            .header("accept", "application/json")
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

        response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse Metric metadata response: {e}"))
        })
    }

    async fn fetch_bid_ask(&self, pool: &Bytes) -> Result<MetricBidAskResponse, RFQError> {
        let endpoint =
            format!("{}/{}/bid_ask", self.chain_endpoint, bytes_to_address_string(pool)?);
        let http_client = Client::new();
        let mut request = http_client
            .get(endpoint)
            .header("accept", "application/json");

        if let Some(secret_key) = &self.secret_key {
            request = request.query(&[("secretKey", secret_key.as_str())]);
        }

        let response = request.send().await.map_err(|e| {
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

    async fn fetch_quote(
        &self,
        pool: &Bytes,
        request: &MetricQuoteRequest,
    ) -> Result<MetricQuoteResponse, RFQError> {
        let endpoint = format!("{}/{}/quote", self.chain_endpoint, bytes_to_address_string(pool)?);
        let http_client = Client::new();
        let response = timeout(
            self.quote_timeout,
            http_client
                .post(endpoint)
                .header("accept", "application/json")
                .json(request)
                .send(),
        )
        .await
        .map_err(|_| {
            RFQError::ConnectionError(format!(
                "Metric quote request timed out after {} seconds",
                self.quote_timeout.as_secs()
            ))
        })?
        .map_err(|e| {
            RFQError::ConnectionError(format!("Failed to send Metric quote request: {e}"))
        })?;

        if !response.status().is_success() {
            return Err(RFQError::QuoteNotFound(format!(
                "Metric quote HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse Metric quote response: {e}"))
        })
    }

    pub async fn request_binding_quote_for_pool(
        &self,
        metadata: &MetricMetadata,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let zero_for_one =
            if params.token_in == metadata.token0 && params.token_out == metadata.token1 {
                true
            } else if params.token_in == metadata.token1 && params.token_out == metadata.token0 {
                false
            } else {
                return Err(RFQError::InvalidInput(format!(
                    "Metric token pair mismatch: {} -> {} is not {} / {}",
                    params.token_in, params.token_out, metadata.token0, metadata.token1
                )));
            };

        // Binding quotes should use fresh Q64 prices, not just the last polling snapshot.
        let bid_ask = self
            .fetch_bid_ask(&metadata.pool_address)
            .await?;
        if !bid_ask.quote_available {
            return Err(RFQError::QuoteNotFound(format!(
                "Metric quote unavailable for pool {} at block {}",
                metadata.pool_address, bid_ask.latest_block
            )));
        }

        let price_limit_x64 = if zero_for_one { "0" } else { Self::U128_MAX };
        let quote_request = MetricQuoteRequest {
            pool: bytes_to_address_string(&metadata.pool_address)?,
            zero_for_one,
            amount_specified: params.amount_in.to_string(),
            price_limit_x64: price_limit_x64.to_string(),
            bid_price_x64: bid_ask.bid_adj.clone(),
            ask_price_x64: bid_ask.ask_adj.clone(),
        };
        let quote = self
            .fetch_quote(&metadata.pool_address, &quote_request)
            .await?;
        quote.validate(params, zero_for_one)?;
        let amount_out = quote.amount_out(zero_for_one)?;

        let mut quote_attributes = HashMap::new();
        quote_attributes.insert("pool".to_string(), metadata.pool_address.clone());
        quote_attributes
            .insert("price_provider".to_string(), metadata.price_provider_address.clone());
        quote_attributes.insert("quoter".to_string(), metadata.quoter_address.clone());
        quote_attributes.insert("zero_for_one".to_string(), vec![u8::from(zero_for_one)].into());
        quote_attributes.insert(
            "amount_specified".to_string(),
            params
                .amount_in
                .to_string()
                .as_bytes()
                .to_vec()
                .into(),
        );
        quote_attributes.insert(
            "price_limit_x64".to_string(),
            biguint_decimal_to_u256_bytes(&quote_request.price_limit_x64)?,
        );
        quote_attributes.insert(
            "bid_price_x64".to_string(),
            biguint_decimal_to_u256_bytes(&quote_request.bid_price_x64)?,
        );
        quote_attributes.insert(
            "ask_price_x64".to_string(),
            biguint_decimal_to_u256_bytes(&quote_request.ask_price_x64)?,
        );
        quote_attributes.insert(
            "amount0_delta".to_string(),
            quote
                .amount0_delta
                .as_bytes()
                .to_vec()
                .into(),
        );
        quote_attributes.insert(
            "amount1_delta".to_string(),
            quote
                .amount1_delta
                .as_bytes()
                .to_vec()
                .into(),
        );
        quote_attributes.insert(
            "latest_block".to_string(),
            U256::from(bid_ask.latest_block)
                .to_be_bytes::<32>()
                .to_vec()
                .into(),
        );
        quote_attributes.insert(
            "quote_expiration".to_string(),
            U256::from(bid_ask.quote_expiration)
                .to_be_bytes::<32>()
                .to_vec()
                .into(),
        );

        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: params.amount_in.clone(),
            amount_out,
            quote_attributes,
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
                    if !bid_ask.quote_available {
                        continue;
                    }

                    // Metric gives raw token availability. For filtering, only count token1 when
                    // it is one of the stable quote tokens we know how to normalize.
                    let tvl = if client.quote_tokens.contains(&pool.token1) {
                        stable_decimals(&pool.token1)
                            .and_then(|decimals| {
                                bid_ask
                                    .total_token1_available()
                                    .ok()
                                    .and_then(|value| value.to_f64())
                                    .map(|raw| raw / 10_f64.powi(decimals as i32))
                            })
                            .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    if tvl < client.tvl {
                        continue;
                    }

                    let component_key =
                        format!("metric_{}_{}", client.chain.id(), pool.pool_address);
                    let component_id = keccak256(component_key.as_bytes()).to_string();
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
        let pool = metadata
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
            })?;

        self.request_binding_quote_for_pool(pool, params)
            .await
    }
}

fn chain_to_metric_path(chain: Chain) -> Result<&'static str, RFQError> {
    match chain {
        Chain::Ethereum => Ok("ethereum"),
        Chain::Base => Ok("base"),
        Chain::Bsc => Ok("bsc"),
        Chain::Arbitrum => Ok("arbitrum"),
        Chain::Polygon => Ok("polygon"),
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

fn biguint_decimal_to_u256_bytes(value: &str) -> Result<Bytes, RFQError> {
    let value = BigUint::from_str(value)
        .map_err(|_| RFQError::ParsingError(format!("Failed to parse uint value: {value}")))?;
    Ok(Bytes::from(
        biguint_to_u256(&value)
            .to_be_bytes::<32>()
            .to_vec(),
    ))
}

fn stable_decimals(address: &Bytes) -> Option<u8> {
    let address = Address::from_slice(address)
        .to_checksum(None)
        .to_lowercase();
    match address.as_str() {
        // Ethereum USDC / Base USDC
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" |
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913" |
        // Ethereum USDT / Base USDT
        "0xdac17f958d2ee523a2206206994597c13d831ec7" |
        "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2" => Some(6),
        // Ethereum DAI
        "0x6b175474e89094c44da98b954eedeac495271d0f" => Some(18),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::rfq::protocols::metric::models::MetricBidAskResponse;

    fn client() -> MetricClient {
        MetricClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "http://localhost:8080".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    fn metadata() -> MetricMetadata {
        MetricMetadata {
            pair: "ethusdc".to_string(),
            pool_address: Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
            price_provider_address: Bytes::from_str("0xbD321D18a7ce5fb91F8b16e026e3258f7b310598")
                .unwrap(),
            quoter_address: Bytes::from_str("0x58F9d1865d4Aeb59a9a7Dc68A3b4e0B42D9Ef5eD").unwrap(),
            token0: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            token1: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            cex_step: Some(0.0002),
            dex_step: Some(0.0),
        }
    }

    fn bid_ask() -> MetricBidAskResponse {
        MetricBidAskResponse {
            pair: "ethusdc".to_string(),
            bid_adj: "55340232221128654848000".to_string(),
            ask_adj: "55358678965202364400000".to_string(),
            quote_available: true,
            total_token0_available: "1000000000000000000".to_string(),
            total_token1_available: "3000000000".to_string(),
            latest_block: 100,
            block_ts: 1_700_000_000,
            server_ts: 1_700_000_001,
            quote_expiration: 1_700_000_005,
            depth: serde_json::json!({}),
        }
    }

    #[test]
    fn test_component_attributes_round_trip_values() {
        let component = client().create_component_with_state(
            "metric_ethusdc".to_string(),
            &metadata(),
            &bid_ask(),
            3000.0,
        );

        assert_eq!(component.component.protocol_system, MetricClient::PROTOCOL_SYSTEM);
        assert_eq!(component.component.tokens, vec![metadata().token0, metadata().token1]);
        assert_eq!(
            component.state.attributes["pool_address"],
            Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap()
        );
        assert_eq!(
            String::from_utf8(component.state.attributes["bid_adj"].to_vec()).unwrap(),
            "55340232221128654848000"
        );
    }

    #[test]
    fn test_stable_decimals() {
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        assert_eq!(stable_decimals(&usdc), Some(6));
    }
}
