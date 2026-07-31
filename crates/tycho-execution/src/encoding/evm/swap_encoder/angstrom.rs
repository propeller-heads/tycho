//! Angstrom pool unlock attestations.
//!
//! Angstrom's Uniswap V4 pools start every block locked, so a swap that wants to trade against
//! one in the same block has to carry an attestation signed by the current Angstrom leader as
//! `hookData`. An attestation is scoped to a block number and carries nothing about the swap
//! itself, so a single fetched window serves every swap, pool and route.
//!
//! Attestations are therefore fetched by a background thread into a process-wide cache and read
//! from that cache while encoding, which keeps the Angstrom API's latency off the encoding path.
//! The API returns a window covering the next `ANGSTROM_BLOCKS_IN_FUTURE` blocks; the executor
//! picks the entry matching `block.number` on chain and ignores the rest.

use std::{
    env,
    sync::{Arc, OnceLock, PoisonError, RwLock},
    thread,
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::encoding::{
    errors::EncodingError,
    evm::constants::{
        ANGSTROM_API_TIMEOUT, ANGSTROM_ATTESTATION_MAX_AGE, ANGSTROM_ATTESTATION_REFRESH_INTERVAL,
        ANGSTROM_ATTESTATION_SIZE, ANGSTROM_DEFAULT_API_URL, ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE,
    },
};

static CACHE: OnceLock<Arc<AttestationCache>> = OnceLock::new();

/// Fetches attestations for the current block and the next `ANGSTROM_BLOCKS_IN_FUTURE` blocks.
type WindowFetcher = Box<dyn Fn() -> Result<AttestationResponse, EncodingError> + Send + Sync>;

/// How to fetch an attestation window, or why it cannot be fetched, plus the last one fetched.
///
/// A refresh replaces the whole window rather than appending to it, so this holds one window of
/// `ANGSTROM_BLOCKS_IN_FUTURE + 1` attestations and does not grow with uptime.
pub(crate) struct AttestationCache {
    fetcher: Result<WindowFetcher, String>,
    window: RwLock<Option<CachedWindow>>,
}

impl AttestationCache {
    /// Returns the process-wide cache, starting its refresh thread on the first call.
    ///
    /// The thread is only started when the Angstrom API is configured, and then runs for the
    /// lifetime of the process. Call this as early as possible so that the first encoded swap
    /// already finds a warm cache.
    pub(crate) fn global() -> &'static Arc<Self> {
        CACHE.get_or_init(|| {
            let fetcher = ApiConfig::from_env().map(ApiConfig::into_fetcher);
            let cache = Arc::new(Self { fetcher, window: RwLock::new(None) });
            Arc::clone(&cache).spawn_refresher();
            cache
        })
    }

    /// Returns the attestation bytes to pass as `hookData` for a swap on an Angstrom pool.
    ///
    /// Costs no network access while the background refresh is healthy. A window older than
    /// `ANGSTROM_ATTESTATION_MAX_AGE`, or a cache that has never been filled, falls back to a
    /// single blocking fetch so that encoding still succeeds at the cost of one API round trip.
    ///
    /// Returns an error if the fallback fetch fails, or if it is needed while the Angstrom API
    /// is unconfigured.
    pub(crate) fn hook_data(&self) -> Result<Vec<u8>, EncodingError> {
        let cached = self
            .window
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(window) = cached.as_ref() {
            if window.fetched_at.elapsed() <= ANGSTROM_ATTESTATION_MAX_AGE {
                return Ok(window.encoded.clone());
            }
        }
        drop(cached);

        if let Err(reason) = &self.fetcher {
            return Err(EncodingError::FatalError(reason.clone()));
        }

        warn!("Angstrom attestation cache is cold or stale, fetching while encoding");
        self.refresh()
    }

    /// Fetches the current attestation window into the cache and returns it.
    fn refresh(&self) -> Result<Vec<u8>, EncodingError> {
        let fetcher = self
            .fetcher
            .as_ref()
            .map_err(|reason| EncodingError::FatalError(reason.clone()))?;

        let encoded = encode_attestations(&fetcher()?)?;
        *self
            .window
            .write()
            .unwrap_or_else(PoisonError::into_inner) =
            Some(CachedWindow { encoded: encoded.clone(), fetched_at: Instant::now() });

        Ok(encoded)
    }

    /// Spawns the thread that keeps the cache warm, leaving the previous window in place
    /// whenever a refresh fails.
    ///
    /// A dedicated thread is used rather than a `tokio` task so that the encoder works whether
    /// or not its consumer runs a runtime, and because the blocking API client cannot be driven
    /// from inside one.
    fn spawn_refresher(self: Arc<Self>) {
        if self.fetcher.is_err() {
            return;
        }

        let spawned = thread::Builder::new()
            .name("angstrom-attestations".to_string())
            .spawn(move || loop {
                if let Err(e) = self.refresh() {
                    warn!("Angstrom attestation refresh failed: {e}");
                }
                thread::sleep(ANGSTROM_ATTESTATION_REFRESH_INTERVAL);
            });

        if let Err(e) = spawned {
            warn!(
                "Failed to start the Angstrom attestation refresher: {e}. Attestations will be \
                 fetched while encoding instead."
            );
        }
    }
}

