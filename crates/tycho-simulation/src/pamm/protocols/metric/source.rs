//! The provider half of the Metric book feed: fetches one complete book per poll from the
//! `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::{collections::HashMap, sync::Arc};

use chrono::DateTime;
use tracing::{debug, warn};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    book::{component::pair_component, Book, BookFeedConfig, BookSnapshot, ReceivedAt},
    pamm::protocols::metric::{
        client::MetricClient,
        models::{MetricBidAskResponse, MetricMetadata},
        state::MetricState,
        PROTOCOL_SYSTEM,
    },
    protocol::models::ProtocolComponent,
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Metric book per poll: every quotable pool between the configured tokens
/// whose USD TVL clears the threshold, as simulate-ready books. `Debug` output omits the API key.
#[derive(derive_more::Debug, Clone)]
pub struct MetricBookSource {
    pub common: BookFeedConfig,
    /// Reads the venue's pool metadata and quotes.
    pub client: MetricClient,
}

impl MetricBookSource {
    /// Builds the simulation component and state for one pool.
    fn build_book(
        &self,
        component_id: Bytes,
        token0: Token,
        token1: Token,
        metadata: MetricMetadata,
        bid_ask: MetricBidAskResponse,
    ) -> (ProtocolComponent, MetricState) {
        let state = MetricState {
            base_token: token0.clone(),
            quote_token: token1.clone(),
            metadata,
            bid_ask,
        };
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
}

impl MetricBookSource {
    /// A metadata failure fails the poll; a single pool's bid/ask failure only drops that pool
    /// from this book.
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
        let metadata = self.client.fetch_metadata().await?;

        let quotable: Vec<MetricMetadata> = metadata
            .into_iter()
            .filter(|pool| {
                self.common
                    .pair_tokens(&pool.token0, &pool.token1)
                    .is_some() &&
                    // v1 metadata carries the USD TVL directly, so no cross-pool price
                    // normalization is needed.
                    self.common
                        .clears_min_tvl(pool.tvl_fiat.unwrap_or(0.0), &pool.pool_address)
            })
            .collect();

        let mut books = HashMap::new();
        for (pool, bid_ask) in self
            .client
            .fetch_bid_ask_each(quotable)
            .await
        {
            let Some((token0, token1)) = self
                .common
                .pair_tokens(&pool.token0, &pool.token1)
            else {
                continue;
            };
            let bid_ask = match bid_ask {
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

            let component_id = pool.pool_address.clone();
            let updated_at = DateTime::from_timestamp(bid_ask.server_ts as i64, 0);
            let book_key = component_id.to_string();
            let (component, state) =
                self.build_book(component_id, token0.clone(), token1.clone(), pool, bid_ask);
            books.insert(book_key, Book { component, state: Arc::new(state), updated_at });
        }
        Ok(books)
    }
}

impl HttpSource for MetricBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        self.fetch_books()
            .await
            .map(BookSnapshot::received_now)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use num_bigint::BigUint;
    use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim};

    use super::*;
    use crate::{
        book::test_token_map,
        evm::protocol::utils::bytes_to_address,
        pamm::protocols::metric::{
            client::DEFAULT_BID_ASK_CONCURRENCY,
            feed::MetricFeedBuilder,
            models::{MetricDepth, MetricDepthBin},
        },
        snapshot_feed::http::test_support::spawn_http_server,
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

    /// The `/bid_ask` target of `pool` as the client requests it (checksummed address).
    fn bid_ask_target(pool: &str) -> String {
        let checksummed = bytes_to_address(&Bytes::from_str(pool).unwrap())
            .unwrap()
            .to_checksum(None);
        format!("/{checksummed}/bid_ask")
    }

    /// A source for `chain_endpoint` with an empty token universe and no TVL floor.
    fn source(chain: Chain, chain_endpoint: &str, api_key: &str) -> MetricBookSource {
        MetricBookSource {
            common: BookFeedConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 },
            client: MetricClient::new(
                format!("{chain_endpoint}/metadata"),
                chain_endpoint.to_string(),
                api_key.to_string(),
                DEFAULT_BID_ASK_CONCURRENCY,
            ),
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
            metadata.pool_address.clone(),
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
        let expected =
            MetricState { base_token: weth(), quote_token: usdc(), metadata, bid_ask: bid_ask() };
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

        assert!(matches!(result, Err(FeedError::Connection(_))), "got {result:?}");
    }

    /// The metadata endpoint is paged by `offset`; the poll follows `nextOffset` and must stop on
    /// the last page, on an empty page, and on an offset that does not advance (the API would
    /// otherwise be polled forever).
    /// One poll fetches every pool's bid/ask and shares a single `request_timeout`, so the
    /// default concurrency has to keep a full poll inside that budget at the pool count Metric
    /// actually serves. Base is the chain with pools. The token universe is stubbed from the
    /// metadata itself: what this measures is the poll, not the pricing.
    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_poll_fits_the_request_timeout_at_the_default_concurrency() {
        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");
        let mut source = source(Chain::Base, "https://api.metric.xyz/public/v1/evm/8453", &api_key);
        let metadata = source
            .client
            .fetch_metadata()
            .await
            .unwrap();
        assert!(!metadata.is_empty(), "Metric serves no Base pools");

        let tokens = metadata
            .iter()
            .flat_map(|pool| [pool.token0.clone(), pool.token1.clone()])
            .map(|address| {
                let token = Token::new(&address, "TKN", 18, 0, &[], Chain::Base, 100);
                (address, token)
            })
            .collect();
        source.common =
            BookFeedConfig { chain: Chain::Base, tokens: Arc::new(tokens), min_tvl_usd: 0.0 };

        let budget = MetricFeedBuilder::default_feed_config().request_timeout;
        let started = std::time::Instant::now();
        let books = source.fetch_books().await.unwrap();
        let elapsed = started.elapsed();

        assert!(!books.is_empty(), "no quotable pool among the {} on Base", metadata.len());
        println!(
            "{} pools, {} books, {elapsed:?} of a {budget:?} budget",
            metadata.len(),
            books.len()
        );
        assert!(elapsed < budget, "a poll of {} pools took {elapsed:?}", metadata.len());
    }
}
