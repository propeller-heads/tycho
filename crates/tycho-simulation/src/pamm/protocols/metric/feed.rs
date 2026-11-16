//! Metric book feed for the `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz>

use std::{future::Future, num::NonZeroUsize, time::Duration};

use reqwest::Url;
use tycho_common::models::Chain;

use crate::{
    book::{chain_id, BookFeedConfig, BookSnapshot, ReceivedAt},
    pamm::protocols::metric::{
        client::{MetricClient, DEFAULT_BID_ASK_CONCURRENCY},
        models::MetricPrices,
        source::{MetricBookAssembler, MetricBookSource, MetricPricesSource},
        FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Metric's book feed: polls the metadata and batch bid/ask endpoints and publishes every poll
/// as a complete [`BookSnapshot`]. Built through [`MetricFeedBuilder`]. `Debug` output omits the
/// API key.
#[derive(Debug, Clone)]
pub struct MetricFeed {
    feed_config: HttpFeedConfig,
    source: MetricBookSource,
}

/// Builds a [`MetricFeed`] from the shared [`BookFeedConfig`] and the Bearer trading key. The feed
/// talks to Metric's public API unless [`base_url`](Self::base_url) says otherwise. The book
/// settings are those of a [`MetricBookAssemblerBuilder`];
/// [`with_assembler`](Self::with_assembler) supplies a built assembler instead.
pub struct MetricFeedBuilder {
    assembler_builder: MetricBookAssemblerBuilder,
    assembler: Option<MetricBookAssembler>,
    base_url: Option<String>,
    api_key: String,
    bid_ask_concurrency: NonZeroUsize,
    feed_config: HttpFeedConfig,
}

impl MetricFeedBuilder {
    /// `api_key` is the Bearer trading key the batch bid/ask endpoint requires; without a
    /// valid one no pool is quotable and the feed publishes empty books.
    pub fn new(book_config: BookFeedConfig, api_key: String) -> Self {
        Self {
            assembler_builder: MetricBookAssemblerBuilder::new(book_config),
            assembler: None,
            base_url: None,
            api_key,
            bid_ask_concurrency: DEFAULT_BID_ASK_CONCURRENCY,
            feed_config: Self::default_feed_config(),
        }
    }

    /// Makes the feed's books with `assembler` instead of one built from this builder's book
    /// settings, which are then unused.
    pub fn with_assembler(mut self, assembler: MetricBookAssembler) -> Self {
        self.assembler = Some(assembler);
        self
    }

    /// Points the feed at another Metric deployment; [`build`](Self::build) rejects a value
    /// that is not a URL.
    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = Some(base_url);
        self
    }

    /// How many of a poll's batch bid/ask requests run at once.
    ///
    /// Each request quotes up to 100 pools, so this only matters for a venue serving more than
    /// that: it keeps a poll's duration flat as the venue adds pools instead of growing until the
    /// loop's `request_timeout` discards the whole poll. Lower it to be gentler on the API.
    pub fn bid_ask_concurrency(mut self, requests_in_flight: NonZeroUsize) -> Self {
        self.bid_ask_concurrency = requests_in_flight;
        self
    }

    /// The feed tuning a new builder starts from.
    ///
    /// Measured live, Metric's quotes change about every 2 s, and a poll — a page of pool
    /// metadata, then a batch of bid/asks — answers in 0.6-1.5 s, so it is polled every 2 s:
    /// once per quote change, and longer than a slow poll, which a shorter interval would follow
    /// with the next poll straight away. A poll that hangs gives up after 5 s, and a book is
    /// withdrawn once 15 s pass without a refresh: longer than one hung poll and its retry, so a
    /// single slow poll does not blank the book.
    ///
    /// Spread from it to change one field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::pamm::protocols::metric::feed::MetricFeedBuilder;
    /// # use tycho_simulation::snapshot_feed::http::HttpFeedConfig;
    /// let tuned = HttpFeedConfig {
    ///     poll_interval: Duration::from_secs(10),
    ///     ..MetricFeedBuilder::default_feed_config()
    /// };
    /// ```
    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            max_snapshot_age: Some(Duration::from_secs(15)),
            ..default_http_feed_config()
        }
    }

    /// Settles this venue's swaps through Tycho's `MetricFallbackRouter`, which retries
    /// against a solver-named fallback pool when the venue reverts. Components then carry
    /// [`FALLBACK_PROTOCOL_SYSTEM`](super::FALLBACK_PROTOCOL_SYSTEM) rather than
    /// [`PROTOCOL_SYSTEM`](super::PROTOCOL_SYSTEM), which is what selects the fallback encoder.
    /// Off by default.
    pub fn with_fallback_router(mut self) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .with_fallback_router();
        self
    }

    /// Tune the price feed loop (poll cadence, request timeout, failure limit)
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails for an empty trading key, for a base URL that is not one, and for a chain with no
    /// numeric chain id, which is all Metric's API is addressed by. Whether the venue serves
    /// that chain is Metric's own answer, and the first poll carries it.
    pub fn build(self) -> Result<MetricFeed, FeedError> {
        let chain = self.assembler_builder.book_config.chain;
        let mut prices_builder = MetricPricesFeedBuilder::new(chain, self.api_key)
            .bid_ask_concurrency(self.bid_ask_concurrency)
            .feed_config(self.feed_config);
        if let Some(base_url) = self.base_url {
            prices_builder = prices_builder.base_url(base_url);
        }
        let MetricPricesFeed { feed_config, source: prices } = prices_builder.build()?;
        let assembler = match self.assembler {
            Some(assembler) => assembler,
            None => self.assembler_builder.build(),
        };
        Ok(MetricFeed { feed_config, source: MetricBookSource { prices, assembler } })
    }
}

