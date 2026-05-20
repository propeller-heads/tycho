use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::OnceLock,
    time::SystemTime,
};

use alloy::{
    primitives::{utils::keccak256, Address, Bytes as AlloyBytes, U256},
    sol_types::SolValue,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use reqwest::Client;
use tokio::time::{interval, timeout, Duration};
use tracing::{error, info, warn};
use tycho_common::{
    models::{protocol::GetAmountOutParams, token::Token, Chain},
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
            MetricBidAskResponse, MetricMetadata, MetricOracleUpdatePolicy,
            MetricSignedOracleUpdateResponse, MetricSignedOracleUpdateSlot,
            ORACLE_UPDATE_POLICY_ATTR,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    tycho_common::dto::{ProtocolComponent, ResponseProtocolState},
};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct MetricClient {
    chain: Chain,
    metadata_endpoint: String,
    // Prefix ending at /{chain}; pool-specific endpoints are derived from it.
    chain_endpoint: String,
    tokens: HashSet<Bytes>,
    #[serde(default)]
    token_metadata: HashMap<Bytes, Token>,
    tvl: f64,
    quote_tokens: HashSet<Bytes>,
    #[serde(skip_serializing, default)]
    secret_key: Option<String>,
    #[serde(skip, default = "OnceLock::new")]
    http_client: OnceLock<Client>,
    poll_time: Duration,
    quote_timeout: Duration,
    oracle_update_policy: MetricOracleUpdatePolicy,
}

impl Clone for MetricClient {
    fn clone(&self) -> Self {
        let http_client = OnceLock::new();
        if let Some(client) = self.http_client.get() {
            let _ = http_client.set(client.clone());
        }

        Self {
            chain: self.chain,
            metadata_endpoint: self.metadata_endpoint.clone(),
            chain_endpoint: self.chain_endpoint.clone(),
            tokens: self.tokens.clone(),
            token_metadata: self.token_metadata.clone(),
            tvl: self.tvl,
            quote_tokens: self.quote_tokens.clone(),
            secret_key: self.secret_key.clone(),
            http_client,
            poll_time: self.poll_time,
            quote_timeout: self.quote_timeout,
            oracle_update_policy: self.oracle_update_policy,
        }
    }
}

impl MetricClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:metric";

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
        Self::new_with_token_metadata(
            chain,
            tokens,
            HashMap::new(),
            tvl,
            quote_tokens,
            base_url,
            secret_key,
            poll_time,
            quote_timeout,
            MetricOracleUpdatePolicy::default_for_chain(chain),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_token_metadata(
        chain: Chain,
        tokens: HashSet<Bytes>,
        token_metadata: HashMap<Bytes, Token>,
        tvl: f64,
        quote_tokens: HashSet<Bytes>,
        base_url: String,
        secret_key: Option<String>,
        poll_time: Duration,
        quote_timeout: Duration,
        oracle_update_policy: MetricOracleUpdatePolicy,
    ) -> Result<Self, RFQError> {
        let chain_path = chain_to_metric_path(chain)?;
        let base_url = base_url.trim_end_matches('/');
        let chain_endpoint = format!("{base_url}/{chain_path}");
        Ok(Self {
            chain,
            metadata_endpoint: format!("{chain_endpoint}/metadata"),
            chain_endpoint,
            tokens,
            token_metadata,
            tvl,
            quote_tokens,
            secret_key,
            http_client: OnceLock::new(),
            poll_time,
            quote_timeout,
            oracle_update_policy,
        })
    }

    fn http_client(&self) -> &Client {
        self.http_client
            .get_or_init(Client::new)
    }

    pub fn create_component_with_state(
        &self,
        component_id: String,
        metadata: &MetricMetadata,
        bid_ask: &MetricBidAskResponse,
        tvl: f64,
    ) -> ComponentWithState {
        let mut static_attributes = HashMap::new();
        static_attributes.insert(
            ORACLE_UPDATE_POLICY_ATTR.to_string(),
            self.oracle_update_policy
                .as_attribute_value(),
        );

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
            static_attributes,
            ..Default::default()
        };

        let mut attributes = HashMap::new();
        attributes.insert("pair".to_string(), metadata.pair.as_bytes().to_vec().into());
        attributes.insert("pool_address".to_string(), metadata.pool_address.clone());
        attributes
            .insert("price_provider_address".to_string(), metadata.price_provider_address.clone());
        attributes.insert("quoter_address".to_string(), metadata.quoter_address.clone());

        let entries: [(&str, Vec<u8>); 9] = [
            ("bid_adj", bid_ask.bid_adj.as_bytes().to_vec()),
            ("ask_adj", bid_ask.ask_adj.as_bytes().to_vec()),
            (
                "total_token0_available",
                bid_ask
                    .total_token0_available
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "total_token1_available",
                bid_ask
                    .total_token1_available
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "latest_block",
                bid_ask
                    .latest_block
                    .to_string()
                    .into_bytes(),
            ),
            (
                "block_ts",
                bid_ask
                    .block_ts
                    .to_string()
                    .into_bytes(),
            ),
            (
                "server_ts",
                bid_ask
                    .server_ts
                    .to_string()
                    .into_bytes(),
            ),
            (
                "quote_expiration",
                bid_ask
                    .quote_expiration
                    .to_string()
                    .into_bytes(),
            ),
            ("depth", serde_json::to_vec(&bid_ask.depth).unwrap_or_default()),
        ];

        for (key, bytes) in entries {
            attributes.insert(key.to_string(), bytes.into());
        }

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

    fn normalize_tvl(
        &self,
        pool: &MetricMetadata,
        bid_ask: &MetricBidAskResponse,
        pool_quotes: &[(MetricMetadata, MetricBidAskResponse)],
    ) -> Option<f64> {
        // Metric availability is raw ERC20 units. Token metadata comes from Tycho so customers can
        // add new quote tokens without a source-code change.
        let token1_amount = metric_available_human(
            bid_ask.total_token1_available(),
            self.token_metadata
                .get(&pool.token1)?
                .decimals,
        )?;

        if self.quote_tokens.contains(&pool.token1) {
            return Some(token1_amount);
        }

        // For non-configured token1 values, normalize through one Metric pool that prices token1
        // in a configured quote token.
        let price = metric_price_in_quote_token(
            &pool.token1,
            pool_quotes,
            &self.quote_tokens,
            &self.token_metadata,
        );
        if price.is_none() {
            warn!(
                pool = ?pool.pool_address,
                token1 = ?pool.token1,
                "Metric one-hop TVL price not found for non-quote token"
            );
        }

        price
            .map(|price| token1_amount * price)
            .filter(|tvl| tvl.is_finite() && *tvl >= 0.0)
    }

    async fn fetch_metadata(&self) -> Result<Vec<MetricMetadata>, RFQError> {
        let response = self
            .http_client()
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
        let mut request = self
            .http_client()
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

    async fn fetch_signed_oracle_update(
        &self,
        pool: &Bytes,
    ) -> Result<MetricSignedOracleUpdateResponse, RFQError> {
        let endpoint =
            format!("{}/{}/get_signed_data", self.chain_endpoint, bytes_to_address_string(pool)?);
        let mut request = self
            .http_client()
            .get(endpoint)
            .header("accept", "application/json");

        if let Some(secret_key) = &self.secret_key {
            request = request.query(&[("secretKey", secret_key.as_str())]);
        }

        let response = timeout(self.quote_timeout, request.send())
            .await
            .map_err(|_| {
                RFQError::ConnectionError(format!(
                    "Metric oracle update request timed out after {} seconds",
                    self.quote_timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                RFQError::ConnectionError(format!("Failed to fetch Metric oracle update: {e}"))
            })?;

        if !response.status().is_success() {
            return Err(RFQError::QuoteNotFound(format!(
                "Metric oracle update HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse Metric oracle update response: {e}"))
        })
    }

    pub async fn request_oracle_update_for_pool(
        &self,
        metadata: &MetricMetadata,
        params: &GetAmountOutParams,
        amount_out: BigUint,
    ) -> Result<SignedQuote, RFQError> {
        if !((params.token_in == metadata.token0 && params.token_out == metadata.token1) ||
            (params.token_in == metadata.token1 && params.token_out == metadata.token0))
        {
            return Err(RFQError::InvalidInput(format!(
                "Metric token pair mismatch: {} -> {} is not {} / {}",
                params.token_in, params.token_out, metadata.token0, metadata.token1
            )));
        }

        let oracle_update = self
            .fetch_signed_oracle_update(&metadata.pool_address)
            .await?;
        // Metric may return multiple slots, but the API does not expose a reliable
        // pool-to-slot mapping yet. Use the first signed slot until that rule is clarified.
        let slot = oracle_update
            .slots
            .first()
            .ok_or_else(|| {
                RFQError::QuoteNotFound(format!(
                    "Metric oracle update returned no signed slots for pool {}",
                    metadata.pool_address
                ))
            })?;

        let mut quote_attributes = HashMap::new();
        quote_attributes
            .insert("oracle_update_target".to_string(), metadata.price_provider_address.clone());
        quote_attributes.insert(
            "oracle_update_0_calldata".to_string(),
            encode_oracle_update_calldata(&oracle_update.feed_creator, slot)?,
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

                // Fetch every Metric pool first, even when `tokens` later filters emitted
                // components. Non-emitted pools can still provide one-hop quote-token prices for
                // TVL normalization, e.g. rETH/WETH using WETH/USDC.
                let mut pool_quotes = Vec::new();
                for pool in &metadata {
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

                    pool_quotes.push((pool.clone(), bid_ask));
                }

                let mut new_components = HashMap::new();
                for (pool, bid_ask) in &pool_quotes {
                    if !client.tokens.is_empty() &&
                        (!client.tokens.contains(&pool.token0) ||
                            !client.tokens.contains(&pool.token1))
                    {
                        continue;
                    }

                    let tvl = client
                        .normalize_tvl(pool, bid_ask, &pool_quotes)
                        .unwrap_or(0.0);
                    if tvl < client.tvl {
                        continue;
                    }

                    let component_key =
                        format!("metric_{}_{}", client.chain.id(), pool.pool_address);
                    let component_id = keccak256(component_key.as_bytes()).to_string();
                    new_components.insert(
                        component_id.clone(),
                        client.create_component_with_state(component_id, pool, bid_ask, tvl),
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

        self.request_oracle_update_for_pool(pool, params, BigUint::default())
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

fn bytes_to_alloy_address(address: &Bytes) -> Result<Address, RFQError> {
    if address.len() != 20 {
        return Err(RFQError::InvalidInput(format!("Invalid EVM address length: {address}")));
    }
    Ok(Address::from_slice(address))
}

fn encode_oracle_update_calldata(
    feed_creator: &Bytes,
    slot: &MetricSignedOracleUpdateSlot,
) -> Result<Bytes, RFQError> {
    let args = (
        bytes_to_alloy_address(feed_creator)?,
        U256::from(slot.deadline),
        biguint_decimal_to_u256(&slot.new_slot_value)?,
        AlloyBytes::from(slot.signature.to_vec()),
    );
    let selector = keccak256("updateBySignature(address,uint256,uint256,bytes)".as_bytes());
    let mut calldata = selector[..4].to_vec();
    calldata.extend(args.abi_encode());
    Ok(calldata.into())
}

fn biguint_decimal_to_u256(value: &str) -> Result<U256, RFQError> {
    let value = BigUint::from_str(value)
        .map_err(|_| RFQError::ParsingError(format!("Failed to parse uint value: {value}")))?;
    Ok(biguint_to_u256(&value))
}

fn metric_price_in_quote_token(
    token: &Bytes,
    pool_quotes: &[(MetricMetadata, MetricBidAskResponse)],
    quote_tokens: &HashSet<Bytes>,
    token_metadata: &HashMap<Bytes, Token>,
) -> Option<f64> {
    let mut best: Option<(f64, f64)> = None;

    for (pool, bid_ask) in pool_quotes {
        let Some(mid_price) = metric_mid_price(bid_ask) else {
            continue;
        };
        let candidate = if &pool.token0 == token && quote_tokens.contains(&pool.token1) {
            let Some(quote_token) = token_metadata.get(&pool.token1) else {
                continue;
            };
            let Some(quote_tvl) =
                metric_available_human(bid_ask.total_token1_available(), quote_token.decimals)
            else {
                continue;
            };
            Some((mid_price, quote_tvl))
        } else if &pool.token1 == token && quote_tokens.contains(&pool.token0) {
            let Some(quote_token) = token_metadata.get(&pool.token0) else {
                continue;
            };
            let Some(quote_tvl) =
                metric_available_human(bid_ask.total_token0_available(), quote_token.decimals)
            else {
                continue;
            };
            Some((1.0 / mid_price, quote_tvl))
        } else {
            None
        };

        let Some((price, quote_tvl)) = candidate else {
            continue;
        };

        // Multiple Metric pools can price the same token in a configured quote token. Use the
        // pool with the largest quote-side availability as the most liquid pricing source.
        if price.is_finite() &&
            price > 0.0 &&
            quote_tvl.is_finite() &&
            quote_tvl > 0.0 &&
            best.as_ref()
                .is_none_or(|(_, best_quote_tvl)| quote_tvl > *best_quote_tvl)
        {
            best = Some((price, quote_tvl));
        }
    }

    best.map(|(price, _)| price)
}

fn metric_mid_price(bid_ask: &MetricBidAskResponse) -> Option<f64> {
    let bid = bid_ask.bid_price().ok()?;
    let ask = bid_ask.ask_price().ok()?;
    let mid = (bid + ask) / 2.0;
    (mid.is_finite() && mid > 0.0).then_some(mid)
}

fn metric_available_human(amount: Result<BigUint, RFQError>, decimals: u32) -> Option<f64> {
    amount
        .ok()?
        .to_f64()
        .map(|raw| raw / 10_f64.powi(decimals as i32))
        .filter(|amount| amount.is_finite() && *amount >= 0.0)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::token::Token;

    use super::*;
    use crate::rfq::protocols::metric::{
        client_builder::MetricClientBuilder,
        models::{MetricBidAskResponse, MetricDepth},
    };

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

    fn live_client() -> MetricClient {
        let base_url = std::env::var("METRIC_API_URL")
            .unwrap_or_else(|_| "http://54.199.103.16:8080".to_string());
        MetricClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            base_url,
            None,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn client_with_tvl_config(
        quote_tokens: HashSet<Bytes>,
        token_metadata: HashMap<Bytes, Token>,
    ) -> MetricClient {
        MetricClient::new_with_token_metadata(
            Chain::Ethereum,
            HashSet::new(),
            token_metadata,
            0.0,
            quote_tokens,
            "http://localhost:8080".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
            MetricOracleUpdatePolicy::default_for_chain(Chain::Ethereum),
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

    fn metric_address(seed: u8) -> Bytes {
        Bytes::from(vec![seed; 20])
    }

    fn q64_price(price: u64) -> String {
        (BigUint::from(price) << 64usize).to_string()
    }

    fn metadata_for_pair(pair: &str, token0: Bytes, token1: Bytes, seed: u8) -> MetricMetadata {
        MetricMetadata {
            pair: pair.to_string(),
            pool_address: metric_address(seed),
            price_provider_address: metric_address(seed + 1),
            quoter_address: metric_address(seed + 2),
            token0,
            token1,
            cex_step: Some(0.0),
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
            depth: MetricDepth::default(),
        }
    }

    fn bid_ask_with_prices(
        bid: u64,
        ask: u64,
        total_token0_available: &str,
        total_token1_available: &str,
    ) -> MetricBidAskResponse {
        MetricBidAskResponse {
            pair: "test".to_string(),
            bid_adj: q64_price(bid),
            ask_adj: q64_price(ask),
            quote_available: true,
            total_token0_available: total_token0_available.to_string(),
            total_token1_available: total_token1_available.to_string(),
            latest_block: 100,
            block_ts: 1_700_000_000,
            server_ts: 1_700_000_001,
            quote_expiration: 1_700_000_005,
            depth: MetricDepth::default(),
        }
    }

    #[test]
    fn test_builder_uses_token_metadata_decimals() {
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let all_tokens = HashMap::from([(
            usdc.clone(),
            Token::new(&usdc, "USDC", 6, 0, &[], Chain::Ethereum, 100),
        )]);

        let client = MetricClientBuilder::new(Chain::Ethereum)
            .token_metadata(all_tokens)
            .build()
            .unwrap();

        assert_eq!(
            client
                .token_metadata
                .get(&usdc)
                .map(|token| token.decimals),
            Some(6)
        );
    }

    #[test]
    fn test_normalize_tvl_uses_configured_quote_decimals() {
        let pool = metadata();
        let bid_ask = bid_ask();
        let quote_tokens = HashSet::from([pool.token1.clone()]);
        let token_metadata = HashMap::from([(
            pool.token1.clone(),
            Token::new(&pool.token1, "USDC", 6, 0, &[], Chain::Ethereum, 100),
        )]);
        let pool_quotes = vec![(pool.clone(), bid_ask.clone())];
        let client = client_with_tvl_config(quote_tokens, token_metadata);

        let tvl = client
            .normalize_tvl(&pool, &bid_ask, &pool_quotes)
            .unwrap();

        assert_eq!(tvl, 3000.0);
    }

    #[test]
    fn test_normalize_tvl_uses_best_one_hop_quote_pool() {
        let reth = metric_address(10);
        let weth = metric_address(20);
        let usdc = metric_address(30);

        let target = metadata_for_pair("rethweth", reth, weth.clone(), 1);
        let target_bid_ask =
            bid_ask_with_prices(1, 1, "100000000000000000000", "2000000000000000000");
        let low_liquidity_quote = metadata_for_pair("wethusdc_low", weth.clone(), usdc.clone(), 4);
        let low_liquidity_bid_ask =
            bid_ask_with_prices(3000, 3000, "1000000000000000000", "1000000000");
        let high_liquidity_quote =
            metadata_for_pair("wethusdc_high", weth.clone(), usdc.clone(), 7);
        // The higher-liquidity WETH/USDC pool has a different price. The expected TVL below proves
        // normalization chooses it by quote-side availability, not by first match.
        let high_liquidity_bid_ask =
            bid_ask_with_prices(3100, 3100, "1000000000000000000", "5000000000");
        let pool_quotes = vec![
            (target.clone(), target_bid_ask.clone()),
            (low_liquidity_quote, low_liquidity_bid_ask),
            (high_liquidity_quote, high_liquidity_bid_ask),
        ];
        let quote_tokens = HashSet::from([usdc.clone()]);
        let token_metadata = HashMap::from([
            (weth.clone(), Token::new(&weth, "WETH", 18, 0, &[], Chain::Ethereum, 100)),
            (usdc.clone(), Token::new(&usdc, "USDC", 6, 0, &[], Chain::Ethereum, 100)),
        ]);
        let client = client_with_tvl_config(quote_tokens, token_metadata);

        let tvl = client
            .normalize_tvl(&target, &target_bid_ask, &pool_quotes)
            .unwrap();

        assert_eq!(tvl, 6200.0);
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
            component.component.static_attributes[ORACLE_UPDATE_POLICY_ATTR],
            MetricOracleUpdatePolicy::Always.as_attribute_value()
        );
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
    fn test_builder_sets_oracle_update_policy_attribute() {
        let client = MetricClientBuilder::new(Chain::Ethereum)
            .oracle_update_policy(MetricOracleUpdatePolicy::RetryOnRevert)
            .build()
            .unwrap();

        let component = client.create_component_with_state(
            "metric_ethusdc".to_string(),
            &metadata(),
            &bid_ask(),
            3000.0,
        );

        assert_eq!(
            component.component.static_attributes[ORACLE_UPDATE_POLICY_ATTR],
            MetricOracleUpdatePolicy::RetryOnRevert.as_attribute_value()
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
                    if bid_ask.quote_available &&
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

        let Some((pool, bid_ask)) = selected else {
            panic!(
                "Metric live API returned no quoteAvailable bid_ask response with ask and bid depth across {} pools; last error: {:?}",
                metadata.len(),
                last_error
            );
        };

        assert_eq!(bid_ask.pair, pool.pair);
        let bid_price = bid_ask.bid_price().unwrap();
        let ask_price = bid_ask.ask_price().unwrap();
        assert!(bid_price.is_finite() && bid_price > 0.0);
        assert!(ask_price.is_finite() && ask_price >= bid_price);
        assert!(bid_ask.total_token0_available().is_ok());
        assert!(bid_ask.total_token1_available().is_ok());
        assert!(bid_ask.latest_block > 0);
        assert!(bid_ask.block_ts > 0);
        assert!(bid_ask.server_ts > 0);
        assert!(bid_ask.quote_expiration > 0);

        for bin in bid_ask
            .depth
            .asks
            .iter()
            .chain(bid_ask.depth.bids.iter())
            .take(6)
        {
            assert!(bin.price().unwrap().is_finite());
            assert!(bin.cumulative_volume().is_ok());
            assert!(BigUint::from_str(&bin.price_impact_e6).is_ok());
        }
    }

    #[test]
    fn test_encode_oracle_update_calldata() {
        let feed_creator = Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap();
        let slot = MetricSignedOracleUpdateSlot {
            slot_id: 0,
            deadline: 1_700_000_000,
            slot_pairs: vec!["weth".to_string(), "usdc".to_string()],
            new_slot_value: "42".to_string(),
            signature: Bytes::from_str(
                "0x111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111b",
            )
            .unwrap(),
            prices: serde_json::json!([]),
        };

        let calldata = encode_oracle_update_calldata(&feed_creator, &slot).unwrap();

        assert_eq!(&calldata[..4], &[0x78, 0xce, 0x3a, 0xe1]);
        assert!(calldata.len() > 4);
    }
}
