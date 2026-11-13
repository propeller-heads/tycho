use std::{
    str::FromStr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy::primitives::keccak256;
use num_bigint::BigUint;
use reqwest::{header::RETRY_AFTER, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote, Bytes,
};

use crate::{
    rfq::{
        errors::RFQError,
        protocols::aqua0::models::{
            Aqua0ErrorBody, Aqua0Market, Aqua0MarketsResponse, Aqua0QuoteRequest,
            Aqua0QuoteResponse, Aqua0StateResponse,
        },
    },
    snapshot_feed::{errors::FeedError, http::fetch_json},
};

/// Aqua0's production Tycho RFQ API.
pub const AQUA0_API_URL: &str = "https://api.aqua0.xyz/api/tycho/rfq";

/// How long approvals stay paused after a 429 that names no wait of its own.
const DEFAULT_PAUSE: Duration = Duration::from_secs(60);

/// Whether Aqua0 refused the request because it does not execute on the chain.
fn refuses_the_chain(body: &str) -> bool {
    body.contains("CHAIN_NOT_SUPPORTED")
}

/// What `/state` said about one pool.
#[derive(Debug)]
pub(crate) enum StateAnswer {
    Quotable(Box<Aqua0StateResponse>),
    /// The pool is listed but no sampled size can be fully backed right now.
    NoLiquidity,
}

/// The pool and strategy a binding quote is asked for, as the backend names them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aqua0Pool {
    pub component_id: String,
    pub chain_id: u64,
    pub pool_id: String,
    pub class_id: String,
}

/// The backend's component id for a pool and strategy:
/// `keccak256("aqua0-rfq-v1:{chain}:{pool}:{class}")`.
pub(crate) fn backend_component_id(
    chain_id: u64,
    pool_id: &str,
    class_id: &str,
) -> Result<String, String> {
    let class_id =
        BigUint::from_str(class_id).map_err(|_| format!("invalid Aqua0 class id {class_id}"))?;
    Ok(format!(
        "{:#x}",
        keccak256(format!("aqua0-rfq-v1:{chain_id}:{}:{class_id}", pool_id.to_lowercase()))
    ))
}

/// Talks to Aqua0's Tycho RFQ API. One instance is shared (via `Arc`) by the feed and every state
/// it emits, so price reads and binding quotes reuse one connection pool and one approval pause.
#[derive(derive_more::Debug)]
pub struct Aqua0Client {
    chain_id: u64,
    base_url: String,
    #[debug(skip)]
    http: Client,
    request_timeout: Duration,
    quote_timeout: Duration,
    /// Set when `/quote` answers 429: no approval is requested, and no pool is published, until
    /// then.
    #[debug(skip)]
    paused_until: Mutex<Option<Instant>>,
}

