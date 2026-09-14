//! Reads the venue whitelist of Titan's PropAMMRouter.
//!
//! Venues on the whitelist may be served under the `propammfallback:` protocol family, which
//! executes their swaps through the router instead of the venue directly, so a stale maker
//! quote falls back to a single-hop Uniswap V3 pool instead of reverting the route.

use std::{collections::HashSet, future::Future, time::Duration};

use alloy::{
    network::Ethereum,
    primitives::{address, Address, TxKind},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use async_stream::stream;
use futures::Stream;
use tokio::time::{sleep, timeout};
use tycho_common::Bytes;

use super::{telemetry, titan::backoff};

/// Titan's PropAMMRouter deployment on Ethereum mainnet: written by LambdaClass, behind a UUPS
/// proxy so upgrades keep the address.
///
/// Must match `tycho-execution`'s `PropAMMFallbackExecutor.PROPAMM_ROUTER`.
/// <https://github.com/lambdaclass/propamm-router-contracts>
pub const FALLBACK_ROUTER_ADDRESS: Address = address!("4DdF368080CD7946db5b459aD591c350158175e1");

sol! {
    /// The whitelist accessor of the PropAMMRouter. The swap surface the executor uses lives in
    /// `tycho-execution`'s `IPropAMMRouter.sol`.
    function getWhitelistedVenues() external view returns (address[] memory venues);
}

/// Error reading the PropAMMRouter's venue whitelist.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FetchVenuesError {
    /// The RPC URL could not be parsed.
    #[error("invalid RPC URL {url:?}: {reason}")]
    InvalidUrl {
        /// The URL that failed to parse.
        url: String,
        /// The parse error.
        reason: String,
    },
    /// The `eth_call` failed or returned undecodable data.
    #[error("getWhitelistedVenues call to the PropAMMRouter failed: {reason}")]
    Call {
        /// Underlying transport or ABI decoding error.
        reason: String,
    },
    /// The `eth_call` did not resolve within the read timeout.
    #[error("getWhitelistedVenues call to the PropAMMRouter timed out after {after:?}")]
    Timeout {
        /// The bound that elapsed.
        after: Duration,
    },
}

/// The outcome of one whitelist read, as delivered to the tracker.
#[derive(Debug)]
pub(super) enum RouterVenuesRead {
    Ok(HashSet<Bytes>),
    Failed(FetchVenuesError),
}

/// Longest a single whitelist read may take. A read that hangs past this is a failure like any
/// other, so a node that accepts the connection and never answers cannot keep the stream in
/// its pre-whitelist state, or freeze a stale whitelist, forever.
pub(super) const WHITELIST_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Reads the whitelist through `fetch`, yielding every outcome: a read is bounded by
/// `read_timeout`; failures (including timeouts) are retried with exponential backoff
/// (`2^attempt` seconds, capped at `max_backoff`); successes are repeated every
/// `refresh_interval`. Never ends. Logs one WARN per failure; consumers of the outcomes should
/// not log them again.
pub(super) fn router_venues_reader<F, Fut>(
    fetch: F,
    read_timeout: Duration,
    max_backoff: Duration,
    refresh_interval: Duration,
) -> impl Stream<Item = RouterVenuesRead> + Send
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<Bytes>, FetchVenuesError>> + Send,
{
    stream! {
        let mut attempt: u32 = 0;
        loop {
            let outcome = match timeout(read_timeout, fetch()).await {
                Ok(outcome) => outcome,
                Err(_elapsed) => Err(FetchVenuesError::Timeout { after: read_timeout }),
            };
            match outcome {
                Ok(venues) => {
                    attempt = 0;
                    telemetry::whitelist_read("ok");
                    let venues: HashSet<Bytes> = venues.into_iter().collect();
                    yield RouterVenuesRead::Ok(venues);
                    sleep(refresh_interval).await;
                }
                Err(error) => {
                    attempt = attempt.saturating_add(1);
                    telemetry::whitelist_read("error");
                    let delay = backoff(attempt, max_backoff);
                    tracing::warn!(
                        error = %error,
                        attempt,
                        retry_secs = delay.as_secs_f64(),
                        "PropAMMRouter whitelist read failed; retrying"
                    );
                    yield RouterVenuesRead::Failed(error);
                    sleep(delay).await;
                }
            }
        }
    }
}

