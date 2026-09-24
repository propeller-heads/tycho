use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{Address, U256};
use num_bigint::BigUint;
use reqwest::{Client, RequestBuilder};
use serde::{
    de::{DeserializeOwned, IgnoredAny},
    Deserialize, Serialize,
};
use tokio::time::{timeout, Duration};
use tracing::{debug, instrument, warn};
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::{
    evm::protocol::u256_num::biguint_to_u256,
    rfq::{
        errors::RFQError,
        protocols::hashflow::models::{
            HashflowChain, HashflowError, HashflowMarketMakerLevels, HashflowMarketMakersResponse,
            HashflowPriceLevelsResponse, HashflowQuoteRequest, HashflowQuoteResponse, HashflowRFQ,
            HashflowRFQOptions, HashflowResponse,
        },
    },
    snapshot_feed::{
        errors::FeedError,
        http::{fetch_with_status, http_error},
    },
};

/// Requests binding Hashflow quotes. One instance is shared (via `Arc`) by every state a
/// [`HashflowFeed`](super::feed::HashflowFeed) emits, so all of them reuse the same HTTP
/// connection pool.
///
/// Serialization keeps the configuration but skips the credentials and the HTTP client: a
/// deserialized client gets a fresh connection pool and empty credentials, so binding quotes
/// fail at call time until re-configured. `Debug` output omits the credentials as well.
#[derive(derive_more::Debug, Serialize, Deserialize)]
pub struct HashflowClient {
    chain: Chain,
    quote_endpoint: String,
    price_levels_endpoint: String,
    market_makers_endpoint: String,
    #[serde(skip_serializing, default)]
    #[debug(skip)]
    source: String,
    #[serde(skip_serializing, default)]
    #[debug(skip)]
    auth_key: String,
    quote_timeout: Duration,
    #[serde(skip)]
    http: Client,
}

impl HashflowClient {
    pub fn new(
        chain: Chain,
        quote_endpoint: String,
        price_levels_endpoint: String,
        market_makers_endpoint: String,
        source: String,
        auth_key: String,
        quote_timeout: Duration,
    ) -> Self {
        HashflowClient {
            chain,
            quote_endpoint,
            price_levels_endpoint,
            market_makers_endpoint,
            source,
            auth_key,
            quote_timeout,
            http: Client::new(),
        }
    }

    /// The query every book request carries: who is asking and about which chain.
    fn chain_query(&self) -> Vec<(&'static str, String)> {
        vec![
            ("source", self.source.clone()),
            ("baseChainType", "evm".to_string()),
            ("baseChainId", self.chain.id().to_string()),
        ]
    }

    /// The makers currently quoting on this chain.
    pub async fn fetch_market_makers(&self) -> Result<Vec<String>, FeedError> {
        let request = self
            .http
            .get(&self.market_makers_endpoint)
            .query(&self.chain_query())
            .header("accept", "application/json")
            .header("Authorization", &self.auth_key);
        let mm_response: HashflowMarketMakersResponse =
            fetch(request, "Hashflow market makers").await?;
        debug!(
            count = mm_response.market_makers.len(),
            market_makers = ?mm_response.market_makers,
            "fetched market makers"
        );
        Ok(mm_response.market_makers)
    }

    /// The price levels the given makers are quoting, keyed by maker name.
    pub async fn fetch_price_levels(
        &self,
        market_makers: &[String],
    ) -> Result<HashMap<String, Vec<HashflowMarketMakerLevels>>, FeedError> {
        let mut query_params = self.chain_query();
        // Add market makers as array parameters
        for mm in market_makers {
            query_params.push(("marketMakers[]", mm.clone()));
        }
        let request = self
            .http
            .get(&self.price_levels_endpoint)
            .query(&query_params)
            .header("accept", "application/json")
            .header("Authorization", &self.auth_key);

        Ok(fetch::<HashflowResponse<HashflowPriceLevelsResponse>>(request, "Hashflow price levels")
            .await?
            .into_result()
            .map_err(|error| FeedError::Connection(format!("Hashflow price levels: {error}")))?
            .levels)
    }

