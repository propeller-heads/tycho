use std::{collections::HashMap, mem};

use chrono::DateTime;
use tracing::{debug, warn};
use tycho_common::models::token::Token;

use crate::{
    book::{component::pair_book, Book, BookFeedConfig, BookSnapshot, Books, ReceivedAt},
    pamm::protocols::metric::{
        client::MetricClient,
        models::{MetricMetadata, MetricPoolPrice, MetricPrices},
        state::MetricState,
        PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Metric book per poll: every unpaused, quotable pool between the
/// configured tokens whose USD TVL clears the threshold, as simulate-ready books built by its
/// [`MetricBookAssembler`]. `Debug` output omits the API key.
#[derive(Debug, Clone)]
pub struct MetricBookSource {
    pub(super) prices: MetricPricesSource,
    pub(super) assembler: MetricBookAssembler,
}

impl HttpSource for MetricBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A metadata failure fails the poll, and so does losing every pool: a book that reads as
    /// nothing withdraws every pair the venue was serving, which is an outage rather than a
    /// poll that worked. Losing some of the pools only drops those from this book.
    ///
    /// Pools outside the token list, under the TVL floor or paused are not quoted.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let prices = self
            .prices
            .poll(|pool| {
                self.assembler
                    .pool_tokens(pool)
                    .is_some()
            })
            .await?;
        Ok(BookSnapshot::received_now(self.assembler.collect(prices)))
    }
}

/// Whether Metric routes to `pool`, which it does only at pause level `0`.
fn is_unpaused(pool: &MetricMetadata) -> bool {
    if !pool.is_unpaused() {
        debug!(pool = %pool.pool_address, pause_level = ?pool.pause_level, "skipping paused pool");
    }
    pool.is_unpaused()
}