/// An encoded attestation window and the time it was fetched.
struct CachedWindow {
    encoded: Vec<u8>,
    fetched_at: Instant,
}

/// A single Angstrom API client, reused across fetches to keep its connection pool warm.
struct ApiConfig {
    client: reqwest::blocking::Client,
    url: String,
    key: String,
    blocks_in_future: u64,
}

impl ApiConfig {
    /// Reads the Angstrom API configuration from the environment.
    ///
    /// Returns the reason Angstrom swaps cannot be encoded when `ANGSTROM_API_KEY` is unset,
    /// which is how consumers that do not route over Angstrom opt out, or when
    /// `ANGSTROM_BLOCKS_IN_FUTURE` is set to something that is not a block count.
    fn from_env() -> Result<Self, String> {
        let key = env::var("ANGSTROM_API_KEY").map_err(|_| {
            "ANGSTROM_API_KEY environment variable is required for Angstrom swaps".to_string()
        })?;

        let client = reqwest::blocking::Client::builder()
            .timeout(ANGSTROM_API_TIMEOUT)
            .build()
            .map_err(|e| format!("Failed to build the Angstrom API client: {e}"))?;

        let url =
            env::var("ANGSTROM_API_URL").unwrap_or_else(|_| ANGSTROM_DEFAULT_API_URL.to_string());
        let blocks_in_future = match env::var("ANGSTROM_BLOCKS_IN_FUTURE") {
            Ok(blocks) => blocks.parse().map_err(|e| {
                format!("ANGSTROM_BLOCKS_IN_FUTURE is set to '{blocks}', not a block count: {e}")
            })?,
            Err(_) => ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE,
        };

        Ok(Self { client, url, key, blocks_in_future })
    }

    /// Moves the client behind the cache's fetch, keeping its connection pool warm across
    /// refreshes.
    fn into_fetcher(self) -> WindowFetcher {
        Box::new(move || self.fetch_off_runtime())
    }

    /// Fetches the current attestation window on a thread of its own.
    ///
    /// The blocking client cannot be driven from inside an async runtime, and callers of
    /// `encode_swap` usually are.
    fn fetch_off_runtime(&self) -> Result<AttestationResponse, EncodingError> {
        thread::scope(|scope| {
            scope
                .spawn(|| self.fetch())
                .join()
                .map_err(|_| {
                    EncodingError::RecoverableError(
                        "Angstrom attestation fetch panicked".to_string(),
                    )
                })
        })?
    }

