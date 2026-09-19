//! Everything that talks to the `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::num::NonZeroUsize;

use futures::{stream, StreamExt};
use reqwest::Client;
use tokio::time::{timeout, Duration};
use tracing::debug;
use tycho_common::Bytes;

use crate::{
    evm::protocol::utils::bytes_to_address,
    pamm::protocols::metric::models::{
        MetricBidAskResponse, MetricMetadata, PaginatedMetadataResponse,
    },
    snapshot_feed::{errors::FeedError, http::fetch_json},
};

/// Page size for the paginated metadata endpoint. The API clamps `count` to `[1, 500]`.
const METADATA_PAGE_SIZE: u32 = 500;

/// Currency the metadata endpoint values `tvlFiat` in; the threshold in `BookFeedConfig` is USD.
const TVL_FIAT_CURRENCY: &str = "USD";

/// Deadline for one pool's bid/ask request. A pool that misses it is left out of the current
/// book; the poll as a whole still succeeds with the other pools.
const BID_ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// How many pools' bid/ask requests are in flight at once unless the builder says otherwise.
///
/// A poll's worst case is the metadata fetch plus `ceil(pools / this) * BID_ASK_TIMEOUT`, and it
/// has to stay inside the feed loop's whole-poll `request_timeout`, so this wants to be at least
/// the number of pools the venue serves. Sixteen is twice the eight Metric serves on Base today.
pub const DEFAULT_BID_ASK_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(16).unwrap();

/// Reads Metric's pool metadata and per-pool quotes. Unlike the RFQ venues' clients this one
/// never requests a quote — a Metric pool is executed against directly — so no state holds it
/// and it is not serialized; it is the feed's half of the integration alone.
///
/// `Debug` output omits the API key.
#[derive(derive_more::Debug, Clone)]
pub struct MetricClient {
    metadata_endpoint: String,
    /// Prefix ending at /public/v1/evm/{chain_id}; pool-specific endpoints are derived from it.
    chain_endpoint: String,
    /// Bearer trading key required by the authenticated endpoints (`bid_ask`).
    #[debug(skip)]
    api_key: String,
    bid_ask_concurrency: NonZeroUsize,
    http: Client,
}

impl MetricClient {
    pub fn new(
        metadata_endpoint: String,
        chain_endpoint: String,
        api_key: String,
        bid_ask_concurrency: NonZeroUsize,
    ) -> Self {
        MetricClient {
            metadata_endpoint,
            chain_endpoint,
            api_key,
            bid_ask_concurrency,
            http: Client::new(),
        }
    }

    /// Every pool the venue lists, by paging through the metadata endpoint until the API reports
    /// no next page.
    pub async fn fetch_metadata(&self) -> Result<Vec<MetricMetadata>, FeedError> {
        let mut pools = Vec::new();
        let mut offset: u64 = 0;
        let mut pages: u32 = 0;

        loop {
            let request = self
                .http
                .get(&self.metadata_endpoint)
                .header("accept", "application/json")
                // `include24h=true` is required for the top-level `tvlFiat` field; without it the
                // API omits TVL and every pool would fall below any non-zero threshold. `fiat`
                // pins the currency `tvlFiat` is valued in so it matches `min_tvl_usd`; the API
                // rejects anything but USD today, so a change there surfaces as an HTTP error
                // instead of a silently re-denominated threshold.
                .query(&[
                    ("count", METADATA_PAGE_SIZE.to_string()),
                    ("offset", offset.to_string()),
                    ("include24h", "true".to_string()),
                    ("fiat", TVL_FIAT_CURRENCY.to_string()),
                ]);
            let page: PaginatedMetadataResponse = fetch_json(request, "Metric metadata").await?;
            let page_len = page.data.len();
            pools.extend(page.data);
            pages += 1;

            // Stop when the API reports the last page, returns nothing, or fails to advance the
            // offset (defensive guard against an infinite loop).
            match page.next_offset {
                Some(next) if page_len > 0 && next > offset => offset = next,
                _ => break,
            }
        }

        debug!(pools = pools.len(), pages, "fetched pool metadata");
        Ok(pools)
    }