/// Returns quotes for the pools whose requests succeeded. Logs and omits failed requests. If every
/// requested pool fails, returns an error: publishing an empty book would withdraw every pair the
/// venue was serving and treat an outage as a successful poll.
async fn quote_pools(
    client: &MetricClient,
    pools: Vec<MetricMetadata>,
) -> Result<MetricPrices, FeedError> {
    let polled = client.fetch_bid_ask_each(pools).await;
    let requested = polled.len();
    let mut failures: Vec<FeedError> = Vec::new();
    let mut answered = Vec::with_capacity(requested);
    for (metadata, bid_ask) in polled {
        match bid_ask {
            Ok(bid_ask) => answered.push(MetricPoolPrice { metadata, bid_ask }),
            Err(e) => {
                warn!(
                    pool = %metadata.pool_address,
                    error = %e,
                    "skipping pool, bid/ask fetch failed"
                );
                failures.push(e);
            }
        }
    }
    if requested > 0 && failures.len() == requested {
        return Err(every_pool_failed(failures));
    }
    Ok(MetricPrices { pools: answered })
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

/// Reads the pools' metadata and quotes through a [`MetricClient`].
#[derive(Clone, Debug)]
pub(super) struct MetricPricesSource {
    client: MetricClient,
}

impl MetricPricesSource {
    pub(super) fn new(client: MetricClient) -> Self {
        Self { client }
    }

    /// Polls the metadata of every pool, then quotes the unpaused ones for which `keep` holds.
    ///
    /// A metadata failure fails the poll, and so does losing every pool; losing some only drops
    /// those from the prices.
    pub(super) async fn poll(
        &self,
        keep: impl Fn(&MetricMetadata) -> bool,
    ) -> Result<MetricPrices, FeedError> {
        let pools = self
            .client
            .fetch_metadata()
            .await?
            .into_iter()
            .filter(|pool| keep(pool))
            .collect();
        quote_pools(&self.client, pools).await
    }
}

impl HttpSource for MetricPricesSource {
    type Snapshot = MetricPrices;

    async fn fetch(&self) -> Result<MetricPrices, FeedError> {
        self.poll(is_unpaused).await
    }
}

/// Turns [`MetricPrices`] into the books [`MetricFeed`](super::feed::MetricFeed) would publish
/// for the same poll: every unpaused, quotable pool between the configured tokens whose USD TVL
/// clears the threshold, as simulate-ready books. Created by
/// [`MetricBookAssemblerBuilder`](super::feed::MetricBookAssemblerBuilder), which also makes the
/// one a [`MetricFeed`](super::feed::MetricFeed) uses.
#[derive(Clone, Debug)]
pub struct MetricBookAssembler {
    /// The protocol system stamped on the components: the venue's own, or its fallback-router
    /// one for a feed built with `with_fallback_router()`.
    pub(super) protocol_system: &'static str,
    pub(super) book_config: BookFeedConfig,
}

impl MetricBookAssembler {
    /// Returns the books for `prices`, keyed by component id.
    pub fn books(&self, prices: MetricPrices) -> HashMap<String, Book> {
        self.collect(prices).into_map()
    }

    /// Returns the token pair of `pool` when this assembler serves it: both tokens are in the
    /// token list, its TVL clears the floor and it is unpaused.
    pub(super) fn pool_tokens(&self, pool: &MetricMetadata) -> Option<(&Token, &Token)> {
        let tokens = self
            .book_config
            .pair_tokens(&pool.token0, &pool.token1)?;
        // v1 metadata carries the USD TVL directly, so no cross-pool price normalization is
        // needed.
        let clears_floor = self
            .book_config
            .clears_min_tvl(pool.tvl_fiat.unwrap_or(0.0), &pool.pool_address);
        (clears_floor && is_unpaused(pool)).then_some(tokens)
    }

    pub(super) fn collect(&self, prices: MetricPrices) -> Books {
        let mut books = Books::new();
        for MetricPoolPrice { metadata, bid_ask } in prices.pools {
            let Some((token0, token1)) = self.pool_tokens(&metadata) else {
                continue;
            };
            if !bid_ask.is_quotable() {
                debug!(pool = %metadata.pool_address, "skipping pool, not quotable");
                continue;
            }

            let component_id = metadata.pool_address.clone();
            let updated_at = DateTime::from_timestamp(bid_ask.server_ts as i64, 0);
            let state = MetricState {
                base_token: token0.clone(),
                quote_token: token1.clone(),
                metadata,
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
        books
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc, time::Duration};

    use chrono::DateTime;
    use rstest::rstest;
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use super::*;
    use crate::{
        book::{assert_same_books, BookFeedConfig},
        pamm::protocols::metric::{
            client::{MetricClient, DEFAULT_BID_ASK_CONCURRENCY},
            feed::{MetricBookAssemblerBuilder, MetricFeedBuilder},
            test_support::{
                assembler_for, batch_body, bid_ask_json, error_entry, metadata_page, pool_json,
                pool_server, prices_source, quote_entry, requested_pools,
                serving_weth_usdc_above_100_usd, weth_usdc_book_config, BATCH_TARGET, BELOW_FLOOR,
                FIRST_POOL, PAUSED, SECOND_POOL, SERVER_TS, UNKNOWN_POOL, UNQUOTABLE,
            },
        },
        snapshot_feed::http::test_support::{spawn_http_server, MockResponse},
    };

    /// How the venue answers a key it has stopped accepting. It is about this request, so a
    /// later poll may be served.
    const REFUSED_KEY: (&str, &str) = ("401 Unauthorized", r#"{"message":"invalid api key"}"#);

    /// How the venue answers for a chain it does not serve, which no later poll reaches either.
    const UNSERVED_CHAIN: (&str, &str) = ("404 Not Found", r#"{"error": "Unknown chainId: 1"}"#);

    /// A source against the Metric deployment at `base_url`, with an empty token universe and no
    /// TVL floor.
    fn source(chain: Chain, base_url: &str, api_key: &str) -> MetricBookSource {
        MetricBookSource {
            prices: prices_source(chain, base_url, api_key),
            assembler: assembler_for(chain),
        }
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
        let mut source = source(Chain::Ethereum, &server.url(), "test_key");
        source.assembler = MetricBookAssemblerBuilder::new(weth_usdc_book_config())
            .with_fallback_router()
            .build();

        let books = source.fetch().await.unwrap().books;

        let book = books.values().next().expect("one book");
        assert_eq!(book.component.protocol_system, "fallback:rfq:metric");
        // A Metric component is identified by its pool address, whatever the system labels it.
        assert_eq!(book.component.id.to_string(), POOL);
    }

    /// Pools outside the token list, under the TVL floor or paused are never asked for a quote.
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
        let quoted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&quoted);
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(("200 OK", page.clone()));
            }
            assert_eq!(request.target, BATCH_TARGET);
            let requested = requested_pools(request);
            seen.lock()
                .unwrap()
                .extend(requested.clone());
            let entries = requested
                .into_iter()
                .filter_map(|pool| match pool.as_str() {
                    FAILING => Some(error_entry(&pool, 500, "boom")),
                    UNQUOTABLE => Some(quote_entry(&pool, &bid_ask_json(SERVER_TS, true))),
                    UNANSWERED => None,
                    _ => Some(quote_entry(&pool, &bid_ask_json(SERVER_TS, false))),
                })
                .collect();
            Some(("200 OK", batch_body(entries)))
        })
        .await;
        let endpoint = server.url();
        let mut source = source(Chain::Ethereum, &endpoint, "key");
        source.assembler = serving_weth_usdc_above_100_usd();

        let books = source.fetch().await.unwrap().books;

        let mut quoted = quoted.lock().unwrap().clone();
        quoted.sort();
        assert_eq!(quoted, [HEALTHY, FAILING, UNQUOTABLE, UNANSWERED]);
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
        source.assembler = serving_weth_usdc_above_100_usd();

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
        source.assembler = serving_weth_usdc_above_100_usd();

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
        source.assembler = serving_weth_usdc_above_100_usd();
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
        let client = MetricClient::new(
            Some("https://api.metric.xyz"),
            Chain::Base.try_id().unwrap(),
            api_key,
            DEFAULT_BID_ASK_CONCURRENCY,
        );
        let metadata = client.fetch_metadata().await.unwrap();
        assert!(!metadata.is_empty(), "Metric serves no Base pools");

        let tokens = metadata
            .iter()
            .flat_map(|pool| [pool.token0.clone(), pool.token1.clone()])
            .map(|address| {
                let token = Token::new(&address, "TKN", 18, 0, &[], Chain::Base, 100);
                (address, token)
            })
            .collect();
        let source = MetricBookSource {
            prices: MetricPricesSource::new(client),
            assembler: MetricBookAssemblerBuilder::new(BookFeedConfig {
                chain: Chain::Base,
                tokens: Arc::new(tokens),
                min_tvl_usd: 0.0,
            })
            .build(),
        };

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

    /// Books rebuilt from serialized prices match the source's published books for the same
    /// poll.
    #[tokio::test]
    async fn prices_round_trip_builds_the_published_books() {
        let server = pool_server().await;
        let source = MetricBookSource {
            prices: prices_source(Chain::Ethereum, &server.url(), "key"),
            assembler: serving_weth_usdc_above_100_usd(),
        };
        let published = source.fetch().await.unwrap().books;
        let prices = source.prices.fetch().await.unwrap();

        let sent = serde_json::to_string(&prices).unwrap();
        let received: MetricPrices = serde_json::from_str(&sent).unwrap();

        assert_eq!(received, prices);
        assert_same_books(&source.assembler.books(received), &published);
        assert_eq!(published.len(), 1, "only the healthy pool is servable");
    }

    #[tokio::test]
    async fn a_prices_poll_fails_when_every_pool_does() {
        let page = metadata_page(&[pool_json(FIRST_POOL, Some(1_000.0))], None);
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(("200 OK", page.clone()));
            }
            Some(("200 OK", batch_body(vec![error_entry(FIRST_POOL, 500, "boom")])))
        })
        .await;

        let result = prices_source(Chain::Ethereum, &server.url(), "key")
            .fetch()
            .await;

        assert!(matches!(result, Err(FeedError::Connection(_))), "got {result:?}");
    }

    /// The assembler keeps a pool only between served tokens, above the floor, unpaused and with
    /// a quote it can serve.
    #[test]
    fn the_assembler_drops_pools_it_cannot_serve() {
        const UNKNOWN_TOKEN: &str = "0x9999999999999999999999999999999999999999";
        let assembler = serving_weth_usdc_above_100_usd();
        let mut paused = pool_json(PAUSED, Some(1_000.0));
        paused["pauseLevel"] = serde_json::json!(1);
        let mut unknown = pool_json(UNKNOWN_POOL, Some(1_000.0));
        unknown["token0"] = serde_json::json!(UNKNOWN_TOKEN);
        let price = |metadata: serde_json::Value, unhealthy: bool| MetricPoolPrice {
            metadata: serde_json::from_value(metadata).unwrap(),
            bid_ask: serde_json::from_str(&bid_ask_json(SERVER_TS, unhealthy)).unwrap(),
        };
        let prices = MetricPrices {
            pools: vec![
                price(pool_json(FIRST_POOL, Some(1_000.0)), false),
                price(pool_json(UNQUOTABLE, Some(1_000.0)), true),
                price(pool_json(BELOW_FLOOR, Some(99.0)), false),
                price(paused, false),
                price(unknown, false),
            ],
        };

        let books = assembler.books(prices);

        let healthy_id = Bytes::from_str(FIRST_POOL)
            .unwrap()
            .to_string();
        assert_eq!(books.keys().collect::<Vec<_>>(), vec![&healthy_id]);
    }
}