    fn fetch(&self) -> Result<AttestationResponse, EncodingError> {
        let response = self
            .client
            .post(&self.url)
            .header("accept", "application/json")
            .header("X-Api-Key", &self.key)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "blocks_in_future": self.blocks_in_future }))
            .send()
            .map_err(|e| {
                EncodingError::RecoverableError(format!("Failed to fetch attestations: {e}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(EncodingError::RecoverableError(format!(
                "Angstrom API request failed with status {status}: {error_text}"
            )));
        }

        let response: AttestationResponse = response.json().map_err(|e| {
            EncodingError::RecoverableError(format!("Failed to parse attestation response: {e}"))
        })?;

        if !response.success {
            return Err(EncodingError::RecoverableError(
                "Angstrom API returned success=false".to_string(),
            ));
        }

        Ok(response)
    }
}

/// Encodes attestations into the `hookData` layout the Uniswap V4 executor expects.
///
/// Every attestation takes exactly `8 + ANGSTROM_ATTESTATION_SIZE` bytes: a big endian block
/// number followed by the attestation itself. Entries for blocks that have already passed are
/// harmless, since the executor selects the entry matching `block.number`.
///
/// Returns an error if the window is empty, since encoding it would produce a swap whose pool
/// stays locked, or if an attestation is not `ANGSTROM_ATTESTATION_SIZE` bytes long, which the
/// executor would reject on chain.
fn encode_attestations(response: &AttestationResponse) -> Result<Vec<u8>, EncodingError> {
    if response.attestations.is_empty() {
        return Err(EncodingError::RecoverableError(
            "Angstrom API returned an empty attestation window".to_string(),
        ));
    }

    let mut encoded =
        Vec::with_capacity(response.attestations.len() * (8 + ANGSTROM_ATTESTATION_SIZE));
    for data in &response.attestations {
        let attestation_hex = data
            .attestation
            .strip_prefix("0x")
            .unwrap_or(&data.attestation);

        let attestation = hex::decode(attestation_hex).map_err(|e| {
            EncodingError::FatalError(format!(
                "Failed to decode Angstrom attestation for block {}: {}",
                data.block_number, e
            ))
        })?;

        if attestation.len() != ANGSTROM_ATTESTATION_SIZE {
            return Err(EncodingError::FatalError(format!(
                "Angstrom attestation for block {} is {} bytes, expected {}",
                data.block_number,
                attestation.len(),
                ANGSTROM_ATTESTATION_SIZE
            )));
        }

        encoded.extend_from_slice(&data.block_number.to_be_bytes());
        encoded.extend_from_slice(&attestation);
    }

    Ok(encoded)
}

/// Response from the Angstrom attestation API.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AttestationResponse {
    pub(crate) success: bool,
    pub(crate) attestations: Vec<AttestationData>,
}

