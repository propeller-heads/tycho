use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::Bytes;

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::hashflow::{client::HashflowClient, source::HashflowBookSource},
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Hashflow's book feed: polls the market makers' price levels over Hashflow's taker API and
/// publishes every poll as a complete [`BookSnapshot`]. Built through [`HashflowFeedBuilder`].
#[derive(Clone, Debug)]
pub struct HashflowFeed {
    feed_config: HttpFeedConfig,
    source: HashflowBookSource,
}

/// Builds a [`HashflowFeed`] from the shared [`BookFeedConfig`] and Hashflow's API credentials.
pub struct HashflowFeedBuilder {
    book_config: BookFeedConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    auth_user: String,
    auth_key: String,
    feed_config: HttpFeedConfig,
    quote_timeout: Duration,
}

impl HashflowFeedBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        auth_user: String,
        auth_key: String,
    ) -> Self {
        Self {
            book_config,
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
                    self.book_config.chain,
                    "https://api.hashflow.com/taker/v3/rfq".to_string(),
                    "https://api.hashflow.com/taker/v3/price-levels".to_string(),
                    "https://api.hashflow.com/taker/v3/market-makers".to_string(),
                    self.auth_user,
                    self.auth_key,
                    self.quote_timeout,
                )),
                book_config: self.book_config,
                usd_quote_tokens: self.usd_quote_tokens,
            },
        })
    }
}

impl SnapshotFeed for HashflowFeed {
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
    use std::{env, str::FromStr, time::Duration};

    use dotenv::dotenv;
    use num_bigint::BigUint;
    use tokio::time::timeout;
    use tycho_common::models::{protocol::GetAmountOutParams, Chain};

    use super::*;
    use crate::{book::test_token_map, snapshot_feed::SnapshotFeed};

    #[tokio::test]
    #[ignore = "hits Hashflow's live API; requires HASHFLOW_USER and HASHFLOW_KEY"]
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
        let book_config =
            BookFeedConfig { chain: Chain::Ethereum, tokens: Arc::new(tokens), min_tvl_usd: 1.0 };
        let feed = HashflowFeedBuilder::new(book_config, quote_tokens, user, key)
            .feed_config(HttpFeedConfig {
                poll_interval: Duration::from_secs(1),
                ..HashflowFeedBuilder::default_feed_config()
            })
            .quote_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let (publisher, mut rx) = Publisher::channel();
        let _feed = tokio::spawn(feed.run(publisher));

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
            assert_eq!(pair.component.protocol_system, "book:hashflow");
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
    #[ignore = "hits Hashflow's live API; requires HASHFLOW_KEY"]
    async fn test_request_binding_quote() {
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        let auth_user = String::from("propellerheads");
        dotenv().expect("Missing .env file");
        let auth_key = env::var("HASHFLOW_KEY").unwrap();

        let book_config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&wbtc, "WBTC", 8)])),
            min_tvl_usd: 10.0,
        };
        let feed = HashflowFeedBuilder::new(book_config, HashSet::new(), auth_user, auth_key)
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
        let client = &feed.source.client;
        let market_makers = client
            .fetch_market_makers()
            .await
            .unwrap();
        let market_maker = client
            .fetch_price_levels(&market_makers)
            .await
            .unwrap()
            .into_iter()
            .find_map(|(name, levels)| {
                levels
                    .iter()
                    .any(|level| level.pair.base_token == weth && level.pair.quote_token == wbtc)
                    .then_some(name)
            })
            .expect("no market maker quotes WETH/WBTC");
        let quote = client
            .request_binding_quote(&params, market_maker)
            .await
            .unwrap();

        assert_eq!(quote.base_token, weth);
        assert_eq!(quote.quote_token, wbtc);
        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));

        // // Assuming the BTC - WETH price doesn't change too much at the time of running this
        assert!(quote.amount_out > BigUint::from(3000000u64));

        assert_eq!(quote.quote_attributes.len(), 12);
        let expected_attributes = [
            "pool",
            "external_account",
            "trader",
            "effective_trader",
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
