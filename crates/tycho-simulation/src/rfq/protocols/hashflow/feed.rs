use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use crate::{
    book::{
        all_pairs::{ComponentLayout, QuoteRule},
        chain_id, BookFeedConfig, BookSnapshot, ReceivedAt,
    },
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::hashflow::{
            client::{HashflowClient, HashflowLevelsClient},
            models::HashflowPrices,
            source::{HashflowBookAssembler, HashflowBookSource, HashflowPricesSource},
            FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM,
        },
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
/// The book settings are those of a [`HashflowBookAssemblerBuilder`];
/// [`with_assembler`](Self::with_assembler) supplies a built assembler instead.
pub struct HashflowFeedBuilder {
    assembler_builder: HashflowBookAssemblerBuilder,
    assembler: Option<HashflowBookAssembler>,
    feed_config: HttpFeedConfig,
}

impl HashflowFeedBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        auth_user: String,
        auth_key: String,
    ) -> Self {
        Self {
            assembler_builder: HashflowBookAssemblerBuilder::new(
                book_config,
                usd_quote_tokens,
                auth_user,
                auth_key,
            ),
            assembler: None,
            feed_config: Self::default_feed_config(),
        }
    }

    /// Makes the feed's books with `assembler` instead of one built from this builder's book
    /// settings, which are then unused.
    pub fn with_assembler(mut self, assembler: HashflowBookAssembler) -> Self {
        self.assembler = Some(assembler);
        self
    }

    /// The feed tuning a new builder starts from: the set every HTTP feed shares, since nothing
    /// about Hashflow's cadence or response times has been measured. Spread from it to change one
    /// field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::rfq::protocols::hashflow::feed::HashflowFeedBuilder;
    /// # use tycho_simulation::snapshot_feed::http::HttpFeedConfig;
    /// let tuned = HttpFeedConfig {
    ///     poll_interval: Duration::from_secs(10),
    ///     ..HashflowFeedBuilder::default_feed_config()
    /// };
    /// ```
    pub fn default_feed_config() -> HttpFeedConfig {
        default_http_feed_config()
    }

    /// Settles this venue's swaps through Tycho's `HashflowFallbackRouter`, which retries
    /// against a solver-named fallback pool when the venue reverts. Components then carry
    /// [`FALLBACK_PROTOCOL_SYSTEM`] rather than [`PROTOCOL_SYSTEM`], which is what selects
    /// the fallback encoder. Off by default.
    pub fn with_fallback_router(mut self) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .with_fallback_router();
        self
    }

    /// Tune the price feed loop (poll cadence, request timeout, failure limit)
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Publishes Hashflow as one component for every pair its market makers price, which a route
    /// may quote as often as `rule` allows — once per maker, since a maker's second quote prices
    /// as if the first had not happened, or once for the venue where that independence is not
    /// worth relying on.
    ///
    /// One component per market maker and pair otherwise.
    pub fn all_pairs(mut self, rule: QuoteRule) -> Self {
        self.assembler_builder = self.assembler_builder.all_pairs(rule);
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .quote_timeout(timeout);
        self
    }

    /// Fails for a chain with no numeric chain id, which is all Hashflow's API is addressed by.
    /// Whether the venue prices that chain is Hashflow's own answer, and the first poll carries
    /// it.
    pub fn build(self) -> Result<HashflowFeed, FeedError> {
        let http = reqwest::Client::new();
        let HashflowPricesFeed { feed_config, source: prices } = HashflowPricesFeedBuilder::new(
            self.assembler_builder.book_config.chain,
            self.assembler_builder.auth_user.clone(),
            self.assembler_builder.auth_key.clone(),
        )
        .feed_config(self.feed_config)
        .http_client(http.clone())
        .build()?;
        let assembler = match self.assembler {
            Some(assembler) => assembler,
            None => self
                .assembler_builder
                .http_client(http)
                .build()?,
        };
        Ok(HashflowFeed { feed_config, source: HashflowBookSource { prices, assembler } })
    }
}

/// Builds a [`HashflowBookAssembler`] from the book settings alone: the shared
/// [`BookFeedConfig`], the USD quote tokens, the firm-quote client's credentials and quote
/// timeout, and the component layout. Makes no request and builds no levels client; pass the
/// result to [`HashflowFeedBuilder::with_assembler`] or use it on its own with prices from a
/// [`HashflowPricesFeed`].
pub struct HashflowBookAssemblerBuilder {
    protocol_system: &'static str,
    component_layout: ComponentLayout<QuoteRule>,
    book_config: BookFeedConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    auth_user: String,
    auth_key: String,
    quote_timeout: Duration,
    http: Option<reqwest::Client>,
}

