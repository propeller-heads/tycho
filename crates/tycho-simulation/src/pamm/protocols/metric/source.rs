//! The provider half of the Metric book feed: fetches one complete book per poll from the
//! `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::mem;

use chrono::DateTime;
use tracing::{debug, warn};

use crate::{
    book::{component::pair_book, BookFeedConfig, BookSnapshot, Books, ReceivedAt},
    pamm::protocols::metric::{
        client::MetricClient, models::MetricMetadata, state::MetricState, PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Metric book per poll: every unpaused, quotable pool between the
/// configured tokens whose USD TVL clears the threshold, as simulate-ready books. `Debug` output
/// omits the API key.
#[derive(derive_more::Debug, Clone)]
pub struct MetricBookSource {
    /// The protocol system stamped on this source's components: the venue's own, or its
    /// fallback-router one for a feed built with `with_fallback_router()`.
    pub protocol_system: &'static str,
    pub book_config: BookFeedConfig,
    /// Reads the venue's pool metadata and quotes.
    pub client: MetricClient,
}

impl HttpSource for MetricBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A metadata failure fails the poll, and so does losing every pool: a book that reads as
    /// nothing withdraws every pair the venue was serving, which is an outage rather than a
    /// poll that worked. Losing some of the pools only drops those from this book.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let metadata = self.client.fetch_metadata().await?;

        let quotable: Vec<MetricMetadata> = metadata
            .into_iter()
            .filter(|pool| {
                self.book_config
                    .pair_tokens(&pool.token0, &pool.token1)
                    .is_some() &&
                    // v1 metadata carries the USD TVL directly, so no cross-pool price
                    // normalization is needed.
                    self.book_config
                        .clears_min_tvl(pool.tvl_fiat.unwrap_or(0.0), &pool.pool_address) &&
                    is_unpaused(pool)
            })
            .collect();

        let polled = self
            .client
            .fetch_bid_ask_each(quotable)
            .await;
        let requested = polled.len();
        let mut failures: Vec<FeedError> = Vec::new();
        let mut books = Books::new();
        for (pool, bid_ask) in polled {
            let bid_ask = match bid_ask {
                Ok(bid_ask) => bid_ask,
                Err(e) => {
                    warn!(pool = %pool.pool_address, error = %e, "skipping pool, bid/ask fetch failed");
                    failures.push(e);
                    continue;
                }
            };
            let Some((token0, token1)) = self
                .book_config
                .pair_tokens(&pool.token0, &pool.token1)
            else {
                continue;
            };
            if !bid_ask.is_quotable() {
                debug!(pool = %pool.pool_address, "skipping pool, not quotable");
                continue;
            }

            let component_id = pool.pool_address.clone();
            let updated_at = DateTime::from_timestamp(bid_ask.server_ts as i64, 0);
            let state = MetricState {
                base_token: token0.clone(),
                quote_token: token1.clone(),
                metadata: pool,
                bid_ask,
            };
            books.insert(pair_book(
                self.protocol_system,
                PROTOCOL_TYPE,
                self.book_config.chain,
                component_id,
                state,
                updated_at,
            ));
        }
        if requested > 0 && failures.len() == requested {
            return Err(every_pool_failed(failures));
        }
        Ok(BookSnapshot::received_now(books))
    }
}

/// Whether Metric routes to `pool`, which it does only at pause level `0`.
fn is_unpaused(pool: &MetricMetadata) -> bool {
    if !pool.is_unpaused() {
        debug!(pool = %pool.pool_address, pause_level = ?pool.pause_level, "skipping paused pool");
    }
    pool.is_unpaused()
}

