use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    time::SystemTime,
};

use alloy::primitives::utils::keccak256;
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use reqwest::Client;
use tokio::time::{interval, timeout, Duration};
use tracing::{debug, error, info, warn};
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
        protocols::liquorice::models::{
            LiquoricePriceLevelsResponse, LiquoriceQuoteRequest, LiquoriceQuoteResponse,
            LiquoriceTokenPairPrice,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LiquoriceClient {
    chain: Chain,
    price_levels_endpoint: String,
    quote_endpoint: String,
    // Tokens that we want prices for
    tokens: HashSet<Bytes>,
    // Min tvl value in the quote token.
    tvl: f64,
    // solver header for authentication
    #[serde(skip_serializing, default)]
    auth_solver: String,
    // key header for authentication
    #[serde(skip_serializing, default)]
    auth_key: String,
    quote_tokens: HashSet<Bytes>,
    poll_time: Duration,
    quote_timeout: Duration,
    quote_expiry_secs: u64,
}

impl LiquoriceClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:liquorice";

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        tokens: HashSet<Bytes>,
        tvl: f64,
        quote_tokens: HashSet<Bytes>,
        auth_solver: String,
        auth_key: String,
        poll_time: Duration,
        quote_timeout: Duration,
        quote_expiry_secs: u64,
    ) -> Result<Self, RFQError> {
        Ok(Self {
            chain,
            price_levels_endpoint: "https://api.liquorice.tech/v1/solver/price-levels".to_string(),
            quote_endpoint: "https://api.liquorice.tech/v1/solver/rfq".to_string(),
            tokens,
            tvl,
            auth_solver,
            auth_key,
            quote_tokens,
            poll_time,
            quote_timeout,
            quote_expiry_secs,
        })
    }

    fn normalize_tvl(
        &self,
        raw_tvl: f64,
        quote_token: Bytes,
        prices_by_mm: &HashMap<String, Vec<LiquoriceTokenPairPrice>>,
    ) -> Result<f64, RFQError> {
        if self.quote_tokens.contains(&quote_token) {
            return Ok(raw_tvl);
        }

        for approved_quote_token in &self.quote_tokens {
            for token_prices in prices_by_mm.values() {
                for token_price in token_prices {
                    if token_price.base_token == quote_token &&
                        token_price.quote_token == *approved_quote_token
                    {
                        if let Some(price) = token_price.get_price_for_amount(1.0) {
                            return Ok(raw_tvl * price);
                        }
                    }
                }
            }
        }

        Ok(0.0)
    }

    fn create_component_with_state(
        &self,
        component_id: String,
        tokens: Vec<Bytes>,
        prices_by_mm: &HashMap<String, LiquoriceTokenPairPrice>,
        tvl: f64,
    ) -> ComponentWithState {
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "liquorice_pool".to_string(),
            chain: self.chain,
            tokens,
            contract_addresses: vec![],
            ..Default::default()
        };

        let mut attributes = HashMap::new();

        let prices_json = serde_json::to_string(&prices_by_mm).unwrap_or_default();
        attributes.insert("prices".to_string(), prices_json.as_bytes().to_vec().into());

        ComponentWithState {
            state: ProtocolComponentState::new(&component_id, attributes, HashMap::new()),
            component: protocol_component,
            component_tvl: Some(tvl),
            entrypoints: vec![],
        }
    }

    fn process_quote_response(
        quote_response: LiquoriceQuoteResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        if !quote_response.liquidity_available {
            debug!(quote_response = ?quote_response, "Liquorice quote response indicates no liquidity");
            return Err(RFQError::QuoteNotFound(format!(
                "Liquorice quote not found for {} {} ->{}",
                params.amount_in, params.token_in, params.token_out,
            )));
        }

        info!("Received Liquorice quote response with {} levels", quote_response.levels.len());

        // Find the valid level with the largest quote_token_amount
        let best_level = quote_response
            .levels
            .iter()
            .filter(|level| level.validate(params).is_ok())
            .filter_map(|level| {
                BigUint::from_str(&level.quote_token_amount)
                    .ok()
                    .map(|amount| (level, amount))
            })
            .max_by(|(_, a), (_, b)| a.cmp(b));

        let (quote_level, _) = best_level.ok_or_else(|| {
            RFQError::QuoteNotFound(format!(
                "No valid Liquorice quote levels for {} {} ->{}",
                params.amount_in, params.token_in, params.token_out,
            ))
        })?;

        let mut quote_attributes: HashMap<String, Bytes> = HashMap::new();

        // calldata (pre-encoded by Liquorice API)
        quote_attributes.insert(
            "calldata".to_string(),
            Bytes::from(
                hex::decode(
                    quote_level
                        .tx
                        .data
                        .trim_start_matches("0x"),
                )
                .map_err(|e| RFQError::ParsingError(format!("Failed to parse calldata: {e}")))?,
            ),
        );

        // base_token_amount as U256 (32 bytes big-endian)
        quote_attributes.insert(
            "base_token_amount".to_string(),
            Bytes::from(
                biguint_to_u256(&BigUint::from_str(&quote_level.base_token_amount).map_err(
                    |_| {
                        RFQError::ParsingError(format!(
                            "Failed to parse base token amount: {}",
                            quote_level.base_token_amount
                        ))
                    },
                )?)
                .to_be_bytes::<32>()
                .to_vec(),
            ),
        );

        // partial fill info (if present)
        if let Some(pf) = &quote_level.partial_fill {
            quote_attributes.insert(
                "partial_fill_offset".to_string(),
                Bytes::from(pf.offset.to_be_bytes().to_vec()),
            );
            quote_attributes.insert(
                "min_base_token_amount".to_string(),
                Bytes::from(
                    biguint_to_u256(&BigUint::from_str(&pf.min_base_token_amount).map_err(
                        |_| {
                            RFQError::ParsingError(format!(
                                "Failed to parse min_base_token_amount: {}",
                                pf.min_base_token_amount
                            ))
                        },
                    )?)
                    .to_be_bytes::<32>()
                    .to_vec(),
                ),
            );
        }

        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: BigUint::from_str(&quote_level.base_token_amount).map_err(|_| {
                RFQError::ParsingError(format!(
                    "Failed to parse amount in string: {}",
                    quote_level.base_token_amount
                ))
            })?,
            amount_out: BigUint::from_str(&quote_level.quote_token_amount).map_err(|_| {
                RFQError::ParsingError(format!(
                    "Failed to parse amount out string: {}",
                    quote_level.quote_token_amount
                ))
            })?,
            quote_attributes,
        })
    }

    async fn fetch_price_levels(
        &self,
    ) -> Result<HashMap<String, Vec<LiquoriceTokenPairPrice>>, RFQError> {
        let query_params = vec![("chainId", self.chain.id().to_string())];

        let http_client = Client::new();
        let request = http_client
            .get(&self.price_levels_endpoint)
            .query(&query_params)
            .header("accept", "application/json")
            .header("solver", &self.auth_solver)
            .header("authorization", &self.auth_key);

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

        let price_response: LiquoricePriceLevelsResponse = response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse price levels response: {e}"))
        })?;

        Ok(price_response.prices)
    }
}