impl HashflowBookAssemblerBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        auth_user: String,
        auth_key: String,
    ) -> Self {
        Self {
            protocol_system: PROTOCOL_SYSTEM,
            component_layout: ComponentLayout::PerPair,
            book_config,
            usd_quote_tokens: usd_quote_tokens.into(),
            auth_user,
            auth_key,
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            http: None,
        }
    }

    /// See [`HashflowFeedBuilder::with_fallback_router`].
    pub fn with_fallback_router(mut self) -> Self {
        self.protocol_system = FALLBACK_PROTOCOL_SYSTEM;
        self
    }

    /// See [`HashflowFeedBuilder::all_pairs`].
    pub fn all_pairs(mut self, rule: QuoteRule) -> Self {
        self.component_layout = ComponentLayout::AllPairs(rule);
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Makes the quote requests through `http`, which another client of this venue may share.
    fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Fails for a chain with no numeric chain id, which is all Hashflow's API is addressed by.
    pub fn build(self) -> Result<HashflowBookAssembler, FeedError> {
        let client = HashflowClient::new(
            None,
            chain_id(self.book_config.chain)?,
            self.auth_user,
            self.auth_key,
            self.quote_timeout,
        )
        .with_http_client(self.http.unwrap_or_default());
        Ok(HashflowBookAssembler {
            component_layout: self.component_layout,
            protocol_system: self.protocol_system,
            client: Arc::new(client),
            book_config: self.book_config,
            usd_quote_tokens: self.usd_quote_tokens,
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

/// Polls Hashflow's taker API and publishes every poll as [`HashflowPrices`]. Requires only the
/// chain and credentials. [`HashflowBookAssembler`] applies the token list and TVL floor. `Debug`
/// output omits the credentials.
#[derive(Clone, Debug)]
pub struct HashflowPricesFeed {
    feed_config: HttpFeedConfig,
    source: HashflowPricesSource,
}

/// Builds a [`HashflowPricesFeed`] from the chain and Hashflow's API credentials.
pub struct HashflowPricesFeedBuilder {
    chain: Chain,
    auth_user: String,
    auth_key: String,
    feed_config: HttpFeedConfig,
    http: Option<reqwest::Client>,
}

impl HashflowPricesFeedBuilder {
    /// The feed tuning starts from [`HashflowFeedBuilder::default_feed_config`].
    pub fn new(chain: Chain, auth_user: String, auth_key: String) -> Self {
        Self {
            chain,
            auth_user,
            auth_key,
            feed_config: HashflowFeedBuilder::default_feed_config(),
            http: None,
        }
    }

    /// Tune the poll loop (poll cadence, request timeout, failure limit).
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Makes the feed's requests through `http`, which another client of this venue may share.
    fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Fails with [`FeedError::Fatal`] for a chain with no numeric chain id, which is all
    /// Hashflow's API is addressed by. Hashflow reports whether it prices the chain on the first
    /// poll.
    pub fn build(self) -> Result<HashflowPricesFeed, FeedError> {
        let client =
            HashflowLevelsClient::new(None, chain_id(self.chain)?, self.auth_user, self.auth_key)
                .with_http_client(self.http.unwrap_or_default());
        Ok(HashflowPricesFeed {
            feed_config: self.feed_config,
            source: HashflowPricesSource::new(client),
        })
    }
}

impl SnapshotFeed for HashflowPricesFeed {
    type Snapshot = HashflowPrices;
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
    use futures::StreamExt;
    use num_bigint::BigUint;
    use tokio::time::timeout;
    use tycho_common::models::{protocol::GetAmountOutParams, Chain};

    use super::*;
    use crate::{
        book::{assert_same_books, chain_without_id, test_token_map},
        rfq::protocols::hashflow::test_support::{
            prices_source, two_maker_prices, two_maker_server, USDC, WETH,
        },
        snapshot_feed::{http::HttpSource, SnapshotFeed, SnapshotFeedEvent, SnapshotFeedStream},
    };

    #[tokio::test]
    #[ignore = "hits Hashflow's live API; requires HASHFLOW_USER and HASHFLOW_KEY"]
    async fn hashflow_api_polling() {
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
    #[ignore = "hits Hashflow's live API; requires HASHFLOW_KEY"]
    async fn request_binding_quote() {
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
            origin: None,
        };
        let client = &feed.source.assembler.client;
        // The request goes to one maker and the API substitutes no other, so it has to be a
        // maker whose own ladder covers the amount; the deepest one leaves the most room for the
        // book to move between the two calls.
        let market_maker = feed
            .source
            .prices
            .fetch()
            .await
            .unwrap()
            .levels_by_maker
            .into_iter()
            .filter_map(|(name, levels)| {
                let (depth, _) = levels
                    .iter()
                    .find(|level| level.pair.base_token == weth && level.pair.quote_token == wbtc)?
                    .levels
                    .totals();
                (depth >= 1.0).then_some((name, depth))
            })
            .max_by(|(_, deepest), (_, depth)| deepest.total_cmp(depth))
            .expect("no market maker quotes one WETH into WBTC")
            .0;
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

    #[tokio::test]
    async fn the_prices_feed_publishes_each_poll() {
        let server = two_maker_server().await;
        let feed = HashflowPricesFeed {
            feed_config: HashflowFeedBuilder::default_feed_config(),
            source: prices_source(&server.url()),
        };
        let mut stream = SnapshotFeedStream::spawn("test", feed);

        let event = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the feed publishes within the timeout")
            .expect("the stream is not over");

        let SnapshotFeedEvent::Published(prices) = event else {
            panic!("expected the first poll to be published");
        };
        assert_eq!(prices, two_maker_prices());
    }

    fn weth_usdc_config() -> (BookFeedConfig, Arc<HashSet<Bytes>>) {
        let weth = Bytes::from_str(WETH).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&usdc, "USDC", 6)])),
            min_tvl_usd: 1.0,
        };
        (config, Arc::new(HashSet::from([usdc])))
    }

    fn credentials() -> (String, String) {
        ("test_user".to_string(), "test_key".to_string())
    }

    /// An assembler built on its own makes the books the feed builder's own assembler makes from
    /// the same settings.
    #[test]
    fn the_assembler_builder_makes_the_books_the_feed_builder_does() {
        let (config, usd_quote_tokens) = weth_usdc_config();
        let (user, key) = credentials();
        let rule = QuoteRule::OncePerMaker;
        let feed = HashflowFeedBuilder::new(
            config.clone(),
            usd_quote_tokens.clone(),
            user.clone(),
            key.clone(),
        )
        .all_pairs(rule)
        .with_fallback_router()
        .build()
        .unwrap();
        let assembler = HashflowBookAssemblerBuilder::new(config, usd_quote_tokens, user, key)
            .all_pairs(rule)
            .with_fallback_router()
            .build()
            .unwrap();

        let books = assembler.books(two_maker_prices());

        assert_eq!(books.len(), 1);
        assert_same_books(
            &books,
            &feed
                .source
                .assembler
                .books(two_maker_prices()),
        );
    }

    #[test]
    fn the_feed_makes_its_books_with_the_assembler_it_is_given() {
        let (config, usd_quote_tokens) = weth_usdc_config();
        let (user, key) = credentials();
        let assembler = HashflowBookAssemblerBuilder::new(
            config.clone(),
            usd_quote_tokens.clone(),
            user.clone(),
            key.clone(),
        )
        .with_fallback_router()
        .build()
        .unwrap();

        let feed = HashflowFeedBuilder::new(config, usd_quote_tokens, user, key)
            .with_assembler(assembler.clone())
            .build()
            .unwrap();

        assert_eq!(feed.source.assembler.protocol_system, FALLBACK_PROTOCOL_SYSTEM);
        assert_same_books(
            &feed
                .source
                .assembler
                .books(two_maker_prices()),
            &assembler.books(two_maker_prices()),
        );
    }

    #[test]
    fn building_an_assembler_for_a_chain_with_no_numeric_chain_id_is_fatal() {
        let (config, usd_quote_tokens) = weth_usdc_config();
        let (user, key) = credentials();
        let config = BookFeedConfig { chain: chain_without_id(), ..config };

        let error = HashflowBookAssemblerBuilder::new(config, usd_quote_tokens, user, key)
            .build()
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    #[test]
    fn the_prices_feed_refuses_a_chain_with_no_numeric_chain_id() {
        let error = HashflowPricesFeedBuilder::new(
            chain_without_id(),
            "test_user".to_string(),
            "test_key".to_string(),
        )
        .build()
        .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        assert!(
            error
                .to_string()
                .contains("no chain id"),
            "{error}"
        );
    }

    #[test]
    fn the_prices_feed_runs_on_the_feed_tuning_it_is_given() {
        let tuned = HttpFeedConfig {
            poll_interval: Duration::from_secs(7),
            ..HashflowFeedBuilder::default_feed_config()
        };

        let feed = HashflowPricesFeedBuilder::new(
            Chain::Ethereum,
            "test_user".to_string(),
            "test_key".to_string(),
        )
        .feed_config(tuned)
        .build()
        .unwrap();

        assert_eq!(feed.feed_config.poll_interval, Duration::from_secs(7));
    }

    #[test]
    fn the_prices_feed_debug_output_omits_the_credentials() {
        let feed = HashflowPricesFeedBuilder::new(
            Chain::Ethereum,
            "secret_user".to_string(),
            "secret_key".to_string(),
        )
        .build()
        .unwrap();
        let rendered = format!("{feed:?}");

        assert!(!rendered.contains("secret_user"));
        assert!(!rendered.contains("secret_key"));
    }
}
