use std::{collections::HashMap, str::FromStr};

use ::http::{Request, Uri};
use num_bigint::BigUint;
use reqwest::Client;
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
        protocols::bebop::models::{BebopOrderToSign, BebopQuoteResponse, BebopSupportedChain},
    },
    snapshot_feed::http::{nothing_refused, read_json},
};

/// Host serving Bebop's PMM API; every per-chain path hangs off it.
const BEBOP_API_HOST: &str = "api.bebop.xyz";

/// Requests binding Bebop quotes. One instance is shared (via `Arc`) by every state a
/// [`BebopFeed`](super::feed::BebopFeed) emits, so all of them reuse the same HTTP
/// connection pool.
#[derive(derive_more::Debug)]
pub struct BebopClient {
    quote_endpoint: String,
    pricing_ws_endpoint: String,
    // key header for authentication
    #[debug(skip)]
    key: String,
    quote_timeout: Duration,
    origin: QuoteOrigin,
    http: Client,
}

/// Who a binding quote is for, which Bebop screens the request by. Every field it has is sent
/// with the request and every field it has not is left out of it.
///
/// Bebop uses these for abuse prevention and address screening, and can configure an API
/// account to require them, refusing a quote request that leaves a required one out. See
/// <https://docs.bebop.xyz/rfq-api/guides/best-practices#6-pass-origin-so-we-can-identify-legitimate-flow>.
#[derive(Clone, Debug, Default)]
pub struct QuoteOrigin {
    /// The real end-user's EOA when the taker is not the end-user's own wallet.
    pub address: Option<Bytes>,
    /// The `to` address of the resulting transaction when a contract executes the swap.
    pub target: Option<Bytes>,
    /// Stable identifier for the upstream flow source when aggregating multiple sources.
    pub source: Option<String>,
}

