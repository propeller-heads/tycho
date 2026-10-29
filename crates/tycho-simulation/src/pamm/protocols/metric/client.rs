//! Everything that talks to the `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::{collections::HashMap, num::NonZeroUsize};

use futures::{stream, StreamExt};
use reqwest::Client;
use tokio::time::{timeout, Duration};
use tracing::debug;
use tycho_common::Bytes;

use crate::{
    evm::protocol::utils::bytes_to_address,
    pamm::protocols::metric::models::{
        MetricApiError, MetricBatchBidAskRequest, MetricBatchBidAskResponse, MetricBidAskResponse,
        MetricMetadata, PaginatedMetadataResponse,
    },
    snapshot_feed::{errors::FeedError, http::fetch_json},
};

/// Whether Metric refused the request because it does not serve the chain.
fn refuses_the_chain(body: &str) -> bool {
    serde_json::from_str::<MetricApiError>(body).is_ok_and(|error| error.refuses_the_chain())
}

/// Metric's public API, where a client talks unless it is given another.
const METRIC_API_URL: &str = "https://api.metric.xyz";

/// Page size for the paginated metadata endpoint. The API clamps `count` to `[1, 500]`.
const METADATA_PAGE_SIZE: u32 = 500;

/// Currency the metadata endpoint values `tvlFiat` in; the threshold in `BookFeedConfig` is USD.
const TVL_FIAT_CURRENCY: &str = "USD";

/// Most pools the batch bid/ask endpoint quotes in one request.
const BID_ASK_BATCH_SIZE: usize = 100;

/// Deadline for one batch bid/ask request. The pools of a batch that misses it are left out of
/// the current book; the poll as a whole still succeeds with the other batches. It is shorter
/// than the feed's whole-poll `request_timeout`, so a hung batch fails here, after the metadata
/// fetch, rather than taking the whole poll with it.
const BID_ASK_TIMEOUT: Duration = Duration::from_secs(3);

/// How many batch bid/ask requests are in flight at once unless the builder says otherwise.
///
/// A poll's worst case is the metadata fetch plus
/// `ceil(pools / (BID_ASK_BATCH_SIZE * this)) * BID_ASK_TIMEOUT`, and it has to stay inside the
/// feed loop's whole-poll `request_timeout`. Metric serves fewer than `BID_ASK_BATCH_SIZE` pools
/// per chain today, so a poll is a single batch.
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
    /// Bearer trading key required by the authenticated endpoints (`bid_ask/batch`).
    #[debug(skip)]
    api_key: String,
    bid_ask_concurrency: NonZeroUsize,
    http: Client,
}

impl MetricClient {
    /// A client for the Metric deployment at `base_url`, serving the chain with `chain_id`.
    /// Every endpoint it requests hangs off that chain's own prefix.
    /// A client for Metric's API on `chain_id`. `base_url` points it somewhere else than the
    /// venue, which is what
    /// [`MetricFeedBuilder::base_url`](super::feed::MetricFeedBuilder::base_url) passes on.
    pub fn new(
        base_url: Option<&str>,
        chain_id: u64,
        api_key: String,
        bid_ask_concurrency: NonZeroUsize,
    ) -> Self {
        let base_url = base_url.unwrap_or(METRIC_API_URL);
        let chain_endpoint = format!("{}/public/v1/evm/{chain_id}", base_url.trim_end_matches('/'));
        MetricClient {
            metadata_endpoint: format!("{chain_endpoint}/metadata"),
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
                ])
                .bearer_auth(&self.api_key);
            let page: PaginatedMetadataResponse =
                fetch_json(request, "Metric metadata", refuses_the_chain).await?;
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
    /// the batches answer.
    ///
    /// Pools are quoted through the batch endpoint, [`BID_ASK_BATCH_SIZE`] per request with at
    /// most `bid_ask_concurrency` requests in flight. A request that fails as a whole fails each
    /// of its pools with the same error.
    pub async fn fetch_bid_ask_each(
        &self,
        pools: Vec<MetricMetadata>,
    ) -> Vec<(MetricMetadata, Result<MetricBidAskResponse, FeedError>)> {
        let batches: Vec<Vec<MetricMetadata>> = pools
            .chunks(BID_ASK_BATCH_SIZE)
            .map(<[MetricMetadata]>::to_vec)
            .collect();
        let answered: Vec<Vec<_>> = stream::iter(batches)
            .map(|batch| self.fetch_bid_ask_batch(batch))
            .buffer_unordered(self.bid_ask_concurrency.get())
            .collect()
            .await;
        answered.into_iter().flatten().collect()
    }

    async fn fetch_bid_ask_batch(
        &self,
        pools: Vec<MetricMetadata>,
    ) -> Vec<(MetricMetadata, Result<MetricBidAskResponse, FeedError>)> {
        let mut quotes = match self.request_bid_ask_batch(&pools).await {
            Ok(quotes) => quotes,
            Err(e) => {
                return pools
                    .into_iter()
                    .map(|pool| (pool, Err(e.clone())))
                    .collect()
            }
        };
        pools
            .into_iter()
            .map(|pool| {
                let quote = quotes
                    .remove(&pool.pool_address)
                    .unwrap_or_else(|| {
                        Err(FeedError::Parsing(format!(
                            "Metric batch bid/ask answered nothing for pool {}",
                            pool.pool_address
                        )))
                    });
                (pool, quote)
            })
            .collect()
    }

