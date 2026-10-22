use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::Bytes;

use crate::{
    book::{
        all_pairs::{ComponentLayout, QuoteRule},
        BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::liquorice::{
            client::LiquoriceClient, source::LiquoriceBookSource, FALLBACK_PROTOCOL_SYSTEM,
            PROTOCOL_SYSTEM,
        },
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Liquorice's book feed: polls the market makers' price levels over Liquorice's solver API and
/// publishes every poll as a complete [`BookSnapshot`]. Built through [`LiquoriceFeedBuilder`].
#[derive(Clone, Debug)]
pub struct LiquoriceFeed {
    feed_config: HttpFeedConfig,
    source: LiquoriceBookSource,
}

/// Builds a [`LiquoriceFeed`] from the shared [`BookFeedConfig`] and Liquorice's solver
/// credentials.
pub struct LiquoriceFeedBuilder {
    protocol_system: &'static str,
    book_config: BookFeedConfig,
    component_layout: ComponentLayout<QuoteRule>,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    auth_solver: String,
    auth_key: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
    quote_expiry_secs: u64,
}

impl LiquoriceFeedBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        auth_solver: String,
        auth_key: String,
    ) -> Self {
        Self {
            book_config,
            usd_quote_tokens: usd_quote_tokens.into(),
            auth_solver,
            auth_key,
            feed_config: Self::default_feed_config(),
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            component_layout: ComponentLayout::PerPair,
            quote_expiry_secs: 300,
            protocol_system: PROTOCOL_SYSTEM,
        }
    }

    /// The feed tuning a new builder starts from: the set every HTTP feed shares, since nothing
    /// about Liquorice's cadence or response times has been measured. Spread from it to change one
    /// field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::rfq::protocols::liquorice::feed::LiquoriceFeedBuilder;
    /// # use tycho_simulation::snapshot_feed::http::HttpFeedConfig;
    /// let tuned = HttpFeedConfig {
    ///     poll_interval: Duration::from_secs(10),
    ///     ..LiquoriceFeedBuilder::default_feed_config()
    /// };
    /// ```
    pub fn default_feed_config() -> HttpFeedConfig {
        default_http_feed_config()
    }

    /// Tune the price feed loop (poll cadence, request timeout, failure limit)
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Publishes Liquorice as one component for every pair its market makers price, which a route
    /// may quote as often as `rule` allows — once per maker, since a maker's second quote prices
    /// as if the first had not happened, or once for the venue where that independence is not
    /// worth relying on.
    ///
    /// One component per market maker and pair otherwise.
    pub fn all_pairs(mut self, rule: QuoteRule) -> Self {
        self.component_layout = ComponentLayout::AllPairs(rule);
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Set the expiry duration for quote requests in seconds
    pub fn quote_expiry_secs(mut self, secs: u64) -> Self {
        self.quote_expiry_secs = secs;
        self
    }

    /// Settles this venue's swaps through Tycho's `LiquoriceFallbackRouter`, which retries
    /// against a solver-named fallback pool when the venue reverts. Components then carry
    /// [`FALLBACK_PROTOCOL_SYSTEM`] rather than [`PROTOCOL_SYSTEM`], which is what selects
    /// the fallback encoder. Off by default.
    ///
    /// The router does not support Liquorice partial fills: a swap whose input is below the
    /// quote's signed amount fills through the fallback pool instead.
    pub fn with_fallback_router(mut self) -> Self {
        self.protocol_system = FALLBACK_PROTOCOL_SYSTEM;
        self
    }

    /// Fails for a chain with no numeric chain id, which is all Liquorice's API is addressed by.
    /// Whether the venue prices that chain is Liquorice's own answer, and the first poll carries
    /// it.
    pub fn build(self) -> Result<LiquoriceFeed, FeedError> {
        let chain_id = self.book_config.chain_id()?;
        Ok(LiquoriceFeed {
            feed_config: self.feed_config,
            source: LiquoriceBookSource {
                protocol_system: self.protocol_system,
                component_layout: self.component_layout,
                client: Arc::new(LiquoriceClient::new(
                    None,
                    chain_id,
                    self.auth_solver,
                    self.auth_key,
                    self.quote_timeout,
                    self.quote_expiry_secs,
                )),
                book_config: self.book_config,
                usd_quote_tokens: self.usd_quote_tokens,
            },
        })
    }
}

impl SnapshotFeed for LiquoriceFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}