/// Reads the router's whitelisted pAMM venues via `eth_call` on the node at `rpc_url`.
///
/// Read at startup with retries and refreshed periodically by `router_venues_reader`, each
/// read bounded by `WHITELIST_READ_TIMEOUT`. The whitelist is governance-gated and changes
/// rarely, and renaming a running component's protocol system would churn every consumer's
/// component set.
///
/// # Errors
///
/// Returns [`FetchVenuesError::InvalidUrl`] if `rpc_url` does not parse, and
/// [`FetchVenuesError::Call`] if the `eth_call` fails or returns undecodable data.
pub async fn fetch_fallback_router_venues(rpc_url: &str) -> Result<Vec<Bytes>, FetchVenuesError> {
    let url: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| FetchVenuesError::InvalidUrl {
            url: rpc_url.to_string(),
            reason: format!("{e}"),
        })?;
    let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(url);
    let response = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(FALLBACK_ROUTER_ADDRESS)),
            input: getWhitelistedVenuesCall {}
                .abi_encode()
                .into(),
            ..Default::default()
        })
        .await
        .map_err(|e| FetchVenuesError::Call { reason: e.to_string() })?;
    let venues = getWhitelistedVenuesCall::abi_decode_returns(&response).map_err(|e| {
        FetchVenuesError::Call { reason: format!("failed to decode response: {e}") }
    })?;
    Ok(venues
        .into_iter()
        .map(|venue| Bytes::from(venue.as_slice().to_vec()))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[tokio::test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    async fn test_fetch_fallback_router_venues_against_mainnet() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set for network tests");

        let venues = fetch_fallback_router_venues(&rpc_url)
            .await
            .expect("whitelist read should succeed");

        // FermiSwap is whitelisted on the live router.
        let fermiswap =
            Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").expect("valid address");
        assert!(venues.contains(&fermiswap), "expected FermiSwap in {venues:?}");
    }

    #[tokio::test]
    async fn test_fetch_fallback_router_venues_invalid_url() {
        let result = fetch_fallback_router_venues("not a url").await;
        assert!(matches!(result, Err(FetchVenuesError::InvalidUrl { .. })));
    }

    /// Reading the whitelist from a different router than the executor calls would let a venue
    /// be served under `propammfallback:` that the executed router rejects.
    #[test]
    fn test_router_address_matches_the_executor() {
        let executor = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tycho-execution/contracts/src/executors/PropAMMFallbackExecutor.sol");
        let source = std::fs::read_to_string(&executor)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", executor.display()));

        let address = FALLBACK_ROUTER_ADDRESS.to_string();
        assert!(source.contains(&address), "PropAMMFallbackExecutor.sol does not use {address}");
    }

    #[tokio::test]
    async fn reader_retries_failures_and_refreshes_after_success() {
        use std::{
            collections::VecDeque,
            sync::{Arc, Mutex},
            time::Duration,
        };

        use futures::StreamExt;

        type ScriptQueue = Arc<Mutex<VecDeque<Result<Vec<Bytes>, FetchVenuesError>>>>;

        let venue = Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap();
        let script: ScriptQueue = Arc::new(Mutex::new(VecDeque::from([
            Err(FetchVenuesError::Call { reason: "first".to_string() }),
            Err(FetchVenuesError::Call { reason: "second".to_string() }),
            Ok(vec![venue.clone()]),
            Ok(vec![]),
        ])));
        let fetch = {
            let script = script.clone();
            move || {
                let next = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok(vec![]));
                async move { next }
            }
        };
        let reader = router_venues_reader(
            fetch,
            Duration::from_secs(1),
            Duration::from_millis(5),
            Duration::from_millis(20),
        );
        tokio::pin!(reader);

        assert!(matches!(reader.next().await, Some(RouterVenuesRead::Failed(_))));
        assert!(matches!(reader.next().await, Some(RouterVenuesRead::Failed(_))));
        match reader.next().await {
            Some(RouterVenuesRead::Ok(venues)) => assert_eq!(venues.len(), 1),
            other => panic!("expected a successful read, got {other:?}"),
        }
        // Refreshed after the interval.
        match tokio::time::timeout(Duration::from_millis(500), reader.next()).await {
            Ok(Some(RouterVenuesRead::Ok(venues))) => assert!(venues.is_empty()),
            other => panic!("expected a refresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reader_fails_a_read_that_never_resolves() {
        use std::{
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
            time::Duration,
        };

        use futures::StreamExt;

        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<Vec<Bytes>, FetchVenuesError>>()
            }
        };
        let reader = router_venues_reader(
            fetch,
            Duration::from_millis(30),
            Duration::from_millis(5),
            Duration::from_secs(60),
        );
        tokio::pin!(reader);

        // Two consecutive timeouts prove the read is bounded and retried.
        for _ in 0..2 {
            match tokio::time::timeout(Duration::from_millis(500), reader.next()).await {
                Ok(Some(RouterVenuesRead::Failed(FetchVenuesError::Timeout { after }))) => {
                    assert_eq!(after, Duration::from_millis(30));
                }
                other => panic!("expected a timeout, got {other:?}"),
            }
        }
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    // `metrics::with_local_recorder` takes a sync closure, so this test drives its own
    // current-thread runtime instead of using `#[tokio::test]`.
    #[test]
    fn whitelist_read_failures_are_counted() {
        use std::time::Duration;

        use futures::StreamExt;
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{counter_value, snapshot_map},
            WHITELIST_READS,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let fetch = || async {
                    Err::<Vec<Bytes>, FetchVenuesError>(FetchVenuesError::Call {
                        reason: "connection refused".to_string(),
                    })
                };
                let reader = router_venues_reader(
                    fetch,
                    Duration::from_millis(30),
                    Duration::from_millis(5),
                    Duration::from_secs(60),
                );
                tokio::pin!(reader);
                for _ in 0..2 {
                    match reader.next().await {
                        Some(RouterVenuesRead::Failed(FetchVenuesError::Call { reason })) => {
                            assert_eq!(reason, "connection refused");
                        }
                        other => panic!("expected a failed read, got {other:?}"),
                    }
                }
            });
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert!(counter_value(&snapshot, WHITELIST_READS, &[("outcome", "error")]) >= 2);
    }
}
