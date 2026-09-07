//! Metric book feed for the `api.metric.xyz` v1 API.
//!
//! Endpoint reference: <https://docs.metric.xyz/RSm94m71kqtGICv4iKRj/developers/api>

use std::future::Future;

use reqwest::Client;
use tokio::sync::watch;
use tycho_common::models::Chain;

use crate::{
    book::{
        errors::FeedError,
        feed_loops::run_http_poll_feed,
        models::{
            default_http_feed_config, BookSnapshot, CommonConfig, HttpFeedConfig, ReceivedAt,
        },
    },
    pamm::protocols::metric::{source::MetricBookSource, PROTOCOL_SYSTEM},
    snapshot_feed::SnapshotFeed,
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

/// Builds a [`MetricFeed`] from the shared [`CommonConfig`] and the Bearer trading key. The feed
/// talks to Metric's public API unless [`base_url`](Self::base_url) says otherwise.
pub struct MetricFeedBuilder {
    common: CommonConfig,
    base_url: String,
    api_key: String,
    feed_config: HttpFeedConfig,
}

impl MetricFeedBuilder {
    /// `api_key` is the Bearer trading key the per-pool bid/ask endpoint requires; without a
    /// valid one no pool is quotable and the feed publishes empty books.
    pub fn new(common: CommonConfig, api_key: String) -> Self {
        Self {
            common,
            base_url: METRIC_API_URL.to_string(),
            api_key,
            feed_config: Self::default_feed_config(),
        }
    }

    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// The feed tuning a new builder starts from: the shared values every HTTP feed uses
    /// (see [`HttpFeedConfig`]). Spread from it to change individual fields:
    /// `HttpFeedConfig { poll_interval: Duration::from_secs(10),
    /// ..MetricFeedBuilder::default_feed_config() }`.
    pub fn default_feed_config() -> HttpFeedConfig {
        default_http_feed_config()
    }

    /// Tune the price feed loop (poll cadence, request timeout, failure limit)
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails for chains Metric does not serve.
    pub fn build(self) -> Result<MetricFeed, FeedError> {
        let chain_id = chain_to_chain_id(self.common.chain)?;
        let base_url = self.base_url.trim_end_matches('/');
        let chain_endpoint = format!("{base_url}/public/v1/evm/{chain_id}");
        Ok(MetricFeed {
            feed_config: self.feed_config,
            source: MetricBookSource {
                common: self.common,
                metadata_endpoint: format!("{chain_endpoint}/metadata"),
                chain_endpoint,
                api_key: self.api_key,
                http: Client::new(),
            },
        })
    }
}

impl SnapshotFeed for MetricFeed {
    type Snapshot = Option<BookSnapshot<ReceivedAt>>;
    type Output = Result<(), FeedError>;

    fn subscribe(
        self,
    ) -> (
        watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
        impl Future<Output = Result<(), FeedError>> + Send + 'static,
    ) {
        let (tx, rx) = watch::channel(None);
        let MetricFeed { feed_config, source } = self;
        let feed = run_http_poll_feed(PROTOCOL_SYSTEM, feed_config, tx, source);
        (rx, feed)
    }
}

fn chain_to_chain_id(chain: Chain) -> Result<u64, FeedError> {
    match chain {
        Chain::Ethereum => Ok(1),
        Chain::Base => Ok(8453),
        Chain::Robinhood => Ok(4663),
        unsupported => Err(FeedError::FatalError(format!(
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
            CommonConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 },
            "secret_key".to_string(),
        )
    }

    #[test]
    fn build_derives_v1_endpoints_from_chain_id() {
        let feed = builder(Chain::Base)
            .base_url("https://metric.example/".to_string())
            .build()
            .unwrap();
        assert_eq!(feed.source.chain_endpoint, "https://metric.example/public/v1/evm/8453");
        assert_eq!(
            feed.source.metadata_endpoint,
            "https://metric.example/public/v1/evm/8453/metadata"
        );
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