    /// The current quote for each pool, paired with the pool it belongs to and in whatever order
    /// the answers arrive.
    ///
    /// Metric prices one pool per request, so asking for them one at a time would make this grow
    /// with the venue's pool count until it exceeds the feed loop's whole-poll deadline — which
    /// discards every pool already fetched, not just the slow one. At most
    /// `bid_ask_concurrency` requests are in flight.
    pub async fn fetch_bid_ask_each(
        &self,
        pools: Vec<MetricMetadata>,
    ) -> Vec<(MetricMetadata, Result<MetricBidAskResponse, FeedError>)> {
        stream::iter(pools)
            .map(|pool| async {
                let bid_ask = self
                    .fetch_bid_ask(&pool.pool_address)
                    .await;
                (pool, bid_ask)
            })
            .buffer_unordered(self.bid_ask_concurrency.get())
            .collect()
            .await
    }

    async fn fetch_bid_ask(&self, pool: &Bytes) -> Result<MetricBidAskResponse, FeedError> {
        let pool = bytes_to_address(pool)
            .map_err(|e| FeedError::Parsing(e.to_string()))?
            .to_checksum(None);
        let request = self
            .http
            .get(format!("{}/{pool}/bid_ask", self.chain_endpoint))
            .header("accept", "application/json")
            .bearer_auth(&self.api_key);
        timeout(BID_ASK_TIMEOUT, fetch_json(request, "Metric bid/ask"))
            .await
            .map_err(|_| {
                FeedError::Connection(format!(
                    "Metric bid/ask request timed out after {} seconds",
                    BID_ASK_TIMEOUT.as_secs()
                ))
            })?
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::snapshot_feed::http::test_support::spawn_http_server;

    /// A metadata page listing `pools` pools, whose contents these tests do not read.
    fn metadata_page(pools: usize, next_offset: Option<u64>) -> String {
        let data: Vec<_> = (0..pools)
            .map(|n| {
                serde_json::json!({
                    "poolAddress": format!("0x{n:040x}"),
                    "token0": format!("0x{:040x}", 1),
                    "token1": format!("0x{:040x}", 2),
                    "tvlFiat": 1.0,
                })
            })
            .collect();
        serde_json::json!({ "data": data, "nextOffset": next_offset }).to_string()
    }

    fn client(chain_endpoint: &str, api_key: &str) -> MetricClient {
        MetricClient::new(
            format!("{chain_endpoint}/metadata"),
            chain_endpoint.to_string(),
            api_key.to_string(),
            DEFAULT_BID_ASK_CONCURRENCY,
        )
    }

    #[rstest]
    #[case::follows_next_offset_to_the_last_page(vec![(1, Some(500u64)), (1, None)], 2)]
    #[case::stops_when_next_offset_does_not_advance(vec![(1, Some(0)), (1, None)], 1)]
    #[case::stops_on_an_empty_page(vec![(0, Some(500)), (1, None)], 0)]
    #[tokio::test]
    async fn fetch_metadata_follows_next_offset_and_stops_when_it_does_not_advance(
        #[case] pages: Vec<(usize, Option<u64>)>,
        #[case] expected_pools: usize,
    ) {
        // Page `i` is served at offset `i * 500`.
        let bodies: Vec<String> = pages
            .iter()
            .map(|(pools, next_offset)| metadata_page(*pools, *next_offset))
            .collect();
        let server = spawn_http_server(move |target| {
            let offset: usize = target
                .split("offset=")
                .nth(1)?
                .split('&')
                .next()?
                .parse()
                .ok()?;
            bodies
                .get(offset / METADATA_PAGE_SIZE as usize)
                .map(|body| ("200 OK", body.clone()))
        })
        .await;
        let endpoint = server.url();
        let client = client(&endpoint, "key");

        // A broken stop condition polls the same page forever; the timeout turns that into a
        // failure.
        let pools = timeout(Duration::from_secs(5), client.fetch_metadata())
            .await
            .expect("pagination must terminate")
            .unwrap();

        assert_eq!(pools.len(), expected_pools);
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_serves_quotable_pools() {
        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");
        let client = client("https://api.metric.xyz/public/v1/evm/8453", &api_key);
        let metadata = client.fetch_metadata().await.unwrap();
        assert!(!metadata.is_empty());

        let mut last_error = None;
        let mut selected = None;
        for pool in &metadata {
            match client
                .fetch_bid_ask(&pool.pool_address)
                .await
            {
                Ok(bid_ask) if bid_ask.is_quotable() => {
                    selected = Some(bid_ask);
                    break;
                }
                Ok(_) => {}
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        let Some(bid_ask) = selected else {
            panic!(
                "Metric live API returned no quotable bid_ask across {} pools; last error: {:?}",
                metadata.len(),
                last_error
            );
        };
        assert!(bid_ask.bid_price().unwrap() > 0.0);
        assert!(bid_ask.ask_price().unwrap() >= bid_ask.bid_price().unwrap());
    }
}
