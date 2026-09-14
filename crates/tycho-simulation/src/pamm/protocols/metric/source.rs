//! The provider half of the Metric book feed: fetches one complete book per poll from the
//! `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::{collections::HashMap, sync::Arc};

use chrono::DateTime;
use reqwest::Client;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{
        component::pair_component,
        errors::FeedError,
        feed_loops::HttpBookSource,
        http::fetch_json,
        models::{Book, CommonConfig},
    },
    evm::protocol::utils::bytes_to_address,
    pamm::protocols::metric::{
        models::{MetricBidAskResponse, MetricMetadata, PaginatedMetadataResponse},
        state::MetricState,
        PROTOCOL_SYSTEM,
    },
    protocol::models::ProtocolComponent,
};

/// Page size for the paginated metadata endpoint. The API clamps `count` to `[1, 500]`.
const METADATA_PAGE_SIZE: u32 = 500;

/// Currency the metadata endpoint values `tvlFiat` in; the threshold in `CommonConfig` is USD.
const TVL_FIAT_CURRENCY: &str = "USD";

/// Deadline for one pool's bid/ask request. A pool that misses it is left out of the current
/// book; the poll as a whole still succeeds with the other pools.
const BID_ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetches one complete Metric book per poll: every quotable pool between the configured tokens
/// whose USD TVL clears the threshold, as simulate-ready books. `Debug` output omits the API key.
#[derive(derive_more::Debug, Clone)]
pub struct MetricBookSource {
    pub common: CommonConfig,
    pub metadata_endpoint: String,
    /// Prefix ending at /public/v1/evm/{chain_id}; pool-specific endpoints are derived from it.
    pub chain_endpoint: String,
    /// Bearer trading key required by the authenticated endpoints (`bid_ask`).
    #[debug(skip)]
    pub api_key: String,
    pub http: Client,
}

impl MetricBookSource {
    /// Builds the simulation component and state for one pool.
    fn build_book(
        &self,
        component_id: &str,
        token0: Token,
        token1: Token,
        metadata: MetricMetadata,
        bid_ask: MetricBidAskResponse,
    ) -> (ProtocolComponent, MetricState) {
        let state = MetricState::new(token0.clone(), token1.clone(), metadata, bid_ask);
        let component = pair_component(
            component_id,
            PROTOCOL_SYSTEM,
            "metric_pool",
            self.common.chain,
            token0,
            token1,
        );
        (component, state)
    }

    /// Fetches every pool by paging through the metadata endpoint until the API reports no next
    /// page.
    async fn fetch_metadata(&self) -> Result<Vec<MetricMetadata>, FeedError> {
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

    async fn fetch_bid_ask(&self, pool: &Bytes) -> Result<MetricBidAskResponse, FeedError> {
        let pool = bytes_to_address(pool)
            .map_err(|e| FeedError::InvalidInput(e.to_string()))?
            .to_checksum(None);
        let request = self
            .http
            .get(format!("{}/{pool}/bid_ask", self.chain_endpoint))
            .header("accept", "application/json")
            .bearer_auth(&self.api_key);
        timeout(BID_ASK_TIMEOUT, fetch_json(request, "Metric bid/ask"))
            .await
            .map_err(|_| {
                FeedError::ConnectionError(format!(
                    "Metric bid/ask request timed out after {} seconds",
                    BID_ASK_TIMEOUT.as_secs()
                ))
            })?
    }
}

impl HttpBookSource for MetricBookSource {
    /// A metadata failure fails the poll; a single pool's bid/ask failure only drops that pool
    /// from this book.
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
        let metadata = self.fetch_metadata().await?;

        let mut books = HashMap::new();
        for pool in metadata {
            let Some((token0, token1)) = self
                .common
                .pair_tokens(&pool.token0, &pool.token1)
            else {
                continue;
            };

            // v1 metadata carries the USD TVL directly, so no cross-pool price normalization is
            // needed.
            let tvl = pool.tvl_fiat.unwrap_or(0.0);
            if !self
                .common
                .clears_min_tvl(tvl, &pool.pool_address.to_string())
            {
                continue;
            }

            let bid_ask = match self
                .fetch_bid_ask(&pool.pool_address)
                .await
            {
                Ok(bid_ask) => bid_ask,
                Err(e) => {
                    warn!(pool = %pool.pool_address, error = %e, "skipping pool, bid/ask fetch failed");
                    continue;
                }
            };
            if !bid_ask.is_quotable() {
                debug!(pool = %pool.pool_address, "skipping pool, not quotable");
                continue;
            }

            let component_id = pool.pool_address.to_string();
            let updated_at = DateTime::from_timestamp(bid_ask.server_ts as i64, 0);
            let (component, state) =
                self.build_book(&component_id, token0.clone(), token1.clone(), pool, bid_ask);
            books.insert(component_id, Book { component, state: Arc::new(state), updated_at });
        }
        Ok(books)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim};

