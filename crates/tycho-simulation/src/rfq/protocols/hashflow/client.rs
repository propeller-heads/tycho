use std::{
    collections::{BTreeSet, HashMap, HashSet},
    str::FromStr,
    time::SystemTime,
};

use alloy::primitives::{utils::keccak256, Address, U256};
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
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
        protocols::hashflow::models::{
            HashflowChain, HashflowMakerLevels, HashflowMarketMakerLevels,
            HashflowMarketMakersResponse, HashflowPriceLevelsResponse, HashflowQuoteRequest,
            HashflowQuoteResponse, HashflowRFQ, HashflowRFQOptions,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HashflowClient {
    chain: Chain,
    price_levels_endpoint: String,
    market_makers_endpoint: String,
    quote_endpoint: String,
    // Tokens that we want prices for
    tokens: HashSet<Bytes>,
    // Min tvl value in the quote token.
    tvl: f64,
    #[serde(skip_serializing, default)]
    auth_key: String,
    #[serde(skip_serializing, default)]
    auth_user: String,
    // Quote tokens to normalize to for TVL purposes. Should have the same prices.
    quote_tokens: HashSet<Bytes>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl HashflowClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:hashflow";

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        tokens: HashSet<Bytes>,
        tvl: f64,
        quote_tokens: HashSet<Bytes>,
        auth_user: String,
        auth_key: String,
        poll_time: Duration,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        Ok(Self {
            chain,
            price_levels_endpoint: "https://api.hashflow.com/taker/v3/price-levels".to_string(),
            market_makers_endpoint: "https://api.hashflow.com/taker/v3/market-makers".to_string(),
            quote_endpoint: "https://api.hashflow.com/taker/v3/rfq".to_string(),
            tokens,
            tvl,
            auth_key,
            auth_user,
            quote_tokens,
            poll_time,
            quote_timeout,
        })
    }

    /// Normalize TVL to a common quote token for comparison
    /// Returns the normalized TVL value, or 0.0 if normalization fails due to no liquidity
    fn normalize_tvl(
        &self,
        raw_tvl: f64,
        quote_token: Bytes,
        levels_by_mm: &HashMap<String, Vec<HashflowMarketMakerLevels>>,
    ) -> Result<f64, RFQError> {
        // If the quote token is already in our approved quote token set, no conversion needed
        if self.quote_tokens.contains(&quote_token) {
            return Ok(raw_tvl);
        }

        // Try to find the price of the quote token in one of the approved quote tokens
        // for normalization.
        for approved_quote_token in &self.quote_tokens {
            for mm_levels_inner in levels_by_mm.values() {
                for quote_mm_level in mm_levels_inner {
                    // Check for direct pair: quote_token/approved_quote_token
                    if quote_mm_level.pair.base_token == quote_token &&
                        quote_mm_level.pair.quote_token == *approved_quote_token
                    {
                        if let Some(price) = quote_mm_level.get_price(1.0) {
                            return Ok(raw_tvl * price);
                        }
                    }
                }
            }
        }

        // If we can't normalize, return TVL 0 (pool will be filtered out)
        Ok(0.0)
    }

    /// The id of this chain's venue component. One per chain, for the venue's life.
    pub fn component_id(&self) -> String {
        format!("{}", keccak256(format!("hashflow_{}", self.chain.id()).as_bytes()))
    }

    /// The venue component for one poll: every market maker's levels on every pair whose tokens
    /// are indexed and whose TVL clears the threshold. `None` when no book clears it.
    ///
    /// The component's `tokens` are every token a book names, sorted. Its `pairs` static
    /// attribute lists the directed pairs that have a book, 40 bytes each (base then quote), so
    /// a consumer can connect those pairs and not every pair of its tokens. Its `books` state
    /// attribute is the books as JSON, sorted by maker, base and quote.
    fn venue_component(
        &self,
        levels_by_mm: &HashMap<String, Vec<HashflowMarketMakerLevels>>,
    ) -> Result<Option<ComponentWithState>, RFQError> {
        let mut books = Vec::new();
        let mut tvl = 0.0;
        for (mm_name, mm_levels) in levels_by_mm {
            for mm_level in mm_levels {
                let base_token = &mm_level.pair.base_token;
                let quote_token = &mm_level.pair.quote_token;
                if !(self.tokens.contains(base_token) && self.tokens.contains(quote_token)) {
                    continue;
                }
                let normalized_tvl = self.normalize_tvl(
                    mm_level.calculate_tvl(),
                    quote_token.clone(),
                    levels_by_mm,
                )?;
                if normalized_tvl < self.tvl {
                    info!(
                        "Filtering out {mm_name} on {base_token}/{quote_token} due to low TVL: {:.2} < {:.2}",
                        normalized_tvl, self.tvl
                    );
                    continue;
                }
                tvl += normalized_tvl;
                books.push(HashflowMakerLevels {
                    market_maker: mm_name.clone(),
                    pair: mm_level.pair.clone(),
                    levels: mm_level.levels.clone(),
                });
            }
        }
        if books.is_empty() {
            return Ok(None);
        }
        books.sort_by(|a, b| {
            (&a.market_maker, &a.pair.base_token, &a.pair.quote_token).cmp(&(
                &b.market_maker,
                &b.pair.base_token,
                &b.pair.quote_token,
            ))
        });

        let mut tokens = BTreeSet::new();
        let mut pairs = BTreeSet::new();
        for book in &books {
            tokens.insert(book.pair.base_token.clone());
            tokens.insert(book.pair.quote_token.clone());
            pairs.insert((book.pair.base_token.clone(), book.pair.quote_token.clone()));
        }
        let mut pairs_attribute = Vec::with_capacity(pairs.len() * 40);
        for (base_token, quote_token) in &pairs {
            pairs_attribute.extend_from_slice(base_token);
            pairs_attribute.extend_from_slice(quote_token);
        }

        let component_id = self.component_id();
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "hashflow_pool".to_string(),
            chain: self.chain,
            tokens: tokens.into_iter().collect(),
            contract_addresses: vec![], // empty for RFQ
            static_attributes: HashMap::from([("pairs".to_string(), pairs_attribute.into())]),
            ..Default::default()
        };

        let books_json = serde_json::to_vec(&books).map_err(|e| {
            RFQError::ParsingError(format!("Failed to serialize Hashflow books: {e}"))
        })?;
        let attributes = HashMap::from([("books".to_string(), books_json.into())]);

        Ok(Some(ComponentWithState {
            state: ProtocolComponentState::new(&component_id, attributes, HashMap::new()),
            component: protocol_component,
            component_tvl: Some(tvl),
            entrypoints: vec![],
        }))
    }

    /// Requests a firm quote, from `market_maker` alone when one is named.
    ///
    /// A named maker is asked with `doNotRetryWithOtherMakers`, so a declined request is an
    /// error and not a quote from a maker the caller did not choose.
    pub async fn request_binding_quote_from(
        &self,
        params: &GetAmountOutParams,
        market_maker: Option<&str>,
    ) -> Result<SignedQuote, RFQError> {
        let hashflow_chain = HashflowChain::from(self.chain);
        // A fresh random address becomes the quote's effectiveTrader — the address Hashflow
        // scopes its strictly increasing quote nonces to — so quotes never invalidate each
        // other, at the cost of a cold nonce storage slot on Hashflow's router (~17k gas per
        // swap). The receiver executes the trade on-chain, so it is Hashflow's trader.
        let effective_trader = Bytes::from(Address::random().to_vec());
        let quote_request = HashflowQuoteRequest {
            source: self.auth_user.clone(),
            base_chain: hashflow_chain.clone(),
            quote_chain: hashflow_chain,
            rfqs: vec![HashflowRFQ {
                base_token: params.token_in.to_string(),
                quote_token: params.token_out.to_string(),
                base_token_amount: Some(params.amount_in.to_string()),
                quote_token_amount: None,
                trader: params.receiver.to_string(),
                effective_trader: Some(effective_trader.to_string()),
                market_makers: market_maker.map(|mm| vec![mm.to_string()]),
                options: market_maker
                    .map(|_| HashflowRFQOptions { do_not_retry_with_other_makers: true }),
            }],
            calldata: false,
        };
        self.send_quote_request(params, &quote_request, &effective_trader)
            .await
    }

    async fn fetch_market_makers(&mut self) -> Result<Vec<String>, RFQError> {
        let query_params = vec![
            ("source", self.auth_user.clone()),
            ("baseChainType", "evm".to_string()),
            ("baseChainId", self.chain.id().to_string()),
        ];

        let http_client = Client::new();
        let request = http_client
            .get(&self.market_makers_endpoint)
            .query(&query_params)
            .header("accept", "application/json")
            .header("Authorization", &self.auth_key);

        let response = request.send().await.map_err(|e| {
            RFQError::ConnectionError(format!("Failed to fetch market makers: {e}"))
        })?;

        if !response.status().is_success() {
            return Err(RFQError::ConnectionError(format!(
                "HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        let mm_response: HashflowMarketMakersResponse = response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse market makers response: {e}"))
        })?;

        info!(
            "Fetched {} market makers: {:?}",
            mm_response.market_makers.len(),
            mm_response.market_makers
        );

        Ok(mm_response.market_makers)
    }

    async fn fetch_price_levels(
        &self,
        market_makers: &Vec<String>,
    ) -> Result<HashMap<String, Vec<HashflowMarketMakerLevels>>, RFQError> {
        let mut query_params = vec![
            ("source", self.auth_user.clone()),
            ("baseChainType", "evm".to_string()),
            ("baseChainId", self.chain.id().to_string()),
        ];

        // Add market makers as array parameters
        for mm in market_makers {
            query_params.push(("marketMakers[]", mm.clone()));
        }

        let http_client = Client::new();
        let request = http_client
            .get(&self.price_levels_endpoint)
            .query(&query_params)
            .header("accept", "application/json")
            .header("Authorization", &self.auth_key);

        let response = request
            .send()
            .await
            .map_err(|e| RFQError::ConnectionError(format!("Failed to fetch price levels: {e}")))?;

        if !response.status().is_success() {
            return Err(RFQError::ConnectionError(format!(
                "HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        let price_response: HashflowPriceLevelsResponse = response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse price levels response: {e}"))
        })?;

        if price_response.status != "success" {
            return Err(RFQError::InvalidInput(format!(
                "API returned error status: {}",
                price_response.error.unwrap_or_default()
            )));
        }

        price_response
            .levels
            .ok_or_else(|| RFQError::ParsingError("API response missing levels".to_string()))
    }
}