/// Builds a [`MetricBookAssembler`] from the book settings alone: the shared [`BookFeedConfig`]
/// and the protocol system. It needs no trading key, since it makes no API call. Pass the result
/// to [`MetricFeedBuilder::with_assembler`] or use it on its own with prices from a
/// [`MetricPricesFeed`].
pub struct MetricBookAssemblerBuilder {
    protocol_system: &'static str,
    book_config: BookFeedConfig,
}

impl MetricBookAssemblerBuilder {
    pub fn new(book_config: BookFeedConfig) -> Self {
        Self { protocol_system: PROTOCOL_SYSTEM, book_config }
    }

    /// See [`MetricFeedBuilder::with_fallback_router`].
    pub fn with_fallback_router(mut self) -> Self {
        self.protocol_system = FALLBACK_PROTOCOL_SYSTEM;
        self
    }

    pub fn build(self) -> MetricBookAssembler {
        MetricBookAssembler { protocol_system: self.protocol_system, book_config: self.book_config }
    }
}

impl SnapshotFeed for MetricFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}

/// Rejects empty bearer tokens before starting the feed. Metric reports `Unknown chainId` for both
/// an empty token and an unsupported chain, but returns 401 for a rejected nonempty key. Without
/// this check, an empty token would be mistaken for an unsupported chain and end the feed.
fn check_api_key(api_key: &str) -> Result<(), FeedError> {
    if api_key.is_empty() {
        return Err(FeedError::InvalidInput("Metric trading key is empty".to_string()));
    }
    Ok(())
}

/// Rejects a base URL that does not parse. Every poll would fail the same way, so the feed does
/// not start.
fn check_base_url(base_url: &str) -> Result<(), FeedError> {
    Url::parse(base_url)
        .map(drop)
        .map_err(|error| FeedError::InvalidInput(format!("Metric base URL {base_url}: {error}")))
}

