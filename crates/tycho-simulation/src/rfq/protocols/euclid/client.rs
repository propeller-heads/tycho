use std::{collections::HashMap, str::FromStr};

use ::http::{Request, Uri};
use reqwest::Client;
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, ClientRequestBuilder, Error as TungsteniteError,
};
use tracing::{debug, instrument, warn};
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote, Bytes,
};

use crate::{
    evm::protocol::utils::bytes_to_address,
    rfq::{
        errors::RFQError,
        protocols::euclid::models::{EuclidFirmResponse, EuclidSupportedChain},
    },
    snapshot_feed::http::{nothing_refused, read_json},
};

/// Euclid's public RFQ gateway: the levels stream lives at `/levels`, binding quotes at `/firm`.
pub(crate) const EUCLID_API_URL: &str = "https://rfq.liquideuclid.com/tycho";

/// The gateway's HTTP base and the WebSocket base derived from it, without a trailing slash.
fn endpoints(api_url: &str) -> (String, String) {
    let base = api_url.trim_end_matches('/');
    let ws_base = base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    (base.to_string(), ws_base)
}

/// Opens Euclid's levels stream: the public WebSocket that pushes every pair the gateway prices
/// on one chain, as a complete frame each time.
#[derive(Clone, Debug)]
pub struct EuclidLevelsClient {
    chain: EuclidSupportedChain,
    levels_ws_endpoint: String,
}

impl EuclidLevelsClient {
    /// A client for the levels stream of `chain` at the gateway under `api_url` (an `https://`
    /// or `http://` base that `/levels` is appended to).
    pub fn new(chain: EuclidSupportedChain, api_url: &str) -> Self {
        let (_, ws_base) = endpoints(api_url);
        EuclidLevelsClient {
            chain,
            levels_ws_endpoint: format!("{ws_base}/levels?chainId={}", chain.id()),
        }
    }

    /// The chain the stream prices.
    pub fn chain(&self) -> EuclidSupportedChain {
        self.chain
    }

    /// The handshake for the levels stream. The stream is public, so it carries no credentials;
    /// the feed builds a fresh handshake for each connection, since the `Sec-WebSocket-Key` the
    /// builder generates is per connection.
    ///
    /// Only a malformed endpoint fails, which is the caller's to classify.
    pub fn levels_handshake(&self) -> Result<Request<()>, TungsteniteError> {
        ClientRequestBuilder::new(Uri::from_str(&self.levels_ws_endpoint)?).into_client_request()
    }
}

/// Requests binding quotes from Euclid's RFQ gateway. One client, with one pooled HTTP
/// connection set, is shared by every state an assembler emits.
#[derive(derive_more::Debug)]
pub struct EuclidClient {
    chain: EuclidSupportedChain,
    firm_endpoint: String,
    /// Sent as `x-api-key` with binding quote requests; the gateway runs keyless by default.
    #[debug(skip)]
    api_key: Option<String>,
    quote_timeout: Duration,
    http: Client,
}

impl EuclidClient {
    /// A client for the gateway at `api_url` (an `https://` or `http://` base that `/firm` is
    /// appended to).
    pub fn new(
        chain: EuclidSupportedChain,
        api_url: &str,
        api_key: Option<String>,
        quote_timeout: Duration,
    ) -> Self {
        let (base, _) = endpoints(api_url);
        EuclidClient {
            chain,
            firm_endpoint: format!("{base}/firm"),
            api_key,
            quote_timeout,
            http: Client::new(),
        }
    }

    /// Requests a binding quote for `params`, retrying a failed connection, a failing status or
    /// an unreadable answer within the quote deadline. A quote the gateway declines fails at
    /// once.
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
        let address = |value: &Bytes| {
            bytes_to_address(value)
                .map(|address| address.to_string())
                .map_err(|e| RFQError::InvalidInput(e.to_string()))
        };
        let body = serde_json::json!({
            "chain_id": self.chain.id(),
            "token_in": address(&params.token_in)?,
            "token_out": address(&params.token_out)?,
            "amount_in": params.amount_in.to_string(),
            "sender": address(&params.sender)?,
            "receiver": address(&params.receiver)?,
        });

