//! Metric book feed for the `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::{future::Future, num::NonZeroUsize, time::Duration};

use tycho_common::models::Chain;

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    pamm::protocols::metric::{
        client::{MetricClient, DEFAULT_BID_ASK_CONCURRENCY},
        source::MetricBookSource,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Metric's public API; [`MetricFeedBuilder::base_url`] points the feed elsewhere.
const METRIC_API_URL: &str = "https://api.metric.xyz";

/// Metric's book feed: polls the metadata and per-pool bid/ask endpoints and publishes every poll
/// as a complete [`BookSnapshot`]. Built through [`MetricFeedBuilder`]. `Debug` output omits the
/// API key.
#[derive(Debug, Clone)]
pub struct MetricFeed {
    feed_config: HttpFeedConfig,
    source: MetricBookSource,
}

/// Builds a [`MetricFeed`] from the shared [`BookFeedConfig`] and the Bearer trading key. The feed
/// talks to Metric's public API unless [`base_url`](Self::base_url) says otherwise.
pub struct MetricFeedBuilder {
    book_config: BookFeedConfig,
    base_url: String,
    api_key: String,
    bid_ask_concurrency: NonZeroUsize,
    feed_config: HttpFeedConfig,
}

impl MetricFeedBuilder {
    /// `api_key` is the Bearer trading key the per-pool bid/ask endpoint requires; without a
    /// valid one no pool is quotable and the feed publishes empty books.
    pub fn new(book_config: BookFeedConfig, api_key: String) -> Self {
        Self {
            book_config,
            base_url: METRIC_API_URL.to_string(),
            api_key,
            bid_ask_concurrency: DEFAULT_BID_ASK_CONCURRENCY,
            feed_config: Self::default_feed_config(),
        }
    }

    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// How many of a poll's per-pool bid/ask requests run at once. Default 16.
    ///
    /// Metric prices one pool per request, so this is what keeps a poll's duration flat as the
    /// venue adds pools instead of growing until the loop's `request_timeout` discards the whole
    /// poll. Raise it for a venue serving many pools, lower it to be gentler on the API.
    pub fn bid_ask_concurrency(mut self, requests_in_flight: NonZeroUsize) -> Self {
        self.bid_ask_concurrency = requests_in_flight;
        self
    }

    /// The feed tuning a new builder starts from. Spread from it to change individual fields:
    /// `HttpFeedConfig { poll_interval: Duration::from_secs(10),
    /// ..MetricFeedBuilder::default_feed_config() }`.
    ///
    /// Metric is the one venue whose poll is many requests — a page of pool metadata, then one
    /// bid/ask per pool — so it costs more per poll than the shared default and is given more
    /// time to finish one. Measured live, it reprices every 2-3 s against a poll that can take
    /// close to 9 s: the cadence is as tight as leaves `request_timeout` room for the worst poll
    /// while still fitting inside the interval, and the withdrawal age is three missed polls.
    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(20),
            request_timeout: Duration::from_secs(15),
            max_snapshot_age: Some(Duration::from_secs(60)),
            ..default_http_feed_config()
        }
    }

    /// Tune the price feed loop (poll cadence, request timeout, failure limit)
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails for chains Metric does not serve.
    pub fn build(self) -> Result<MetricFeed, FeedError> {
        let chain_id = chain_to_chain_id(self.book_config.chain)?;
        let base_url = self.base_url.trim_end_matches('/');
        let chain_endpoint = format!("{base_url}/public/v1/evm/{chain_id}");
        Ok(MetricFeed {
            feed_config: self.feed_config,
            source: MetricBookSource {
                book_config: self.book_config,
                client: MetricClient::new(
                    format!("{chain_endpoint}/metadata"),
                    chain_endpoint,
                    self.api_key,
                    self.bid_ask_concurrency,
                ),
            },
        })
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

fn chain_to_chain_id(chain: Chain) -> Result<u64, FeedError> {
    match chain {
        Chain::Ethereum => Ok(1),
        Chain::Base => Ok(8453),
        Chain::Robinhood => Ok(4663),
        unsupported => Err(FeedError::Fatal(format!(
            "Metric does not support chain in this integration: {unsupported:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::*;

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
}
