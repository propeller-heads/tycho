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

use super::{
    backoff,
    telemetry::{self, ReadOutcome},
};

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
        /// The read timeout that elapsed.
        after: Duration,
    },
}

/// The default [`WhitelistReaderSettings::read_timeout`].
pub(super) const WHITELIST_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The timings of a [`whitelist_reader`].
#[derive(Clone, Copy, Debug)]
pub(super) struct WhitelistReaderSettings {
    /// Longest a single read may take. A slower read fails with `FetchVenuesError::Timeout` and
    /// is retried, so a node that accepts the connection and never answers cannot block the first
    /// read or a refresh forever.
    pub read_timeout: Duration,
    /// Cap on the `2^attempt` seconds backoff between a failed read and its retry.
    pub max_backoff: Duration,
    /// How long after a successful read the next one starts.
    pub refresh_interval: Duration,
}

/// Reads the whitelist through `fetch` in a loop and yields every successful read. A failed or
/// slow read (see [`WhitelistReaderSettings`]) is logged once at WARN, counted, and retried after
/// a backoff; nothing is yielded until a read succeeds, so a consumer only ever sees whitelists.
/// The stream never ends.
pub(super) fn whitelist_reader<F, Fut>(
    fetch: F,
    settings: WhitelistReaderSettings,
) -> impl Stream<Item = HashSet<Bytes>> + Send
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<Bytes>, FetchVenuesError>> + Send,
{
    let WhitelistReaderSettings { read_timeout, max_backoff, refresh_interval } = settings;
    stream! {
        let mut attempt: u32 = 0;
        loop {
            let outcome = timeout(read_timeout, fetch())
                .await
                .unwrap_or(Err(FetchVenuesError::Timeout { after: read_timeout }));
            match outcome {
                Ok(venues) => {
                    attempt = 0;
                    telemetry::record_whitelist_read(ReadOutcome::Ok);
                    let venues: HashSet<Bytes> = venues.into_iter().collect();
                    yield venues;
                    sleep(refresh_interval).await;
                }
                Err(error) => {
                    attempt = attempt.saturating_add(1);
                    telemetry::record_whitelist_read(ReadOutcome::Error);
                    let delay = backoff(attempt, max_backoff);
                    tracing::warn!(
                        error = %error,
                        attempt,
                        retry_secs = delay.as_secs_f64(),
                        "PropAMMRouter whitelist read failed; retrying"
                    );
                    sleep(delay).await;
                }
            }
        }
    }
}

/// Reads the router's whitelisted pAMM venues via `eth_call` on the node at `rpc_url`.
///
/// A single read with no timeout or retry; the caller bounds it.
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
    use std::{
        collections::VecDeque,
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use futures::StreamExt;

    use super::{
        super::telemetry::{
            recorded::{counter_value, record_async},
            WHITELIST_READS,
        },
        *,
    };

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

    /// Settings that retry and refresh quickly enough for a test.
    fn fast_settings() -> WhitelistReaderSettings {
        WhitelistReaderSettings {
            read_timeout: Duration::from_secs(1),
            max_backoff: Duration::from_millis(5),
            refresh_interval: Duration::from_millis(20),
        }
    }

    #[test]
    fn reader_retries_failures_and_refreshes_after_success() {
        type ScriptQueue = Arc<Mutex<VecDeque<Result<Vec<Bytes>, FetchVenuesError>>>>;

        let venue = Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap();
        let script: ScriptQueue = Arc::new(Mutex::new(VecDeque::from([
            Err(FetchVenuesError::Call { reason: "first".to_string() }),
            Err(FetchVenuesError::Call { reason: "second".to_string() }),
            Ok(vec![venue.clone()]),
            Ok(vec![]),
        ])));
        let fetch = move || {
            let next = script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(vec![]));
            async move { next }
        };
        let ((), snapshot) = record_async(async {
            let reader = whitelist_reader(fetch, fast_settings());
            tokio::pin!(reader);

            // The two failures are retried without yielding; the first item is the first
            // successful read.
            let venues = reader.next().await.expect("never ends");
            assert_eq!(venues, HashSet::from([venue]));
            // The reader reads again after `refresh_interval`.
            match tokio::time::timeout(Duration::from_millis(500), reader.next()).await {
                Ok(Some(venues)) => assert!(venues.is_empty()),
                other => panic!("expected a refresh, got {other:?}"),
            }
        });
        assert_eq!(counter_value(&snapshot, WHITELIST_READS, &[("outcome", "error")]), 2);
        assert_eq!(counter_value(&snapshot, WHITELIST_READS, &[("outcome", "ok")]), 2);
    }

    #[test]
    fn reader_fails_a_read_that_never_resolves() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<Vec<Bytes>, FetchVenuesError>>()
            }
        };
        let ((), snapshot) = record_async(async {
            let settings = WhitelistReaderSettings {
                read_timeout: Duration::from_millis(30),
                refresh_interval: Duration::from_secs(60),
                ..fast_settings()
            };
            let reader = whitelist_reader(fetch, settings);
            tokio::pin!(reader);
            // Every read times out, so nothing is ever yielded.
            assert!(tokio::time::timeout(Duration::from_millis(300), reader.next())
                .await
                .is_err());
        });
        // Two or more calls prove the read is bounded and retried.
        assert!(calls.load(Ordering::SeqCst) >= 2);
        assert!(counter_value(&snapshot, WHITELIST_READS, &[("outcome", "error")]) >= 2);
    }

    #[test]
    fn failed_reads_are_counted_and_never_yielded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<Vec<Bytes>, FetchVenuesError>(FetchVenuesError::Call {
                        reason: "connection refused".to_string(),
                    })
                }
            }
        };
        let ((), snapshot) = record_async(async {
            let reader = whitelist_reader(fetch, fast_settings());
            tokio::pin!(reader);
            assert!(tokio::time::timeout(Duration::from_millis(100), reader.next())
                .await
                .is_err());
        });
        // Each failed call is counted before the reader sleeps, and the timeout can only
        // interrupt the sleep, so the count matches the calls exactly.
        let calls = calls.load(Ordering::SeqCst);
        assert!(calls >= 2, "only {calls} read attempts");
        assert_eq!(
            counter_value(&snapshot, WHITELIST_READS, &[("outcome", "error")]),
            calls as u64
        );
    }
}