    /// Each requested pool's quote or error, by pool address.
    async fn request_bid_ask_batch(
        &self,
        pools: &[MetricMetadata],
    ) -> Result<HashMap<Bytes, Result<MetricBidAskResponse, FeedError>>, FeedError> {
        let pools = pools
            .iter()
            .map(|pool| {
                bytes_to_address(&pool.pool_address).map(|address| address.to_checksum(None))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| FeedError::Parsing(e.to_string()))?;
        let request = self
            .http
            .post(format!("{}/bid_ask/batch", self.chain_endpoint))
            .header("accept", "application/json")
            .bearer_auth(&self.api_key)
            .json(&MetricBatchBidAskRequest { pools });
        let response: MetricBatchBidAskResponse = timeout(
            BID_ASK_TIMEOUT,
            fetch_json(request, "Metric batch bid/ask", refuses_the_chain),
        )
        .await
        .map_err(|_| {
            FeedError::Connection(format!(
                "Metric batch bid/ask request timed out after {BID_ASK_TIMEOUT:?}",
            ))
        })??;
        Ok(response
            .results
            .into_iter()
            .map(|result| (result.pool_address.clone(), result.into_quote()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::models::Chain;

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

    fn client(base_url: &str, chain_id: u64, api_key: &str) -> MetricClient {
        MetricClient::new(
            Some(base_url),
            chain_id,
            api_key.to_string(),
            DEFAULT_BID_ASK_CONCURRENCY,
        )
    }

    /// The metadata endpoint is paged by `offset`; the poll follows `nextOffset` and must stop
    /// on the last page, on an empty page, and on an offset that does not advance (the API
    /// would otherwise be polled forever).
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
        let server = spawn_http_server(move |request| {
            let offset: usize = request
                .target
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
        let client = client(&endpoint, Chain::Ethereum.id(), "key");

        // A broken stop condition polls the same page forever; the timeout turns that into a
        // failure.
        let pools = timeout(Duration::from_secs(5), client.fetch_metadata())
            .await
            .expect("pagination must terminate")
            .unwrap();

        assert_eq!(pools.len(), expected_pools);
    }

    // MegaETH (4326) is omitted: Metric answers for its chain id but lists no pools on it.
    #[rstest]
    #[case::ethereum(1)]
    #[case::bsc(56)]
    #[case::polygon(137)]
    #[case::monad(143)]
    #[case::hyperevm(999)]
    #[case::robinhood(4663)]
    #[case::base(8453)]
    #[case::arbitrum(42161)]
    #[case::avalanche(43114)]
    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_serves_quotable_pools(#[case] chain_id: u64) {
        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");
        let client = client("https://api.metric.xyz", chain_id, &api_key);
        let metadata = client.fetch_metadata().await.unwrap();
        assert!(!metadata.is_empty());

        let pools = metadata.len();
        let mut last_error = None;
        let mut selected = None;
        for (_, bid_ask) in client
            .fetch_bid_ask_each(metadata)
            .await
        {
            match bid_ask {
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
                "Metric live API on chain {chain_id} returned no quotable bid_ask across {} pools; last error: {:?}",
                pools,
                last_error
            );
        };
        let bid = bid_ask
            .bid_price()
            .unwrap()
            .expect("the selected pool quotes a bid");
        let ask = bid_ask
            .ask_price()
            .unwrap()
            .expect("the selected pool quotes an ask");
        assert!(bid > 0.0);
        assert!(ask >= bid);
    }

    /// Every endpoint hangs off the chain's own prefix, whether or not the base URL is given
    /// with a trailing slash.
    #[test]
    fn the_endpoints_carry_the_chain_the_client_serves() {
        let chain_id = Chain::Arbitrum.id();
        let client = client("https://api.metric.xyz/", chain_id, "key");

        assert_eq!(
            client.chain_endpoint,
            format!("https://api.metric.xyz/public/v1/evm/{chain_id}")
        );
        assert_eq!(
            client.metadata_endpoint,
            format!("https://api.metric.xyz/public/v1/evm/{chain_id}/metadata")
        );
    }

    /// Metric answers a pool it does not know with the same 404 as a chain it does not serve, so
    /// only the refusal that names the chain ends the feed.
    #[rstest]
    #[case::an_unserved_chain(r#"{"error": "Unknown chainId: 130"}"#, true)]
    #[case::a_404_about_something_else(
        r#"{"error": "Pair metadata not found for 0x0000000000000000000000000000000000000001"}"#,
        false
    )]
    #[tokio::test]
    async fn only_the_refusal_naming_the_chain_is_fatal(
        #[case] body: &'static str,
        #[case] fatal: bool,
    ) {
        let server = spawn_http_server(move |_| Some(("404 Not Found", body.to_string()))).await;
        let client = client(&server.url(), Chain::Unichain.id(), "key");

        let error = client
            .fetch_metadata()
            .await
            .unwrap_err();

        assert_eq!(error.is_fatal(), fatal, "{error}");
    }

    /// Unichain (130) is a chain Metric does not serve, and the words it refuses one with are
    /// what this client reads to stop the feed instead of polling on.
    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_refuses_a_chain_it_does_not_serve() {
        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");
        let client = client("https://api.metric.xyz", Chain::Unichain.id(), &api_key);

        let error = client
            .fetch_metadata()
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }
}
