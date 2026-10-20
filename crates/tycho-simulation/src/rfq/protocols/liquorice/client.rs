use std::{
    collections::HashMap,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use num_bigint::BigUint;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use tokio::time::{timeout, Duration};
use tracing::{debug, instrument, warn};
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote, Bytes,
};

use crate::{
    evm::protocol::u256_num::biguint_to_u256,
    rfq::{
        errors::RFQError,
        protocols::liquorice::models::{
            LiquoricePriceLevelsResponse, LiquoriceQuoteRequest, LiquoriceQuoteResponse,
            LiquoriceTokenPairPrice,
        },
    },
    snapshot_feed::{errors::FeedError, http::read_json},
};

/// Whether Liquorice refused the request because it does not price the chain. It rejects the
/// query parameter in plain text, naming the field and then the chain ids it does take.
fn refuses_the_chain(body: &str) -> bool {
    body.contains("chainId: invalid value")
}

/// Liquorice's public solver API, where a client talks unless it is given another.
const LIQUORICE_API_URL: &str = "https://api.liquorice.tech";

/// Requests binding Liquorice quotes. One instance is shared (via `Arc`) by every state a
/// [`LiquoriceFeed`](super::feed::LiquoriceFeed) emits, so all of them reuse the same HTTP
/// connection pool.
///
/// It is also how the feed reads the venue's price levels, so every request to Liquorice —
/// quotes and books alike — goes through this one type and its one connection pool.
#[derive(derive_more::Debug)]
pub struct LiquoriceClient {
    chain_id: u64,
    quote_endpoint: String,
    price_levels_endpoint: String,
    // solver header for authentication
    #[debug(skip)]
    auth_solver: String,
    // key header for authentication
    #[debug(skip)]
    auth_key: String,
    quote_timeout: Duration,
    quote_expiry_secs: u64,
    http: Client,
    /// Liquorice issues credentials under two schemes: newer accounts authenticate with
    /// `Authorization: Basic base64(solver:key)`, older ones with a separate `solver` header
    /// beside `authorization`. Every client starts on Basic and switches to the legacy pair for
    /// good once the venue answers 401.
    use_legacy_auth: AtomicBool,
}

impl LiquoriceClient {
    /// A client for Liquorice's solver API, whose quote and price-level endpoints share the
    /// version's own path. `base_url` points it somewhere else than the venue, which is how a
    /// test reaches a server of its own.
    pub fn new(
        base_url: Option<&str>,
        chain_id: u64,
        auth_solver: String,
        auth_key: String,
        quote_timeout: Duration,
        quote_expiry_secs: u64,
    ) -> Self {
        let base_url = base_url.unwrap_or(LIQUORICE_API_URL);
        let solver = format!("{}/v1/solver", base_url.trim_end_matches('/'));
        LiquoriceClient {
            chain_id,
            quote_endpoint: format!("{solver}/rfq"),
            price_levels_endpoint: format!("{solver}/price-levels"),
            auth_solver,
            auth_key,
            quote_timeout,
            quote_expiry_secs,
            http: Client::new(),
            use_legacy_auth: AtomicBool::new(false),
        }
    }

    /// Every maker's current price levels, keyed by maker name.
    pub async fn fetch_price_levels(
        &self,
    ) -> Result<HashMap<String, Vec<LiquoriceTokenPairPrice>>, FeedError> {
        let response = self
            .send_authed(|| {
                self.http
                    .get(&self.price_levels_endpoint)
                    .query(&[("chainId", self.chain_id.to_string())])
                    .header("accept", "application/json")
            })
            .await
            .map_err(|e| {
                FeedError::Connection(format!("Failed to fetch Liquorice price levels: {e}"))
            })?;
        let price_response: LiquoricePriceLevelsResponse =
            read_json(response, "Liquorice price levels", refuses_the_chain).await?;
        Ok(price_response.prices)
    }