impl Aqua0Client {
    pub(crate) fn new(
        base_url: &str,
        chain_id: u64,
        request_timeout: Duration,
        quote_timeout: Duration,
    ) -> Self {
        Self {
            chain_id,
            base_url: base_url
                .trim_end_matches('/')
                .to_string(),
            http: Client::new(),
            request_timeout,
            quote_timeout,
            paused_until: Mutex::new(None),
        }
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// How much longer approvals are paused, or `None` when they are not.
    pub(crate) fn paused_for(&self) -> Option<Duration> {
        let until = (*self
            .paused_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))?;
        until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    fn pause(&self, wait: Duration) {
        let mut until = self
            .paused_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = Instant::now() + wait;
        if until.is_none_or(|current| current < next) {
            *until = Some(next);
        }
    }

    pub(crate) async fn fetch_markets(&self) -> Result<Vec<Aqua0Market>, FeedError> {
        let request = self
            .http
            .get(format!("{}/markets", self.base_url))
            .timeout(self.request_timeout)
            .query(&[("chainId", self.chain_id.to_string())]);
        let response: Aqua0MarketsResponse =
            fetch_json(request, "Aqua0 markets", refuses_the_chain).await?;
        Ok(response.markets)
    }

    /// Indicative levels for one listed market at the given comma-separated sample sizes.
    pub(crate) async fn fetch_state(
        &self,
        market: &Aqua0Market,
        amount0_samples: &str,
        amount1_samples: &str,
    ) -> Result<StateAnswer, FeedError> {
        let response = self
            .http
            .get(format!("{}/state", self.base_url))
            .timeout(self.request_timeout)
            .query(&[
                ("chainId", self.chain_id.to_string()),
                ("poolId", market.pool_id.clone()),
                ("classId", market.class_id.clone()),
                ("amount0Samples", amount0_samples.to_string()),
                ("amount1Samples", amount1_samples.to_string()),
            ])
            .send()
            .await
            .map_err(|error| FeedError::Connection(format!("Aqua0 state request: {error}")))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| FeedError::Connection(format!("Aqua0 state response: {error}")))?;
        if status.is_success() {
            let state: Aqua0StateResponse = serde_json::from_slice(&body)
                .map_err(|error| FeedError::Parsing(format!("Aqua0 state: {error}")))?;
            // No level in either direction is the same answer as the refusal below.
            if state.directions.zero_for_one.is_empty() && state.directions.one_for_zero.is_empty()
            {
                return Ok(StateAnswer::NoLiquidity);
            }
            return Ok(StateAnswer::Quotable(Box::new(state)));
        }
        let error: Aqua0ErrorBody = serde_json::from_slice(&body).unwrap_or_default();
        if status == StatusCode::BAD_REQUEST &&
            error.code.as_deref() == Some("RFQ_REFUSED") &&
            error
                .message
                .as_deref()
                .is_some_and(|message| message.contains("no quotable"))
        {
            return Ok(StateAnswer::NoLiquidity);
        }
        Err(FeedError::Connection(format!(
            "Aqua0 state HTTP error {status}: {}",
            String::from_utf8_lossy(&body)
        )))
    }

    /// Requests a binding approval for one exact-input fill through `params.sender`, the Tycho
    /// router. The backend reserves LP backing for it until its deadline, so only call this to
    /// execute.
    pub(crate) async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
        pool: &Aqua0Pool,
    ) -> Result<SignedQuote, RFQError> {
        if let Some(left) = self.paused_for() {
            return Err(RFQError::QuoteNotFound(format!(
                "Aqua0 approvals are paused for another {}s after a 429",
                left.as_secs().max(1)
            )));
        }
        let request = Aqua0QuoteRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            component_id: pool.component_id.clone(),
            chain_id: pool.chain_id,
            pool_id: pool.pool_id.clone(),
            class_id: pool.class_id.clone(),
            token_in: params.token_in.to_string(),
            token_out: params.token_out.to_string(),
            amount_in: params.amount_in.to_string(),
            expected_router: params.sender.to_string(),
        };
        let response = self
            .http
            .post(format!("{}/quote", self.base_url))
            .timeout(self.quote_timeout)
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let header = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            let body: Aqua0ErrorBody = response
                .json()
                .await
                .unwrap_or_default();
            let wait = header
                .or(body.retry_after_seconds)
                .map_or(DEFAULT_PAUSE, Duration::from_secs);
            return Err(self.paused_by(wait));
        }
        let body = response
            .bytes()
            .await
            .map_err(RFQError::from)?;
        if !status.is_success() {
            return Err(RFQError::QuoteNotFound(format!(
                "Aqua0 binding quote HTTP error {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let quote: Aqua0QuoteResponse = serde_json::from_slice(&body)
            .map_err(|error| RFQError::ParsingError(format!("Invalid Aqua0 quote: {error}")))?;
        check_binding_quote(&quote, &request, params, unix_now()?)
    }

    fn paused_by(&self, wait: Duration) -> RFQError {
        warn!(
            wait_secs = wait.as_secs(),
            "Aqua0 refused an approval with 429, pausing approvals and withdrawing its pools"
        );
        self.pause(wait);
        RFQError::QuoteNotFound(format!(
            "Aqua0 refused the approval with 429; retry after {}s",
            wait.as_secs()
        ))
    }
}

pub(crate) fn unix_now() -> Result<u64, RFQError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RFQError::FatalError(error.to_string()))?
        .as_secs())
}