/// Polls Metric's metadata and batch bid/ask endpoints and publishes every poll as
/// [`MetricPrices`]: each unpaused pool that answered its quote request. Requires only the chain
/// and trading key. [`MetricBookAssembler`] applies the token list, TVL floor and quote-usability
/// checks.
///
/// A poll succeeds while any pool answers; a pool that fails is left out, so a reader loses only
/// that pool's pairs. `Debug` output omits the API key.
#[derive(Debug, Clone)]
pub struct MetricPricesFeed {
    feed_config: HttpFeedConfig,
    source: MetricPricesSource,
}

/// Builds a [`MetricPricesFeed`] from the chain and the Bearer trading key. The feed talks to
/// Metric's public API unless [`base_url`](Self::base_url) says otherwise.
pub struct MetricPricesFeedBuilder {
    chain: Chain,
    api_key: String,
    base_url: Option<String>,
    bid_ask_concurrency: NonZeroUsize,
    feed_config: HttpFeedConfig,
}

impl MetricPricesFeedBuilder {
    /// The feed tuning starts from [`MetricFeedBuilder::default_feed_config`].
    pub fn new(chain: Chain, api_key: String) -> Self {
        Self {
            chain,
            api_key,
            base_url: None,
            bid_ask_concurrency: DEFAULT_BID_ASK_CONCURRENCY,
            feed_config: MetricFeedBuilder::default_feed_config(),
        }
    }

    /// Points the feed at another Metric deployment; [`build`](Self::build) rejects a value that
    /// is not a URL.
    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = Some(base_url);
        self
    }

    /// How many of a poll's batch bid/ask requests run at once. See
    /// [`MetricFeedBuilder::bid_ask_concurrency`].
    pub fn bid_ask_concurrency(mut self, requests_in_flight: NonZeroUsize) -> Self {
        self.bid_ask_concurrency = requests_in_flight;
        self
    }

    /// Tune the poll loop (poll cadence, request timeout, failure limit).
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails with [`FeedError::InvalidInput`] for an empty trading key or a base URL that is not
    /// one, and with [`FeedError::Fatal`] for a chain with no numeric chain id, which is all
    /// Metric's API is addressed by. Whether the venue serves that chain is Metric's own answer,
    /// and the first poll carries it.
    pub fn build(self) -> Result<MetricPricesFeed, FeedError> {
        check_api_key(&self.api_key)?;
        let chain_id = chain_id(self.chain)?;
        if let Some(base_url) = &self.base_url {
            check_base_url(base_url)?;
        }
        let client = MetricClient::new(
            self.base_url.as_deref(),
            chain_id,
            self.api_key,
            self.bid_ask_concurrency,
        );
        Ok(MetricPricesFeed {
            feed_config: self.feed_config,
            source: MetricPricesSource::new(client),
        })
    }
}

impl SnapshotFeed for MetricPricesFeed {
    type Snapshot = MetricPrices;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use futures::StreamExt;
    use tokio::time::timeout;

    use super::*;
    use crate::{
        book::{assert_same_books, chain_without_id},
        pamm::protocols::metric::test_support::{
            pool_server, pools_of, prices_source, weth_usdc_book_config, BELOW_FLOOR, FIRST_POOL,
            UNQUOTABLE,
        },
        snapshot_feed::{http::HttpSource, SnapshotFeedEvent, SnapshotFeedStream},
    };

