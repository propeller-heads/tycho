//! ElFomoFi's complete helper books, using shared polling or head-triggered fetching.
use std::time::Duration;

use futures::StreamExt;

use super::{client::ElfomofiClient, source::ElfomofiBookSource};
use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    snapshot_feed::{
        errors::FeedError,
        evm_heads::head_events,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        trigger::run_triggered_feed,
        ws::WsFeedConfig,
        Publisher, SnapshotFeed,
    },
};

#[derive(derive_more::Debug)]
pub struct ElfomofiFeed {
    config: HttpFeedConfig,
    source: ElfomofiBookSource,
    #[debug(skip)]
    ws_rpc_url: Option<String>,
}

/// Endpoints are supplied by the caller and must serve `book_config.chain`.
pub struct ElfomofiFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    ws_rpc_url: Option<String>,
    polling: bool,
    discover_tokens: bool,
    config: HttpFeedConfig,
}

impl ElfomofiFeedBuilder {
    pub fn new(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self {
            book_config,
            rpc_url,
            ws_rpc_url: None,
            polling: false,
            discover_tokens: true,
            config: Self::default_feed_config(),
        }
    }

    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            max_snapshot_age: Some(Duration::from_secs(10)),
            ..default_http_feed_config()
        }
    }

    pub fn ws_rpc_url(mut self, url: String) -> Self {
        self.ws_rpc_url = Some(url);
        self.polling = false;
        self
    }
    pub fn polling(mut self) -> Self {
        self.polling = true;
        self
    }
    pub fn feed_config(mut self, config: HttpFeedConfig) -> Self {
        self.config = config;
        self
    }

    /// Enrich unknown helper tokens over RPC. Disabled, only the configured universe is served.
    /// Configured token metadata always takes precedence over discovered metadata.
    pub fn discover_tokens(mut self, enabled: bool) -> Self {
        self.discover_tokens = enabled;
        self
    }

    pub fn build(self) -> Result<ElfomofiFeed, FeedError> {
        if self.config.poll_interval.is_zero() ||
            self.config.request_timeout.is_zero() ||
            self.config
                .max_snapshot_age
                .is_none_or(|age| age.as_secs() == 0)
        {
            return Err(FeedError::InvalidInput(
                "ElFomoFi requires positive durations and a snapshot age of at least one second"
                    .into(),
            ));
        }
        if !self.book_config.min_tvl_usd.is_finite() || self.book_config.min_tvl_usd < 0.0 {
            return Err(FeedError::InvalidInput(
                "ElFomoFi minimum TVL must be finite and nonnegative".into(),
            ));
        }
        for (address, token) in self.book_config.tokens.iter() {
            if address.len() != 20 ||
                address != &token.address ||
                token.chain != self.book_config.chain
            {
                return Err(FeedError::InvalidInput(
                    "ElFomoFi token metadata does not match its address or chain".into(),
                ));
            }
        }
        let client = ElfomofiClient::new(self.book_config.chain, self.rpc_url)?;
        let ws_rpc_url = if self.polling {
            None
        } else {
            let url = self.ws_rpc_url.ok_or_else(|| FeedError::InvalidInput("ElFomoFi head updates require an explicit WebSocket endpoint; select polling() for HTTP".into()))?;
            let parsed = reqwest::Url::parse(&url).map_err(|_| {
                FeedError::InvalidInput("Invalid ElFomoFi WebSocket endpoint".into())
            })?;
            if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
                return Err(FeedError::InvalidInput(
                    "ElFomoFi WebSocket endpoint must use WS or WSS".into(),
                ));
            }
            Some(url)
        };
        let source = ElfomofiBookSource::new(
            self.book_config,
            client,
            self.config
                .max_snapshot_age
                .expect("validated snapshot age")
                .as_secs(),
            self.discover_tokens,
        );
        Ok(ElfomofiFeed { config: self.config, source, ws_rpc_url })
    }
}

impl SnapshotFeed for ElfomofiFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    async fn run(self, publisher: Publisher<Self::Snapshot>) -> Result<(), FeedError> {
        if let Some(ws_rpc_url) = self.ws_rpc_url {
            let events = head_events(
                ws_rpc_url,
                self.source.chain().id(),
                WsFeedConfig {
                    connect_timeout: self.config.request_timeout,
                    read_idle_timeout: self
                        .config
                        .max_snapshot_age
                        .expect("validated snapshot age"),
                    backoff_unit: self.config.poll_interval,
                    max_backoff_exp: self.config.max_backoff_exp,
                    max_consecutive_failures: self.config.max_consecutive_failures,
                    max_snapshot_age: self.config.max_snapshot_age,
                },
            )
            .map(|event| event.map(|_| ()));
            run_triggered_feed(self.config, publisher, self.source, events).await
        } else {
            run_http_poll_feed(self.config, publisher, self.source).await
        }
    }
}
