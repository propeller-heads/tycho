use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::Bytes;

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::native::{
            client::NativeClient, models::NativeSupportedChain, source::NativeBookSource,
        },
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Native Relay's swap API; the orderbook and firm-quote paths are appended per request.
const NATIVE_API_URL: &str = "https://v2.api.native.org/swap-api-v2/v1";

/// Native Relay's book feed: polls the aggregated orderbook and publishes every poll as a
/// complete [`BookSnapshot`]. Built through [`NativeFeedBuilder`].
#[derive(Clone, Debug)]
pub struct NativeFeed {
    feed_config: HttpFeedConfig,
    source: NativeBookSource,
}

/// Builds a [`NativeFeed`] from the shared [`BookFeedConfig`] and Native's API key.
pub struct NativeFeedBuilder {
    book_config: BookFeedConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    api_key: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
}

impl NativeFeedBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        api_key: String,
    ) -> Self {
        Self {
            book_config,
            usd_quote_tokens: usd_quote_tokens.into(),
            api_key,
            feed_config: Self::default_feed_config(),
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
        }
    }

    /// The feed tuning a new builder starts from: the shared values every HTTP feed uses
    /// (see [`HttpFeedConfig`]). Spread from it to change individual fields:
    /// `HttpFeedConfig { poll_interval: Duration::from_secs(10),
    /// ..NativeFeedBuilder::default_feed_config() }`.
    pub fn default_feed_config() -> HttpFeedConfig {
        default_http_feed_config()
    }

    /// Tune the price feed loop (poll cadence, request timeout, failure limit)
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Deadline for binding quote requests. Default: 5 s.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Fails for chains Native does not serve.
    pub fn build(self) -> Result<NativeFeed, FeedError> {
        let chain =
            NativeSupportedChain::try_from(self.book_config.chain).map_err(FeedError::Fatal)?;
        Ok(NativeFeed {
            feed_config: self.feed_config,
            source: NativeBookSource {
                client: Arc::new(NativeClient::new(
                    chain,
                    NATIVE_API_URL.to_string(),
                    self.api_key,
                    self.quote_timeout,
                )),
                book_config: self.book_config,
                usd_quote_tokens: self.usd_quote_tokens,
            },
        })
    }
}

impl SnapshotFeed for NativeFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}
