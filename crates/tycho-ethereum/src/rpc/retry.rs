//! RPC retry logic and error classification
//!
//! This module contains retry logic for RPC requests, including exponential backoff
//! and intelligent error classification to determine which errors should be retried.

use std::time::Duration;

use alloy::{
    primitives::private::serde::Deserialize,
    rpc::json_rpc::ErrorPayload,
    transports::{RpcError, TransportErrorKind},
};
use backoff::backoff::Backoff;
pub use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
use serde_json::value::RawValue;
use tracing::{debug, warn};

use crate::rpc::config::RPCRetryConfig;

/// Extension trait to implement retry logic for [`RpcError<TransportErrorKind>`].
///
/// This trait provides methods to analyze RPC errors and determine whether they should
/// be retried, as well as extract backoff hints from error responses.
///
/// # Attribution
/// Adapted from alloy-transport:
/// https://github.com/alloy-rs/alloy/blob/a3899575fbc0c789275f95661516b99e9a92838d/crates/transport/src/error.rs#L156
/// License: MIT OR Apache-2.0
pub(crate) trait RetryableError {
    /// Analyzes whether to retry the request depending on the error.
    ///
    /// Returns `true` for transient errors that are likely to succeed on retry:
    /// - Rate limiting (429) errors
    /// - Service unavailable (503) errors
    /// - Missing batch responses
    /// - Null responses
    /// - Certain JSON-RPC error codes
    ///
    /// Returns `false` for permanent errors that won't succeed on retry:
    /// - Serialization/deserialization errors
    /// - Invalid request errors
    /// - Backend gone errors
    fn is_retryable(&self) -> bool;

    /// Fetches the backoff hint from the error message if present.
    ///
    /// Some RPC providers (e.g., Infura) include a suggested backoff duration
    /// in their rate limit error responses under `data.rate.backoff_seconds`.
    /// This method extracts that hint if available.
    fn backoff_hint(&self) -> Option<Duration>;

    /// Converts the RPC error into a [backoff::Error] for retry logic.
    fn to_backoff(self) -> backoff::Error<Self>
    where
        Self: Sized,
    {
        if self.is_retryable() {
            if let Some(hint) = self.backoff_hint() {
                backoff::Error::retry_after(self, hint)
            } else {
                backoff::Error::transient(self)
            }
        } else {
            backoff::Error::permanent(self)
        }
    }
}

/// Extends alloy's default retry logic with additional error code classification.
///
/// This function is used in combination with alloy's [`ErrorPayload::is_retry_err()`] to
/// provide more conservative retry behavior. The final retry decision is:
/// `alloy.is_retry_err() || has_custom_retry_code()`.
///
/// We add explicit handling for error codes that may be transient (like `-32000` for
/// "header not found" or `-32603` for "internal error") and default unknown codes to
/// retryable to err on the side of safety.
pub(super) fn has_custom_retry_code<T>(e: &ErrorPayload<T>) -> bool {
    match e.code {
        // Retryable errors (transient issues)
        -32000 => true, // "header not found" - block may not be available yet
        -32005 => true, // "limit exceeded" - rate limiting, backoff and retry
        -32603 => true, // "internal error" - temporary server issue

        // Non-retryable errors (permanent issues)
        -32600 => false, // "invalid request" - malformed request
        -32601 => false, // "method not found" - method doesn't exist
        -32602 => false, // "invalid params" - wrong parameters
        -32604 => false, // "method not supported" - not supported by this node

        // Other non EIP-1474 errors
        3 => false, // "execution reverted" - special error for `eth_call` and `eth_estimateGas`

        // Default: retry unknown error codes (conservative approach)
        // perf: consider being less conservative to reduce unnecessary retries
        _ => true,
    }
}