        let start_time = std::time::Instant::now();
        const MAX_RETRIES: u32 = 3;
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            let elapsed = start_time.elapsed();
            if elapsed >= self.quote_timeout {
                return Err(last_error.unwrap_or_else(|| self.timed_out()));
            }
            let remaining_time = self.quote_timeout - elapsed;

            let mut request = self
                .http
                .post(&self.firm_endpoint)
                .json(&body)
                .header("accept", "application/json");
            if let Some(key) = &self.api_key {
                request = request.header("x-api-key", key);
            }

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote request failed");
                    last_error = Some(RFQError::ConnectionError(format!(
                        "Failed to send Euclid quote request: {e}"
                    )));
                    continue;
                }
                Err(_) => return Err(self.timed_out()),
            };

            match read_json::<EuclidFirmResponse>(response, "Euclid quote", nothing_refused)
                .await
                .map_err(RFQError::from)
            {
                Ok(quote_response) => return Self::process_quote_response(quote_response, params),
                Err(error) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, %error, "quote response failed");
                    last_error = Some(error);
                    if attempt < MAX_RETRIES - 1 {
                        sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Euclid quote request failed after retries".to_string())
        }))
    }

    fn timed_out(&self) -> RFQError {
        RFQError::ConnectionError(format!(
            "Euclid quote request timed out after {:?}",
            self.quote_timeout
        ))
    }

    fn process_quote_response(
        quote_response: EuclidFirmResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        match quote_response {
            EuclidFirmResponse::Quote(quote) => {
                quote.validate(params)?;
                // The order hash is how Euclid's gateway records the fill, so it is what to quote
                // when reporting one.
                debug!(order_hash = %quote.order_hash, amount_out = %quote.amount_out, "received quote");

                let quote_attributes: HashMap<String, Bytes> = HashMap::from([
                    // The router the calldata targets: the executor allows only this one.
                    ("tx_to".to_string(), quote.tx_to),
                    ("calldata".to_string(), quote.calldata),
                    (
                        "partial_fill_offset".to_string(),
                        Bytes::from(
                            quote
                                .partial_fill_offset
                                .to_be_bytes()
                                .to_vec(),
                        ),
                    ),
                ]);
                Ok(SignedQuote {
                    base_token: params.token_in.clone(),
                    quote_token: params.token_out.clone(),
                    amount_in: quote.amount_in,
                    amount_out: quote.amount_out,
                    quote_attributes,
                })
            }
            EuclidFirmResponse::Declined(declined) => Err(RFQError::QuoteNotFound(format!(
                "Euclid declined the quote: {}",
                declined.error
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

    use num_bigint::BigUint;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::snapshot_feed::http::test_support::{spawn_http_server, MockResponse};

    /// A binding quote recorded from Euclid's gateway on Ethereum.
    const FIRM_QUOTE: &str = include_str!("test_responses/firm_quote.json");

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const ROUTER: &str = "0x1644d2477f809cc2c71bccfd6dc9497e3f83210d";

    /// The recorded quote, with its expiry moved into the future: the original lasted thirty
    /// seconds.
    fn live_firm_quote() -> String {
        let mut quote: serde_json::Value = serde_json::from_str(FIRM_QUOTE).unwrap();
        quote["expiry"] = serde_json::json!(u64::MAX);
        quote.to_string()
    }

    fn recorded_params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from_str("3000000000000000").unwrap(),
            token_in: Bytes::from_str(WETH).unwrap(),
            token_out: Bytes::from_str(USDC).unwrap(),
            sender: Bytes::from_str(ROUTER).unwrap(),
            receiver: Bytes::from_str(ROUTER).unwrap(),
            origin: None,
        }
    }

    fn client_for(api_url: &str, api_key: Option<&str>, quote_timeout: Duration) -> EuclidClient {
        EuclidClient::new(
            EuclidSupportedChain::Ethereum,
            api_url,
            api_key.map(str::to_string),
            quote_timeout,
        )
    }

    #[test]
    fn debug_output_omits_credentials() {
        let rendered =
            format!("{:?}", client_for(EUCLID_API_URL, Some("secret_key"), Duration::from_secs(5)));
        assert!(!rendered.contains("secret_key"));
        assert!(rendered.contains("firm_endpoint"));
    }

    #[test]
    fn the_endpoints_derive_from_the_api_url_and_chain() {
        let client = client_for("https://rfq.example.com/tycho/", None, Duration::from_secs(5));
        assert_eq!(client.firm_endpoint, "https://rfq.example.com/tycho/firm");

        let levels = EuclidLevelsClient::new(
            EuclidSupportedChain::Ethereum,
            "https://rfq.example.com/tycho/",
        );
        assert_eq!(levels.levels_ws_endpoint, "wss://rfq.example.com/tycho/levels?chainId=1");
        assert_eq!(
            levels
                .levels_handshake()
                .unwrap()
                .uri()
                .to_string(),
            "wss://rfq.example.com/tycho/levels?chainId=1"
        );
        assert_eq!(levels.chain(), EuclidSupportedChain::Ethereum);

        let local =
            EuclidLevelsClient::new(EuclidSupportedChain::Ethereum, "http://127.0.0.1:8080/tycho");
        assert_eq!(local.levels_ws_endpoint, "ws://127.0.0.1:8080/tycho/levels?chainId=1");
    }

    #[test]
    fn a_quote_becomes_the_attributes_the_encoder_reads() {
        let response: EuclidFirmResponse = serde_json::from_str(&live_firm_quote()).unwrap();
        let quote = EuclidClient::process_quote_response(response, &recorded_params()).unwrap();

        assert_eq!(quote.amount_in, BigUint::from_str("3000000000000000").unwrap());
        assert_eq!(quote.amount_out, BigUint::from_str("7030498").unwrap());
        assert_eq!(
            quote.quote_attributes["tx_to"],
            Bytes::from_str("0x1111111254eeb25477b68fb85ed929f73a960582").unwrap()
        );
        assert_eq!(quote.quote_attributes["calldata"][..4], [0x5a, 0x09, 0x98, 0x43]);
        assert_eq!(quote.quote_attributes["partial_fill_offset"].as_ref(), 8u64.to_be_bytes());
    }

    #[test]
    fn a_decline_is_no_quote() {
        let response: EuclidFirmResponse =
            serde_json::from_str(r#"{"error":"insufficient_liquidity"}"#).unwrap();
        let error = EuclidClient::process_quote_response(response, &recorded_params()).unwrap_err();
        assert!(
            matches!(error, RFQError::QuoteNotFound(ref message) if message.contains("insufficient_liquidity")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn the_request_carries_the_trade_and_returns_the_quote() {
        let seen = Arc::new(Mutex::new(None));
        let seen_by_server = Arc::clone(&seen);
        let body = live_firm_quote();
        let server = spawn_http_server(move |request| {
            *seen_by_server.lock().unwrap() = Some((request.target.clone(), request.body.clone()));
            Some(("200 OK", body.clone()))
        })
        .await;

        let client = client_for(&server.url(), None, Duration::from_secs(5));
        let quote = client
            .request_binding_quote(&recorded_params())
            .await
            .unwrap();
        assert_eq!(quote.amount_out, BigUint::from_str("7030498").unwrap());

        let (target, body) = seen.lock().unwrap().take().unwrap();
        assert_eq!(target, "/firm");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["chain_id"], 1);
        assert_eq!(body["amount_in"], "3000000000000000");
        assert_eq!(
            body["token_in"]
                .as_str()
                .unwrap()
                .to_lowercase(),
            WETH
        );
        assert_eq!(
            body["receiver"]
                .as_str()
                .unwrap()
                .to_lowercase(),
            ROUTER
        );
    }

    /// The raw request a one-shot server received, for the headers the shared mock does not keep.
    async fn capture_one_request(response_body: String) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_by_server = Arc::clone(&captured);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 16 * 1024];
            let read = stream
                .read(&mut buffer)
                .await
                .unwrap_or(0);
            *captured_by_server.lock().unwrap() =
                String::from_utf8_lossy(&buffer[..read]).to_lowercase();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream
                .write_all(response.as_bytes())
                .await;
        });
        (format!("http://{address}"), captured)
    }

    #[tokio::test]
    async fn a_configured_key_is_sent_and_no_key_sends_none() {
        let (url, captured) = capture_one_request(live_firm_quote()).await;
        client_for(&url, Some("solver-key"), Duration::from_secs(5))
            .request_binding_quote(&recorded_params())
            .await
            .unwrap();
        assert!(captured
            .lock()
            .unwrap()
            .contains("x-api-key: solver-key"));

        let (url, captured) = capture_one_request(live_firm_quote()).await;
        client_for(&url, None, Duration::from_secs(5))
            .request_binding_quote(&recorded_params())
            .await
            .unwrap();
        assert!(!captured
            .lock()
            .unwrap()
            .contains("x-api-key"));
    }

    #[tokio::test]
    async fn a_failing_server_is_retried_until_it_quotes() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = live_firm_quote();
        let server = spawn_http_server(move |_| {
            Some(if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                ("502 Bad Gateway", "<html>bad gateway</html>".to_string())
            } else {
                ("200 OK", body.clone())
            })
        })
        .await;

        let quote = client_for(&server.url(), None, Duration::from_secs(5))
            .request_binding_quote(&recorded_params())
            .await
            .unwrap();
        assert_eq!(quote.amount_out, BigUint::from_str("7030498").unwrap());
        assert_eq!(server.request_count(), 3);
    }

    /// A failing status is about the attempt — a rate limit, a proxy between the solver and the
    /// gateway — so it is retried, and the last answer is what the caller sees.
    #[tokio::test]
    async fn a_failing_status_is_retried_then_reported() {
        let server = spawn_http_server(|_| {
            Some(("429 Too Many Requests", r#"{"error":"rate limited"}"#.to_string()))
        })
        .await;

        let error = client_for(&server.url(), None, Duration::from_secs(5))
            .request_binding_quote(&recorded_params())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RFQError::ConnectionError(ref message) if message.contains("rate limited")),
            "{error:?}"
        );
        assert_eq!(server.request_count(), 3);
    }

    /// A decline is the gateway's answer at this size, so it is not asked again.
    #[tokio::test]
    async fn a_declined_quote_is_not_retried() {
        let server = spawn_http_server(|_| {
            Some(("200 OK", r#"{"error":"insufficient_liquidity"}"#.to_string()))
        })
        .await;

        let error = client_for(&server.url(), None, Duration::from_secs(5))
            .request_binding_quote(&recorded_params())
            .await
            .unwrap_err();
        assert!(matches!(error, RFQError::QuoteNotFound(_)), "{error:?}");
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn a_slow_server_times_out_within_the_deadline() {
        let body = live_firm_quote();
        let server = spawn_http_server(move |_| {
            Some(MockResponse {
                status: "200 OK",
                body: body.clone(),
                delay: Duration::from_millis(500),
            })
        })
        .await;

        let start = std::time::Instant::now();
        let error = client_for(&server.url(), None, Duration::from_millis(200))
            .request_binding_quote(&recorded_params())
            .await
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(error, RFQError::ConnectionError(ref m) if m.contains("timed out")),
            "{error:?}"
        );
        assert!(elapsed < Duration::from_millis(450), "took {elapsed:?}");
    }
}
