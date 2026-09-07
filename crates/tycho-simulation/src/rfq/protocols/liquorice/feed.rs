use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::{sync::watch, time::Duration};
use tycho_common::Bytes;

use super::source::LiquoriceBookSource;
use crate::{
    book::{
        errors::FeedError,
        feed_loops::run_http_poll_feed,
        models::{
            default_http_feed_config, BookSnapshot, CommonConfig, HttpFeedConfig, ReceivedAt,
        },
    },
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::liquorice::{client::LiquoriceClient, PROTOCOL_SYSTEM},
    },
    snapshot_feed::SnapshotFeed,
};

/// Liquorice's book feed: polls the market makers' price levels over Liquorice's solver API and
/// publishes every poll as a complete [`BookSnapshot`]. Built through [`LiquoriceFeedBuilder`].
#[derive(Clone, Debug)]
pub struct LiquoriceFeed {
    feed_config: HttpFeedConfig,
    source: LiquoriceBookSource,
}

/// Builds a [`LiquoriceFeed`] from the shared [`CommonConfig`] and Liquorice's solver
/// credentials.
pub struct LiquoriceFeedBuilder {
    common: CommonConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    auth_solver: String,
    auth_key: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
    quote_expiry_secs: u64,
}

impl LiquoriceFeedBuilder {
    pub fn new(
        common: CommonConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        auth_solver: String,
        auth_key: String,
    ) -> Self {
        Self {
            common,
            usd_quote_tokens: usd_quote_tokens.into(),
            auth_solver,
            auth_key,
            feed_config: Self::default_feed_config(),
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            quote_expiry_secs: 300,
        }
    }

    /// The feed tuning a new builder starts from: the shared values every HTTP feed uses
    /// (see [`HttpFeedConfig`]). Spread from it to change individual fields:
    /// `HttpFeedConfig { poll_interval: Duration::from_secs(10),
    /// ..LiquoriceFeedBuilder::default_feed_config() }`.
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

    /// Set the expiry duration for quote requests in seconds
    pub fn quote_expiry_secs(mut self, secs: u64) -> Self {
        self.quote_expiry_secs = secs;
        self
    }

    pub fn build(self) -> Result<LiquoriceFeed, FeedError> {
        Ok(LiquoriceFeed {
            feed_config: self.feed_config,
            source: LiquoriceBookSource {
                client: Arc::new(LiquoriceClient::new(
                    self.common.chain,
                    "https://api.liquorice.tech/v1/solver/rfq".to_string(),
                    self.auth_solver,
                    self.auth_key,
                    self.quote_timeout,
                    self.quote_expiry_secs,
                )),
                common: self.common,
                usd_quote_tokens: self.usd_quote_tokens,
                price_levels_endpoint: "https://api.liquorice.tech/v1/solver/price-levels"
                    .to_string(),
            },
        })
    }
}

impl SnapshotFeed for LiquoriceFeed {
    type Snapshot = Option<BookSnapshot<ReceivedAt>>;
    type Output = Result<(), FeedError>;

    fn subscribe(
        self,
    ) -> (
        watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
        impl Future<Output = Result<(), FeedError>> + Send + 'static,
    ) {
        let (tx, rx) = watch::channel(None);
        let LiquoriceFeed { feed_config, source } = self;
        let feed = run_http_poll_feed(PROTOCOL_SYSTEM, feed_config, tx, source);
        (rx, feed)
    }
}