impl<E: std::borrow::Borrow<RawValue>> RetryableError for RpcError<TransportErrorKind, E> {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(err) => err.is_retry_err(),
            Self::SerError(_) => false,
            Self::DeserError { text, .. } => {
                if let Ok(resp) = serde_json::from_str::<ErrorPayload>(text) {
                    return resp.is_retry_err() || has_custom_retry_code(&resp);
                }

                // some providers send invalid JSON RPC in the error case (no `id:u64`), but the
                // text should be a `JsonRpcError`
                #[derive(Deserialize)]
                struct Resp {
                    error: ErrorPayload,
                }

                if let Ok(resp) = serde_json::from_str::<Resp>(text) {
                    return resp.error.is_retry_err() || has_custom_retry_code(&resp.error);
                }

                // Some nodes return `{"result": null}` under load (e.g. rate limiting)
                // instead of a proper error response. Alloy treats this as a success
                // with a null result, which fails deserialization. Retry like NullResp.
                if text.trim() == "null" {
                    return true;
                }

                false
            }
            Self::ErrorResp(err) => err.is_retry_err() || has_custom_retry_code(err),
            Self::NullResp => true,
            _ => false,
        }
    }

    fn backoff_hint(&self) -> Option<Duration> {
        if let Self::ErrorResp(resp) = self {
            let data = resp.try_data_as::<serde_json::Value>();
            if let Some(Ok(data)) = data {
                let backoff_seconds = &data["rate"]["backoff_seconds"];
                // Try u64 first (whole numbers)
                if let Some(seconds) = backoff_seconds.as_u64() {
                    return Some(Duration::from_secs(seconds));
                }
                // Then try f64 (decimals) - round up for safety
                if let Some(seconds) = backoff_seconds.as_f64() {
                    return Some(Duration::from_secs(seconds.ceil() as u64));
                }
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    inner: ExponentialBackoff,
    attempts_left: usize,
    max_retries: usize,
}

impl Backoff for RetryPolicy {
    fn reset(&mut self) {
        self.attempts_left = self.max_retries;
        self.inner.reset();
    }

    fn next_backoff(&mut self) -> Option<Duration> {
        if self.attempts_left == 0 {
            warn!(max_retries = self.max_retries, "RPC retry attempts exhausted");
            return None;
        }
        self.attempts_left -= 1;
        let backoff_duration = self.inner.next_backoff();
        debug!(
            attempts_left = self.attempts_left,
            max_retries = self.max_retries,
            backoff_ms = backoff_duration.map(|d| d.as_millis() as u64),
            "RPC request failed, retrying after backoff"
        );
        backoff_duration
    }
}

impl From<RPCRetryConfig> for RetryPolicy {
    fn from(value: RPCRetryConfig) -> Self {
        let policy = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(value.initial_backoff_ms))
            .with_max_interval(Duration::from_millis(value.max_backoff_ms))
            .build();

        Self::new(policy, value.max_retries)
    }
}

impl From<&RetryPolicy> for RPCRetryConfig {
    fn from(value: &RetryPolicy) -> Self {
        Self {
            max_retries: value.max_retries,
            initial_backoff_ms: value.inner.initial_interval.as_millis() as u64,
            max_backoff_ms: value.inner.max_interval.as_millis() as u64,
        }
    }
}

impl RetryPolicy {
    pub(crate) fn new(backoff: ExponentialBackoff, max_attempts: usize) -> Self {
        Self { inner: backoff, attempts_left: max_attempts, max_retries: max_attempts }
    }

    /// Executes an RPC request with automatic retry on transient failures.
    pub(crate) async fn call_with_retry<F, Fut, T, E>(&self, mut operation: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: RetryableError + std::fmt::Debug,
    {
        // Currently we clone the policy for each retry_request call to avoid state sharing issues.
        // TODO: consider if we should share state for concurrent retries from multiple tasks
        let policy = self.clone();

        backoff::future::retry(policy, || {
            let fut = operation();
            async move {
                fut.await.map_err(|e| {
                    if e.is_retryable() {
                        debug!(error = ?e, "RPC request failed, will retry");
                    } else {
                        warn!(error = ?e, "RPC request failed, non-retryable");
                    }
                    e.to_backoff()
                })
            }
        })
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        borrow::Cow,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Instant,
    };

    use alloy::rpc::client::ClientBuilder;
    use mockito::{Mock, ServerGuard};
    use rstest::rstest;
    use serde::de::Error;

    use super::*;

    pub(crate) const MOCK_RETRY_POLICY_MAX_ATTEMPTS: usize = 3;

    pub(crate) fn mock_retry_policy() -> RetryPolicy {
        let inner = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(10))
            .with_multiplier(2.0)
            .with_max_interval(Duration::from_millis(100))
            .build();

        RetryPolicy::new(inner, MOCK_RETRY_POLICY_MAX_ATTEMPTS)
    }

    async fn mock_success(server: &mut ServerGuard) -> Mock {
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"jsonrpc":"2.0","id":0,"result":"0xabc"}"#)
            .expect(1)
            .create_async()
            .await
    }

    async fn batch_mock_success_at(server: &mut ServerGuard, retry_num: usize) -> Mock {
        // Note that for the batch requests, the ids increment per request retry, starting from 0.
        // So the correct ids for the response are `retry_num * calls_per_batch + request_index`.
        let first_id = retry_num * 2; // batch size is 2
        let second_id = first_id + 1;

        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"[
                    {{"jsonrpc":"2.0","id":{first_id},"result":"0xabc"}},
                    {{"jsonrpc":"2.0","id":{second_id},"result":"0xdef"}}
                ]"#,
            ))
            .expect(1)
            .create_async()
            .await
    }

    async fn mock_permanent_failure(server: &mut ServerGuard) -> Mock {
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"Invalid Request"}}"#,
            )
            .expect(1)
            .create_async()
            .await
    }

    async fn mock_retryable_failure(server: &mut ServerGuard) -> Mock {
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(429)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":429,"message":"Too Many Requests"}}"#,
            )
            .expect(1)
            .create_async()
            .await
    }

    #[rstest]
    // Retryable error codes
    #[case::header_not_found(-32000, true, "header not found", "block may not be available yet")]
    #[case::limit_exceeded(-32005, true, "limit exceeded", "rate limiting")]
    #[case::internal_error(-32603, true, "internal error", "temporary server issue")]
    // Non-retryable error codes
    #[case::invalid_request(-32600, false, "invalid request", "malformed request")]
    #[case::method_not_found(-32601, false, "method not found", "method doesn't exist")]
    #[case::invalid_params(-32602, false, "invalid params", "wrong parameters")]
    #[case::method_not_supported(-32604, false, "method not supported", "not supported by node")]
    // EVM execution errors (EIP-1474)
    #[case::execution_reverted(3, false, "execution reverted", "contract reverted")]
    // Unknown error codes (should be retryable by default for safety)
    #[case::unknown_error(-99999, true, "unknown error", "should be retryable by default")]
    fn test_json_rpc_error_code_classification(
        #[case] error_code: i64,
        #[case] expected_retryable: bool,
        #[case] message: &str,
        #[case] description: &str,
    ) {
        // Create an ErrorPayload with the specific error code
        let error_payload =
            ErrorPayload { code: error_code, message: Cow::from(message.to_string()), data: None };

        let err = RpcError::<TransportErrorKind>::ErrorResp(error_payload);
        let is_retryable = err.is_retryable();

        assert_eq!(
            is_retryable,
            expected_retryable,
            "Error code {} ({} : {}) should be {}retryable",
            error_code,
            message,
            description,
            if expected_retryable { "" } else { "non-" }
        );
    }

    #[rstest]
    #[case::rate_limited(-32005, true, "rate limited")]
    #[case::invalid_request(-32600, false, "invalid request")]
    #[test]
    fn test_deser_error_with_json_rpc_codes(
        #[case] error_code: i64,
        #[case] expected_retryable: bool,
        #[case] message: &str,
    ) {
        let deser_err = RpcError::<TransportErrorKind>::DeserError {
            err: serde_json::Error::custom("test"),
            text: format!(
                r#"{{"jsonrpc":"2.0","id":0,"error":{{"code":{error_code},"message":"{message}"}}}}"#,
            ),
        };

        assert_eq!(
            deser_err.is_retryable(),
            expected_retryable,
            "DeserError with code {} should be {}retryable",
            error_code,
            if expected_retryable { "" } else { "non-" }
        );
    }

    #[test]
    fn test_deser_error_null_result_is_retryable() {
        let deser_err = RpcError::<TransportErrorKind>::DeserError {
            err: serde_json::Error::custom(
                "invalid type: null, expected struct StorageRangeResultWrapper",
            ),
            text: "null".to_string(),
        };
        assert!(
            deser_err.is_retryable(),
            "DeserError with null text should be retryable (node returned result: null)"
        );
    }

    #[test]
    fn test_deser_error_without_id_field() {
        // Some providers send invalid JSON RPC without the id field
        let json_without_id = r#"{"error":{"code":-32005,"message":"Rate limited"}}"#;
        let deser_err = RpcError::<TransportErrorKind>::DeserError {
            err: serde_json::Error::custom("test"),
            text: json_without_id.to_string(),
        };
        assert!(
            deser_err.is_retryable(),
            "DeserError should parse error from invalid JSON RPC format"
        );
    }

    #[rstest]
    #[case::integer(10_f64, Duration::from_secs(10))]
    #[case::float(5.7, Duration::from_secs(6))]
    #[test]
    fn test_backoff_hint_extraction(#[case] backoff_value: f64, #[case] duration: Duration) {
        use alloy::rpc::json_rpc::ErrorPayload;

        // Create a mock error response with backoff hint
        // Need to create Box<RawValue> to satisfy trait bounds
        let data_json = serde_json::json!({
            "rate": {
                "backoff_seconds": backoff_value
            }
        });
        let data = serde_json::value::to_raw_value(&data_json).unwrap();

        let error_payload =
            ErrorPayload { code: -32005, message: "Rate limited".into(), data: Some(data) };

        let err = RpcError::ErrorResp(error_payload);
        let hint = err.backoff_hint();

        assert_eq!(hint, Some(duration));
    }

    #[test]
    fn test_backoff_hint_missing() {
        use alloy::rpc::json_rpc::ErrorPayload;

        // Create a mock error response without backoff hint
        let error_payload = ErrorPayload { code: -32005, message: "Some error".into(), data: None };

        let err = RpcError::<TransportErrorKind>::ErrorResp(error_payload);
        let hint = err.backoff_hint();

        assert_eq!(hint, None);
    }

    #[test]
    fn test_retry_policy_custom() {
        let exp_policy: ExponentialBackoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(100))
            .with_multiplier(2.0)
            .with_max_interval(Duration::from_secs(10))
            .with_max_elapsed_time(Some(Duration::from_secs(60)))
            .build();

        let retry_count = 5;

        let policy = RetryPolicy::new(exp_policy.clone(), retry_count);

        assert_eq!(policy.max_retries, retry_count);
        assert_eq!(policy.attempts_left, retry_count);
        assert_eq!(policy.inner.multiplier, exp_policy.multiplier);
        assert_eq!(policy.inner.max_interval, exp_policy.max_interval);
        assert_eq!(policy.inner.initial_interval, exp_policy.initial_interval);
    }

    #[tokio::test]
    async fn test_retry_on_rate_limit_then_succeed() {
        let mut server = mockito::Server::new_async().await;

        // First `max_retries` requests: 429 rate limit
        for _ in 0..MOCK_RETRY_POLICY_MAX_ATTEMPTS {
            let _mock_rate_limit = mock_retryable_failure(&mut server).await;
        }

        // Final request: success
        let mock_success = mock_success(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        assert!(result.is_ok(), "Expected eventual success after retries");
        assert_eq!(result.unwrap(), "0xabc");

        mock_success.assert();
    }

    #[tokio::test]
    async fn test_retry_exceeds_max_attempts() {
        let mut server = mockito::Server::new_async().await;

        // First `max_retries` requests: 429 rate limit
        for _ in 0..MOCK_RETRY_POLICY_MAX_ATTEMPTS {
            let _mock_rate_limit = mock_retryable_failure(&mut server).await;
        }

        // One extra request to exceed max attempts
        let mock_rate_limit = mock_retryable_failure(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        assert!(result.is_err(), "Expected failure after exceeding max retries");

        mock_rate_limit.assert()
    }

    #[tokio::test]
    async fn test_no_retry_on_permanent_error() {
        let mut server = mockito::Server::new_async().await;

        // 400 Bad Request - should NOT be retried
        let mock_failure = mock_permanent_failure(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        assert!(result.is_err(), "Expected immediate failure on non-retryable error");

        mock_failure.assert()
    }

    #[tokio::test]
    async fn test_backoff_hint_respected() {
        let mut server = mockito::Server::new_async().await;
        let start_time = Instant::now();

        // First request: rate limited with 1-second backoff hint
        let _mock_rate_limit = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "jsonrpc":"2.0",
                    "id":0,
                    "error":{
                        "code":-32005,
                        "message":"Rate limited",
                        "data":{"rate":{"backoff_seconds":1}}
                    }
                }"#,
            )
            .expect(1)
            .create_async()
            .await;

        // Second request: success
        let mock_success = mock_success(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        let elapsed = start_time.elapsed();

        assert!(result.is_ok(), "Expected eventual success after backoff");
        assert!(
            elapsed >= Duration::from_secs(1),
            "Expected at least 1 seconds delay due to backoff hint, got {:?}",
            elapsed
        );

        mock_success.assert();
    }

    #[tokio::test]
    async fn test_exponential_backoff() {
        let max_retries = 3;
        let start_interval = 50;
        let multiplier = 2.0;

        let mut server = mockito::Server::new_async().await;
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_clone = request_count.clone();
        let start_time = Instant::now();

        // First `max_retries` requests: 429 rate limit
        for _ in 0..max_retries {
            let _mock_rate_limit = mock_retryable_failure(&mut server).await;
        }

        // Last request: success
        let mock_success = mock_success(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let exp_policy: ExponentialBackoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(start_interval))
            .with_multiplier(multiplier)
            .build();
        let policy = RetryPolicy::new(exp_policy, max_retries);

        let result = policy
            .call_with_retry(|| async {
                request_count_clone.fetch_add(1, Ordering::SeqCst);
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        assert!(result.is_ok(), "Expected eventual success after retries");

        let elapsed = start_time.elapsed();

        // Calculate expected elapsed time due-to-backoff as a geometric series sum
        let expected_average = (multiplier.powi(max_retries as i32) - 1.0) / (multiplier - 1.0) *
            start_interval as f64;
        // Due to default 50% jitter applied by backoff crate we expect half of that time at minimum
        let expected_min = expected_average * 0.5;

        assert!(
            elapsed >= Duration::from_millis(expected_min as u64),
            "Expected at least {}ms elapsed time due to backoff, got {:?}",
            expected_min,
            elapsed
        );

        mock_success.assert();
    }

    /// Test that multiple requests can use the same policy without interfering
    /// with each other's retry attempts. This verifies that each retry_request
    /// call gets its own independent backoff state.
    #[tokio::test]
    async fn test_multiple_requests_with_shared_policy() {
        let mut server = mockito::Server::new_async().await;

        // First request: MOCK_RETRY_POLICY_MAX_ATTEMPTS failures then success
        for _ in 0..MOCK_RETRY_POLICY_MAX_ATTEMPTS {
            let _mock_rate_limit = mock_retryable_failure(&mut server).await;
        }
        let mock_success1 = mock_success(&mut server).await;

        // Second request: MOCK_RETRY_POLICY_MAX_ATTEMPTS failures then success
        for _ in 0..MOCK_RETRY_POLICY_MAX_ATTEMPTS {
            let _mock_rate_limit = mock_retryable_failure(&mut server).await;
        }
        let mock_success2 = mock_success(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());
        let shared_policy = mock_retry_policy();

        // First request - should succeed after retries
        let result1 = shared_policy
            .call_with_retry(|| async {
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        assert!(result1.is_ok(), "First request should succeed after retries");
        assert_eq!(result1.unwrap(), "0xabc");

        // Second request - should also succeed after retries
        // This verifies that the first request didn't exhaust the policy's retry attempts
        let result2 = shared_policy
            .call_with_retry(|| async {
                client
                    .request_noparams::<String>("eth_blockNumber")
                    .await
            })
            .await;

        assert!(result2.is_ok(), "Second request should succeed after retries");
        assert_eq!(result2.unwrap(), "0xabc");

        mock_success1.assert();
        mock_success2.assert();
    }

    #[tokio::test]
    async fn test_batch_request_retry_on_transient_failure() {
        let mut server = mockito::Server::new_async().await;

        // Simulate a batch request scenario: first `max_retries` attempts fail with 429
        for _ in 0..MOCK_RETRY_POLICY_MAX_ATTEMPTS {
            let _mock_rate_limit = mock_retryable_failure(&mut server).await;
        }

        // Success response with batch results
        let mock_success = batch_mock_success_at(&mut server, MOCK_RETRY_POLICY_MAX_ATTEMPTS).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        // Simulate a batch operation that makes multiple calls
        let result = mock_retry_policy()
            .call_with_retry(|| async {
                let mut batch = client.new_batch();
                let call1 = batch.add_call::<_, String>("eth_blockNumber", &())?;
                let call2 = batch.add_call::<_, String>("eth_blockNumber", &())?;

                batch.send().await?;

                let result1 = call1.await?;
                let result2 = call2.await?;

                Ok::<_, RpcError<TransportErrorKind>>((result1, result2))
            })
            .await;

        assert!(result.is_ok(), "Batch request should eventually succeed after retries");
        let (res1, res2) = result.unwrap();
        assert_eq!(res1, "0xabc");
        assert_eq!(res2, "0xdef");

        mock_success.assert();
    }

    #[tokio::test]
    async fn test_batch_request_no_retry_on_permanent_failure() {
        let mut server = mockito::Server::new_async().await;

        // Mock permanent error (400 Bad Request) - should only be called once
        let mock_failure = mock_permanent_failure(&mut server).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                let mut batch = client.new_batch();
                let call1 = batch.add_call::<_, String>("eth_blockNumber", &())?;
                let call2 = batch.add_call::<_, String>("eth_blockNumber", &())?;

                batch.send().await?;

                let result1 = call1.await?;
                let result2 = call2.await?;

                Ok::<_, RpcError<TransportErrorKind>>((result1, result2))
            })
            .await;

        assert!(result.is_err(), "Batch request should fail immediately on permanent error");

        mock_failure.assert();
    }

    #[tokio::test]
    async fn test_batch_with_partial_retriable_item_failures() {
        let mut server = mockito::Server::new_async().await;

        // First `max_retries` requests are partially failing with one retriable error
        let _mock_partial_failures = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"jsonrpc":"2.0","id":0,"result":"0xabc"},
                    {"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"header not found"}}
                ]"#,
            )
            .expect(MOCK_RETRY_POLICY_MAX_ATTEMPTS)
            .create_async()
            .await;

        let mock_success = batch_mock_success_at(&mut server, MOCK_RETRY_POLICY_MAX_ATTEMPTS).await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                let mut batch = client.new_batch();
                let call1 = batch.add_call::<_, String>("eth_blockNumber", &())?;
                let call2 = batch.add_call::<_, String>("eth_blockNumber", &())?;

                batch.send().await?;

                let result1 = call1.await?;
                let result2 = call2.await?;

                Ok::<_, RpcError<TransportErrorKind>>((result1, result2))
            })
            .await;

        assert!(result.is_ok(), "Batch request should eventually succeed after retries");

        mock_success.assert();
    }

    #[tokio::test]
    async fn test_batch_with_partial_permanent_item_failures() {
        let mut server = mockito::Server::new_async().await;

        // Mock batch response with one permanent error
        let mock_partial_failure = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"jsonrpc":"2.0","id":0,"result":"0xabc"},
                    {"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}
                ]"#,
            )
            .expect(1)
            .create_async()
            .await;

        let client = ClientBuilder::default().http(server.url().parse().unwrap());

        let result = mock_retry_policy()
            .call_with_retry(|| async {
                let mut batch = client.new_batch();
                let call1 = batch.add_call::<_, String>("eth_blockNumber", &())?;
                let call2 = batch.add_call::<_, String>("eth_blockNumber", &())?;

                batch.send().await?;

                let result1 = call1.await?;
                let result2 = call2.await?;

                Ok::<_, RpcError<TransportErrorKind>>((result1, result2))
            })
            .await;

        // The batch itself succeeds, but when we await individual results, the second one fails
        // This failure happens after batch.send(), so it won't trigger a retry of the whole batch
        assert!(result.is_err(), "Should fail when individual batch item has permanent error");

        mock_partial_failure.assert();
    }
}
