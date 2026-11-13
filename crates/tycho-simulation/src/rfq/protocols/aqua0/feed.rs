use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use num_bigint::BigUint;
use tycho_common::Bytes;

use crate::{
    book::{chain_id, BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::aqua0::{client::Aqua0Client, source::Aqua0BookSource, AQUA0_API_URL},
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Aqua0's book feed: every market Aqua0 lists on the chain whose tokens are in the configured
/// universe, each priced from the backend's sampled fills. Built through [`Aqua0FeedBuilder`].
#[derive(Debug)]
pub struct Aqua0Feed {
    feed_config: HttpFeedConfig,
    source: Aqua0BookSource,
}

/// Builds an [`Aqua0Feed`] from the shared [`BookFeedConfig`]. Aqua0's RFQ API takes no
/// credentials.
pub struct Aqua0FeedBuilder {
    book_config: BookFeedConfig,
    base_url: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
    markets_refresh: Duration,
    sample_sizes: HashMap<Bytes, Vec<BigUint>>,
}

impl Aqua0FeedBuilder {
    pub fn new(book_config: BookFeedConfig) -> Self {
        Self {
            book_config,
            base_url: AQUA0_API_URL.to_string(),
            feed_config: Self::default_feed_config(),
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            markets_refresh: Duration::from_secs(60),
            sample_sizes: HashMap::new(),
        }
    }

    /// The shared HTTP feed tuning. Aqua0's levels expire 10 seconds after the backend prices
    /// them, so a poll interval above that leaves gaps where nothing is quotable.
    pub fn default_feed_config() -> HttpFeedConfig {
        default_http_feed_config()
    }

    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Another Aqua0 deployment, e.g. a staging API or a local server.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// How often the market list is read again; markets it drops leave the book. One minute by
    /// default.
    pub fn markets_refresh(mut self, refresh: Duration) -> Self {
        self.markets_refresh = refresh;
        self
    }

    /// The input sizes, in the token's smallest unit, to sample when selling `token`; at most
    /// eight. A token without an entry samples 1, 10, 100, 1,000 and 10,000 whole units.
    pub fn sample_sizes(mut self, token: Bytes, sizes: Vec<BigUint>) -> Self {
        self.sample_sizes.insert(token, sizes);
        self
    }

    /// Fails for a chain with no numeric id, or an invalid sample list. Whether Aqua0 executes on
    /// the chain is the backend's answer, and the first poll carries it.
    pub fn build(self) -> Result<Aqua0Feed, FeedError> {
        let chain_id = chain_id(self.book_config.chain)?;
        for (token, sizes) in &self.sample_sizes {
            if sizes.is_empty() ||
                sizes.len() > 8 ||
                sizes
                    .iter()
                    .any(|size| size == &BigUint::default())
            {
                return Err(FeedError::InvalidInput(format!(
                    "Aqua0 sample sizes for {token} must be one to eight positive amounts"
                )));
            }
        }
        if self.markets_refresh.is_zero() {
            return Err(FeedError::InvalidInput("markets_refresh must not be zero".into()));
        }
        let client = Aqua0Client::new(
            &self.base_url,
            chain_id,
            self.feed_config.request_timeout,
            self.quote_timeout,
        );
        Ok(Aqua0Feed {
            feed_config: self.feed_config,
            source: Aqua0BookSource::new(
                Arc::new(client),
                self.book_config,
                Arc::new(self.sample_sizes),
                self.markets_refresh,
            ),
        })
    }
}

impl SnapshotFeed for Aqua0Feed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}