    use super::*;
    use crate::{
        book::{http::test_support::spawn_http_server, models::test_token_map},
        pamm::protocols::metric::models::{MetricDepth, MetricDepthBin},
    };

    fn pool_json(pool: &str, tvl_fiat: Option<f64>) -> serde_json::Value {
        serde_json::json!({
            "poolAddress": pool,
            "token0": weth().address.to_string(),
            "token1": usdc().address.to_string(),
            "tvlFiat": tvl_fiat,
        })
    }

    fn metadata_page(pools: &[serde_json::Value], next_offset: Option<u64>) -> String {
        serde_json::json!({ "data": pools, "nextOffset": next_offset }).to_string()
    }

    /// The bid/ask body of a healthy, quotable pool; `unhealthy` flips the provider status.
    fn bid_ask_json(server_ts: u64, unhealthy: bool) -> String {
        let mut body = serde_json::json!({
            "bidAdj": q64(3000).to_string(),
            "askAdj": q64(3010).to_string(),
            "totalToken0Available": "1000000000000000000",
            "totalToken1Available": "3000000000",
            "serverTs": server_ts,
            "depth": {
                "asks": [{
                    "binIdx": 0,
                    "price": q64(3010).to_string(),
                    "cumulativeVolume": "3000000000",
                    "cumulativeInputVolume": "1000000000000000000"
                }],
                "bids": []
            }
        });
        if unhealthy {
            body["priceProviderStatus"] = serde_json::json!("degraded");
        }
        body.to_string()
    }

    /// The `/bid_ask` target of `pool` as the source requests it (checksummed address).
    fn bid_ask_target(pool: &str) -> String {
        let checksummed = bytes_to_address(&Bytes::from_str(pool).unwrap())
            .unwrap()
            .to_checksum(None);
        format!("/{checksummed}/bid_ask")
    }

