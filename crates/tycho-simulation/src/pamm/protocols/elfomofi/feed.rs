//! Complete helper books fetched through the shared HTTP polling feed.
use std::time::Duration;

use super::{client::ElfomofiClient, models::max_snapshot_age, source::ElfomofiBookSource};
use crate::{
    book::{all_pairs::ComponentLayout, BookFeedConfig, BookSnapshot, ReceivedAt},
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

#[derive(derive_more::Debug)]
pub struct ElfomofiFeed {
    config: HttpFeedConfig,
    source: ElfomofiBookSource,
}

/// The RPC endpoint must serve `book_config.chain`.
pub struct ElfomofiFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    layout: ComponentLayout<()>,
    config: HttpFeedConfig,
}

impl ElfomofiFeedBuilder {
    pub fn new(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self {
            book_config,
            rpc_url,
            layout: ComponentLayout::AllPairs(()),
            config: Self::default_feed_config(),
        }
    }

    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            max_snapshot_age: Some(Duration::from_secs(2)),
            ..default_http_feed_config()
        }
    }

    /// Publishes one component per pair instead of one component for the venue.
    /// The encoder still permits only one ElFomoFi swap in a solution.
    pub fn per_pair(mut self) -> Self {
        self.layout = ComponentLayout::PerPair;
        self
    }

    pub fn feed_config(mut self, config: HttpFeedConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> Result<ElfomofiFeed, FeedError> {
        let maximum_age = max_snapshot_age(self.book_config.chain)?;
        if self.config.poll_interval.is_zero() ||
            self.config.request_timeout.is_zero() ||
            self.config
                .max_snapshot_age
                .is_none_or(|age| age.as_secs() == 0 || age > maximum_age)
        {
            return Err(FeedError::InvalidInput(format!(
                "ElFomoFi requires positive durations and a snapshot age from 1s to {}s on {}",
                maximum_age.as_secs(),
                self.book_config.chain
            )));
        }
        if !self.book_config.min_tvl_usd.is_finite() || self.book_config.min_tvl_usd < 0.0 {
            return Err(FeedError::InvalidInput(
                "ElFomoFi minimum TVL must be finite and nonnegative".into(),
            ));
        }
        let client = ElfomofiClient::new(self.book_config.chain, &self.rpc_url)?;
        let source = ElfomofiBookSource::new(
            self.book_config,
            client,
            self.config
                .max_snapshot_age
                .expect("validated snapshot age")
                .as_secs(),
            self.layout,
        );
        Ok(ElfomofiFeed { config: self.config, source })
    }
}

impl SnapshotFeed for ElfomofiFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    async fn run(self, publisher: Publisher<Self::Snapshot>) -> Result<(), FeedError> {
        run_http_poll_feed(self.config, publisher, self.source).await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use tycho_common::models::Chain;

    use super::*;

    #[test]
    fn defaults_match_the_builder_on_every_supported_chain() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let universe =
                BookFeedConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 };
            let default_config = ElfomofiFeedBuilder::default_feed_config();
            let feed = ElfomofiFeedBuilder::new(universe.clone(), "https://example.test".into())
                .build()
                .unwrap();
            assert_eq!(feed.config.max_snapshot_age, Some(Duration::from_secs(2)));
            assert_eq!(feed.config.poll_interval, default_config.poll_interval);
            assert_eq!(feed.config.request_timeout, default_config.request_timeout);
            assert_eq!(feed.config.max_snapshot_age, default_config.max_snapshot_age);

            let custom = HttpFeedConfig {
                max_snapshot_age: Some(Duration::from_secs(3)),
                ..ElfomofiFeedBuilder::default_feed_config()
            };
            assert!(ElfomofiFeedBuilder::new(universe, "https://example.test".into())
                .feed_config(custom)
                .build()
                .is_err());
        }
    }
}