#[async_trait]
impl RFQClient for HashflowClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let mut client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_component: Option<ProtocolComponent> = None;
            let mut ticker = interval(client.poll_time);

            info!("Starting Hashflow price levels polling every {} seconds", client.poll_time.as_secs());
            info!("TVL threshold: {:.2}", client.tvl);

            loop {
                ticker.tick().await;

                let market_makers;
                match client.fetch_market_makers().await {
                    Ok(mms) => {
                        market_makers = mms;
                        info!("Successfully fetched market makers");
                    }
                    Err(e) => {
                        info!("Failed to fetch market makers: {}", e);
                        continue;
                    }
                }

                match client.fetch_price_levels(&market_makers).await {
                    Ok(levels_by_mm) => {
                        info!("Fetched price levels from {} market makers", levels_by_mm.len());
                        let mut states = HashMap::new();
                        let mut removed_components = HashMap::new();
                        match client.venue_component(&levels_by_mm)? {
                            Some(component_with_state) => {
                                current_component = Some(component_with_state.component.clone());
                                states.insert(component_with_state.component.id.clone(), component_with_state);
                            }
                            // The venue lost its last book, so the component leaves the market.
                            None => {
                                if let Some(component) = current_component.take() {
                                    removed_components.insert(component.id.clone(), component);
                                }
                            }
                        }

                        let snapshot = Snapshot {
                            states,
                            vm_storage: HashMap::new(),
                        };

                        let timestamp = SystemTime::now().duration_since(
                            SystemTime::UNIX_EPOCH
                        ).map_err(
                            |_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".into())
                        )?.as_secs();

                        let msg = StateSyncMessage::<TimestampHeader> {
                            header: TimestampHeader { timestamp },
                            snapshots: snapshot,
                            deltas: None,
                            removed_components,
                        };

                        yield Ok(("hashflow".to_string(), msg));
                    },
                    Err(e) => {
                        error!("Failed to fetch price levels from Hashflow API: {}", e);
                        continue;
                    }
                }
            }
        })
    }

    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        self.request_binding_quote_from(params, None)
            .await
    }
}