    /// A source for `chain_endpoint` with an empty token universe and no TVL floor.
    fn source(chain: Chain, chain_endpoint: &str, api_key: &str) -> MetricBookSource {
        MetricBookSource {
            common: CommonConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 },
            metadata_endpoint: format!("{chain_endpoint}/metadata"),
            chain_endpoint: chain_endpoint.to_string(),
            api_key: api_key.to_string(),
            http: Client::new(),
        }
    }

    fn weth() -> Token {
        let address = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        Token::new(&address, "WETH", 18, 0, &[], Chain::Ethereum, 100)
    }

    fn usdc() -> Token {
        let address = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        Token::new(&address, "USDC", 6, 0, &[], Chain::Ethereum, 100)
    }

    fn metadata() -> MetricMetadata {
        MetricMetadata {
            pool_address: Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
            token0: weth().address,
            token1: usdc().address,
            tvl_fiat: Some(1_000_000.0),
        }
    }

    fn q64(price: u64) -> BigUint {
        BigUint::from(price) << 64usize
    }

    fn bid_ask() -> MetricBidAskResponse {
        MetricBidAskResponse {
            bid_adj: q64(3000),
            ask_adj: q64(3010),
            total_token0_available: Some(BigUint::from(1_000_000_000_000_000_000u64)),
            total_token1_available: Some(BigUint::from(3_000_000_000u64)),
            server_ts: 100,
            price_provider_status: Some("healthy".to_string()),
            depth: MetricDepth {
                asks: vec![MetricDepthBin {
                    bin_idx: 0,
                    price: q64(3010),
                    cumulative_volume: BigUint::from(3_000_000_000u64),
                    cumulative_input_volume: BigUint::from(1_000_000_000_000_000_000u64),
                }],
                bids: vec![],
            },
        }
    }

    #[test]
    fn build_pair_component_and_state() {
        let metadata = metadata();
        let source = source(Chain::Ethereum, "https://metric.example/public/v1/evm/1", "key");
        let (component, state) = source.build_book(
            &metadata.pool_address.to_string(),
            weth(),
            usdc(),
            metadata.clone(),
            bid_ask(),
        );

        assert_eq!(component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(component.protocol_type_name, "metric_pool");
        assert_eq!(component.tokens, vec![weth(), usdc()]);
        assert_eq!(
            component.id,
            Bytes::from(
                metadata
                    .pool_address
                    .to_string()
                    .as_str()
            )
        );
        assert!(component.contract_ids.is_empty());
        let expected = MetricState::new(weth(), usdc(), metadata, bid_ask());
        assert!(state.eq(&expected), "state should carry the pool's metadata and bid/ask");
    }

    #[tokio::test]
    async fn fetch_books_drops_failed_and_unquotable_pools_but_keeps_the_rest() {
        const HEALTHY: &str = "0x1111111111111111111111111111111111111111";
        const FAILING: &str = "0x2222222222222222222222222222222222222222";
        const UNQUOTABLE: &str = "0x3333333333333333333333333333333333333333";
        const BELOW_FLOOR: &str = "0x4444444444444444444444444444444444444444";
        const UNPRICED: &str = "0x5555555555555555555555555555555555555555";
        const SERVER_TS: u64 = 1_700_000_000;
        let page = metadata_page(
            &[
                pool_json(HEALTHY, Some(1_000.0)),
                pool_json(FAILING, Some(1_000.0)),
                pool_json(UNQUOTABLE, Some(1_000.0)),
                pool_json(BELOW_FLOOR, Some(99.0)),
                pool_json(UNPRICED, None),
            ],
            None,
        );
        let server = spawn_http_server(move |target| {
            if target.starts_with("/metadata?") {
                return Some(("200 OK", page.clone()));
            }
            if target == bid_ask_target(HEALTHY) {
                return Some(("200 OK", bid_ask_json(SERVER_TS, false)));
            }
            if target == bid_ask_target(FAILING) {
                return Some(("500 Internal Server Error", "boom".to_string()));
            }
            if target == bid_ask_target(UNQUOTABLE) {
                return Some(("200 OK", bid_ask_json(SERVER_TS, true)));
            }
            // Pools filtered out on metadata must never get here; answering them as healthy
            // makes such a slip show up as an extra emitted book.
            Some(("200 OK", bid_ask_json(SERVER_TS, false)))
        })
        .await;
        let endpoint = server.url();
        let mut source = source(Chain::Ethereum, &endpoint, "key");
        source.common.tokens = Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ]));
        source.common.min_tvl_usd = 100.0;

        let books = source.fetch_books().await.unwrap();

        let healthy_id = Bytes::from_str(HEALTHY)
            .unwrap()
            .to_string();
        assert_eq!(books.keys().collect::<Vec<_>>(), vec![&healthy_id]);
        let book = &books[&healthy_id];
        assert_eq!(book.component.id, Bytes::from(healthy_id.as_str()));
        assert_eq!(book.updated_at, DateTime::from_timestamp(SERVER_TS as i64, 0));
    }

    #[tokio::test]
    async fn metadata_failure_fails_the_poll() {
        let server =
            spawn_http_server(|_| Some(("500 Internal Server Error", "boom".to_string()))).await;
        let endpoint = server.url();
        let source = source(Chain::Ethereum, &endpoint, "key");

        let result = source.fetch_books().await;

        assert!(matches!(result, Err(FeedError::ConnectionError(_))), "got {result:?}");
    }

    /// The metadata endpoint is paged by `offset`; the poll follows `nextOffset` and must stop on
    /// the last page, on an empty page, and on an offset that does not advance (the API would
    /// otherwise be polled forever).
    #[rstest]
    #[case::follows_next_offset_to_the_last_page(
        vec![(Some(1u64), Some(500u64)), (Some(2), None)],
        2
    )]
    #[case::stops_when_next_offset_does_not_advance(vec![(Some(1), Some(0)), (Some(2), None)], 1)]
    #[case::stops_on_an_empty_page(vec![(None, Some(500)), (Some(2), None)], 0)]
    #[tokio::test]
    async fn fetch_metadata_follows_next_offset_and_stops_when_it_does_not_advance(
        #[case] pages: Vec<(Option<u64>, Option<u64>)>,
        #[case] expected_pools: usize,
    ) {
        // Page `i` is served at offset `i * 500`; each carries at most one pool named after it.
        let bodies: Vec<String> = pages
            .iter()
            .map(|(pool, next_offset)| {
                let pools: Vec<_> = pool
                    .map(|n| pool_json(&format!("0x{n:040x}"), Some(1.0)))
                    .into_iter()
                    .collect();
                metadata_page(&pools, *next_offset)
            })
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
        let source = source(Chain::Ethereum, &endpoint, "key");

        // A broken stop condition polls the same page forever; the timeout turns that into a
        // failure.
        let pools = timeout(Duration::from_secs(5), source.fetch_metadata())
            .await
            .expect("pagination must terminate")
            .unwrap();

        assert_eq!(pools.len(), expected_pools);
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_serves_quotable_pools() {
        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");
        let source = source(Chain::Base, "https://api.metric.xyz/public/v1/evm/8453", &api_key);
        let metadata = source.fetch_metadata().await.unwrap();
        assert!(!metadata.is_empty());

        let mut last_error = None;
        let mut selected = None;
        for pool in &metadata {
            match source
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
