//! Helper-backed native orderbooks refreshed on canonical block heads.
use std::{collections::HashSet, time::Duration};

use tycho_common::{models::Chain, Bytes};

use self::client_builder::ElfomofiClientBuilder;
use crate::{
    book::BookSnapshot,
    snapshot_feed::{errors::FeedError, Publisher, SnapshotFeed},
};

pub(crate) mod client;
pub(crate) mod client_builder;
mod heads;
pub mod models;
mod rpc;
pub mod state;
mod tokens;

/// Orderbook update and freshness settings. RPC endpoints come from the regular RPC configuration.
#[derive(Clone, Debug)]
pub struct ElfomofiOptions {
    /// An empty set includes all pairs; otherwise both pair tokens must be present.
    pub tokens: HashSet<Bytes>,
    /// Explicitly selects HTTP polling. `None` uses WebSocket `newHeads` notifications.
    pub poll_time: Option<Duration>,
    /// Bounds each orderbook RPC operation, including reading its response body.
    pub quote_timeout: Duration,
    /// Maximum age of the block and observation used for indicative quotes.
    pub max_age: Duration,
}

impl Default for ElfomofiOptions {
    fn default() -> Self {
        Self {
            tokens: HashSet::new(),
            poll_time: None,
            quote_timeout: Duration::from_secs(5),
            max_age: Duration::from_secs(10),
        }
    }
}

impl ElfomofiOptions {
    pub fn tokens(mut self, tokens: HashSet<Bytes>) -> Self {
        self.tokens = tokens;
        self
    }

    pub fn poll_time(mut self, poll_time: Duration) -> Self {
        self.poll_time = Some(poll_time);
        self
    }

    pub fn quote_timeout(mut self, quote_timeout: Duration) -> Self {
        self.quote_timeout = quote_timeout;
        self
    }

    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }
}

/// A complete helper snapshot anchored to one canonical block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElfomofiBlock {
    pub number: u64,
    pub hash: Bytes,
    pub timestamp: u64,
}

/// Configures ElFomoFi using the ordinary RPC environment.
pub struct ElfomofiFeedBuilder {
    chain: Chain,
    options: ElfomofiOptions,
}

impl ElfomofiFeedBuilder {
    pub fn new(chain: Chain) -> Self {
        Self { chain, options: ElfomofiOptions::default() }
    }

    pub fn options(mut self, options: ElfomofiOptions) -> Self {
        self.options = options;
        self
    }

    pub fn build(self) -> Result<ElfomofiFeed, FeedError> {
        let options = self.options;
        let mut builder = ElfomofiClientBuilder::from_env(self.chain)
            .tokens(options.tokens)
            .quote_timeout(options.quote_timeout)
            .max_age(options.max_age);
        if let Some(poll_time) = options.poll_time {
            builder = builder.poll_time(poll_time);
        }
        Ok(ElfomofiFeed { client: builder.build()?, max_age: options.max_age })
    }
}

/// Publishes the full set of orderbooks after each new block head.
#[derive(Debug)]
pub struct ElfomofiFeed {
    client: client::ElfomofiClient,
    max_age: Duration,
}

impl SnapshotFeed for ElfomofiFeed {
    type Snapshot = BookSnapshot<ElfomofiBlock>;
    type Error = FeedError;

    async fn run(self, publisher: Publisher<Self::Snapshot>) -> Result<(), FeedError> {
        publisher
            .publishing(Some(self.max_age), self.client.stream())
            .await
    }
}

#[cfg(test)]
mod tests;