/// What a poll reports when none of the pools it asked answered: the pools' own verdict where
/// they agree on it, and an unreachable venue where they do not. One pool calling its failure
/// fatal is about that pool; every pool calling it fatal is about the venue, and nothing is
/// gained by polling on. The individual failures are logged as they arrive.
fn every_pool_failed(failures: Vec<FeedError>) -> FeedError {
    let summary =
        format!("none of Metric's {} quotable pools answered its bid/ask request", failures.len());
    let mut failures = failures.into_iter();
    let Some(first) = failures.next() else {
        return FeedError::Connection(summary);
    };
    if failures.all(|other| mem::discriminant(&other) == mem::discriminant(&first)) {
        first.in_context(summary)
    } else {
        FeedError::Connection(summary)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use super::*;
    use crate::{
        book::test_token_map,
        evm::protocol::utils::bytes_to_address,
        pamm::protocols::metric::{
            client::DEFAULT_BID_ASK_CONCURRENCY, feed::MetricFeedBuilder, FALLBACK_PROTOCOL_SYSTEM,
            PROTOCOL_SYSTEM,
        },
        snapshot_feed::http::test_support::{spawn_http_server, MockRequest, MockResponse},
    };

    const FIRST_POOL: &str = "0x1111111111111111111111111111111111111111";
    const SECOND_POOL: &str = "0x2222222222222222222222222222222222222222";

    /// How the venue answers a key it has stopped accepting. It is about this request, so a
    /// later poll may be served.
    const REFUSED_KEY: (&str, &str) = ("401 Unauthorized", r#"{"message":"invalid api key"}"#);

    /// How the venue answers for a chain it does not serve, which no later poll reaches either.
    const UNSERVED_CHAIN: (&str, &str) = ("404 Not Found", r#"{"error": "Unknown chainId: 1"}"#);

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

    /// The batch bid/ask target on Ethereum as the client requests it.
    const BATCH_TARGET: &str = "/public/v1/evm/1/bid_ask/batch";

    /// The pools a batch bid/ask request names, as the lowercase hex its tests address them by.
    fn requested_pools(request: &MockRequest) -> Vec<String> {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        body["pools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pool| {
                let pool = pool.as_str().unwrap();
                // The client sends checksummed addresses.
                let checksummed = bytes_to_address(&Bytes::from_str(pool).unwrap())
                    .unwrap()
                    .to_checksum(None);
                assert_eq!(pool, checksummed);
                pool.to_lowercase()
            })
            .collect()
    }

    /// A batch answer holding `pool`'s quote.
    fn quote_entry(pool: &str, bid_ask: &str) -> serde_json::Value {
        let quote: serde_json::Value = serde_json::from_str(bid_ask).unwrap();
        serde_json::json!({ "poolAddress": pool, "quote": quote })
    }

    /// A batch answer holding the error Metric gave for `pool`.
    fn error_entry(pool: &str, status: u16, error: &str) -> serde_json::Value {
        serde_json::json!({ "poolAddress": pool, "error": { "status": status, "error": error } })
    }

    fn batch_body(entries: Vec<serde_json::Value>) -> String {
        serde_json::json!({ "results": entries }).to_string()
    }

    /// A source against the Metric deployment at `base_url`, with an empty token universe and no
    /// TVL floor.
    fn source(chain: Chain, base_url: &str, api_key: &str) -> MetricBookSource {
        MetricBookSource {
            protocol_system: PROTOCOL_SYSTEM,
            book_config: BookFeedConfig {
                chain,
                tokens: Arc::new(HashMap::new()),
                min_tvl_usd: 0.0,
            },
            client: MetricClient::new(
                Some(base_url),
                chain.try_id().unwrap(),
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

    fn q64(price: u64) -> BigUint {
        BigUint::from(price) << 64usize
    }

    #[tokio::test]
    async fn a_fallback_source_labels_its_components_for_the_fallback_router() {
        const POOL: &str = "0x1111111111111111111111111111111111111111";
        const SERVER_TS: u64 = 1_700_000_000;
        let page = metadata_page(&[pool_json(POOL, Some(1_000.0))], None);
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(("200 OK", page.clone()));
            }
            Some(("200 OK", batch_body(vec![quote_entry(POOL, &bid_ask_json(SERVER_TS, false))])))
        })
        .await;
        let mut source = MetricBookSource {
            protocol_system: FALLBACK_PROTOCOL_SYSTEM,
            ..source(Chain::Ethereum, &server.url(), "test_key")
        };
        source.book_config.tokens = Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ]));

        let books = source.fetch().await.unwrap().books;

        let book = books.values().next().expect("one book");
        assert_eq!(book.component.protocol_system, "fallback:rfq:metric");
        // A Metric component is identified by its pool address, whatever the system labels it.
        assert_eq!(book.component.id.to_string(), POOL);
    }

    #[tokio::test]
    async fn fetch_books_drops_failed_and_unquotable_pools_but_keeps_the_rest() {
        const HEALTHY: &str = "0x1111111111111111111111111111111111111111";
        const FAILING: &str = "0x2222222222222222222222222222222222222222";
        const UNQUOTABLE: &str = "0x3333333333333333333333333333333333333333";
        const BELOW_FLOOR: &str = "0x4444444444444444444444444444444444444444";
        const UNPRICED: &str = "0x5555555555555555555555555555555555555555";
        const PAUSED: &str = "0x6666666666666666666666666666666666666666";
        const UNANSWERED: &str = "0x7777777777777777777777777777777777777777";
        const SERVER_TS: u64 = 1_700_000_000;
        let mut paused = pool_json(PAUSED, Some(1_000.0));
        paused["pauseLevel"] = serde_json::json!(1);
        let page = metadata_page(
            &[
                pool_json(HEALTHY, Some(1_000.0)),
                pool_json(FAILING, Some(1_000.0)),
                pool_json(UNQUOTABLE, Some(1_000.0)),
                pool_json(BELOW_FLOOR, Some(99.0)),
                pool_json(UNPRICED, None),
                paused,
                pool_json(UNANSWERED, Some(1_000.0)),
            ],
            None,
        );
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(("200 OK", page.clone()));
            }
            assert_eq!(request.target, BATCH_TARGET);
            let entries = requested_pools(request)
                .into_iter()
                .filter_map(|pool| match pool.as_str() {
                    FAILING => Some(error_entry(&pool, 500, "boom")),
                    UNQUOTABLE => Some(quote_entry(&pool, &bid_ask_json(SERVER_TS, true))),
                    UNANSWERED => None,
                    // Pools filtered out on metadata must never get here; answering them as
                    // healthy makes such a slip show up as an extra emitted book.
                    _ => Some(quote_entry(&pool, &bid_ask_json(SERVER_TS, false))),
                })
                .collect();
            Some(("200 OK", batch_body(entries)))
        })
        .await;
        let endpoint = server.url();
        let mut source = source(Chain::Ethereum, &endpoint, "key");
        source.book_config.tokens = Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ]));
        source.book_config.min_tvl_usd = 100.0;

        let books = source.fetch().await.unwrap().books;

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

        let result = source.fetch().await;

        assert!(matches!(result, Err(FeedError::Connection(_))), "got {result:?}");
    }

    /// A key the venue has stopped accepting answers every pool the same way. The empty book
    /// that leaves would withdraw every pair while reporting a poll that worked, so the feed's
    /// failure budget would never see the outage. What the pools answered decides the failure:
    /// a chain the venue has stopped serving is a chain no later poll reaches either, while one
    /// pool alone saying so is about that pool, whatever the pool beside it answered.
    #[rstest]
    #[case::the_batch_on_a_refused_key(Err(REFUSED_KEY), false)]
    #[case::the_batch_on_an_unserved_chain(Err(UNSERVED_CHAIN), true)]
    #[case::every_pool_refusing_the_chain(
        Ok(["Unknown chainId: 1", "Unknown chainId: 1"]),
        true
    )]
    #[case::one_pool_of_two_refusing_the_chain(
        Ok(["Unknown chainId: 1", "Pair metadata not found"]),
        false
    )]
    #[tokio::test]
    async fn losing_every_pool_fails_the_poll(
        #[case] answer: Result<[&'static str; 2], (&'static str, &'static str)>,
        #[case] fatal: bool,
    ) {
        let page = metadata_page(
            &[pool_json(FIRST_POOL, Some(1_000.0)), pool_json(SECOND_POOL, Some(1_000.0))],
            None,
        );
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(("200 OK", page.clone()));
            }
            let errors = match answer {
                Ok(errors) => errors,
                Err((status, body)) => return Some((status, body.to_string())),
            };
            let entries = requested_pools(request)
                .iter()
                .map(|pool| {
                    let error = if pool == FIRST_POOL { errors[0] } else { errors[1] };
                    error_entry(pool, 404, error)
                })
                .collect();
            Some(("200 OK", batch_body(entries)))
        })
        .await;
        let endpoint = server.url();
        let mut source = source(Chain::Ethereum, &endpoint, "revoked");
        source.book_config.tokens = Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ]));

        let result = source.fetch().await;

        let error = result.expect_err("a poll that lost every pool must fail");
        assert_eq!(error.is_fatal(), fatal, "{error}");
        assert!(
            error
                .to_string()
                .contains("none of Metric's 2 quotable pools"),
            "{error}"
        );
    }

    /// The batch endpoint quotes at most 100 pools per request, so a venue listing more is
    /// quoted over several requests, and every pool still makes it into the book.
    #[tokio::test]
    async fn pools_beyond_one_batch_are_quoted_in_further_batches() {
        const POOLS: usize = 150;
        const SERVER_TS: u64 = 1_700_000_000;
        let pools: Vec<String> = (1..=POOLS)
            .map(|n| format!("0x{n:040x}"))
            .collect();
        let page = metadata_page(
            &pools
                .iter()
                .map(|pool| pool_json(pool, Some(1_000.0)))
                .collect::<Vec<_>>(),
            None,
        );
        let batch_sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&batch_sizes);
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(("200 OK", page.clone()));
            }
            let requested = requested_pools(request);
            seen.lock()
                .unwrap()
                .push(requested.len());
            let entries = requested
                .iter()
                .map(|pool| quote_entry(pool, &bid_ask_json(SERVER_TS, false)))
                .collect();
            Some(("200 OK", batch_body(entries)))
        })
        .await;
        let mut source = source(Chain::Ethereum, &server.url(), "key");
        source.book_config.tokens = Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ]));

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.len(), POOLS);
        let mut batch_sizes = batch_sizes.lock().unwrap().clone();
        batch_sizes.sort_unstable();
        assert_eq!(batch_sizes, vec![50, 100]);
    }

    /// A batch that never answers fails on its own deadline, inside the poll's
    /// `request_timeout`, so the feed loop sees a failed poll it can back off from instead of a
    /// poll that hangs to the whole-poll deadline.
    #[tokio::test(start_paused = true)]
    async fn a_hung_batch_fails_the_poll_within_the_request_timeout() {
        let page = metadata_page(&[pool_json(FIRST_POOL, Some(1_000.0))], None);
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(MockResponse::from(("200 OK", page.clone())));
            }
            Some(MockResponse {
                status: "200 OK",
                body: batch_body(vec![]),
                delay: Duration::from_secs(3600),
            })
        })
        .await;
        let mut source = source(Chain::Ethereum, &server.url(), "key");
        source.book_config.tokens = Arc::new(test_token_map(&[
            (&weth().address, "WETH", 18),
            (&usdc().address, "USDC", 6),
        ]));
        let started = tokio::time::Instant::now();

        let error = source
            .fetch()
            .await
            .expect_err("a hung batch must fail the poll");

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        assert!(
            error
                .to_string()
                .contains("batch bid/ask request timed out"),
            "{error}"
        );
        assert!(started.elapsed() < MetricFeedBuilder::default_feed_config().request_timeout);
    }

    /// One poll fetches every pool's bid/ask and shares a single `request_timeout`, so the
    /// default concurrency has to keep a full poll inside that budget at the pool count Metric
    /// actually serves. Base is the chain with pools. The token universe is stubbed from the
    /// metadata itself: what this measures is the poll, not the pricing.
    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_poll_fits_the_request_timeout_at_the_default_concurrency() {
        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");
        let mut source = source(Chain::Base, "https://api.metric.xyz", &api_key);
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
        source.book_config =
            BookFeedConfig { chain: Chain::Base, tokens: Arc::new(tokens), min_tvl_usd: 0.0 };

        let budget = MetricFeedBuilder::default_feed_config().request_timeout;
        let started = std::time::Instant::now();
        let books = source.fetch().await.unwrap().books;
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
