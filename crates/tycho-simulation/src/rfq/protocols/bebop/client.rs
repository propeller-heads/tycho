use std::{collections::HashMap, str::FromStr};

use ::http::{Request, Uri};
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, ClientRequestBuilder, Error as TungsteniteError,
};
use tracing::{instrument, warn};
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote, Bytes,
};

use crate::{
    evm::protocol::utils::bytes_to_address,
    rfq::{
        errors::RFQError,
        protocols::bebop::models::{BebopOrderToSign, BebopQuoteResponse},
    },
};

/// Requests binding Bebop quotes. One instance is shared (via `Arc`) by every state a
/// [`BebopFeed`](super::feed::BebopFeed) emits, so all of them reuse the same HTTP
/// connection pool.
///
/// Serialization keeps the configuration but skips the credential and the HTTP client: a
/// deserialized client gets a fresh connection pool and an empty key, so binding quotes fail at
/// call time until re-configured. `Debug` output omits the credential as well.
#[derive(derive_more::Debug, Serialize, Deserialize)]
pub struct BebopClient {
    quote_endpoint: String,
    pricing_ws_endpoint: String,
    // key header for authentication
    #[serde(skip_serializing, default)]
    #[debug(skip)]
    key: String,
    quote_timeout: Duration,
    /// The real end-user's EOA when the taker is not the end-user's own wallet.
    origin_address: Option<Bytes>,
    /// The `to` address of the resulting transaction when a contract executes the swap.
    origin_target: Option<Bytes>,
    /// Stable identifier for the upstream flow source when aggregating multiple sources.
    origin_source: Option<String>,
    #[serde(skip)]
    http: Client,
}

impl BebopClient {
    pub fn new(
        quote_endpoint: String,
        pricing_ws_endpoint: String,
        key: String,
        quote_timeout: Duration,
        origin_address: Option<Bytes>,
        origin_target: Option<Bytes>,
        origin_source: Option<String>,
    ) -> Self {
        BebopClient {
            quote_endpoint,
            pricing_ws_endpoint,
            key,
            quote_timeout,
            origin_address,
            origin_target,
            origin_source,
            http: Client::new(),
        }
    }

    /// The authenticated handshake for the pricing WebSocket. The feed opens one connection at
    /// a time and builds a fresh handshake for each, since the `Sec-WebSocket-Key` the builder
    /// generates is per connection; the rest of the handshake comes from the endpoint.
    ///
    /// Only a malformed endpoint or header value fails, which is the caller's to classify.
    pub fn pricing_handshake(&self) -> Result<Request<()>, TungsteniteError> {
        ClientRequestBuilder::new(Uri::from_str(&self.pricing_ws_endpoint)?)
            .with_header("Authorization", format!("Bearer {}", self.key))
            .into_client_request()
    }