    fn builder(chain: Chain) -> MetricFeedBuilder {
        MetricFeedBuilder::new(
            BookFeedConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 },
            "secret_key".to_string(),
        )
    }

    #[test]
    fn debug_output_omits_the_api_key() {
        let feed = builder(Chain::Ethereum)
            .base_url("https://metric.example".to_string())
            .build()
            .unwrap();
        let rendered = format!("{feed:?}");

        assert!(!rendered.contains("secret_key"));
        assert!(rendered.contains("metric.example"));
    }

    /// A base URL the venue cannot be reached at would fail every poll alike, so the feed does
    /// not start.
    #[test]
    fn a_base_url_that_is_not_one_is_rejected() {
        let error = builder(Chain::Ethereum)
            .base_url("api.metric.xyz".to_string())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    /// Metric answers an empty bearer token the way it refuses a chain, which no retry would
    /// recover from, so the feed does not start without a key.
    #[test]
    fn an_empty_trading_key_is_rejected() {
        let config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(HashMap::new()),
            min_tvl_usd: 0.0,
        };

        let error = MetricFeedBuilder::new(config, String::new())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[tokio::test]
    async fn the_prices_feed_publishes_each_poll() {
        let server = pool_server().await;
        let feed = MetricPricesFeedBuilder::new(Chain::Ethereum, "key".to_string())
            .base_url(server.url())
            .build()
            .unwrap();
        let mut stream = SnapshotFeedStream::spawn("test", feed);

        let event = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the feed publishes within the timeout")
            .expect("the stream is not over");

        let SnapshotFeedEvent::Published(prices) = event else {
            panic!("expected the first poll to be published");
        };
        assert_eq!(pools_of(&prices), [FIRST_POOL, UNQUOTABLE, BELOW_FLOOR]);
    }

    /// An assembler built on its own makes the books the feed builder's own assembler makes from
    /// the same settings.
    #[tokio::test]
    async fn the_assembler_builder_makes_the_books_the_feed_builder_does() {
        let server = pool_server().await;
        let prices = prices_source(Chain::Ethereum, &server.url(), "key")
            .fetch()
            .await
            .unwrap();
        let feed = MetricFeedBuilder::new(weth_usdc_book_config(), "key".to_string())
            .with_fallback_router()
            .build()
            .unwrap();
        let assembler = MetricBookAssemblerBuilder::new(weth_usdc_book_config())
            .with_fallback_router()
            .build();

        let books = assembler.books(prices.clone());

        assert_eq!(books.len(), 1);
        assert_same_books(&books, &feed.source.assembler.books(prices));
    }

    #[test]
    fn the_feed_makes_its_books_with_the_assembler_it_is_given() {
        let assembler = MetricBookAssemblerBuilder::new(weth_usdc_book_config())
            .with_fallback_router()
            .build();

        let feed = MetricFeedBuilder::new(weth_usdc_book_config(), "key".to_string())
            .with_assembler(assembler)
            .build()
            .unwrap();

        assert_eq!(feed.source.assembler.protocol_system, FALLBACK_PROTOCOL_SYSTEM);
    }

    #[test]
    fn the_prices_feed_rejects_an_empty_key() {
        let error = MetricPricesFeedBuilder::new(Chain::Ethereum, String::new())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[test]
    fn the_prices_feed_refuses_a_chain_with_no_numeric_chain_id() {
        let error = MetricPricesFeedBuilder::new(chain_without_id(), "key".to_string())
            .build()
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        assert!(
            error
                .to_string()
                .contains("no chain id"),
            "{error}"
        );
    }

    #[test]
    fn the_prices_feed_runs_on_the_feed_tuning_it_is_given() {
        let tuned = HttpFeedConfig {
            poll_interval: Duration::from_secs(7),
            ..MetricFeedBuilder::default_feed_config()
        };

        let feed = MetricPricesFeedBuilder::new(Chain::Ethereum, "key".to_string())
            .feed_config(tuned)
            .build()
            .unwrap();

        assert_eq!(feed.feed_config.poll_interval, Duration::from_secs(7));
    }

    #[test]
    fn the_prices_feed_rejects_an_invalid_base_url() {
        let error = MetricPricesFeedBuilder::new(Chain::Ethereum, "secret_key".to_string())
            .base_url("api.metric.xyz".to_string())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[test]
    fn the_prices_feed_debug_output_omits_the_api_key() {
        let feed = MetricPricesFeedBuilder::new(Chain::Ethereum, "secret_key".to_string())
            .base_url("https://metric.example".to_string())
            .build()
            .unwrap();
        let rendered = format!("{feed:?}");

        assert!(!rendered.contains("secret_key"));
        assert!(rendered.contains("metric.example"));
    }
}
