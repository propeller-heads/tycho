use std::{collections::HashSet, future::Future, sync::Arc};

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
        protocols::hashflow::{
            client::HashflowClient, source::HashflowBookSource, PROTOCOL_SYSTEM,
        },
    },
    snapshot_feed::SnapshotFeed,
};

/// Hashflow's book feed: polls the market makers' price levels over Hashflow's taker API and
/// publishes every poll as a complete [`BookSnapshot`]. Built through [`HashflowFeedBuilder`].
#[derive(Clone, Debug)]
pub struct HashflowFeed {
    feed_config: HttpFeedConfig,
    source: HashflowBookSource,
}

/// Builds a [`HashflowFeed`] from the shared [`CommonConfig`] and Hashflow's API credentials.
///
/// # Example
/// ```rust
/// use tycho_simulation::{
///     book::{constants::usd_stablecoins_for_chain, models::{CommonConfig, HttpFeedConfig}},
///     rfq::protocols::hashflow::feed::HashflowFeedBuilder,
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
/// let feed = HashflowFeedBuilder::new(common, usd_quote_tokens, "auth_user".to_string(), "auth_key".to_string())
///     .feed_config(HttpFeedConfig { poll_interval: Duration::from_secs(10), ..HashflowFeedBuilder::default_feed_config() })
///     .build()
///     .unwrap();
/// ```
pub struct HashflowFeedBuilder {
    common: CommonConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    auth_user: String,
    auth_key: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
}

impl HashflowFeedBuilder {
    pub fn new(
        common: CommonConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        auth_user: String,
        auth_key: String,
    ) -> Self {
        Self {
            common,
            usd_quote_tokens: usd_quote_tokens.into(),
            auth_user,
            auth_key,
            feed_config: Self::default_feed_config(),
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
        }
    }

    /// The feed tuning a new builder starts from: the shared values every HTTP feed uses
    /// (see [`HttpFeedConfig`]). Spread from it to change individual fields:
    /// `HttpFeedConfig { poll_interval: Duration::from_secs(10),
    /// ..HashflowFeedBuilder::default_feed_config() }`.
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

    pub fn build(self) -> Result<HashflowFeed, FeedError> {
        Ok(HashflowFeed {
            feed_config: self.feed_config,
            source: HashflowBookSource {
                client: Arc::new(HashflowClient::new(
                    self.common.chain,
                    "https://api.hashflow.com/taker/v3/rfq".to_string(),
                    self.auth_user,
                    self.auth_key,
                    self.quote_timeout,
                )),
                common: self.common,
                usd_quote_tokens: self.usd_quote_tokens,
                price_levels_endpoint: "https://api.hashflow.com/taker/v3/price-levels".to_string(),
                market_makers_endpoint: "https://api.hashflow.com/taker/v3/market-makers"
                    .to_string(),
            },
        })
    }
}

impl SnapshotFeed for HashflowFeed {
    type Snapshot = Option<BookSnapshot<ReceivedAt>>;
    type Output = Result<(), FeedError>;

    fn subscribe(
        self,
    ) -> (
        watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
        impl Future<Output = Result<(), FeedError>> + Send + 'static,
    ) {
        let (tx, rx) = watch::channel(None);
        let HashflowFeed { feed_config, source } = self;
        let feed = run_http_poll_feed(PROTOCOL_SYSTEM, feed_config, tx, source);
        (rx, feed)
    }
}

#[cfg(test)]
mod tests {
    use std::{env, str::FromStr, time::Duration};

    use dotenv::dotenv;
    use num_bigint::BigUint;
    use tokio::time::timeout;
    use tycho_common::models::{protocol::GetAmountOutParams, Chain};

    use super::*;
    use crate::book::models::test_token_map;

    #[tokio::test]
    #[ignore] // Requires network access and HASHFLOW_KEY environment variable
    async fn test_hashflow_api_polling() {
        dotenv().expect("Missing .env file");
        let user = std::env::var("HASHFLOW_USER").expect("HASHFLOW_USER not set");
        let key = std::env::var("HASHFLOW_KEY").expect("HASHFLOW_KEY not set");

        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        let tokens = test_token_map(&[(&wbtc, "WBTC", 8), (&weth, "WETH", 18)]);

        let quote_tokens = HashSet::from([
            Bytes::from_str("0xa0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(), // USDT
        ]);

        // $1 minimum TVL - very low to capture most pairs
        let common =
            CommonConfig { chain: Chain::Ethereum, tokens: Arc::new(tokens), min_tvl_usd: 1.0 };
        let feed = HashflowFeedBuilder::new(common, quote_tokens, user, key)
            .feed_config(HttpFeedConfig {
                poll_interval: Duration::from_secs(1),
                ..HashflowFeedBuilder::default_feed_config()
            })
            .quote_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let (mut rx, feed) = feed.subscribe();
        let _feed = tokio::spawn(feed);

        let pairs = timeout(Duration::from_secs(10), async {
            loop {
                rx.changed().await.expect("feed ended");
                if let Some(snapshot) = rx.borrow_and_update().clone() {
                    assert!(snapshot.anchor.0.timestamp() > 0);
                    return snapshot.books;
                }
            }
        })
        .await
        .expect("no book received within 10 seconds");

        assert!(
            !pairs.is_empty(),
            "Should have received at least 1 component with $1 TVL threshold"
        );
        println!("Received {} components in this book", pairs.len());
        for pair in pairs.values() {
            assert_eq!(pair.component.protocol_system, "rfq:hashflow");
            // A streamed state carries a market maker's levels: it can quote a spot price.
            let [base, quote] = pair.component.tokens.as_slice() else {
                panic!("expected exactly two tokens")
            };
            assert!(
                pair.state
                    .spot_price(base, quote)
                    .unwrap() >
                    0.0
            );
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access and setting proper env vars
    async fn test_request_binding_quote() {
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        let auth_user = String::from("propellerheads");
        dotenv().expect("Missing .env file");
        let auth_key = env::var("HASHFLOW_KEY").unwrap();

        let common = CommonConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&wbtc, "WBTC", 8)])),
            min_tvl_usd: 10.0,
        };
        let feed = HashflowFeedBuilder::new(common, HashSet::new(), auth_user, auth_key)
            .quote_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        let params = GetAmountOutParams {
            amount_in: BigUint::from(1_000000000000000000u64),
            token_in: weth.clone(),
            token_out: wbtc.clone(),
            sender: router.clone(),
            receiver: router.clone(),
        };
        let quote = feed
            .source
            .client
            .request_binding_quote(&params)
            .await
            .unwrap();

        assert_eq!(quote.base_token, weth);
        assert_eq!(quote.quote_token, wbtc);
        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));

        // // Assuming the BTC - WETH price doesn't change too much at the time of running this
        assert!(quote.amount_out > BigUint::from(3000000u64));

        assert_eq!(quote.quote_attributes.len(), 11);
        let expected_attributes = [
            "pool",
            "external_account",
            "trader",
            "base_token",
            "quote_token",
            "base_token_amount",
            "quote_token_amount",
            "quote_expiry",
            "nonce",
            "tx_id",
            "signature",
        ];
        for attr in expected_attributes {
            assert!(
                quote
                    .quote_attributes
                    .contains_key(attr),
                "Missing attribute: {attr}"
            );
        }
        assert_eq!(
            quote
                .quote_attributes
                .get("trader")
                .unwrap(),
            &router
        );
    }
}