/// The attestation unlocking Angstrom's pools for one block.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AttestationData {
    #[serde(rename = "blockNumber")]
    pub(crate) block_number: u64,
    #[serde(rename = "unlockData")]
    pub(crate) attestation: String,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::*;

    /// Two attestations retrieved from the Angstrom API in the past.
    fn attestation_response() -> AttestationResponse {
        AttestationResponse {
            success: true,
            attestations: vec![
                AttestationData {
                    block_number: 12345678,
                    attestation: "0xd437f3372f3add2c2bc3245e6bd6f9c202e61bb367c79a6f740c7c12ca9c54a760bead943516fafaf8a4fe65a907b31d45c2ab4b525f9f32ec2771033e0832359ceb2e38d9288a755c7c366ce889b0df24b5821b1c".to_string(),
                },
                AttestationData {
                    block_number: 12345679,
                    attestation: "0xd437f3372f3add2c2bc3245e6bd6f9c202e61bb30c337ddae661e68cc6986c7784cd0aaec455b1f7514b6cd91bff26f002ce7cb42b3b1e2092ea4d1c1fb1e0641cbccfb021b31de25462f25b355cc99c7d509cdc1b".to_string(),
                },
            ],
        }
    }

    /// A cache serving `fetcher` instead of the Angstrom API.
    fn cache_with(fetcher: WindowFetcher, window: Option<CachedWindow>) -> AttestationCache {
        AttestationCache { fetcher: Ok(fetcher), window: RwLock::new(window) }
    }

    /// A fetch answering `attestation_response()` from memory, alongside its call count.
    fn counted_fetch() -> (WindowFetcher, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        let fetcher: WindowFetcher = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(attestation_response())
        });

        (fetcher, calls)
    }

    /// A cache belonging to a consumer that never configured the Angstrom API.
    fn unconfigured_cache() -> AttestationCache {
        AttestationCache { fetcher: Err("no API key".to_string()), window: RwLock::new(None) }
    }

    /// A fetch standing in for an Angstrom API that cannot be reached.
    fn failing_fetch() -> WindowFetcher {
        Box::new(|| Err(EncodingError::RecoverableError("the API is down".to_string())))
    }

    /// The window `counted_fetch` puts in the cache, as `hook_data` returns it.
    fn fetched_window() -> Vec<u8> {
        encode_attestations(&attestation_response()).unwrap()
    }

    fn window_fetched_ago(age: Duration) -> Option<CachedWindow> {
        let fetched_at = Instant::now()
            .checked_sub(age)
            .expect("the monotonic clock is past the requested age");
        Some(CachedWindow { encoded: vec![1, 2, 3], fetched_at })
    }

    fn stale() -> Option<CachedWindow> {
        window_fetched_ago(ANGSTROM_ATTESTATION_MAX_AGE + Duration::from_secs(1))
    }

    #[test]
    fn test_fresh_window_is_served_without_a_fetch() {
        let (fetch, calls) = counted_fetch();
        let cache = cache_with(fetch, window_fetched_ago(Duration::ZERO));

        assert_eq!(cache.hook_data().unwrap(), vec![1, 2, 3]);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_stale_window_is_replaced_by_a_fetch() {
        let (fetch, calls) = counted_fetch();
        let cache = cache_with(fetch, stale());

        assert_eq!(cache.hook_data().unwrap(), fetched_window());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_cold_cache_is_filled_by_a_fetch() {
        let (fetch, calls) = counted_fetch();
        let cache = cache_with(fetch, None);

        assert_eq!(cache.hook_data().unwrap(), fetched_window());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_fetched_window_is_served_to_the_next_caller() {
        let (fetch, calls) = counted_fetch();
        let cache = cache_with(fetch, None);

        cache.hook_data().unwrap();

        assert_eq!(cache.hook_data().unwrap(), fetched_window());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_unconfigured_cache_reports_the_configuration_error() {
        let err = unconfigured_cache()
            .hook_data()
            .unwrap_err();

        assert_eq!(err, EncodingError::FatalError("no API key".to_string()));
    }

    #[test]
    fn test_failed_refresh_keeps_the_previous_window() {
        let cache = cache_with(failing_fetch(), window_fetched_ago(Duration::ZERO));

        let err = cache.refresh().unwrap_err();

        assert_eq!(err, EncodingError::RecoverableError("the API is down".to_string()));
        assert_eq!(cache.hook_data().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn test_failed_fetch_surfaces_on_a_stale_window() {
        let cache = cache_with(failing_fetch(), stale());

        let err = cache.hook_data().unwrap_err();

        assert_eq!(err, EncodingError::RecoverableError("the API is down".to_string()));
    }

    #[test]
    fn test_unparsable_blocks_in_future_is_rejected() {
        env::set_var("ANGSTROM_API_KEY", "test-key");
        env::set_var("ANGSTROM_BLOCKS_IN_FUTURE", "five");

        let err = ApiConfig::from_env().err();

        env::remove_var("ANGSTROM_BLOCKS_IN_FUTURE");
        env::remove_var("ANGSTROM_API_KEY");

        let err = err.expect("an unparsable block count must be rejected");
        assert!(
            err.contains("ANGSTROM_BLOCKS_IN_FUTURE is set to 'five'"),
            "unexpected error: {err}"
        );
    }

    /// Reads the current block number over JSON-RPC, using `RPC_URL`.
    fn current_block_number() -> u64 {
        let url = env::var("RPC_URL").expect("RPC_URL must be set");
        let response: serde_json::Value = reqwest::blocking::Client::new()
            .post(url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "method": "eth_blockNumber", "params": [], "id": 1
            }))
            .send()
            .expect("the RPC must answer")
            .json()
            .expect("the RPC must return JSON");

        let block_number = response["result"]
            .as_str()
            .expect("eth_blockNumber must return a result");

        u64::from_str_radix(block_number.trim_start_matches("0x"), 16)
            .expect("eth_blockNumber must return a hex number")
    }

    /// Guards the assumption the whole cache rests on: that a fetched window brackets the block
    /// the transaction will land in. Nothing in the encoder checks the block numbers it encodes,
    /// so an API that started returning only past blocks would otherwise go unnoticed.
    #[test]
    #[ignore] // Performs real Angstrom API and RPC calls
    fn test_live_window_brackets_the_current_block() {
        let api = ApiConfig::from_env().expect("ANGSTROM_API_KEY must be set");

        // Fetch the window first: a block arriving before the RPC call keeps the head inside the
        // window, whereas one arriving after would push the window past the head.
        let response = api
            .fetch_off_runtime()
            .expect("the Angstrom API must answer");
        let head = current_block_number();

        let mut blocks: Vec<u64> = response
            .attestations
            .iter()
            .map(|attestation| attestation.block_number)
            .collect();
        blocks.sort_unstable();

        assert_eq!(
            blocks.len() as u64,
            ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE + 1,
            "expected the current block plus {ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE} future ones, \
             got {blocks:?}"
        );
        assert!(
            blocks
                .windows(2)
                .all(|pair| pair[1] == pair[0] + 1),
            "attested blocks are not consecutive: {blocks:?}"
        );
        assert!(
            blocks.contains(&head),
            "window {blocks:?} does not cover the current block {head}"
        );
        assert!(
            blocks
                .last()
                .is_some_and(|last| *last > head),
            "window {blocks:?} leaves no future block for the transaction to land in, head {head}"
        );
    }

    #[test]
    fn test_encode_attestations_format() {
        let encoded = encode_attestations(&attestation_response()).unwrap();

        // 8 bytes block number + 85 bytes attestation, twice
        assert_eq!(encoded.len(), 186);
        assert_eq!(
            hex::encode(&encoded),
            String::from(concat!(
                // First attestation block number (12345678)
                "0000000000bc614e",
                // First attestation data
                "d437f3372f3add2c2bc3245e6bd6f9c202e61bb367c79a6f740c7c12ca9c54a760bead943516fafaf8a4fe65a907b31d45c2ab4b525f9f32ec2771033e0832359ceb2e38d9288a755c7c366ce889b0df24b5821b1c",
                // Second attestation block number (12345679)
                "0000000000bc614f",
                // Second attestation data
                "d437f3372f3add2c2bc3245e6bd6f9c202e61bb30c337ddae661e68cc6986c7784cd0aaec455b1f7514b6cd91bff26f002ce7cb42b3b1e2092ea4d1c1fb1e0641cbccfb021b31de25462f25b355cc99c7d509cdc1b"
            ))
        );
    }

    #[test]
    fn test_encode_attestations_rejects_empty_window() {
        let empty = AttestationResponse { success: true, attestations: vec![] };

        let err = encode_attestations(&empty).unwrap_err();

        assert_eq!(
            err,
            EncodingError::RecoverableError(
                "Angstrom API returned an empty attestation window".to_string()
            )
        );
    }

    #[test]
    fn test_encode_attestations_rejects_wrong_size() {
        let response = AttestationResponse {
            success: true,
            attestations: vec![AttestationData {
                block_number: 12345678,
                attestation: "0xdeadbeef".to_string(),
            }],
        };

        let err = encode_attestations(&response).unwrap_err();

        assert!(format!("{err}").contains("is 4 bytes, expected 85"));
    }

    #[test]
    fn test_encode_attestations_rejects_invalid_hex() {
        let response = AttestationResponse {
            success: true,
            attestations: vec![AttestationData {
                block_number: 12345678,
                attestation: "0xnothex".to_string(),
            }],
        };

        let err = encode_attestations(&response).unwrap_err();

        assert!(format!("{err}").contains("Failed to decode Angstrom attestation for block"));
    }
}
