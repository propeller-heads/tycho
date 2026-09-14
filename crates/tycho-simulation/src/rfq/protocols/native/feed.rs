use std::{collections::HashSet, future::Future, sync::Arc};

use reqwest::Client;
use tokio::{sync::watch, time::Duration};
use tycho_common::Bytes;

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
        protocols::native::{
            client::NativeClient, models::NativeSupportedChain, source::NativeBookSource,
            PROTOCOL_SYSTEM,
        },
    },
    snapshot_feed::SnapshotFeed,
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

/// Builds a [`NativeFeed`] from the shared [`CommonConfig`] and Native's API key.
///
/// # Example
/// ```rust
/// use tycho_simulation::{
///     book::{constants::usd_stablecoins_for_chain, models::{CommonConfig, HttpFeedConfig}},
///     rfq::protocols::native::feed::NativeFeedBuilder,
/// };
/// use tycho_common::{models::{token::Token, Chain}, Bytes};
/// use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
///
/// let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
/// let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
/// let mut tokens = HashMap::new();
/// tokens.insert(weth.clone(), Token::new(&weth, "WETH", 18, 0, &[Some(15_000)], Chain::Ethereum, 100));
/// tokens.insert(usdc.clone(), Token::new(&usdc, "USDC", 6, 0, &[Some(40_000)], Chain::Ethereum, 100));
///
/// let usd_quote_tokens = usd_stablecoins_for_chain(&Chain::Ethereum).unwrap();
/// let common = CommonConfig { chain: Chain::Ethereum, tokens: Arc::new(tokens), min_tvl_usd: 500.0 };
/// let feed = NativeFeedBuilder::new(common, usd_quote_tokens, "api_key".to_string())
///     .feed_config(HttpFeedConfig { poll_interval: Duration::from_secs(10), ..NativeFeedBuilder::default_feed_config() })
///     .build()
///     .unwrap();
/// ```
pub struct NativeFeedBuilder {
    common: CommonConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    api_key: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
}

impl NativeFeedBuilder {
    pub fn new(
        common: CommonConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        api_key: String,
    ) -> Self {
        Self {
            common,
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
            NativeSupportedChain::try_from(self.common.chain).map_err(FeedError::FatalError)?;
        Ok(NativeFeed {
            feed_config: self.feed_config,
            source: NativeBookSource {
                client: Arc::new(NativeClient::new(
                    chain,
                    NATIVE_API_URL.to_string(),
                    self.api_key,
                    self.quote_timeout,
                )),
                common: self.common,
                chain,
                usd_quote_tokens: self.usd_quote_tokens,
                orderbook_endpoint: format!("{NATIVE_API_URL}/orderbook"),
                http: Client::new(),
            },
        })
    }
}

impl SnapshotFeed for NativeFeed {
    type Snapshot = Option<BookSnapshot<ReceivedAt>>;
    type Output = Result<(), FeedError>;

    fn subscribe(
        self,
    ) -> (
        watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
        impl Future<Output = Result<(), FeedError>> + Send + 'static,
    ) {
        let (tx, rx) = watch::channel(None);
        let NativeFeed { feed_config, source } = self;
        let feed = run_http_poll_feed(PROTOCOL_SYSTEM, feed_config, tx, source);
        (rx, feed)
    }
}
