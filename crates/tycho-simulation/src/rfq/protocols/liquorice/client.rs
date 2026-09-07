use std::{collections::HashMap, str::FromStr, time::SystemTime};

use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
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
        protocols::liquorice::models::{LiquoriceQuoteRequest, LiquoriceQuoteResponse},
    },
};

/// Requests binding Liquorice quotes. One instance is shared (via `Arc`) by every state a
/// [`LiquoriceFeed`](super::feed::LiquoriceFeed) emits, so all of them reuse the same HTTP
/// connection pool.
///
/// Serialization keeps the configuration but skips the credentials and the HTTP client: a
/// deserialized client gets a fresh connection pool and empty credentials, so binding quotes
/// fail at call time until re-configured. `Debug` output omits the credentials as well.
#[derive(derive_more::Debug, Serialize, Deserialize)]
pub struct LiquoriceClient {
    chain: Chain,
    quote_endpoint: String,
    // solver header for authentication
    #[serde(skip_serializing, default)]
    #[debug(skip)]
    auth_solver: String,
    // key header for authentication
    #[serde(skip_serializing, default)]
    #[debug(skip)]
    auth_key: String,
    quote_timeout: Duration,
    quote_expiry_secs: u64,
    #[serde(skip, default)]
    http: Client,
}

impl LiquoriceClient {
    pub fn new(
        chain: Chain,
        quote_endpoint: String,
        auth_solver: String,
        auth_key: String,
        quote_timeout: Duration,
        quote_expiry_secs: u64,
    ) -> Self {
        LiquoriceClient {
            chain,
            quote_endpoint,
            auth_solver,
            auth_key,
            quote_timeout,
            quote_expiry_secs,
            http: Client::new(),
        }
    }

    /// The solver header value, shared with the price polling requests.
    pub(crate) fn auth_solver(&self) -> &str {
        &self.auth_solver
    }

    /// The authorization key, shared with the price polling requests.
    pub(crate) fn auth_key(&self) -> &str {
        &self.auth_key
    }

    /// The shared HTTP connection pool, also used for price polling.
    pub(crate) fn http(&self) -> &Client {
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

            let request = self
                .http
                .post(&url)
                .json(&quote_request)
                .header("accept", "application/json")
                .header("solver", &self.auth_solver)
                .header("authorization", &self.auth_key);

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote request failed");
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
                        warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "error response parsing failed");
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
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %err_msg, "returned non-200 status");
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
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote response parsing failed");
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

    fn process_quote_response(
        quote_response: LiquoriceQuoteResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        if !quote_response.liquidity_available {
            debug!(?quote_response, "quote response indicates no liquidity");
            return Err(RFQError::QuoteNotFound(format!(
                "Liquorice quote not found for {} {} ->{}",
                params.amount_in, params.token_in, params.token_out,
            )));
        }

        debug!(levels = quote_response.levels.len(), "received quote response");

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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn create_test_client(quote_endpoint: String, quote_timeout: Duration) -> LiquoriceClient {
        LiquoriceClient::new(
            Chain::Ethereum,
            quote_endpoint,
            "test_solver".to_string(),
            "test_key".to_string(),
            quote_timeout,
            300,
        )
    }

    #[test]
    fn debug_output_omits_credentials() {
        let client =
            create_test_client("https://liquorice.example".to_string(), Duration::from_secs(1));
        let rendered = format!("{client:?}");

        assert!(!rendered.contains("test_solver"));
        assert!(!rendered.contains("test_key"));
        assert!(rendered.contains("liquorice.example"));
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

        let client_short_timeout = create_test_client(
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

        let client_long_timeout = create_test_client(
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

        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));
        assert_eq!(quote.amount_out, BigUint::from(3329502u64));

        let final_count = *request_count.lock().unwrap();
        assert_eq!(final_count, 3, "Expected 3 requests, got {}", final_count);
    }
}