    #[instrument(
        name = "quote_request",
        level = "error",
        skip_all,
        fields(token_in = %params.token_in, token_out = %params.token_out, amount_in = %params.amount_in)
    )]
    /// Requests a signed quote from `market_maker` alone, the maker whose ladder the state
    /// priced the swap against, so the quote commits to the trade that was simulated.
    pub async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
        market_maker: &str,
    ) -> Result<SignedQuote, RFQError> {
        let expiry = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".into()))?
            .as_secs() +
            self.quote_expiry_secs;

        let rfq_id = uuid::Uuid::new_v4().to_string();

        let quote_request = LiquoriceQuoteRequest {
            chain_id: self.chain_id,
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
                        "Liquorice quote request timed out after {:?}",
                        self.quote_timeout
                    ))
                }));
            }

            let remaining_time = self.quote_timeout - elapsed;

            let send = self.send_authed(|| {
                self.http
                    .post(&url)
                    .json(&quote_request)
                    .header("accept", "application/json")
            });

            let response = match timeout(remaining_time, send).await {
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
                        "Liquorice quote request timed out after {:?}",
                        self.quote_timeout
                    )));
                }
            };

            let status = response.status();
            if status != 200 {
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
                // The venue answering a quote request at all means the request reached it,
                // so a status it refuses with is about this attempt: a rate limit, a gateway
                // between us and it, or a key to be fixed at the venue while the feed runs on.
                last_error = Some(RFQError::ConnectionError(format!(
                    "Liquorice quote request failed with {status}: {err_msg}",
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

            return Self::process_quote_response(quote_response, params, market_maker);
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Liquorice quote request failed after retries".to_string())
        }))
    }

    /// The `Authorization: Basic <base64(solver:key)>` value the venue expects from an account
    /// on the current credential scheme.
    fn basic_auth_header(&self) -> String {
        let credentials = format!("{}:{}", self.auth_solver, self.auth_key);
        format!("Basic {}", BASE64_STANDARD.encode(credentials.as_bytes()))
    }

    /// Puts whichever credential scheme is currently in use on `request`.
    fn apply_auth(&self, request: RequestBuilder) -> RequestBuilder {
        if self
            .use_legacy_auth
            .load(Ordering::Relaxed)
        {
            request
                .header("solver", &self.auth_solver)
                .header("authorization", &self.auth_key)
        } else {
            request.header("authorization", self.basic_auth_header())
        }
    }

    /// Sends `build()` under the current credential scheme, retrying once under the legacy one
    /// when the venue answers 401. The scheme that is accepted is kept for the client's lifetime,
    /// so an account on legacy credentials pays the rejected attempt only on its first request.
    async fn send_authed(
        &self,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<Response, reqwest::Error> {
        let response = self.apply_auth(build()).send().await?;
        if response.status() == StatusCode::UNAUTHORIZED &&
            !self
                .use_legacy_auth
                .swap(true, Ordering::Relaxed)
        {
            warn!("Basic auth rejected, retrying with the legacy solver and authorization headers");
            return self.apply_auth(build()).send().await;
        }
        Ok(response)
    }

    fn process_quote_response(
        quote_response: LiquoriceQuoteResponse,
        params: &GetAmountOutParams,
        market_maker: &str,
    ) -> Result<SignedQuote, RFQError> {
        if !quote_response.liquidity_available {
            debug!(?quote_response, "quote response indicates no liquidity");
            return Err(RFQError::QuoteNotFound(format!(
                "Liquorice quote not found for {} {} ->{}",
                params.amount_in, params.token_in, params.token_out,
            )));
        }

        debug!(levels = quote_response.levels.len(), "received quote response");

        // The best valid level the maker that priced the swap offers.
        let best_level = quote_response
            .levels
            .iter()
            .filter_map(|level| {
                if level.maker != market_maker || level.validate(params).is_err() {
                    return None;
                }
                let amount = BigUint::from_str(&level.quote_token_amount).ok()?;
                Some((level, amount))
            })
            .max_by_key(|(_, amount)| amount.clone());

        let (quote_level, _) = best_level.ok_or_else(|| {
            RFQError::QuoteNotFound(format!(
                "No valid Liquorice quote levels from {market_maker} for {} {} ->{}",
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use dotenv::dotenv;
    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;
    use crate::snapshot_feed::http::test_support::{spawn_http_server, MockResponse};

    /// A binding quote as Liquorice answers one, recorded from its API.
    const BINDING_QUOTE: &str = r#"{"rfqId":"test-rfq-id","liquidityAvailable":true,"levels":[{"makerRfqId":"maker-rfq-1","maker":"test-maker","nonce":"0x0000000000000000000000000000000000000000000000000000000000000001","expiry":1707847360,"tx":{"to":"0x71D9750ECF0c5081FAE4E3EDC4253E52024b0B59","data":"0xdeadbeef"},"baseToken":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","quoteToken":"0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599","baseTokenAmount":"1000000000000000000","quoteTokenAmount":"3329502","partialFill":null,"allowances":[]}]}"#;

    fn create_test_client(base_url: String, quote_timeout: Duration) -> LiquoriceClient {
        LiquoriceClient::new(
            Some(&base_url),
            Chain::Ethereum.id(),
            "test_solver".to_string(),
            "test_key".to_string(),
            quote_timeout,
            300,
        )
    }

    /// Liquorice rejects Basic auth for an account issued legacy credentials, so the client has
    /// to answer a 401 by re-sending the same request under the legacy headers — and then stay
    /// there, rather than spending a rejected attempt on every later request.
    #[tokio::test]
    async fn a_rejected_basic_auth_falls_back_to_the_legacy_headers() {
        use std::sync::atomic::AtomicUsize;

        use crate::snapshot_feed::http::test_support::spawn_http_server;

        let attempts = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&attempts);
        let server = spawn_http_server(move |_| {
            let attempt = served.fetch_add(1, Ordering::SeqCst);
            Some(if attempt == 0 {
                ("401 Unauthorized", r#"{"message":"unauthorized"}"#.to_string())
            } else {
                ("200 OK", r#"{"prices":{}}"#.to_string())
            })
        })
        .await;
        let client = LiquoriceClient::new(
            Some(&server.url()),
            Chain::Ethereum.id(),
            "test_solver".to_string(),
            "test_key".to_string(),
            Duration::from_secs(1),
            300,
        );

        client
            .fetch_price_levels()
            .await
            .expect("the retry under the legacy headers is served");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        client
            .fetch_price_levels()
            .await
            .expect("the second poll is served too");
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "the rejected scheme is not tried again");
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
            origin: None,
        }
    }

    #[test]
    fn process_quote_response_no_liquidity() {
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
        let result = LiquoriceClient::process_quote_response(response, &params, "test-maker");
        assert!(
            matches!(result, Err(RFQError::QuoteNotFound(_))),
            "expected QuoteNotFound, got {:?}",
            result
        );
    }

    #[test]
    fn process_quote_response_partial_fill_attributes() {
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

        let quote =
            LiquoriceClient::process_quote_response(response, &params, "test-maker").unwrap();

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

    /// The quote is the level of the maker whose ladder priced the swap, which is the trade the
    /// route was built on, even where another maker's level in the same response pays more.
    #[test]
    fn a_level_from_another_maker_is_not_the_quote() {
        use crate::rfq::protocols::liquorice::models::LiquoriceQuoteResponse;

        let token_in = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
        let token_out = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";
        let amount_in = 1_000_000_000_000_000_000u64;
        let ours = make_quote_level(token_in, token_out, &amount_in.to_string(), "3000000", None);
        let mut theirs =
            make_quote_level(token_in, token_out, &amount_in.to_string(), "3500000", None);
        theirs.maker = "other-maker".to_string();
        let params = make_params(token_in, token_out, amount_in);

        let quote = LiquoriceClient::process_quote_response(
            LiquoriceQuoteResponse {
                rfq_id: "r1".to_string(),
                liquidity_available: true,
                levels: vec![ours, theirs.clone()],
            },
            &params,
            "test-maker",
        )
        .unwrap();
        assert_eq!(quote.amount_out, BigUint::from(3_000_000u64));

        let only_theirs = LiquoriceClient::process_quote_response(
            LiquoriceQuoteResponse {
                rfq_id: "r1".to_string(),
                liquidity_available: true,
                levels: vec![theirs],
            },
            &params,
            "test-maker",
        );
        assert!(matches!(only_theirs, Err(RFQError::QuoteNotFound(_))), "{only_theirs:?}");
    }

    #[test]
    fn process_quote_response_selects_best_valid_level() {
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

        let quote =
            LiquoriceClient::process_quote_response(response, &params, "test-maker").unwrap();
        assert_eq!(quote.amount_out, BigUint::from(3_500_000u64));
    }

    /// A status the venue refuses a quote with says the request reached it and this attempt
    /// failed — a rate limit, a gateway in between, a key to be fixed at the venue — so the
    /// state asks again instead of being retired.
    #[tokio::test]
    async fn a_refused_status_leaves_the_component_quotable() {
        let server =
            spawn_http_server(|_| Some(("429 Too Many Requests", "slow down".to_string()))).await;
        let client = create_test_client(format!("{}/quote", server.url()), Duration::from_secs(1));

        let error = client
            .request_binding_quote(&create_test_quote_params(), "test-maker")
            .await
            .unwrap_err();

        assert!(matches!(error, RFQError::ConnectionError(_)), "{error}");
        assert!(error.to_string().contains("429"), "{error}");
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
            origin: None,
        }
    }

    #[tokio::test]
    async fn liquorice_quote_timeout() {
        let server = spawn_http_server(|_| {
            Some(MockResponse {
                status: "200 OK",
                body: BINDING_QUOTE.to_string(),
                delay: Duration::from_millis(500),
            })
        })
        .await;

        let client_short_timeout =
            create_test_client(format!("{}/rfq", server.url()), Duration::from_millis(200));
        let params = create_test_quote_params();

        let start = std::time::Instant::now();
        let result = client_short_timeout
            .request_binding_quote(&params, "test-maker")
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

        let client_long_timeout =
            create_test_client(format!("{}/rfq", server.url()), Duration::from_secs(1));

        let result = client_long_timeout
            .request_binding_quote(&params, "test-maker")
            .await;
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
    }

    #[tokio::test]
    async fn liquorice_quote_retry_on_bad_response() {
        // The venue answers the first two attempts with a server error and the third with a
        // quote.
        // The route counts its own attempts, so one test's retries never reach another's.
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = spawn_http_server(move |_| {
            Some(if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                ("500 Internal Server Error", "Internal Server Error".to_string())
            } else {
                ("200 OK", BINDING_QUOTE.to_string())
            })
        })
        .await;

        let client = create_test_client(format!("{}/rfq", server.url()), Duration::from_secs(5));
        let params = create_test_quote_params();
        let result = client
            .request_binding_quote(&params, "test-maker")
            .await;

        assert!(result.is_ok(), "Expected success after retries, got: {:?}", result);
        let quote = result.unwrap();

        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));
        assert_eq!(quote.amount_out, BigUint::from(3329502u64));

        assert_eq!(server.request_count(), 3);
    }

    /// The book request asks about the chain under the parameter Liquorice reads it from.
    #[tokio::test]
    async fn the_book_request_carries_the_chain_id() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&targets);
        let server = spawn_http_server(move |request| {
            recorded
                .lock()
                .unwrap()
                .push(request.target.to_string());
            Some(("200 OK", r#"{"prices":{}}"#.to_string()))
        })
        .await;
        let chain_id = Chain::Arbitrum.id();
        let client = LiquoriceClient::new(
            Some(&server.url()),
            chain_id,
            "solver".to_string(),
            "key".to_string(),
            Duration::from_secs(5),
            60,
        );

        client
            .fetch_price_levels()
            .await
            .unwrap();

        let targets = targets.lock().unwrap();
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert!(targets[0].contains(&format!("chainId={chain_id}")), "{targets:?}");
    }

    /// Optimism (10) is a chain Liquorice does not price, and the words it refuses one with are
    /// what this client reads to stop the feed instead of polling on.
    #[tokio::test]
    #[ignore = "hits Liquorice's live API; requires LIQUORICE_USER and LIQUORICE_KEY"]
    async fn live_liquorice_api_refuses_a_chain_it_does_not_price() {
        dotenv().ok();
        let auth_solver = std::env::var("LIQUORICE_USER").expect("LIQUORICE_USER not set");
        let auth_key = std::env::var("LIQUORICE_KEY").expect("LIQUORICE_KEY not set");
        let client =
            LiquoriceClient::new(None, 10, auth_solver, auth_key, Duration::from_secs(10), 60);

        let error = client
            .fetch_price_levels()
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    /// A bad request is ordinarily worth another poll, so only the refusal that names the chain
    /// ends the feed.
    #[rstest]
    #[case::an_unserved_chain(
        "Failed to deserialize query string: chainId: invalid value: 10, expected one of: 1, 11155111, 42161",
        true
    )]
    #[case::a_missing_parameter(
        "Failed to deserialize query string: missing field `chainId`",
        false
    )]
    #[tokio::test]
    async fn only_the_refusal_naming_the_chain_is_fatal(
        #[case] body: &'static str,
        #[case] fatal: bool,
    ) {
        let server = spawn_http_server(move |_| Some(("400 Bad Request", body.to_string()))).await;
        let client = LiquoriceClient::new(
            Some(&server.url()),
            10,
            "solver".to_string(),
            "key".to_string(),
            Duration::from_secs(5),
            60,
        );

        let error = client
            .fetch_price_levels()
            .await
            .unwrap_err();

        assert_eq!(error.is_fatal(), fatal, "{error}");
    }
}