impl BebopClient {
    /// A client for Bebop's public API on `chain`, whose quote and pricing endpoints share the
    /// chain's own path.
    pub fn new(
        chain: BebopSupportedChain,
        key: String,
        quote_timeout: Duration,
        origin: QuoteOrigin,
    ) -> Self {
        let chain_path = format!("{BEBOP_API_HOST}/pmm/{}/v3", chain.as_str());
        BebopClient {
            quote_endpoint: format!("https://{chain_path}/quote"),
            pricing_ws_endpoint: format!("wss://{chain_path}/pricing?format=protobuf"),
            key,
            quote_timeout,
            origin,
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

        // The request's own origin names the end user this trade is for; the builder's is the
        // one flow behind every request a feed makes.
        if let Some(origin_address) = params
            .origin
            .as_ref()
            .or(self.origin.address.as_ref())
        {
            query.push((
                "origin_address",
                bytes_to_address(origin_address)
                    .map_err(|e| RFQError::InvalidInput(e.to_string()))?
                    .to_string(),
            ));
        }
        if let Some(origin_target) = &self.origin.target {
            query.push((
                "origin_target",
                bytes_to_address(origin_target)
                    .map_err(|e| RFQError::InvalidInput(e.to_string()))?
                    .to_string(),
            ));
        }
        if let Some(origin_source) = &self.origin.source {
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
                        "Bebop quote request timed out after {:?}",
                        self.quote_timeout
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
                        "Bebop quote request timed out after {:?}",
                        self.quote_timeout
                    )));
                }
            };

            let quote_response = match read_json::<BebopQuoteResponse>(
                response,
                "Bebop quote",
                nothing_refused,
            )
            .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(attempt = attempt + 1, max_attempts = MAX_RETRIES, error = %e, "quote response failed");
                    last_error = Some(RFQError::from(e));
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
            BebopQuoteResponse::Error(err) => {
                let message = format!(
                    "Bebop API error: code {} - {} (requestId: {})",
                    err.error.error_code, err.error.message, err.error.request_id
                );
                // A code the venue has not been seen to decline a trade with is taken to be
                // the request itself being wrong.
                Err(if err.error.declines_the_trade() {
                    RFQError::QuoteNotFound(message)
                } else {
                    RFQError::FatalError(message)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use rstest::rstest;
    use tycho_common::models::protocol::GetAmountOutParams;

    use super::*;
    use crate::{
        rfq::errors::RFQError,
        snapshot_feed::http::test_support::{spawn_http_server, MockResponse},
    };

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
            BebopSupportedChain::Ethereum,
            "secret_key".to_string(),
            Duration::from_secs(30),
            QuoteOrigin::default(),
        )
    }

    #[test]
    fn debug_output_omits_credentials() {
        let rendered = format!("{:?}", test_client());

        assert!(!rendered.contains("secret_key"));
        assert!(rendered.contains("quote_endpoint"));
    }

    #[test]
    fn process_bebop_quote_response_aggregate_order() {
        let quote_response: BebopQuoteResponse = serde_json::from_str(AGGREGATE_ORDER).unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("20000000000").unwrap(),
            token_in: Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            token_out: Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap(),
            sender: Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap(),
            receiver: Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap(),
            origin: None,
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_out, BigUint::from_str("52571055094221715780641").unwrap());
        assert_eq!(res.amount_in, BigUint::from_str("20000000000").unwrap());
        assert_eq!(res.base_token, params.token_in);
        assert_eq!(res.quote_token, params.token_out);
    }

    #[test]
    fn process_bebop_quote_response_aggregate_order_with_multihop() {
        let quote_response: BebopQuoteResponse =
            serde_json::from_str(AGGREGATE_ORDER_WITH_MULTIHOP).unwrap();
        let params = GetAmountOutParams {
            amount_in: BigUint::from_str("43067495979235520920162").unwrap(),
            token_in: Bytes::from_str("0xDEf1CA1fb7FBcDC777520aa7f396b4E015F497aB").unwrap(),
            token_out: Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(),
            sender: Bytes::from_str("0x809305d724B6E79C71e10a097ABadd1274B9C279").unwrap(),
            receiver: Bytes::from_str("0x809305d724B6E79C71e10a097ABadd1274B9C279").unwrap(),
            origin: None,
        };
        let res = BebopClient::process_quote_response(quote_response, &params).unwrap();
        assert_eq!(res.amount_out, BigUint::from_str("11186653890").unwrap());
        assert_eq!(res.amount_in, BigUint::from_str("43067495979235520920162").unwrap());
        assert_eq!(res.base_token, params.token_in);
        assert_eq!(res.quote_token, params.token_out);
    }

    #[test]
    fn process_bebop_quote_response_single_order() {
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
            origin: None,
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
    fn process_bebop_quote_response_single_order_router_mode() {
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
            origin: None,
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

    /// A client whose quotes go to a server of the test's own, with everything else as
    /// [`BebopClient::new`] builds it for Ethereum.
    fn create_test_client(quote_endpoint: String, quote_timeout: Duration) -> BebopClient {
        BebopClient {
            quote_endpoint,
            ..BebopClient::new(
                BebopSupportedChain::Ethereum,
                "test_key".to_string(),
                quote_timeout,
                QuoteOrigin::default(),
            )
        }
    }

    /// Sends one binding quote request and asserts the server received `expected` as its only
    /// `origin_address`.
    async fn assert_origin_sent(
        client_origin: Option<Bytes>,
        request_origin: Option<Bytes>,
        expected: &Bytes,
    ) {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::UrlEncoded(
                "origin_address".into(),
                bytes_to_address(expected)
                    .unwrap()
                    .to_string(),
            ))
            .with_body(include_str!("test_responses/aggregate_order.json"))
            .create_async()
            .await;
        let client = BebopClient {
            origin: QuoteOrigin { address: client_origin, ..Default::default() },
            ..create_test_client(server.url(), Duration::from_secs(5))
        };
        let params = GetAmountOutParams { origin: request_origin, ..create_test_quote_params() };

        client
            .request_binding_quote(&params)
            .await
            .unwrap();

        mock.assert_async().await;
    }

    /// The end user a quote is for is the trade's own, where the feed's origin stands for every
    /// trade it quotes.
    #[tokio::test]
    async fn the_requests_origin_is_the_one_bebop_is_told() {
        let request_origin = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let client_origin = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();

        assert_origin_sent(
            Some(client_origin.clone()),
            Some(request_origin.clone()),
            &request_origin,
        )
        .await;
        assert_origin_sent(Some(client_origin.clone()), None, &client_origin).await;
    }

    /// Bebop names the trade it will not do in an error body under HTTP 200; the two codes and
    /// their wording are what the production error log records. A size past its depth or below
    /// its minimum leaves the route to be priced elsewhere, where a code it has not been seen to
    /// decline with is the request being wrong.
    #[rstest]
    #[case::insufficient_liquidity(
        102,
        "InsufficientLiquidity: Insufficient liquidity for pair",
        true
    )]
    #[case::min_size(104, "MinSize: Request quotes over $1.", true)]
    #[case::a_code_it_has_not_declined_with(1, "Something else went wrong", false)]
    #[tokio::test]
    async fn only_a_code_that_is_not_a_decline_is_fatal(
        #[case] code: u32,
        #[case] says: &'static str,
        #[case] declined: bool,
    ) {
        let server = spawn_http_server(move |_| {
            Some((
                "200 OK",
                format!(
                    r#"{{"error":{{"errorCode":{code},"message":"{says}","requestId":"abc"}}}}"#
                ),
            ))
        })
        .await;
        let client = create_test_client(format!("{}/quote", server.url()), Duration::from_secs(1));

        let error = client
            .request_binding_quote(&create_test_quote_params())
            .await
            .unwrap_err();

        assert!(
            if declined {
                matches!(error, RFQError::QuoteNotFound(_))
            } else {
                matches!(error, RFQError::FatalError(_))
            },
            "{error}"
        );
        assert!(error.to_string().contains(says), "{error}");
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
            origin: None,
        }
    }

    #[tokio::test]
    async fn bebop_quote_timeout() {
        let server = spawn_http_server(|_| {
            Some(MockResponse {
                status: "200 OK",
                body: AGGREGATE_ORDER.to_string(),
                delay: Duration::from_millis(500),
            })
        })
        .await;

        // Test 1: Client with short timeout (200ms) - should timeout
        let client_short_timeout =
            create_test_client(format!("{}/quote", server.url()), Duration::from_millis(200));
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
        let client_long_timeout =
            create_test_client(format!("{}/quote", server.url()), Duration::from_secs(1));

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

    #[tokio::test]
    async fn bebop_quote_retry_on_bad_response() {
        // The venue answers the first two attempts with a server error and the third with a
        // quote.
        // The route counts its own attempts, so one test's retries never reach another's.
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = spawn_http_server(move |_| {
            Some(if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                ("500 Internal Server Error", "Internal Server Error".to_string())
            } else {
                ("200 OK", AGGREGATE_ORDER.to_string())
            })
        })
        .await;

        let client = create_test_client(format!("{}/quote", server.url()), Duration::from_secs(5));
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
        assert_eq!(server.request_count(), 3);
    }

    #[test]
    fn process_bebop_quote_response_aggregate_order_router_mode() {
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
            origin: None,
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

    /// Both endpoints Bebop serves a chain under hang off that chain's own path.
    #[test]
    fn the_endpoints_carry_the_chain_the_client_prices_on() {
        let client = BebopClient::new(
            BebopSupportedChain::Base,
            "secret_key".to_string(),
            Duration::from_secs(30),
            QuoteOrigin::default(),
        );

        assert_eq!(client.quote_endpoint, "https://api.bebop.xyz/pmm/base/v3/quote");
        assert_eq!(
            client.pricing_ws_endpoint,
            "wss://api.bebop.xyz/pmm/base/v3/pricing?format=protobuf"
        );
    }
}