#[async_trait]
impl RFQClient for LiquoriceClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_components: HashMap<String, ComponentWithState> = HashMap::new();
            let mut ticker = interval(client.poll_time);

            info!("Starting Liquorice price levels polling every {} seconds", client.poll_time.as_secs());
            info!("TVL threshold: {:.2}", client.tvl);

            loop {
                ticker.tick().await;

                match client.fetch_price_levels().await {
                    Ok(prices_by_mm) => {
                        let mut new_components = HashMap::new();

                        // Group qualifying MMs by token pair
                        struct PricesWithTvl {
                            // MM name -> price levels for the token pair
                            mm_prices: HashMap<String, LiquoriceTokenPairPrice>,
                            // The highest TVL among MMs for this token pair, used as the component's TVL
                            tvl: f64,
                        }
                        let mut pair_mm_prices: HashMap<(Bytes, Bytes), PricesWithTvl> = HashMap::new();

                        info!("Fetched price levels from {} market makers", prices_by_mm.len());
                        for (mm_name, token_pair_prices) in prices_by_mm.iter() {
                            for token_pair_price in token_pair_prices {
                                let base_token = &token_pair_price.base_token;
                                let quote_token = &token_pair_price.quote_token;

                                if !client.tokens.contains(base_token) || !client.tokens.contains(quote_token) {
                                    continue;
                                }

                                let tvl = token_pair_price.calculate_tvl();
                                let normalized_tvl = client.normalize_tvl(
                                    tvl,
                                    token_pair_price.quote_token.clone(),
                                    &prices_by_mm,
                                )?;

                                if normalized_tvl < client.tvl {
                                    info!("Filtering out MM {} for pair {}/{} due to low TVL: {:.2} < {:.2}",
                                          mm_name, hex::encode(base_token), hex::encode(quote_token),
                                          normalized_tvl, client.tvl);
                                    continue;
                                }

                                let entry = pair_mm_prices
                                    .entry((base_token.clone(), quote_token.clone()))
                                    .or_insert_with(|| PricesWithTvl { mm_prices: HashMap::new(), tvl: f64::NEG_INFINITY });
                                entry.tvl = entry.tvl.max(normalized_tvl);
                                entry.mm_prices.insert(mm_name.clone(), token_pair_price.clone());
                            }
                        }

                        for ((base_token, quote_token), PricesWithTvl { mm_prices, tvl: component_tvl }) in pair_mm_prices {
                            let pair_str = format!("liquorice_{}/{}", hex::encode(&base_token), hex::encode(&quote_token));
                            let component_id = format!("{}", keccak256(pair_str.as_bytes()));

                            let tokens = vec![base_token, quote_token];

                            let component_with_state = client.create_component_with_state(
                                component_id.clone(),
                                tokens,
                                &mm_prices,
                                component_tvl,
                            );
                            new_components.insert(component_id, component_with_state);
                        }

                        let removed_components: HashMap<String, ProtocolComponent> = current_components
                            .iter()
                            .filter(|&(id, _)| !new_components.contains_key(id))
                            .map(|(k, v)| (k.clone(), v.component.clone()))
                            .collect();

                        current_components = new_components.clone();

                        let snapshot = Snapshot {
                            states: new_components,
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

                        yield Ok(("liquorice".to_string(), msg));
                    },
                    Err(e) => {
                        error!("Failed to fetch price levels from Liquorice API: {}", e);
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
        let expiry = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".into()))?
            .as_secs() +
            self.quote_expiry_secs;

        let rfq_id = uuid::Uuid::new_v4().to_string();

        let quote_request = LiquoriceQuoteRequest {
            chain_id: self.chain.id(),
            rfq_id: rfq_id.clone(),
            expiry,
            base_token: params.token_in.to_string(),
            quote_token: params.token_out.to_string(),
            trader: params.receiver.to_string(),
            effective_trader: Some(params.sender.to_string()),
            base_token_amount: Some(params.amount_in.to_string()),
            quote_token_amount: None,
        };

        debug!(quote_request = ?quote_request, "Sending Liquorice quote request");

        let url = self.quote_endpoint.clone();

        let start_time = std::time::Instant::now();
        const MAX_RETRIES: u32 = 3;
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            let elapsed = start_time.elapsed();
            if elapsed >= self.quote_timeout {
                return Err(last_error.unwrap_or_else(|| {
                    RFQError::ConnectionError(format!(
                        "Liquorice quote request timed out after {} seconds",
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
                .header("solver", &self.auth_solver)
                .header("authorization", &self.auth_key);

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    warn!(
                        "Liquorice quote request failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    last_error = Some(RFQError::ConnectionError(format!(
                        "Failed to send Liquorice quote request: {e}"
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
                        "Liquorice quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    )));
                }
            };

            if response.status() != 200 {
                let err_msg = match response.text().await {
                    Ok(text) => text,
                    Err(e) => {
                        warn!(
                            "Liquorice error response parsing failed (attempt {}/{}): {}",
                            attempt + 1,
                            MAX_RETRIES,
                            e
                        );
                        last_error = Some(RFQError::ParsingError(format!(
                            "Failed to read response text from Liquorice failed request: {e}"
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
                    "Failed to send Liquorice quote request: {err_msg}",
                )));
                if attempt < MAX_RETRIES - 1 {
                    warn!(
                        "Liquorice returned non-200 status (attempt {}/{}): {}",
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
                .json::<LiquoriceQuoteResponse>()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(
                        "Liquorice quote response parsing failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    last_error = Some(RFQError::ParsingError(format!(
                        "Failed to parse Liquorice quote response: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
            };

            return Self::process_quote_response(quote_response, params);
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Liquorice quote request failed after retries".to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use super::*;
    use crate::rfq::protocols::liquorice::models::{LiquoricePriceLevel, LiquoriceTokenPairPrice};

    #[test]
    fn test_normalize_tvl_same_quote_token() {
        let client = create_test_client();
        let prices = HashMap::new();

        let result = client.normalize_tvl(
            1000.0,
            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            &prices,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1000.0);
    }

    #[test]
    fn test_normalize_tvl_different_quote_token() {
        let client = create_test_client();
        let mut prices = HashMap::new();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();

        let eth_usdc_price = LiquoriceTokenPairPrice {
            base_token: weth.clone(),
            quote_token: usdc,
            levels: vec![LiquoricePriceLevel { quantity: 1.0, price: 3000.0 }],
            updated_at: None,
        };

        prices.insert("test_mm".to_string(), vec![eth_usdc_price]);

        let result = client.normalize_tvl(2.0, weth, &prices);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 6000.0);
    }

    #[test]
    fn test_normalize_tvl_no_conversion_available() {
        let client = create_test_client();
        let prices = HashMap::new();
        let result = client.normalize_tvl(
            1000.0,
            Bytes::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            &prices,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0.0);
    }

    fn create_test_client() -> LiquoriceClient {
        let quote_tokens = HashSet::from([
            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(), // USDT
        ]);

        LiquoriceClient::new(
            Chain::Ethereum,
            HashSet::new(),
            1.0,
            quote_tokens,
            "test_solver".to_string(),
            "test_key".to_string(),
            Duration::from_secs(5),
            Duration::from_secs(5),
            300,
        )
        .unwrap()
    }

    async fn create_delayed_response_server(delay_ms: u64) -> std::net::SocketAddr {
        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let json_response = r#"{"rfqId":"test-rfq-id","liquidityAvailable":true,"levels":[{"makerRfqId":"maker-rfq-1","maker":"test-maker","nonce":"0x0000000000000000000000000000000000000000000000000000000000000001","expiry":1707847360,"tx":{"to":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","data":"0xdeadbeef"},"baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","baseTokenAmount":"1000000000000000000","quoteTokenAmount":"3329502","partialFill":null,"allowances":[]}]}"#;

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let json_response_clone = json_response.to_owned();
                tokio::spawn(async move {
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
        addr
    }

    fn create_test_liquorice_client(
        quote_endpoint: String,
        quote_timeout: Duration,
    ) -> LiquoriceClient {
        let token_in = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let token_out = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();

        LiquoriceClient {
            chain: Chain::Ethereum,
            price_levels_endpoint: "http://unused/price-levels".to_string(),
            quote_endpoint,
            tokens: HashSet::from([token_in, token_out]),
            tvl: 10.0,
            auth_solver: "test_solver".to_string(),
            auth_key: "test_key".to_string(),
            quote_tokens: HashSet::new(),
            poll_time: Duration::from_secs(0),
            quote_timeout,
            quote_expiry_secs: 300,
        }
    }

    fn make_quote_level(
        base_token: &str,
        quote_token: &str,
        base_token_amount: &str,
        quote_token_amount: &str,
        partial_fill: Option<crate::rfq::protocols::liquorice::models::LiquoricePartialFill>,
    ) -> crate::rfq::protocols::liquorice::models::LiquoriceQuoteLevel {
        use crate::rfq::protocols::liquorice::models::{LiquoriceQuoteLevel, LiquoriceTx};
        LiquoriceQuoteLevel {
            maker_rfq_id: "maker-1".to_string(),
            maker: "test-maker".to_string(),
            expiry: 9999999999,
            tx: LiquoriceTx {
                to: "0x1111111111111111111111111111111111111111".to_string(),
                data: "0xdeadbeef".to_string(),
            },
            base_token: base_token.to_string(),
            quote_token: quote_token.to_string(),
            base_token_amount: base_token_amount.to_string(),
            quote_token_amount: quote_token_amount.to_string(),
            partial_fill,
        }
    }

    fn make_params(token_in: &str, token_out: &str, amount_in: u64) -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from(amount_in),
            token_in: Bytes::from_str(token_in).unwrap(),
            token_out: Bytes::from_str(token_out).unwrap(),
            sender: Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap(),
            receiver: Bytes::from_str("0x4444444444444444444444444444444444444444").unwrap(),
        }
    }

    #[test]
    fn test_process_quote_response_no_liquidity() {
        use crate::rfq::protocols::liquorice::models::LiquoriceQuoteResponse;

        let response = LiquoriceQuoteResponse {
            rfq_id: "r1".to_string(),
            liquidity_available: false,
            levels: vec![],
        };
        let params = make_params(
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
            1_000_000_000_000_000_000,
        );
        let result = LiquoriceClient::process_quote_response(response, &params);
        assert!(
            matches!(result, Err(RFQError::QuoteNotFound(_))),
            "expected QuoteNotFound, got {:?}",
            result
        );
    }

    #[test]
    fn test_process_quote_response_partial_fill_attributes() {
        use crate::rfq::protocols::liquorice::models::{
            LiquoricePartialFill, LiquoriceQuoteResponse,
        };

        let token_in = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
        let token_out = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";
        let amount_in = 1_000_000_000_000_000_000u64;

        let level = make_quote_level(
            token_in,
            token_out,
            &amount_in.to_string(),
            "3329502",
            Some(LiquoricePartialFill {
                offset: 68,
                min_base_token_amount: "500000000000000000".to_string(),
            }),
        );
        let response = LiquoriceQuoteResponse {
            rfq_id: "r1".to_string(),
            liquidity_available: true,
            levels: vec![level],
        };
        let params = make_params(token_in, token_out, amount_in);

        let quote = LiquoriceClient::process_quote_response(response, &params).unwrap();

        // partial_fill_offset: 4-byte big-endian encoding of 68
        let offset_bytes = quote.quote_attributes["partial_fill_offset"].clone();
        assert_eq!(offset_bytes.as_ref(), &68u32.to_be_bytes());

        // min_base_token_amount: 32-byte big-endian U256 of 500000000000000000
        let min_amount_bytes = quote.quote_attributes["min_base_token_amount"].clone();
        assert_eq!(min_amount_bytes.len(), 32);
        let expected_min = BigUint::from(500_000_000_000_000_000u64);
        let actual_min = BigUint::from_bytes_be(min_amount_bytes.as_ref());
        assert_eq!(actual_min, expected_min);
    }

    #[test]
    fn test_process_quote_response_selects_best_valid_level() {
        use crate::rfq::protocols::liquorice::models::LiquoriceQuoteResponse;

        let token_in = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
        let token_out = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";
        let amount_in = 1_000_000_000_000_000_000u64;

        // One level with wrong base_token_amount (will fail validate) and two
        // valid levels with different quote_token_amounts; expect the higher one
        // to be chosen.
        let invalid_level = make_quote_level(token_in, token_out, "999", "9999999", None);
        let lower_level =
            make_quote_level(token_in, token_out, &amount_in.to_string(), "3000000", None);
        let best_level =
            make_quote_level(token_in, token_out, &amount_in.to_string(), "3500000", None);

        let response = LiquoriceQuoteResponse {
            rfq_id: "r1".to_string(),
            liquidity_available: true,
            levels: vec![invalid_level, lower_level, best_level],
        };
        let params = make_params(token_in, token_out, amount_in);

        let quote = LiquoriceClient::process_quote_response(response, &params).unwrap();
        assert_eq!(quote.amount_out, BigUint::from(3_500_000u64));
    }

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
    async fn test_liquorice_quote_timeout() {
        let addr = create_delayed_response_server(500).await;

        let client_short_timeout = create_test_liquorice_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_millis(200),
        );
        let params = create_test_quote_params();

        let start = std::time::Instant::now();
        let result = client_short_timeout
            .request_binding_quote(&params)
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            RFQError::ConnectionError(msg) => {
                assert!(msg.contains("timed out"), "Expected timeout error, got: {}", msg);
            }
            _ => panic!("Expected ConnectionError, got: {:?}", err),
        }
        assert!(
            elapsed.as_millis() >= 200 && elapsed.as_millis() < 400,
            "Expected timeout around 200ms, got: {:?}",
            elapsed
        );

        let client_long_timeout = create_test_liquorice_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );

        let result = client_long_timeout
            .request_binding_quote(&params)
            .await;
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
    }

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

        let json_response = r#"{"rfqId":"test-rfq-id","liquidityAvailable":true,"levels":[{"makerRfqId":"maker-rfq-1","maker":"test-maker","nonce":"0x0000000000000000000000000000000000000000000000000000000000000001","expiry":1707847360,"tx":{"to":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","data":"0xdeadbeef"},"baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","baseTokenAmount":"1000000000000000000","quoteTokenAmount":"3329502","partialFill":null,"allowances":[]}]}"#;

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let count_clone = request_count_clone.clone();
                let json_response_clone = json_response.to_owned();
                tokio::spawn(async move {
                    *count_clone.lock().unwrap() += 1;
                    let count = *count_clone.lock().unwrap();
                    println!("Mock server: Received request #{count}");

                    if count <= 2 {
                        let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 21\r\n\r\nInternal Server Error";
                        let _ = stream
                            .write_all(response.as_bytes())
                            .await;
                    } else {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            json_response_clone.len(),
                            json_response_clone
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
    async fn test_liquorice_quote_retry_on_bad_response() {
        let (addr, request_count) = create_retry_server().await;

        let client = create_test_liquorice_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(5),
        );
        let params = create_test_quote_params();
        let result = client
            .request_binding_quote(&params)
            .await;

        assert!(result.is_ok(), "Expected success after retries, got: {:?}", result);
        let quote = result.unwrap();

        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));
        assert_eq!(quote.amount_out, BigUint::from(3329502u64));

        let final_count = *request_count.lock().unwrap();
        assert_eq!(final_count, 3, "Expected 3 requests, got {}", final_count);
    }
}
