use std::{
    collections::HashMap,
    mem,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use chrono::DateTime;
use tokio::time::{timeout, Instant};
use tracing::{debug, warn};
use tycho_common::models::token::Token;

use crate::{
    book::{component::pair_book, Book, BookFeedConfig, BookSnapshot, Books, ReceivedAt},
    pamm::protocols::metric::{
        client::{MetricClient, METRIC_DEFAULT_MIN_TVL_USD},
        models::{MetricMetadata, MetricPoolPrice, MetricPrices},
        state::MetricState,
        PROTOCOL_TYPE,
    },
    snapshot_feed::{errors::FeedError, http::HttpSource},
};

/// Fetches one complete Metric book per poll: every unpaused, quotable pool between the
/// configured tokens whose USD TVL clears the threshold, as simulate-ready books built by its
/// [`MetricBookAssembler`]. Pause level and TVL are as of the last metadata refresh; whether a
/// pool quotes is as of this poll. `Debug` output omits the API key.
#[derive(Debug, Clone)]
pub struct MetricBookSource {
    pub(super) prices: MetricPricesSource,
    pub(super) assembler: MetricBookAssembler,
}

impl HttpSource for MetricBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// Losing every pool fails the poll: a book that reads as nothing withdraws every pair the
    /// venue was serving, which is an outage rather than a poll that worked. Losing some of the
    /// pools only drops those from this book. A failed metadata refresh fails it only as
    /// [`MetricPricesSource::poll`] says.
    ///
    /// Pools outside the token list, under the TVL floor or paused are not quoted.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let prices = self
            .prices
            .poll(self.assembler.book_config.min_tvl_usd, |pool| {
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

/// How long a poll quotes the pools of the metadata it last fetched before fetching it again.
/// Quotes change about every 2 s, but Metric's periodic capture reads a pool's status only about
/// every 30 s, so fetching metadata on every poll would mostly fetch it unchanged. Refreshing at
/// half that capture interval costs one request in seven polls and halves how late a pool Metric
/// adds or unpauses reaches the books. Each quote's own status is still checked on every poll,
/// and a paused pool's quote reports `pool_paused` with no price and no depth.
const METADATA_REFRESH_AFTER: Duration = Duration::from_secs(15);

/// How long fetched metadata stands in for refreshes that fail. A failed refresh is retried one
/// [`METADATA_REFRESH_AFTER`] later rather than on the next poll, so a metadata endpoint that is
/// down or rate-limiting is asked no more often than a healthy one, and five failed refreshes in a
/// row cost no quotes. Past this age a failed refresh fails the poll, so an endpoint that stays
/// broken (an outage, or an answer that no longer parses) withdraws the books and shows as a
/// failing feed instead of quoting a pool list that no longer changes.
const METADATA_SERVE_FOR: Duration = Duration::from_secs(90);

/// Deadline for a refresh while fetched metadata can stand in for it: a refresh that misses it
/// is a failed refresh, and the poll quotes the metadata it has. With the 3 s batch bid/ask
/// deadline after it, a poll that refreshes still ends inside the feed's 5 s `request_timeout`. A
/// poll with no metadata to stand in waits for its fetch within the whole poll's deadline instead,
/// since it has no pools to quote without it.
const METADATA_REFRESH_TIMEOUT: Duration = Duration::from_millis(1500);

/// Reads the pools' metadata and quotes through a [`MetricClient`].
#[derive(Clone, derive_more::Debug)]
pub(super) struct MetricPricesSource {
    client: MetricClient,
    #[debug(skip)]
    metadata: MetadataCache,
}

/// The metadata a poll last fetched, kept to quote its pools until it is due for a refresh.
/// Clones of a source share it.
#[derive(Clone)]
struct MetadataCache {
    refresh_after: Duration,
    serve_for: Duration,
    refresh_timeout: Duration,
    last: Arc<Mutex<Option<FetchedMetadata>>>,
}

#[derive(Clone)]
struct FetchedMetadata {
    /// The TVL floor it was fetched with; metadata for another floor lists other pools.
    min_tvl_usd: f64,
    fetched_at: Instant,
    /// When a poll next refreshes it: `refresh_after` past the last attempt, whether that
    /// attempt fetched it or failed.
    refresh_at: Instant,
    pools: Arc<[MetricMetadata]>,
}

impl MetadataCache {
    /// The metadata last fetched with `min_tvl_usd`.
    fn get(&self, min_tvl_usd: f64) -> Option<FetchedMetadata> {
        let last = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        // The floor is the caller's own setting, never computed, so it compares exactly.
        #[expect(clippy::float_cmp, reason = "the floor is a setting compared with itself")]
        let same_floor = last.min_tvl_usd == min_tvl_usd;
        same_floor.then_some(last)
    }

    fn store(&self, fetched: FetchedMetadata) {
        *self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(fetched);
    }
}

impl MetricPricesSource {
    pub(super) fn new(client: MetricClient) -> Self {
        Self {
            client,
            metadata: MetadataCache {
                refresh_after: METADATA_REFRESH_AFTER,
                serve_for: METADATA_SERVE_FOR,
                refresh_timeout: METADATA_REFRESH_TIMEOUT,
                last: Arc::default(),
            },
        }
    }

    /// Quotes the pools with a USD TVL of at least `min_tvl_usd` for which `keep` holds, listed
    /// by the metadata this source last fetched, which it refreshes [`METADATA_REFRESH_AFTER`]
    /// after its last attempt.
    ///
    /// A failed refresh fails the poll only once the metadata it has is [`METADATA_SERVE_FOR`]
    /// old, or when there is none, or when no retry can answer differently. Losing every pool
    /// fails the poll; losing some only drops those from the prices.
    pub(super) async fn poll(
        &self,
        min_tvl_usd: f64,
        keep: impl Fn(&MetricMetadata) -> bool,
    ) -> Result<MetricPrices, FeedError> {
        let metadata = self.metadata(min_tvl_usd).await?;
        let pools = metadata
            .iter()
            .filter(|pool| keep(pool))
            .cloned()
            .collect();
        quote_pools(&self.client, pools).await
    }

    /// The metadata to quote this poll: the last fetched until it is due for a refresh, a refresh
    /// once it is, and the last fetched again while it can stand in for a refresh that failed.
    async fn metadata(&self, min_tvl_usd: f64) -> Result<Arc<[MetricMetadata]>, FeedError> {
        let now = Instant::now();
        let last = self.metadata.get(min_tvl_usd);
        if let Some(last) = &last {
            if now < last.refresh_at {
                return Ok(Arc::clone(&last.pools));
            }
        }
        let standing_in =
            last.filter(|last| now.duration_since(last.fetched_at) < self.metadata.serve_for);

        let fetch = self.client.fetch_metadata(min_tvl_usd);
        let deadline = self.metadata.refresh_timeout;
        let refreshed = match standing_in {
            None => fetch.await,
            Some(_) => timeout(deadline, fetch)
                .await
                .unwrap_or_else(|_elapsed| {
                    Err(FeedError::Connection(format!(
                        "Metric metadata refresh timed out after {deadline:?}"
                    )))
                }),
        };

        match (refreshed, standing_in) {
            (Ok(pools), _) => {
                let pools: Arc<[MetricMetadata]> = pools.into();
                self.metadata.store(FetchedMetadata {
                    min_tvl_usd,
                    fetched_at: now,
                    refresh_at: now + self.metadata.refresh_after,
                    pools: Arc::clone(&pools),
                });
                Ok(pools)
            }
            (Err(error), Some(last)) if !error.is_fatal() => {
                warn!(
                    %error,
                    age_secs = now.duration_since(last.fetched_at).as_secs(),
                    "Metric metadata refresh failed, quoting the pools it last listed"
                );
                let pools = Arc::clone(&last.pools);
                self.metadata.store(FetchedMetadata {
                    refresh_at: now + self.metadata.refresh_after,
                    ..last
                });
                Ok(pools)
            }
            (Err(error), _) => Err(error),
        }
    }
}

impl HttpSource for MetricPricesSource {
    type Snapshot = MetricPrices;

    async fn fetch(&self) -> Result<MetricPrices, FeedError> {
        self.poll(METRIC_DEFAULT_MIN_TVL_USD, is_unpaused)
            .await
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
            let Some(updated_at) = i64::try_from(bid_ask.server_ts)
                .ok()
                .and_then(|secs| DateTime::from_timestamp(secs, 0))
            else {
                warn!(
                    pool = %metadata.pool_address,
                    server_ts = bid_ask.server_ts,
                    "skipping pool, server timestamp out of range"
                );
                continue;
            };
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
                Some(updated_at),
            ));
        }
        books
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

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
        snapshot_feed::http::test_support::{spawn_http_server, MockHttpServer, MockResponse},
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
            Some(("200 OK", batch_body(&[quote_entry(POOL, &bid_ask_json(SERVER_TS, false))])))
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
            let entries: Vec<_> = requested
                .into_iter()
                .filter_map(|pool| match pool.as_str() {
                    FAILING => Some(error_entry(&pool, 500, "boom")),
                    UNQUOTABLE => Some(quote_entry(&pool, &bid_ask_json(SERVER_TS, true))),
                    UNANSWERED => None,
                    _ => Some(quote_entry(&pool, &bid_ask_json(SERVER_TS, false))),
                })
                .collect();
            Some(("200 OK", batch_body(&entries)))
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
        assert_eq!(book.updated_at, DateTime::from_timestamp(i64::try_from(SERVER_TS).unwrap(), 0));
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
            let entries: Vec<_> = requested_pools(request)
                .iter()
                .map(|pool| {
                    let error = if pool == FIRST_POOL { errors[0] } else { errors[1] };
                    error_entry(pool, 404, error)
                })
                .collect();
            Some(("200 OK", batch_body(&entries)))
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
            let entries: Vec<_> = requested
                .iter()
                .map(|pool| quote_entry(pool, &bid_ask_json(SERVER_TS, false)))
                .collect();
            Some(("200 OK", batch_body(&entries)))
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
                body: batch_body(&[]),
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
        let metadata = client
            .fetch_metadata(0.0)
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

    /// The book source asks Metric for its own book floor; the prices source, which has no book
    /// settings, asks for the floor Metric applies by default.
    #[tokio::test]
    async fn each_source_asks_metric_for_its_tvl_floor() {
        let page = metadata_page(&[], None);
        let floors = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&floors);
        let server = spawn_http_server(move |request| {
            let floor = request
                .target
                .split("minTvlFiat=")
                .nth(1)?
                .split('&')
                .next()?
                .to_string();
            recorded.lock().unwrap().push(floor);
            Some(("200 OK", page.clone()))
        })
        .await;
        let book_source = MetricBookSource {
            prices: prices_source(Chain::Ethereum, &server.url(), "key"),
            assembler: serving_weth_usdc_above_100_usd(),
        };

        book_source.fetch().await.unwrap();
        book_source
            .prices
            .fetch()
            .await
            .unwrap();

        assert_eq!(*floors.lock().unwrap(), vec!["100", "10"]);
    }

    /// A Metric deployment listing [`FIRST_POOL`] and quoting it healthy, whose n-th metadata
    /// request (from 0) gets `metadata(n)`. Returns it with its count of metadata and of
    /// bid/ask requests.
    async fn metric_answering_metadata(
        metadata: impl Fn(usize) -> MockResponse + Send + Sync + 'static,
    ) -> (MockHttpServer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let metadata_requests = Arc::new(AtomicUsize::new(0));
        let bid_ask_requests = Arc::new(AtomicUsize::new(0));
        let (listed, quoted) = (Arc::clone(&metadata_requests), Arc::clone(&bid_ask_requests));
        let server = spawn_http_server(move |request| {
            if request
                .target
                .starts_with("/public/v1/evm/1/metadata?")
            {
                return Some(metadata(listed.fetch_add(1, Ordering::SeqCst)));
            }
            quoted.fetch_add(1, Ordering::SeqCst);
            let entries = [quote_entry(FIRST_POOL, &bid_ask_json(SERVER_TS, false))];
            Some(MockResponse::from(("200 OK", batch_body(&entries))))
        })
        .await;
        (server, metadata_requests, bid_ask_requests)
    }

    /// The first metadata request lists [`FIRST_POOL`]; every later one gets `then`.
    fn listing_first_pool_then(then: fn() -> MockResponse) -> impl Fn(usize) -> MockResponse {
        move |nth| {
            if nth == 0 {
                let page = metadata_page(&[pool_json(FIRST_POOL, Some(1_000.0))], None);
                MockResponse::from(("200 OK", page))
            } else {
                then()
            }
        }
    }

    fn listing_nothing() -> MockResponse {
        MockResponse::from(("200 OK", metadata_page(&[], None)))
    }

    fn failing() -> MockResponse {
        MockResponse::from(("500 Internal Server Error", "boom".to_string()))
    }

    fn refusing_the_chain() -> MockResponse {
        let (status, body) = UNSERVED_CHAIN;
        MockResponse::from((status, body.to_string()))
    }

    fn hanging() -> MockResponse {
        MockResponse {
            status: "200 OK",
            body: metadata_page(&[], None),
            delay: Duration::from_secs(3600),
        }
    }

    /// A book source against `server` serving WETH/USDC above 100 USD, refreshing its metadata
    /// once it is `refresh_after` old and quoting the last fetched for `serve_for`.
    fn caching_source(
        server: &MockHttpServer,
        refresh_after: Duration,
        serve_for: Duration,
    ) -> MetricBookSource {
        let mut source = source(Chain::Ethereum, &server.url(), "key");
        source.assembler = serving_weth_usdc_above_100_usd();
        source.prices.metadata.refresh_after = refresh_after;
        source.prices.metadata.serve_for = serve_for;
        source
    }

    const LONG: Duration = Duration::from_secs(3600);

    /// Polls between refreshes quote the pools the last metadata listed without asking for it
    /// again, so only the quotes are fetched at the poll cadence.
    #[tokio::test]
    async fn polls_between_refreshes_quote_the_pools_last_listed() {
        let (server, metadata_requests, bid_ask_requests) =
            metric_answering_metadata(listing_first_pool_then(listing_nothing)).await;
        let source = caching_source(&server, LONG, LONG);

        let first = source.fetch().await.unwrap().books;
        let second = source.fetch().await.unwrap().books;

        assert_eq!(metadata_requests.load(Ordering::SeqCst), 1);
        assert_eq!(bid_ask_requests.load(Ordering::SeqCst), 2);
        assert_eq!(second.len(), 1);
        assert_same_books(&second, &first);
    }

    /// Metadata due for a refresh is fetched again, and the pools the refresh lists are the ones
    /// quoted.
    #[tokio::test]
    async fn metadata_due_for_a_refresh_is_fetched_again() {
        let (server, metadata_requests, _) =
            metric_answering_metadata(listing_first_pool_then(listing_nothing)).await;
        let source = caching_source(&server, Duration::ZERO, LONG);

        source.fetch().await.unwrap();
        let after_refresh = source.fetch().await.unwrap().books;

        assert_eq!(metadata_requests.load(Ordering::SeqCst), 2);
        assert!(after_refresh.is_empty(), "the refresh listed no pools");
    }

    /// A refresh that fails or misses its deadline costs no quotes while the metadata last
    /// fetched can stand in for it; the poll quotes the pools it listed.
    #[rstest]
    #[case::failing(failing)]
    #[case::hanging(hanging)]
    #[tokio::test]
    async fn a_failed_refresh_quotes_the_pools_last_listed(#[case] refresh: fn() -> MockResponse) {
        let (server, metadata_requests, _) =
            metric_answering_metadata(listing_first_pool_then(refresh)).await;
        let mut source = caching_source(&server, Duration::ZERO, LONG);
        source.prices.metadata.refresh_timeout = Duration::from_millis(50);
        let first = source.fetch().await.unwrap().books;

        let second = source.fetch().await.unwrap().books;

        assert_eq!(metadata_requests.load(Ordering::SeqCst), 2);
        assert_same_books(&second, &first);
    }

    /// Makes the metadata `source` last fetched due for a refresh on its next poll.
    fn make_due(source: &MetricBookSource) {
        let mut last = source
            .prices
            .metadata
            .last
            .lock()
            .unwrap();
        last.as_mut()
            .expect("a poll has stored its metadata")
            .refresh_at = Instant::now();
    }

    /// A failed refresh is retried a refresh interval later, not on the next poll, so a metadata
    /// endpoint that is down or rate-limiting is asked no more often than a healthy one.
    #[tokio::test]
    async fn a_failed_refresh_is_retried_a_refresh_interval_later() {
        let (server, metadata_requests, bid_ask_requests) =
            metric_answering_metadata(listing_first_pool_then(failing)).await;
        let source = caching_source(&server, LONG, LONG);
        source.fetch().await.unwrap();
        make_due(&source);

        let after_failure = source.fetch().await.unwrap().books;
        let next = source.fetch().await.unwrap().books;

        assert_eq!(metadata_requests.load(Ordering::SeqCst), 2);
        assert_eq!(bid_ask_requests.load(Ordering::SeqCst), 3);
        assert_eq!(after_failure.len(), 1);
        assert_same_books(&next, &after_failure);
    }

    /// Metadata stands in for failed refreshes only so long: past that, and for a failure no
    /// retry can answer differently, the refresh's failure is the poll's.
    #[rstest]
    #[case::once_it_is_too_old(failing, Duration::ZERO, false)]
    #[case::when_the_venue_refuses_the_chain(refusing_the_chain, LONG, true)]
    #[tokio::test]
    async fn a_failed_refresh_fails_the_poll(
        #[case] refresh: fn() -> MockResponse,
        #[case] serve_for: Duration,
        #[case] fatal: bool,
    ) {
        let (server, _, _) = metric_answering_metadata(listing_first_pool_then(refresh)).await;
        let source = caching_source(&server, Duration::ZERO, serve_for);
        source.fetch().await.unwrap();

        let error = source
            .fetch()
            .await
            .expect_err("the refresh's failure must fail the poll");

        assert_eq!(error.is_fatal(), fatal, "{error}");
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
            Some(("200 OK", batch_body(&[error_entry(FIRST_POOL, 500, "boom")])))
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