/// A binding quote is accepted only for exactly the swap asked for, through the router asked
/// for, with hook data and an unexpired deadline. Anything else answers another request.
fn check_binding_quote(
    quote: &Aqua0QuoteResponse,
    request: &Aqua0QuoteRequest,
    params: &GetAmountOutParams,
    now: u64,
) -> Result<SignedQuote, RFQError> {
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let mismatch = |field: &str| {
        RFQError::FatalError(format!("Aqua0 binding quote {field} does not match the request"))
    };
    if quote.schema_version != "aqua0-rfq-quote-v1" {
        return Err(mismatch("schemaVersion"));
    }
    if quote.request_id != request.request_id {
        return Err(mismatch("requestId"));
    }
    if !same(&quote.component_id, &request.component_id) {
        return Err(mismatch("componentId"));
    }
    if quote.chain_id != request.chain_id {
        return Err(mismatch("chainId"));
    }
    if !same(&quote.token_in, &request.token_in) || !same(&quote.token_out, &request.token_out) {
        return Err(mismatch("token"));
    }
    if quote.amount_in != request.amount_in {
        return Err(mismatch("amountIn"));
    }
    if !same(&quote.router, &request.expected_router) ||
        !same(&quote.executor, &request.expected_router)
    {
        return Err(mismatch("router"));
    }
    let amount_out = BigUint::from_str(&quote.amount_out)
        .map_err(|_| RFQError::ParsingError("Invalid Aqua0 amountOut".into()))?;
    if amount_out == BigUint::default() {
        return Err(mismatch("amountOut"));
    }
    let deadline = quote
        .deadline
        .parse::<u64>()
        .map_err(|_| RFQError::ParsingError("Invalid Aqua0 deadline".into()))?;
    if deadline <= now {
        return Err(RFQError::QuoteNotFound("Aqua0 binding quote is already expired".into()));
    }
    let hook_data = Bytes::from_str(&quote.hook_data)
        .map_err(|error| RFQError::ParsingError(format!("Invalid Aqua0 hookData: {error}")))?;
    if hook_data.is_empty() {
        return Err(mismatch("hookData"));
    }
    debug!(%amount_out, deadline, "Aqua0 binding quote accepted");
    Ok(SignedQuote {
        base_token: params.token_in.clone(),
        quote_token: params.token_out.clone(),
        amount_in: params.amount_in.clone(),
        amount_out,
        quote_attributes: [("hook_data".to_string(), hook_data)].into(),
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::snapshot_feed::http::test_support::spawn_http_server;

    const ROUTER: &str = "0x924f147c50ea59f5180a26031a8b65b2aa1e81cd";

    fn params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: 1_000_000u32.into(),
            token_in: Bytes::from_str("0xaf88d065e77c8cc2239327c5edb3a432268e5831").unwrap(),
            token_out: Bytes::from_str("0xb96aa6babccd738d6644add4912fe5efbebf5a25").unwrap(),
            sender: Bytes::from_str(ROUTER).unwrap(),
            receiver: Bytes::from_str(ROUTER).unwrap(),
            origin: None,
        }
    }

    fn pool() -> Aqua0Pool {
        let pool_id = format!("0x{}", "11".repeat(32));
        let class_id = BigUint::from_bytes_be(&[0x11; 32]).to_string();
        Aqua0Pool {
            component_id: backend_component_id(42161, &pool_id, &class_id).unwrap(),
            chain_id: 42161,
            pool_id,
            class_id,
        }
    }

    fn request() -> Aqua0QuoteRequest {
        let (params, pool) = (params(), pool());
        Aqua0QuoteRequest {
            request_id: "r-1".into(),
            component_id: pool.component_id,
            chain_id: pool.chain_id,
            pool_id: pool.pool_id,
            class_id: pool.class_id,
            token_in: params.token_in.to_string(),
            token_out: params.token_out.to_string(),
            amount_in: params.amount_in.to_string(),
            expected_router: params.sender.to_string(),
        }
    }

    fn quote_for(request: &Aqua0QuoteRequest) -> Aqua0QuoteResponse {
        Aqua0QuoteResponse {
            schema_version: "aqua0-rfq-quote-v1".into(),
            request_id: request.request_id.clone(),
            component_id: request.component_id.clone(),
            chain_id: request.chain_id,
            token_in: request.token_in.clone(),
            token_out: request.token_out.clone(),
            amount_in: request.amount_in.clone(),
            amount_out: "999000".into(),
            router: request.expected_router.clone(),
            executor: request.expected_router.clone(),
            hook_data: "0xdeadbeef".into(),
            deadline: "1000".into(),
        }
    }

    /// The backend's id for a production Polygon market, read from its `/state` answer.
    #[test]
    fn component_id_matches_the_backend() {
        assert_eq!(
            backend_component_id(
                137,
                "0x296223482f3c0bde400d7421d2465b582d1bc2f97d6e48122160c7f89e1858bc",
                "18718221311474880640371721162177634097308696695063475841855932547632989755580",
            )
            .unwrap(),
            super::super::source::tests::POLYGON_COMPONENT_ID
        );
        assert!(backend_component_id(1, "0x00", "not-a-number").is_err());
    }

    #[test]
    fn an_exact_quote_is_accepted() {
        let request = request();
        let signed = check_binding_quote(&quote_for(&request), &request, &params(), 999).unwrap();
        assert_eq!(signed.amount_out, BigUint::from(999_000u32));
        assert_eq!(signed.quote_attributes["hook_data"].to_string(), "0xdeadbeef");
    }

    #[rstest]
    #[case::schema(|q: &mut Aqua0QuoteResponse| q.schema_version = "v0".into(), "schemaVersion")]
    #[case::request_id(|q: &mut Aqua0QuoteResponse| q.request_id = "other".into(), "requestId")]
    #[case::component(
        |q: &mut Aqua0QuoteResponse| q.component_id = format!("0x{}", "22".repeat(32)),
        "componentId"
    )]
    #[case::chain(|q: &mut Aqua0QuoteResponse| q.chain_id = 8453, "chainId")]
    #[case::token_in(
        |q: &mut Aqua0QuoteResponse| q.token_in = format!("0x{}", "33".repeat(20)),
        "token"
    )]
    #[case::token_out(
        |q: &mut Aqua0QuoteResponse| q.token_out = format!("0x{}", "33".repeat(20)),
        "token"
    )]
    #[case::amount_in(|q: &mut Aqua0QuoteResponse| q.amount_in = "999999".into(), "amountIn")]
    #[case::router(
        |q: &mut Aqua0QuoteResponse| q.router = format!("0x{}", "44".repeat(20)),
        "router"
    )]
    #[case::executor(
        |q: &mut Aqua0QuoteResponse| q.executor = format!("0x{}", "44".repeat(20)),
        "router"
    )]
    #[case::zero_out(|q: &mut Aqua0QuoteResponse| q.amount_out = "0".into(), "amountOut")]
    #[case::empty_hook_data(|q: &mut Aqua0QuoteResponse| q.hook_data = "0x".into(), "hookData")]
    fn a_quote_for_anything_else_is_refused(
        #[case] change: fn(&mut Aqua0QuoteResponse),
        #[case] field: &str,
    ) {
        let request = request();
        let mut quote = quote_for(&request);
        change(&mut quote);
        let error = check_binding_quote(&quote, &request, &params(), 999).unwrap_err();
        assert!(
            matches!(&error, RFQError::FatalError(message) if message.contains(field)),
            "{error}"
        );
    }

    #[test]
    fn an_expired_or_malformed_quote_is_refused() {
        let request = request();
        let quote = quote_for(&request);
        assert!(matches!(
            check_binding_quote(&quote, &request, &params(), 1000),
            Err(RFQError::QuoteNotFound(_))
        ));
        let mut bad = quote.clone();
        bad.amount_out = "-1".into();
        assert!(matches!(
            check_binding_quote(&bad, &request, &params(), 999),
            Err(RFQError::ParsingError(_))
        ));
        let mut bad = quote;
        bad.hook_data = "not hex".into();
        assert!(matches!(
            check_binding_quote(&bad, &request, &params(), 999),
            Err(RFQError::ParsingError(_))
        ));
    }

    #[tokio::test]
    async fn a_429_pauses_approvals_without_another_request() {
        let server = spawn_http_server(|_| {
            Some((
                "429 Too Many Requests",
                r#"{"error":"RATE_LIMITED","code":"RATE_LIMITED","message":"banned","retryAfterSeconds":900}"#
                    .to_string(),
            ))
        })
        .await;
        let client =
            Aqua0Client::new(&server.url(), 42161, Duration::from_secs(5), Duration::from_secs(5));

        let first = client
            .request_binding_quote(&params(), &pool())
            .await
            .unwrap_err();
        assert!(matches!(first, RFQError::QuoteNotFound(ref m) if m.contains("900s")), "{first}");
        let left = client.paused_for().unwrap();
        assert!(left > Duration::from_secs(890) && left <= Duration::from_secs(900));

        let second = client
            .request_binding_quote(&params(), &pool())
            .await
            .unwrap_err();
        assert!(matches!(second, RFQError::QuoteNotFound(ref m) if m.contains("paused")));
        assert_eq!(server.request_count(), 1, "a paused client does not ask again");
    }

    #[tokio::test]
    async fn a_refused_quote_is_recoverable_and_does_not_pause() {
        let server = spawn_http_server(|_| {
            Some((
                "400 Bad Request",
                r#"{"error":"RFQ_REFUSED","code":"RFQ_REFUSED","message":"cannot fully back"}"#
                    .to_string(),
            ))
        })
        .await;
        let client =
            Aqua0Client::new(&server.url(), 42161, Duration::from_secs(5), Duration::from_secs(5));
        let error = client
            .request_binding_quote(&params(), &pool())
            .await
            .unwrap_err();
        assert!(matches!(error, RFQError::QuoteNotFound(_)), "{error}");
        assert!(client.paused_for().is_none());
    }

    #[tokio::test]
    async fn a_binding_quote_is_one_keyless_post() {
        let server = spawn_http_server(|request| {
            assert!(request.target.starts_with("/quote"), "{}", request.target);
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            Some((
                "200 OK",
                serde_json::json!({
                    "schemaVersion": "aqua0-rfq-quote-v1", "requestId": body["requestId"],
                    "componentId": body["componentId"], "chainId": 42161,
                    "tokenIn": body["tokenIn"], "tokenOut": body["tokenOut"],
                    "amountIn": body["amountIn"], "amountOut": "200",
                    "router": body["expectedRouter"], "executor": body["expectedRouter"],
                    "hookData": "0xdeadbeef", "swapId": format!("0x{}", "22".repeat(32)),
                    "nonce": "1", "deadline": "4102444800", "ranges": [],
                })
                .to_string(),
            ))
        })
        .await;
        let client =
            Aqua0Client::new(&server.url(), 42161, Duration::from_secs(5), Duration::from_secs(5));
        let signed = client
            .request_binding_quote(&params(), &pool())
            .await
            .unwrap();
        assert_eq!(signed.amount_out, BigUint::from(200u32));
        assert_eq!(server.request_count(), 1);
    }
}