impl HashflowClient {
    async fn send_quote_request(
        &self,
        params: &GetAmountOutParams,
        quote_request: &HashflowQuoteRequest,
        effective_trader: &Bytes,
    ) -> Result<SignedQuote, RFQError> {
        let url = self.quote_endpoint.clone();

        let start_time = std::time::Instant::now();
        const MAX_RETRIES: u32 = 3;
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            // Check if we have time remaining for this attempt
            let elapsed = start_time.elapsed();
            if elapsed >= self.quote_timeout {
                return Err(last_error.unwrap_or_else(|| {
                    RFQError::ConnectionError(format!(
                        "Hashflow quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    ))
                }));
            }

            let remaining_time = self.quote_timeout - elapsed;

            let http_client = Client::new();
            let request = http_client
                .post(&url)
                .json(&quote_request)
                .header("accept", "application/json")
                .header("Authorization", &self.auth_key);

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    warn!(
                        "Hashflow quote request failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    last_error = Some(RFQError::ConnectionError(format!(
                        "Failed to send Hashflow quote request: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
                Err(_) => {
                    return Err(RFQError::ConnectionError(format!(
                        "Hashflow quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    )));
                }
            };

            if response.status() != 200 {
                let err_msg = match response.text().await {
                    Ok(text) => text,
                    Err(e) => {
                        warn!(
                            "Hashflow error response parsing failed (attempt {}/{}): {}",
                            attempt + 1,
                            MAX_RETRIES,
                            e
                        );
                        last_error = Some(RFQError::ParsingError(format!(
                            "Failed to read response text from Hashflow failed request: {e}"
                        )));
                        if attempt < MAX_RETRIES - 1 {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        } else {
                            return Err(last_error.unwrap());
                        }
                    }
                };
                last_error = Some(RFQError::FatalError(format!(
                    "Failed to send Hashflow quote request: {err_msg}",
                )));
                if attempt < MAX_RETRIES - 1 {
                    warn!(
                        "Hashflow returned non-200 status (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        err_msg
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                } else {
                    return Err(last_error.unwrap());
                }
            }

            let quote_response = match response
                .json::<HashflowQuoteResponse>()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(
                        "Hashflow quote response parsing failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    last_error = Some(RFQError::ParsingError(format!(
                        "Failed to parse Hashflow quote response: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
            };

            match quote_response.status.as_str() {
                "success" => {
                    if let Some(quotes) = quote_response.quotes {
                        if quotes.is_empty() {
                            return Err(RFQError::QuoteNotFound(format!(
                                "Hashflow quote not found for {} {} ->{}",
                                params.amount_in, params.token_in, params.token_out,
                            )));
                        }
                        // We assume there will be only one quote request at a time
                        let quote = quotes[0].clone();
                        quote.validate(params, effective_trader)?;

                        let mut quote_attributes: HashMap<String, Bytes> = HashMap::new();
                        quote_attributes.insert("pool".to_string(), quote.quote_data.pool);
                        if let Some(external_account) = quote.quote_data.external_account {
                            quote_attributes
                                .insert("external_account".to_string(), external_account);
                        } else {
                            quote_attributes.insert(
                                "external_account".to_string(),
                                Bytes::from_str(&Address::ZERO.to_string()).map_err(|_| {
                                    RFQError::ParsingError(
                                        "Failed to parse zero address".to_string(),
                                    )
                                })?,
                            );
                        }
                        quote_attributes.insert("trader".to_string(), quote.quote_data.trader);
                        quote_attributes
                            .insert("effective_trader".to_string(), effective_trader.clone());
                        quote_attributes
                            .insert("base_token".to_string(), quote.quote_data.base_token);
                        quote_attributes
                            .insert("quote_token".to_string(), quote.quote_data.quote_token);
                        quote_attributes.insert(
                            "base_token_amount".to_string(),
                            Bytes::from(
                                biguint_to_u256(
                                    &BigUint::from_str(&quote.quote_data.base_token_amount)
                                        .map_err(|_| {
                                            RFQError::ParsingError(format!(
                                                "Failed to parse base token amount: {}",
                                                quote.quote_data.base_token_amount
                                            ))
                                        })?,
                                )
                                .to_be_bytes::<32>()
                                .to_vec(),
                            ),
                        );
                        quote_attributes.insert(
                            "quote_token_amount".to_string(),
                            Bytes::from(
                                biguint_to_u256(
                                    &BigUint::from_str(&quote.quote_data.quote_token_amount)
                                        .map_err(|_| {
                                            RFQError::ParsingError(format!(
                                                "Failed to parse quote token amount: {}",
                                                quote.quote_data.quote_token_amount
                                            ))
                                        })?,
                                )
                                .to_be_bytes::<32>()
                                .to_vec(),
                            ),
                        );
                        quote_attributes.insert(
                            "quote_expiry".to_string(),
                            Bytes::from(
                                U256::from(quote.quote_data.quote_expiry)
                                    .to_be_bytes::<32>()
                                    .to_vec(),
                            ),
                        );
                        quote_attributes.insert(
                            "nonce".to_string(),
                            Bytes::from(
                                U256::from(quote.quote_data.nonce)
                                    .to_be_bytes::<32>()
                                    .to_vec(),
                            ),
                        );
                        quote_attributes.insert("tx_id".to_string(), quote.quote_data.tx_id);
                        quote_attributes.insert("signature".to_string(), quote.signature);

                        let signed_quote = SignedQuote {
                            base_token: params.token_in.clone(),
                            quote_token: params.token_out.clone(),
                            amount_in: BigUint::from_str(&quote.quote_data.base_token_amount)
                                .map_err(|_| {
                                    RFQError::ParsingError(format!(
                                        "Failed to parse amount in string: {}",
                                        quote.quote_data.base_token_amount
                                    ))
                                })?,
                            amount_out: BigUint::from_str(&quote.quote_data.quote_token_amount)
                                .map_err(|_| {
                                    RFQError::ParsingError(format!(
                                        "Failed to parse amount out string: {}",
                                        quote.quote_data.quote_token_amount
                                    ))
                                })?,
                            quote_attributes,
                        };
                        return Ok(signed_quote);
                    } else {
                        return Err(RFQError::QuoteNotFound(format!(
                            "Hashflow quote not found for {} {} ->{}",
                            params.amount_in, params.token_in, params.token_out,
                        )));
                    }
                }
                "fail" => {
                    return Err(RFQError::FatalError(format!(
                        "Hashflow API error: {:?}",
                        quote_response.error
                    )));
                }
                _ => {
                    return Err(RFQError::FatalError(
                        "Hashflow API error: Unknown status".to_string(),
                    ));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Hashflow quote request failed after retries".to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{env, str::FromStr, time::Duration};

    use dotenv::dotenv;
    use futures::StreamExt;
    use tokio::time::timeout;

    use super::*;
    use crate::rfq::{
        constants::get_hashflow_auth,
        protocols::hashflow::models::{HashflowPair, HashflowPriceLevel},
    };

    #[test]
    fn test_normalize_tvl_same_quote_token() {
        let client = create_test_client();
        let levels = HashMap::new();

        // USDC is in our quote tokens, so no normalization should happen
        let result = client.normalize_tvl(
            1000.0,
            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            &levels,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1000.0);
    }

    #[test]
    fn test_normalize_tvl_different_quote_token() {
        let client = create_test_client();
        let mut levels = HashMap::new();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();

        // Create mock levels for ETH/USDC pair for normalization
        let eth_usdc_level = HashflowMarketMakerLevels {
            pair: HashflowPair { base_token: weth.clone(), quote_token: usdc },
            levels: vec![
                HashflowPriceLevel { quantity: 1.0, price: 3000.0 }, /* 1 ETH = 3000 USDC */
            ],
        };

        levels.insert("test_mm".to_string(), vec![eth_usdc_level]);

        // Test normalizing ETH TVL to USDC
        let result = client.normalize_tvl(2.0, weth, &levels);
        assert!(result.is_ok());
        // 2 ETH * 3000 USDC/ETH = 6000 USDC
        assert_eq!(result.unwrap(), 6000.0);
    }

    #[test]
    fn test_normalize_tvl_no_conversion_available() {
        let client = create_test_client();
        let levels = HashMap::new();
        let result = client.normalize_tvl(
            1000.0,
            Bytes::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            &levels,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0.0);
    }

    fn create_test_client() -> HashflowClient {
        let quote_tokens = HashSet::from([
            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(), // USDT
        ]);

        HashflowClient::new(
            Chain::Ethereum,
            HashSet::new(),
            1.0,
            quote_tokens,
            "test_user".to_string(),
            "test_key".to_string(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    #[tokio::test]
    #[ignore] // Requires network access and HASHFLOW_KEY environment variable
    async fn test_hashflow_api_polling() {
        dotenv().expect("Missing .env file");
        let auth = get_hashflow_auth().unwrap();

        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        let tokens = HashSet::from([wbtc, weth.clone()]);

        let quote_tokens = HashSet::from([
            Bytes::from_str("0xa0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(), // USDT
        ]);

        let client = HashflowClient::new(
            Chain::Ethereum,
            tokens,
            1.0, // $1 minimum TVL - very low to capture most pairs
            quote_tokens,
            auth.user,
            auth.key,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let mut stream = client.stream();

        let result = timeout(Duration::from_secs(10), async {
            let mut message_count = 0;
            let max_messages = 3;
            let mut total_components_received = 0;

            while let Some(result) = stream.next().await {
                match result {
                    Ok((component_id, msg)) => {
                        println!("Received message with ID: {component_id}");

                        assert!(!component_id.is_empty());
                        assert_eq!(component_id, "hashflow");
                        assert!(msg.header.timestamp > 0);

                        let snapshot = &msg.snapshots;
                        total_components_received += snapshot.states.len();

                        println!("Received {} components in this message (Total so far: {})",
                                snapshot.states.len(), total_components_received);

                        assert!(snapshot.states.len() <= 1, "one venue component per chain");
                        for (id, component_with_state) in &snapshot.states {
                            assert_eq!(id, &client.component_id());
                            let books: Vec<HashflowMakerLevels> = serde_json::from_slice(
                                &component_with_state.state.attributes["books"],
                            )
                            .unwrap();
                            assert!(!books.is_empty());
                            let pairs = &component_with_state.component.static_attributes["pairs"];
                            assert_eq!(pairs.len() % 40, 0);
                            if let Some(tvl) = component_with_state.component_tvl {
                                assert!(tvl >= 1.0);
                                println!("Component {id} TVL: ${tvl:.2}");
                            }
                        }

                        message_count += 1;
                        if message_count >= max_messages {
                            break;
                        }
                    }
                    Err(e) => {
                        panic!("Stream error: {e}");
                    }
                }
            }

            assert!(message_count > 0, "Should have received at least one message");
            assert!(total_components_received >= 1, "Should have received at least 1 component with $1 TVL threshold");
            println!("Successfully received {message_count} messages with {total_components_received} total components");
        })
        .await;

        match result {
            Ok(_) => println!("Test completed successfully"),
            Err(_) => panic!("Test timed out - no messages received within 5 seconds"),
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access and setting proper env vars
    async fn test_request_binding_quote() {
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        let auth_user = String::from("propellerheads");
        dotenv().expect("Missing .env file");
        let auth_key = env::var("HASHFLOW_KEY").unwrap();

        let client = HashflowClient::new(
            Chain::Ethereum,
            HashSet::from_iter(vec![weth.clone(), wbtc.clone()]),
            10.0,
            HashSet::new(),
            auth_user,
            auth_key,
            Duration::from_secs(0),
            Duration::from_secs(5),
        )
        .unwrap();

        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        let params = GetAmountOutParams {
            amount_in: BigUint::from(1_000000000000000000u64),
            token_in: weth.clone(),
            token_out: wbtc.clone(),
            sender: router.clone(),
            receiver: router.clone(),
        };
        let quote = client
            .request_binding_quote(&params)
            .await
            .unwrap();

        assert_eq!(quote.base_token, weth);
        assert_eq!(quote.quote_token, wbtc);
        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));

        // // Assuming the BTC - WETH price doesn't change too much at the time of running this
        assert!(quote.amount_out > BigUint::from(3000000u64));

        assert_eq!(quote.quote_attributes.len(), 12);
        let expected_attributes = [
            "pool",
            "external_account",
            "trader",
            "effective_trader",
            "base_token",
            "quote_token",
            "base_token_amount",
            "quote_token_amount",
            "quote_expiry",
            "nonce",
            "tx_id",
            "signature",
        ];
        for attr in expected_attributes {
            assert!(
                quote
                    .quote_attributes
                    .contains_key(attr),
                "Missing attribute: {attr}"
            );
        }
        assert_eq!(
            quote
                .quote_attributes
                .get("trader")
                .unwrap(),
            &router
        );
    }

    /// Response template; the mock server replaces `{{EFFECTIVE_TRADER}}` with the address the
    /// request carried, echoing it like the real API.
    const QUOTE_RESPONSE: &str = r#"{"status":"success","error":null,"rfqId":"test-rfq-id","internalRfqIds":null,"quotes":[{"quoteData":{"pool":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","externalAccount":null,"trader":"0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35","effectiveTrader":"{{EFFECTIVE_TRADER}}","baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","baseTokenAmount":"1000000000000000000","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","quoteTokenAmount":"3329502","quoteExpiry":1707847360,"nonce":1707844960943648659,"txid":"0x0000000000000000000000000000000000000000000000000000000000000001"},"signature":"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef12"}]}"#;

    const QUOTE_RESPONSE_WITHOUT_EFFECTIVE_TRADER: &str = r#"{"status":"success","error":null,"rfqId":"test-rfq-id","internalRfqIds":null,"quotes":[{"quoteData":{"pool":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","externalAccount":null,"trader":"0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35","baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","baseTokenAmount":"1000000000000000000","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","quoteTokenAmount":"3329502","quoteExpiry":1707847360,"nonce":1707844960943648659,"txid":"0x0000000000000000000000000000000000000000000000000000000000000001"},"signature":"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef12"}]}"#;

    /// Reads one HTTP request off the stream and returns its body.
    async fn read_request_body(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw);
            if let Some(header_end) = text.find("\r\n\r\n") {
                let content_length: usize = text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if raw.len() >= header_end + 4 + content_length {
                    return text[header_end + 4..].to_string();
                }
            }
            if n == 0 {
                return String::new();
            }
        }
    }

    /// Extracts the effectiveTrader value from a request body.
    fn effective_trader_of(request_body: &str) -> String {
        let start = request_body
            .find("\"effectiveTrader\":\"")
            .expect("request carries no effectiveTrader") +
            "\"effectiveTrader\":\"".len();
        request_body[start..start + request_body[start..].find('"').unwrap()].to_string()
    }

    /// Creates a mock server that answers with `json_response` after a delay, substituting the
    /// request's effectiveTrader for `{{EFFECTIVE_TRADER}}`. Returns the address and a log of
    /// the received request bodies.
    async fn create_delayed_response_server(
        delay_ms: u64,
        json_response: &'static str,
    ) -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::sync::{Arc, Mutex};

        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let request_log: Arc<Mutex<Vec<String>>> = Arc::default();
        let request_log_server = request_log.clone();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let json_response_clone = json_response.to_owned();
                let request_log = request_log_server.clone();
                tokio::spawn(async move {
                    let body = read_request_body(&mut stream).await;
                    let json_response_clone = json_response_clone
                        .replace("{{EFFECTIVE_TRADER}}", &effective_trader_of(&body));
                    request_log.lock().unwrap().push(body);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        json_response_clone.len(),
                        json_response_clone
                    );
                    let _ = stream
                        .write_all(response.as_bytes())
                        .await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, request_log)
    }

    fn create_test_hashflow_client(
        quote_endpoint: String,
        quote_timeout: Duration,
    ) -> HashflowClient {
        let token_in = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let token_out = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();

        HashflowClient {
            chain: Chain::Ethereum,
            price_levels_endpoint: "http://unused/price-levels".to_string(),
            market_makers_endpoint: "http://unused/market-makers".to_string(),
            quote_endpoint,
            tokens: HashSet::from([token_in, token_out]),
            tvl: 10.0,
            auth_key: "test_key".to_string(),
            auth_user: "test_user".to_string(),
            quote_tokens: HashSet::new(),
            poll_time: Duration::from_secs(0),
            quote_timeout,
        }
    }

    /// Helper function to create test quote params
    fn create_test_quote_params() -> GetAmountOutParams {
        let token_in = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let token_out = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        GetAmountOutParams {
            amount_in: BigUint::from(1_000000000000000000u64),
            token_in,
            token_out,
            sender: router.clone(),
            receiver: router,
        }
    }

    #[tokio::test]
    async fn test_request_binding_quote_without_effective_trader() {
        // A response that drops the requested effectiveTrader would leave the quote in the
        // trader's shared nonce scope, so the client rejects it.
        let (addr, _) =
            create_delayed_response_server(0, QUOTE_RESPONSE_WITHOUT_EFFECTIVE_TRADER).await;
        let client = create_test_hashflow_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );
        let params = create_test_quote_params();

        let err = client
            .request_binding_quote(&params)
            .await
            .unwrap_err();

        assert!(format!("{err:?}").contains("Effective trader mismatch"));
    }

    #[tokio::test]
    async fn test_request_binding_quote_field_mapping() {
        // The wire request carries the receiver as Hashflow's trader and a fresh random
        // address as the effectiveTrader — a new one per quote request.
        let (addr, request_log) = create_delayed_response_server(0, QUOTE_RESPONSE).await;
        let client = create_test_hashflow_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );
        let params = create_test_quote_params();

        let first_quote = client
            .request_binding_quote(&params)
            .await
            .unwrap();
        client
            .request_binding_quote(&params)
            .await
            .unwrap();

        let requests = request_log.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for body in requests.iter() {
            assert!(
                body.contains(&format!("\"trader\":\"{}\"", params.receiver)),
                "trader is not the receiver: {body}"
            );
        }
        let first = effective_trader_of(&requests[0]);
        let second = effective_trader_of(&requests[1]);
        assert_eq!(first.len(), 42, "effective trader is not an address");
        assert_ne!(first, second, "effective traders are not unique per quote");
        assert_ne!(first, params.receiver.to_string(), "effective trader equals the trader");
        assert_eq!(
            first_quote
                .quote_attributes
                .get("effective_trader")
                .unwrap()
                .to_string(),
            first,
            "quote attributes do not carry the requested effective trader"
        );
    }

    #[tokio::test]
    async fn test_request_binding_quote_names_the_maker() {
        let (addr, request_log) = create_delayed_response_server(0, QUOTE_RESPONSE).await;
        let client = create_test_hashflow_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );
        let params = create_test_quote_params();

        client
            .request_binding_quote_from(&params, Some("mm1"))
            .await
            .unwrap();
        client
            .request_binding_quote(&params)
            .await
            .unwrap();

        let requests = request_log.lock().unwrap();
        assert!(requests[0].contains("\"marketMakers\":[\"mm1\"]"), "{}", requests[0]);
        assert!(requests[0].contains("\"doNotRetryWithOtherMakers\":true"), "{}", requests[0]);
        assert!(!requests[1].contains("marketMakers"), "{}", requests[1]);
        assert!(!requests[1].contains("options"), "{}", requests[1]);
    }

    fn levels(pair: (&Bytes, &Bytes), levels: &[(f64, f64)]) -> HashflowMarketMakerLevels {
        HashflowMarketMakerLevels {
            pair: HashflowPair { base_token: pair.0.clone(), quote_token: pair.1.clone() },
            levels: levels
                .iter()
                .map(|&(quantity, price)| HashflowPriceLevel { quantity, price })
                .collect(),
        }
    }

    /// WETH and WBTC quoted against USDC by two makers, with USDC as the TVL quote token.
    fn venue_test_client() -> (HashflowClient, Bytes, Bytes, Bytes) {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let mut client =
            create_test_hashflow_client("http://unused/rfq".to_string(), Duration::from_secs(1));
        client.tokens = HashSet::from([weth.clone(), wbtc.clone(), usdc.clone()]);
        client.quote_tokens = HashSet::from([usdc.clone()]);
        client.tvl = 100.0;
        (client, weth, wbtc, usdc)
    }

    #[test]
    fn test_venue_component() {
        let (client, weth, wbtc, usdc) = venue_test_client();
        let levels_by_mm = HashMap::from([
            (
                "mm_b".to_string(),
                vec![
                    levels((&weth, &usdc), &[(1.0, 3000.0)]),
                    levels((&wbtc, &usdc), &[(0.001, 65.0)]),
                ],
            ),
            ("mm_a".to_string(), vec![levels((&weth, &usdc), &[(2.0, 3000.0)])]),
        ]);

        let component = client
            .venue_component(&levels_by_mm)
            .unwrap()
            .expect("two books clear the threshold");

        assert_eq!(component.component.id, client.component_id());
        assert_eq!(component.component_tvl, Some(9000.0), "the WBTC book is below the threshold");
        let mut expected_tokens = vec![weth.clone(), usdc.clone()];
        expected_tokens.sort();
        assert_eq!(component.component.tokens, expected_tokens);
        let mut expected_pairs = weth.to_vec();
        expected_pairs.extend_from_slice(&usdc);
        assert_eq!(component.component.static_attributes["pairs"].to_vec(), expected_pairs);

        let books: Vec<HashflowMakerLevels> =
            serde_json::from_slice(&component.state.attributes["books"]).unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].market_maker, "mm_a", "books are sorted by maker");
        assert_eq!(books[0].levels[0].quantity, 2.0);
        assert_eq!(books[1].market_maker, "mm_b");
        assert_eq!(books[1].pair.base_token, weth);
    }

    #[test]
    fn test_venue_component_without_books() {
        let (client, _, wbtc, usdc) = venue_test_client();
        let levels_by_mm =
            HashMap::from([("mm_a".to_string(), vec![levels((&wbtc, &usdc), &[(0.001, 65.0)])])]);
        assert!(client
            .venue_component(&levels_by_mm)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_hashflow_quote_timeout() {
        let (addr, _) = create_delayed_response_server(500, QUOTE_RESPONSE).await;

        // Test 1: Client with short timeout (200ms) - should timeout
        let client_short_timeout = create_test_hashflow_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_millis(200),
        );
        let params = create_test_quote_params();

        // This should timeout after 200ms
        let start = std::time::Instant::now();
        let result = client_short_timeout
            .request_binding_quote(&params)
            .await;
        let elapsed = start.elapsed();

        // Verify that we got a timeout error
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            RFQError::ConnectionError(msg) => {
                assert!(msg.contains("timed out"), "Expected timeout error, got: {}", msg);
            }
            _ => panic!("Expected ConnectionError, got: {:?}", err),
        }
        // Should have timed out around 200ms, definitely less than 400ms
        assert!(
            elapsed.as_millis() >= 200 && elapsed.as_millis() < 400,
            "Expected timeout around 200ms, got: {:?}",
            elapsed
        );

        // Test 2: Client with long timeout (1 second) - should wait and receive response
        // Note: With retry logic, we may need multiple attempts if the response is malformed,
        // so we need a longer timeout to account for retries
        let client_long_timeout = create_test_hashflow_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );

        // This should wait for the response (500ms)
        let result = client_long_timeout
            .request_binding_quote(&params)
            .await;

        // Should succeed - the server waits 500ms which is within the 1s timeout
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
    }

    /// Helper function to create a mock server that fails twice, then succeeds
    async fn create_retry_server() -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<u32>>)
    {
        use std::sync::{Arc, Mutex};

        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let request_count = Arc::new(Mutex::new(0u32));
        let request_count_clone = request_count.clone();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let count_clone = request_count_clone.clone();
                tokio::spawn(async move {
                    *count_clone.lock().unwrap() += 1;
                    let count = *count_clone.lock().unwrap();
                    println!("Mock server: Received request #{count}");

                    let body = read_request_body(&mut stream).await;
                    if count <= 2 {
                        let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 21\r\n\r\nInternal Server Error";
                        let _ = stream
                            .write_all(response.as_bytes())
                            .await;
                    } else {
                        let json_response = QUOTE_RESPONSE
                            .replace("{{EFFECTIVE_TRADER}}", &effective_trader_of(&body));
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            json_response.len(),
                            json_response
                        );
                        let _ = stream
                            .write_all(response.as_bytes())
                            .await;
                    }
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, request_count)
    }

    #[tokio::test]
    async fn test_hashflow_quote_retry_on_bad_response() {
        let (addr, request_count) = create_retry_server().await;

        let client = create_test_hashflow_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(5),
        );
        let params = create_test_quote_params();
        let result = client
            .request_binding_quote(&params)
            .await;

        assert!(result.is_ok(), "Expected success after retries, got: {:?}", result);
        let quote = result.unwrap();

        // Verify the quote is parsed as expected
        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));
        assert_eq!(quote.amount_out, BigUint::from(3329502u64));

        // Verify exactly 3 requests were made (2 failures + 1 success)
        let final_count = *request_count.lock().unwrap();
        assert_eq!(final_count, 3, "Expected 3 requests, got {}", final_count);
    }

    #[test]
    fn test_hashflow_client_serialize_deserialize_roundtrip() {
        let token_in = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let token_out = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let quote_token = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

        let original = HashflowClient {
            chain: Chain::Ethereum,
            price_levels_endpoint: "https://api.hashflow.com/price_levels".to_string(),
            market_makers_endpoint: "https://api.hashflow.com/market_makers".to_string(),
            quote_endpoint: "https://api.hashflow.com/quote".to_string(),
            tokens: HashSet::from([token_in.clone(), token_out.clone()]),
            tvl: 50.5,
            auth_key: "secret_key".to_string(),
            auth_user: "secret_user".to_string(),
            quote_tokens: HashSet::from([quote_token.clone()]),
            poll_time: Duration::from_secs(10),
            quote_timeout: Duration::from_millis(5500),
        };

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: HashflowClient = serde_json::from_str(&serialized).unwrap();

        // Fields that should round-trip correctly
        assert_eq!(deserialized.chain, original.chain);
        assert_eq!(deserialized.price_levels_endpoint, original.price_levels_endpoint);
        assert_eq!(deserialized.market_makers_endpoint, original.market_makers_endpoint);
        assert_eq!(deserialized.quote_endpoint, original.quote_endpoint);
        assert_eq!(deserialized.tokens, original.tokens);
        assert_eq!(deserialized.tvl, original.tvl);
        assert_eq!(deserialized.quote_tokens, original.quote_tokens);
        assert_eq!(deserialized.poll_time, original.poll_time);
        assert_eq!(deserialized.quote_timeout, original.quote_timeout);

        // auth_key and auth_user should NOT round-trip (skip_serializing + default)
        assert_eq!(deserialized.auth_key, "");
        assert_eq!(deserialized.auth_user, "");
        assert_ne!(deserialized.auth_key, original.auth_key);
        assert_ne!(deserialized.auth_user, original.auth_user);
    }

    #[test]
    fn test_hashflow_client_deserialize_with_credentials() {
        // When auth_key and auth_user are provided in JSON, they should be deserialized
        // (skip_serializing only affects serialization, not deserialization)
        let json = r#"{
            "chain": "ethereum",
            "price_levels_endpoint": "https://api.hashflow.com/price_levels",
            "market_makers_endpoint": "https://api.hashflow.com/market_makers",
            "quote_endpoint": "https://api.hashflow.com/quote",
            "tokens": [],
            "tvl": 10.0,
            "auth_key": "provided_key",
            "auth_user": "provided_user",
            "quote_tokens": [],
            "poll_time": {"secs": 10, "nanos": 0},
            "quote_timeout": {"secs": 30, "nanos": 0}
        }"#;

        let client: HashflowClient = serde_json::from_str(json).unwrap();

        // Credentials should be deserialized from JSON
        assert_eq!(client.auth_key, "provided_key");
        assert_eq!(client.auth_user, "provided_user");
    }
}
