use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::Bytes;

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::native::{
            client::NativeClient, models::NativeSupportedChain, source::NativeBookSource,
            FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM,
        },
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Native Relay's book feed: polls the aggregated orderbook and publishes every poll as a
/// complete [`BookSnapshot`]. Built through [`NativeFeedBuilder`].
#[derive(Clone, Debug)]
pub struct NativeFeed {
    feed_config: HttpFeedConfig,
    source: NativeBookSource,
}

/// Builds a [`NativeFeed`] from the shared [`BookFeedConfig`] and Native's API key.
pub struct NativeFeedBuilder {
    protocol_system: &'static str,
    book_config: BookFeedConfig,
    all_pairs: bool,
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
            protocol_system: PROTOCOL_SYSTEM,
            all_pairs: false,
        }
    }

    /// The feed tuning a new builder starts from: the set every HTTP feed shares, since nothing
    /// about Native's cadence or response times has been measured. Spread from it to change one
    /// field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::rfq::protocols::native::feed::NativeFeedBuilder;
    /// # use tycho_simulation::snapshot_feed::http::HttpFeedConfig;
    /// let tuned = HttpFeedConfig {
    ///     poll_interval: Duration::from_secs(10),
    ///     ..NativeFeedBuilder::default_feed_config()
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

    /// Publishes Native as one component for every pair it prices, which a route may take a
    /// single quote from: Native picks the makers behind a firm quote itself and signs once, so
    /// a route that filled against two of its pairs would be quoted for one of them.
    ///
    /// One component per pair otherwise.
    pub fn all_pairs(mut self) -> Self {
        self.all_pairs = true;
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Settles this venue's swaps through Tycho's `NativeFallbackRouter`, which retries
    /// against a solver-named fallback pool when the venue reverts. Components then carry
    /// [`FALLBACK_PROTOCOL_SYSTEM`] rather than [`PROTOCOL_SYSTEM`], which is what selects
    /// the fallback encoder. Off by default.
    ///
    /// The router trades ERC20s only, so the feed then offers no native-token books.
    pub fn with_fallback_router(mut self) -> Self {
        self.protocol_system = FALLBACK_PROTOCOL_SYSTEM;
        self
    }

    /// Fails for chains Native does not serve.
    pub fn build(self) -> Result<NativeFeed, FeedError> {
        let chain =
            NativeSupportedChain::try_from(self.book_config.chain).map_err(FeedError::Fatal)?;
        Ok(NativeFeed {
            feed_config: self.feed_config,
            source: NativeBookSource {
                protocol_system: self.protocol_system,
                all_pairs: self.all_pairs,
                client: Arc::new(NativeClient::new(None, chain, self.api_key, self.quote_timeout)),
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_common::models::Chain;

    use super::*;

    #[test]
    fn building_for_a_chain_native_does_not_serve_is_fatal() {
        let config = BookFeedConfig {
            chain: Chain::Polygon,
            tokens: Arc::new(HashMap::new()),
            min_tvl_usd: 0.0,
        };

        let error = NativeFeedBuilder::new(config, HashSet::new(), "key".to_string())
            .build()
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }
}