    #[instrument(
        name = "quote_request",
        level = "error",
        skip_all,
        fields(token_in = %params.token_in, token_out = %params.token_out, amount_in = %params.amount_in)
    )]
    pub async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let sell_token = bytes_to_address(&params.token_in)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?
            .to_string();
        let buy_token = bytes_to_address(&params.token_out)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?
            .to_string();
        let sell_amount = params.amount_in.to_string();
        let sender = bytes_to_address(&params.sender)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?
            .to_string();
        let receiver = bytes_to_address(&params.receiver)
            .map_err(|e| RFQError::InvalidInput(e.to_string()))?
            .to_string();

        let url = self.quote_endpoint.clone();

        let mut query = vec![
            ("sell_tokens", sell_token),
            ("buy_tokens", buy_token),
            ("sell_amounts", sell_amount),
            ("taker_address", sender),
            ("receiver_address", receiver),
            ("approval_type", "Standard".into()),
            ("skip_validation", "true".into()),
            ("skip_taker_checks", "true".into()),
            ("gasless", "false".into()),
            ("expiry_type", "standard".into()),
            ("fee", "0".into()),
            ("is_ui", "false".into()),
        ];

        if let Some(origin_address) = &self.origin_address {
            query.push((
                "origin_address",
                bytes_to_address(origin_address)
                    .map_err(|e| RFQError::InvalidInput(e.to_string()))?
                    .to_string(),
            ));
        }
        if let Some(origin_target) = &self.origin_target {
            query.push((
                "origin_target",
                bytes_to_address(origin_target)
                    .map_err(|e| RFQError::InvalidInput(e.to_string()))?
                    .to_string(),
            ));
        }
        if let Some(origin_source) = &self.origin_source {
            query.push(("origin_source", origin_source.clone()));
        }

        let start_time = std::time::Instant::now();
        const MAX_RETRIES: u32 = 3;
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            // Check if we have time remaining for this attempt
            let elapsed = start_time.elapsed();
            if elapsed >= self.quote_timeout {
                return Err(last_error.unwrap_or_else(|| {
                    RFQError::ConnectionError(format!(
                        "Bebop quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    ))
                }));
            }

            let remaining_time = self.quote_timeout - elapsed;

            let request = self
                .http
                .get(&url)
                .query(&query)
                .header("accept", "application/json")
                .bearer_auth(&self.key);

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote request failed");
                    last_error = Some(RFQError::ConnectionError(format!(
                        "Failed to send Bebop quote request: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
                Err(_) => {
                    return Err(RFQError::ConnectionError(format!(
                        "Bebop quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    )));
                }
            };

            let quote_response = match response
                .json::<BebopQuoteResponse>()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote response parsing failed");
                    last_error = Some(RFQError::ParsingError(format!(
                        "Failed to parse Bebop quote response: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
            };

            return Self::process_quote_response(quote_response, params);
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Bebop quote request failed after retries".to_string())
        }))
    }

    fn process_quote_response(
        quote_response: BebopQuoteResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        match quote_response {
            BebopQuoteResponse::Success(quote) => {
                quote.validate(params)?;

                let mut quote_attributes: HashMap<String, Bytes> = HashMap::new();
                // The contract the calldata targets: either the Bebop settlement
                // or the Bebop router.
                quote_attributes.insert("tx_to".into(), quote.tx.to);
                quote_attributes.insert("calldata".into(), quote.tx.data);
                quote_attributes.insert(
                    "partial_fill_offset".into(),
                    Bytes::from(
                        quote
                            .partial_fill_offset
                            .to_be_bytes()
                            .to_vec(),
                    ),
                );
                let signed_quote = match quote.to_sign {
                    BebopOrderToSign::Single(ref single) => SignedQuote {
                        base_token: params.token_in.clone(),
                        quote_token: params.token_out.clone(),
                        amount_in: BigUint::from_str(&single.taker_amount).map_err(|_| {
                            RFQError::ParsingError(format!(
                                "Failed to parse amount in string: {}",
                                single.taker_amount
                            ))
                        })?,
                        amount_out: BigUint::from_str(&single.maker_amount).map_err(|_| {
                            RFQError::ParsingError(format!(
                                "Failed to parse amount out string: {}",
                                single.maker_amount
                            ))
                        })?,
                        quote_attributes,
                    },
                    BebopOrderToSign::Aggregate(aggregate) => {
                        // Sum taker_amounts for taker_tokens matching the token_in
                        let amount_in: BigUint = aggregate
                            .taker_tokens
                            .iter()
                            .zip(&aggregate.taker_amounts)
                            .flat_map(|(tokens, amounts)| {
                                tokens
                                    .iter()
                                    .zip(amounts)
                                    .filter_map(|(token, amount)| {
                                        if token == &params.token_in {
                                            BigUint::from_str(amount).ok()
                                        } else {
                                            None
                                        }
                                    })
                            })
                            .sum();

                        // Sum maker_amounts for maker_tokens matching the token_out
                        let amount_out: BigUint = aggregate
                            .maker_tokens
                            .iter()
                            .zip(&aggregate.maker_amounts)
                            .flat_map(|(tokens, amounts)| {
                                tokens
                                    .iter()
                                    .zip(amounts)
                                    .filter_map(|(token, amount)| {
                                        if token == &params.token_out {
                                            BigUint::from_str(amount).ok()
                                        } else {
                                            None
                                        }
                                    })
                            })
                            .sum();

                        SignedQuote {
                            base_token: params.token_in.clone(),
                            quote_token: params.token_out.clone(),
                            amount_in,
                            amount_out,
                            quote_attributes,
                        }
                    }
                };

                Ok(signed_quote)
            }
            BebopQuoteResponse::Error(err) => Err(RFQError::FatalError(format!(
                "Bebop API error: code {} - {} (requestId: {})",
                err.error.error_code, err.error.message, err.error.request_id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::{Arc, Mutex},
    };

    use tokio::net::TcpListener;
    use tycho_common::models::protocol::GetAmountOutParams;

    use super::*;
    use crate::rfq::errors::RFQError;

    /// Quote responses recorded from Bebop's API.
    const AGGREGATE_ORDER: &str = include_str!("test_responses/aggregate_order.json");
    const AGGREGATE_ORDER_ROUTER_MODE: &str =
        include_str!("test_responses/aggregate_order_router_mode.json");
    const AGGREGATE_ORDER_WITH_MULTIHOP: &str =
        include_str!("test_responses/aggregate_order_with_multihop.json");
    const SINGLE_ORDER: &str = include_str!("test_responses/single_order.json");
    const SINGLE_ORDER_ROUTER_MODE: &str =
        include_str!("test_responses/single_order_router_mode.json");

    /// BebopSettlement.swapSingle
    const SWAP_SINGLE_SELECTOR: [u8; 4] = [0x4d, 0xce, 0xbc, 0xba];
    /// BebopRouter.swap
    const ROUTER_SWAP_SELECTOR: [u8; 4] = [0x95, 0x86, 0xd0, 0xe8];

    fn test_client() -> BebopClient {
        BebopClient::new(
            "https://api.bebop.xyz/pmm/ethereum/v3/quote".to_string(),
            "wss://api.bebop.xyz/pmm/ethereum/v3/pricing?format=protobuf".to_string(),
            "secret_key".to_string(),
            Duration::from_secs(30),
            None,
            None,
            None,
        )
    }

    #[test]
    fn serialization_skips_credentials() {
        let original = test_client();

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: BebopClient = serde_json::from_str(&serialized).unwrap();

        assert!(!serialized.contains("secret_key"));
        assert!(!serialized.contains("secret_key"));
        assert_eq!(deserialized.quote_endpoint, original.quote_endpoint);
        assert_eq!(deserialized.quote_timeout, original.quote_timeout);
    }

    #[test]
    fn debug_output_omits_credentials() {
        let rendered = format!("{:?}", test_client());

        assert!(!rendered.contains("secret_key"));
        assert!(rendered.contains("quote_endpoint"));
    }

    #[test]
    fn test_process_bebop_quote_response_aggregate_order() {
        let quote_response: BebopQuoteResponse = serde_json::from_str(AGGREGATE_ORDER).unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("20000000000").unwrap(),
            token_in: Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            token_out: Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap(),
            sender: Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap(),
            receiver: Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap(),
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_out, BigUint::from_str("52571055094221715780641").unwrap());
        assert_eq!(res.amount_in, BigUint::from_str("20000000000").unwrap());
        assert_eq!(res.base_token, params.token_in);
        assert_eq!(res.quote_token, params.token_out);
    }

    #[test]
    fn test_process_bebop_quote_response_aggregate_order_with_multihop() {
        let quote_response: BebopQuoteResponse =
            serde_json::from_str(AGGREGATE_ORDER_WITH_MULTIHOP).unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("43067495979235520920162").unwrap(),
            token_in: Bytes::from_str("0xDEf1CA1fb7FBcDC777520aa7f396b4E015F497aB").unwrap(),
            token_out: Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(),
            sender: Bytes::from_str("0x809305d724B6E79C71e10a097ABadd1274B9C279").unwrap(),
            receiver: Bytes::from_str("0x809305d724B6E79C71e10a097ABadd1274B9C279").unwrap(),
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_out, BigUint::from_str("11186653890").unwrap());
        assert_eq!(res.amount_in, BigUint::from_str("43067495979235520920162").unwrap());
        assert_eq!(res.base_token, params.token_in);
        assert_eq!(res.quote_token, params.token_out);
    }

    #[test]
    fn test_process_bebop_quote_response_single_order() {
        // Captured from a settlement-mode API account: the signed order's taker and receiver
        // are the requested sender/receiver and the calldata targets the settlement contract.
        let quote_response: BebopQuoteResponse = serde_json::from_str(SINGLE_ORDER).unwrap();
        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("1000000000000000000").unwrap(),
            token_in: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            token_out: Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            sender: router.clone(),
            receiver: router,
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_in, BigUint::from_str("1000000000000000000").unwrap());
        assert_eq!(res.amount_out, BigUint::from_str("2915408").unwrap());
        let settlement = Bytes::from_str("0xbbbbbBB520d69a9775E85b458C58c648259FAD5F").unwrap();
        assert_eq!(
            res.quote_attributes
                .get("tx_to")
                .unwrap(),
            &settlement
        );
        assert_eq!(
            res.quote_attributes
                .get("calldata")
                .unwrap()[..4],
            SWAP_SINGLE_SELECTOR
        );
    }

    #[test]
    fn test_process_bebop_quote_response_single_order_router_mode() {
        // Captured from an API account configured for router-mode settlement: the signed
        // order's taker and receiver are the Bebop router contract (= tx.to), not the
        // requested sender/receiver.
        let quote_response: BebopQuoteResponse =
            serde_json::from_str(SINGLE_ORDER_ROUTER_MODE).unwrap();
        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("1000000000000000000").unwrap(),
            token_in: Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            token_out: Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            sender: router.clone(),
            receiver: router,
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_in, BigUint::from_str("1000000000000000000").unwrap());
        assert_eq!(res.amount_out, BigUint::from_str("2926296").unwrap());
        let bebop_router = Bytes::from_str("0xBeb0009ACa35087ce7cCF11637E24dd1Aad3bf2A").unwrap();
        assert_eq!(
            res.quote_attributes
                .get("tx_to")
                .unwrap(),
            &bebop_router
        );
        assert_eq!(
            res.quote_attributes
                .get("calldata")
                .unwrap()[..4],
            ROUTER_SWAP_SELECTOR
        );
    }

    /// Helper function to create a mock server that responds after a delay
    async fn create_delayed_response_server(delay_ms: u64) -> std::net::SocketAddr {
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    sleep(Duration::from_millis(delay_ms)).await;

                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        AGGREGATE_ORDER.len(),
                        AGGREGATE_ORDER
                    );
                    let _ = stream
                        .write_all(response.as_bytes())
                        .await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        addr
    }

    fn create_test_client(quote_endpoint: String, quote_timeout: Duration) -> BebopClient {
        BebopClient::new(
            quote_endpoint,
            "wss://api.bebop.xyz/pmm/ethereum/v3/pricing?format=protobuf".to_string(),
            "test_key".to_string(),
            quote_timeout,
            None,
            None,
            None,
        )
    }

    /// Helper function to create test quote params matching aggregate_order.json
    fn create_test_quote_params() -> GetAmountOutParams {
        let token_in = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let token_out = Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap();
        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        GetAmountOutParams {
            amount_in: BigUint::from_str("20000000000").unwrap(),
            token_in,
            token_out,
            sender: router.clone(),
            receiver: router,
        }
    }

    #[tokio::test]
    async fn test_bebop_quote_timeout() {
        let addr = create_delayed_response_server(500).await;

        // Test 1: Client with short timeout (200ms) - should timeout
        let client_short_timeout = create_test_client(
            format!("http://127.0.0.1:{}/quote", addr.port()),
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

        // Test 2: Client with long timeout (1 seconds) - should wait and receive response
        // Note: With retry logic, we may need multiple attempts if the response is malformed,
        // so we need a longer timeout to account for retries
        let client_long_timeout = create_test_client(
            format!("http://127.0.0.1:{}/quote", addr.port()),
            Duration::from_secs(1),
        );

        let result = client_long_timeout
            .request_binding_quote(&params)
            .await;

        // Should succeed - the server waits 500ms which is within the 1s timeout
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
        let quote = result.unwrap();

        // Verify the quote matches what we expect from aggregate_order.json
        assert_eq!(quote.base_token, params.token_in);
        assert_eq!(quote.quote_token, params.token_out);
    }

    /// Helper function to create a mock server that fails twice, then succeeds with
    /// aggregate_order.json
    async fn create_retry_server() -> (std::net::SocketAddr, Arc<Mutex<u32>>) {
        use std::sync::{Arc, Mutex};

        use tokio::io::AsyncWriteExt;

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

                    if count <= 2 {
                        let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 21\r\n\r\nInternal Server Error";
                        let _ = stream
                            .write_all(response.as_bytes())
                            .await;
                    } else {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            AGGREGATE_ORDER.len(),
                            AGGREGATE_ORDER
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
        (addr, request_count)
    }

    #[tokio::test]
    async fn test_bebop_quote_retry_on_bad_response() {
        let (addr, request_count) = create_retry_server().await;

        let client = create_test_client(
            format!("http://127.0.0.1:{}/quote", addr.port()),
            Duration::from_secs(5),
        );
        let params = create_test_quote_params();
        let result = client
            .request_binding_quote(&params)
            .await;

        assert!(result.is_ok(), "Expected success after retries, got: {:?}", result);
        let quote = result.unwrap();

        // Verify the quote (amounts from aggregate_order.json)
        assert_eq!(quote.amount_in, BigUint::from_str("20000000000").unwrap());
        assert_eq!(quote.amount_out, BigUint::from_str("52571055094221715780641").unwrap());

        // Verify exactly 3 requests were made (2 failures + 1 success)
        let final_count = *request_count.lock().unwrap();
        assert_eq!(final_count, 3, "Expected 3 requests, got {}", final_count);
    }

    #[test]
    fn test_process_bebop_quote_response_aggregate_order_router_mode() {
        // Captured from a router-mode API account: an aggregate order split across three
        // makers where the signed order's taker and receiver are the Bebop router (= tx.to).
        let quote_response: BebopQuoteResponse =
            serde_json::from_str(AGGREGATE_ORDER_ROUTER_MODE).unwrap();
        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("20000000000").unwrap(),
            token_in: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            token_out: Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap(),
            sender: router.clone(),
            receiver: router,
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_in, BigUint::from_str("20000000000").unwrap());
        assert_eq!(res.amount_out, BigUint::from_str("52577858553072299423490").unwrap());
        let bebop_router = Bytes::from_str("0xBeb0009ACa35087ce7cCF11637E24dd1Aad3bf2A").unwrap();
        assert_eq!(
            res.quote_attributes
                .get("tx_to")
                .unwrap(),
            &bebop_router
        );
        assert_eq!(
            res.quote_attributes
                .get("calldata")
                .unwrap()[..4],
            ROUTER_SWAP_SELECTOR
        );
    }
}