    /// Requests a signed quote from `market_maker` alone: the API does not fall back to another
    /// maker when it declines, so a quote always comes from the maker whose levels priced it.
    #[instrument(
        name = "quote_request",
        level = "error",
        skip_all,
        fields(token_in = %params.token_in, token_out = %params.token_out, amount_in = %params.amount_in, %market_maker)
    )]
    pub async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
        market_maker: String,
    ) -> Result<SignedQuote, RFQError> {
        let hashflow_chain = HashflowChain::from(self.chain);
        // A fresh random address becomes the quote's effectiveTrader — the address Hashflow
        // scopes its strictly increasing quote nonces to — so quotes never invalidate each
        // other, at the cost of a cold nonce storage slot on Hashflow's router (~17k gas per
        // swap). The receiver executes the trade on-chain, so it is Hashflow's trader.
        let effective_trader = Bytes::from(Address::random().to_vec());
        let quote_request = HashflowQuoteRequest {
            source: self.source.clone(),
            base_chain: hashflow_chain.clone(),
            quote_chain: hashflow_chain,
            rfqs: vec![HashflowRFQ {
                base_token: params.token_in.to_string(),
                quote_token: params.token_out.to_string(),
                base_token_amount: Some(params.amount_in.to_string()),
                quote_token_amount: None,
                trader: params.receiver.to_string(),
                effective_trader: Some(effective_trader.to_string()),
                market_makers: vec![market_maker],
                options: HashflowRFQOptions { do_not_retry_with_other_makers: true },
            }],
            calldata: false,
        };

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

            let request = self
                .http
                .post(&url)
                .json(&quote_request)
                .header("accept", "application/json")
                .header("Authorization", &self.auth_key);

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote request failed");
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
                        warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "error response parsing failed");
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
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %err_msg, "returned non-200 status");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                } else {
                    return Err(last_error.unwrap());
                }
            }

            let quote_response = match response
                .json::<HashflowResponse<HashflowQuoteResponse>>()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote response parsing failed");
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

            match quote_response.into_result() {
                Ok(quote_response) => {
                    if let Some(quotes) = quote_response.quotes {
                        if quotes.is_empty() {
                            return Err(RFQError::QuoteNotFound(format!(
                                "Hashflow quote not found for {} {} ->{}",
                                params.amount_in, params.token_in, params.token_out,
                            )));
                        }
                        // We assume there will be only one quote request at a time
                        let quote = quotes[0].clone();
                        quote.validate(params, &effective_trader)?;

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
                Err(error) if error.code == HashflowError::NO_MAKER_SUPPORTS_REQUEST => {
                    return Err(RFQError::QuoteNotFound(format!("Hashflow quote: {error}")));
                }
                Err(error) => {
                    return Err(RFQError::FatalError(format!("Hashflow quote: {error}")));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Hashflow quote request failed after retries".to_string())
        }))
    }
}

