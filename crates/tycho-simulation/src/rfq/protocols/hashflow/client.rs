use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{Address, U256};
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::{timeout, Duration};
use tracing::{instrument, warn};
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
            HashflowChain, HashflowQuoteRequest, HashflowQuoteResponse, HashflowRFQ,
        },
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
        source: String,
        auth_key: String,
        quote_timeout: Duration,
    ) -> Self {
        HashflowClient {
            chain,
            quote_endpoint,
            source,
            auth_key,
            quote_timeout,
            http: Client::new(),
        }
    }

    /// The Hashflow source (account) identifier, shared with the price polling requests.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The authorization key, shared with the price polling requests.
    pub fn auth_key(&self) -> &str {
        &self.auth_key
    }

    /// The shared HTTP connection pool, also used for price polling.
    pub fn http(&self) -> &Client {
        &self.http
    }

    #[instrument(
        name = "quote_request",
        skip_all,
        fields(token_in = %params.token_in, token_out = %params.token_out, amount_in = %params.amount_in)
    )]
    pub async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let hashflow_chain = HashflowChain::from(self.chain);
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
                effective_trader: None,
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
                .json::<HashflowQuoteResponse>()
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
                        quote.validate(params)?;

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
    use std::str::FromStr;

    use super::*;

    /// Helper function to create a mock server that responds after a delay
    async fn create_delayed_response_server(delay_ms: u64) -> std::net::SocketAddr {
        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let json_response = r#"{"status":"success","error":null,"rfqId":"test-rfq-id","internalRfqIds":null,"quotes":[{"quoteData":{"pool":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","externalAccount":null,"trader":"0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35","effectiveTrader":"0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35","baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","baseTokenAmount":"1000000000000000000","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","quoteTokenAmount":"3329502","quoteExpiry":1707847360,"nonce":1707844960943648659,"txid":"0x0000000000000000000000000000000000000000000000000000000000000001"},"signature":"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef12"}]}"#;

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

    fn create_test_client(quote_endpoint: String, quote_timeout: Duration) -> HashflowClient {
        HashflowClient::new(
            Chain::Ethereum,
            quote_endpoint,
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
    async fn test_hashflow_quote_timeout() {
        let addr = create_delayed_response_server(500).await;

        // Test 1: Client with short timeout (200ms) - should timeout
        let client_short_timeout = create_test_client(
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
        let client_long_timeout = create_test_client(
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

        let json_response = r#"{"status":"success","error":null,"rfqId":"test-rfq-id","internalRfqIds":null,"quotes":[{"quoteData":{"pool":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","externalAccount":null,"trader":"0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35","effectiveTrader":"0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35","baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","baseTokenAmount":"1000000000000000000","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","quoteTokenAmount":"3329502","quoteExpiry":1707847360,"nonce":1707844960943648659,"txid":"0x0000000000000000000000000000000000000000000000000000000000000001"},"signature":"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef12"}]}"#;

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
    async fn test_hashflow_quote_retry_on_bad_response() {
        let (addr, request_count) = create_retry_server().await;

        let client = create_test_client(
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
}