/// Sends a book request and parses a successful body as `T`. A failing status is a connection
/// error carrying the API's error when the body is one, and the body as sent when it is not:
/// Cloudflare, in front of the API, answers some requests with an HTML page.
async fn fetch<T: DeserializeOwned>(request: RequestBuilder, what: &str) -> Result<T, FeedError> {
    let (status, body) = fetch_with_status(request, what).await?;
    if !status.is_success() {
        let api_error = serde_json::from_slice::<HashflowResponse<IgnoredAny>>(&body)
            .ok()
            .and_then(|response| response.into_result().err());
        return Err(match api_error {
            Some(error) => http_error(what, status, error),
            None => http_error(what, status, String::from_utf8_lossy(&body)),
        });
    }
    serde_json::from_slice(&body)
        .map_err(|e| FeedError::Parsing(format!("Failed to parse {what} response: {e}")))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;

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

    fn create_test_client(quote_endpoint: String, quote_timeout: Duration) -> HashflowClient {
        HashflowClient::new(
            Chain::Ethereum,
            quote_endpoint,
            "https://api.hashflow.com/taker/v3/price-levels".to_string(),
            "https://api.hashflow.com/taker/v3/market-makers".to_string(),
            "test_user".to_string(),
            "test_key".to_string(),
            quote_timeout,
        )
    }

    #[test]
    fn debug_output_omits_credentials() {
        let client =
            create_test_client("https://hashflow.example".to_string(), Duration::from_secs(1));
        let rendered = format!("{client:?}");

        assert!(!rendered.contains("test_user"));
        assert!(!rendered.contains("test_key"));
        assert!(rendered.contains("hashflow.example"));
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
        let client = create_test_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );
        let params = create_test_quote_params();

        let err = client
            .request_binding_quote(&params, "mm1".to_string())
            .await
            .unwrap_err();

        assert!(format!("{err:?}").contains("Effective trader mismatch"));
    }

    #[tokio::test]
    async fn test_request_binding_quote_field_mapping() {
        // The wire request carries the receiver as Hashflow's trader, a fresh random address as
        // the effectiveTrader — a new one per quote request — and the one maker it goes to.
        let (addr, request_log) = create_delayed_response_server(0, QUOTE_RESPONSE).await;
        let client = create_test_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );
        let params = create_test_quote_params();

        let first_quote = client
            .request_binding_quote(&params, "mm1".to_string())
            .await
            .unwrap();
        client
            .request_binding_quote(&params, "mm1".to_string())
            .await
            .unwrap();

        let requests = request_log.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for body in requests.iter() {
            assert!(
                body.contains(&format!("\"trader\":\"{}\"", params.receiver)),
                "trader is not the receiver: {body}"
            );
            assert!(
                body.contains(
                    r#""marketMakers":["mm1"],"options":{"doNotRetryWithOtherMakers":true}"#
                ),
                "request is not pinned to the state's maker: {body}"
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
    async fn test_hashflow_quote_timeout() {
        let (addr, _) = create_delayed_response_server(500, QUOTE_RESPONSE).await;

        // Test 1: Client with short timeout (200ms) - should timeout
        let client_short_timeout = create_test_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_millis(200),
        );
        let params = create_test_quote_params();

        // This should timeout after 200ms
        let start = std::time::Instant::now();
        let result = client_short_timeout
            .request_binding_quote(&params, "mm1".to_string())
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
        let client_long_timeout = create_test_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );

        // This should wait for the response (500ms)
        let result = client_long_timeout
            .request_binding_quote(&params, "mm1".to_string())
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

    /// A maker declining is a `fail` inside an HTTP 200 (bodies recorded live), answered at
    /// once: no maker quoting is a missing quote, anything else fatal.
    #[rstest]
    #[case::no_maker_supports_the_request(
        r#"{"status":"fail","rfqId":"0x225000000000000000000000000000ffffffffffffff00317477065b24ec0000","error":{"code":82,"message":"No maker supports this request"}}"#,
        RFQError::QuoteNotFound(
            "Hashflow quote: No maker supports this request (code 82)".to_string()
        )
    )]
    #[case::exceeds_supported_amounts(
        r#"{"status":"fail","rfqId":"0x225000000000000000000000000000ffffffffffffff00317498228a10fe0000","error":{"code":76,"message":"Exceeds supported amounts"}}"#,
        RFQError::FatalError("Hashflow quote: Exceeds supported amounts (code 76)".to_string())
    )]
    #[tokio::test]
    async fn a_declined_quote_is_reported_without_retrying(
        #[case] body: &'static str,
        #[case] expected: RFQError,
    ) {
        let (addr, request_log) = create_delayed_response_server(0, body).await;
        let client = create_test_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(1),
        );

        let error = client
            .request_binding_quote(&create_test_quote_params(), "mm1".to_string())
            .await
            .unwrap_err();

        assert_eq!(format!("{error:?}"), format!("{expected:?}"));
        assert_eq!(request_log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_hashflow_quote_retry_on_bad_response() {
        let (addr, request_count) = create_retry_server().await;

        let client = create_test_client(
            format!("http://127.0.0.1:{}/rfq", addr.port()),
            Duration::from_secs(5),
        );
        let params = create_test_quote_params();
        let result = client
            .request_binding_quote(&params, "mm1".to_string())
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
}
