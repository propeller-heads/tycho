use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::bebop::{
            client::{BebopClient, BebopPricingClient, QuoteOrigin},
            models::{BebopPrices, BebopSupportedChain},
            source::{BebopBookAssembler, BebopBookSource, BebopPricesSource},
            FALLBACK_PROTOCOL_SYSTEM, PROTOCOL_SYSTEM,
        },
    },
    snapshot_feed::{
        errors::FeedError,
        ws::{default_ws_feed_config, run_ws_feed, WsFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Bebop's book feed: streams market-maker price levels over Bebop's pricing WebSocket and
/// publishes every update as a complete [`BookSnapshot`]. Built through [`BebopFeedBuilder`].
#[derive(Clone, Debug)]
pub struct BebopFeed {
    feed_config: WsFeedConfig,
    source: BebopBookSource,
}

/// Builds a [`BebopFeed`] from the shared [`BookFeedConfig`] and Bebop's API key. The book
/// settings are those of a [`BebopBookAssemblerBuilder`]; [`with_assembler`](Self::with_assembler)
/// supplies a built assembler instead.
pub struct BebopFeedBuilder {
    assembler_builder: BebopBookAssemblerBuilder,
    assembler: Option<BebopBookAssembler>,
    feed_config: WsFeedConfig,
}

impl BebopFeedBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        ws_key: String,
    ) -> Self {
        Self {
            assembler_builder: BebopBookAssemblerBuilder::new(
                book_config,
                usd_quote_tokens,
                ws_key,
            ),
            assembler: None,
            feed_config: Self::default_feed_config(),
        }
    }

    /// Makes the feed's books with `assembler` instead of one built from this builder's book
    /// settings, which are then unused.
    pub fn with_assembler(mut self, assembler: BebopBookAssembler) -> Self {
        self.assembler = Some(assembler);
        self
    }

    /// Publishes Bebop as one component for every pair it prices, which a route may take a
    /// single quote from: Bebop picks the makers behind a firm quote itself and signs once, so
    /// a route that filled against two of its pairs would be quoted for one of them.
    ///
    /// One component per pair otherwise, in each orientation Bebop publishes it.
    pub fn all_pairs(mut self) -> Self {
        self.assembler_builder = self.assembler_builder.all_pairs();
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .quote_timeout(timeout);
        self
    }

    /// Set the real end-user's EOA, for a taker that is not the end-user's own wallet, sent as
    /// `origin_address` with binding quote requests.
    ///
    /// Bebop screens quote requests by these three, and can configure an API account to require
    /// one and refuse a request that leaves it out. See
    /// <https://docs.bebop.xyz/rfq-api/guides/best-practices#6-pass-origin-so-we-can-identify-legitimate-flow>.
    pub fn origin_address(mut self, origin_address: Bytes) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .origin_address(origin_address);
        self
    }

    /// Set the `to` address of the resulting transaction, where a contract executes the swap,
    /// sent as `origin_target` with binding quote requests. See
    /// [`origin_address`](Self::origin_address) for what Bebop does with it.
    pub fn origin_target(mut self, origin_target: Bytes) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .origin_target(origin_target);
        self
    }

    /// Set a stable identifier for the upstream flow source, where a solver aggregates several,
    /// sent as `origin_source` with binding quote requests. See
    /// [`origin_address`](Self::origin_address) for what Bebop does with it.
    pub fn origin_source(mut self, origin_source: String) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .origin_source(origin_source);
        self
    }

    /// The feed tuning a new builder starts from: the set every WebSocket feed shares, since
    /// nothing about how often Bebop pushes, or how long it goes quiet, has been measured. Spread
    /// from it to change one field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::rfq::protocols::bebop::feed::BebopFeedBuilder;
    /// # use tycho_simulation::snapshot_feed::ws::WsFeedConfig;
    /// let tuned = WsFeedConfig {
    ///     read_idle_timeout: Duration::from_secs(30),
    ///     ..BebopFeedBuilder::default_feed_config()
    /// };
    /// ```
    pub fn default_feed_config() -> WsFeedConfig {
        default_ws_feed_config()
    }

    /// Settles this venue's swaps through Tycho's `BebopFallbackRouter`, which retries
    /// against a solver-named fallback pool when the venue reverts. Components then carry
    /// [`FALLBACK_PROTOCOL_SYSTEM`] rather than [`PROTOCOL_SYSTEM`], which is what selects
    /// the fallback encoder. Off by default.
    pub fn with_fallback_router(mut self) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .with_fallback_router();
        self
    }

    /// Tune the price feed loop (timeouts, backoff, failure limit)
    pub fn feed_config(mut self, feed_config: WsFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails for chains Bebop does not serve.
    pub fn build(self) -> Result<BebopFeed, FeedError> {
        let BebopPricesFeed { feed_config, source: prices } = BebopPricesFeedBuilder::new(
            self.assembler_builder.book_config.chain,
            self.assembler_builder.api_key.clone(),
        )
        .feed_config(self.feed_config)
        .build()?;
        let assembler = match self.assembler {
            Some(assembler) => assembler,
            None => self.assembler_builder.build()?,
        };
        Ok(BebopFeed { feed_config, source: BebopBookSource { prices, assembler } })
    }
}

/// Builds a [`BebopBookAssembler`] from the book settings alone: the shared [`BookFeedConfig`],
/// the USD quote tokens, the firm-quote client's API key, quote timeout and origin, and the
/// component layout. Opens no connection and builds no pricing client; pass the result to
/// [`BebopFeedBuilder::with_assembler`] or use it on its own with prices from a
/// [`BebopPricesFeed`].
pub struct BebopBookAssemblerBuilder {
    protocol_system: &'static str,
    book_config: BookFeedConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    api_key: String,
    quote_timeout: Duration,
    origin: QuoteOrigin,
    all_pairs: bool,
}

impl BebopBookAssemblerBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
        api_key: String,
    ) -> Self {
        Self {
            protocol_system: PROTOCOL_SYSTEM,
            book_config,
            usd_quote_tokens: usd_quote_tokens.into(),
            api_key,
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            origin: QuoteOrigin::default(),
            all_pairs: false,
        }
    }

    /// See [`BebopFeedBuilder::all_pairs`].
    pub fn all_pairs(mut self) -> Self {
        self.all_pairs = true;
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// See [`BebopFeedBuilder::origin_address`].
    pub fn origin_address(mut self, origin_address: Bytes) -> Self {
        self.origin.address = Some(origin_address);
        self
    }

    /// See [`BebopFeedBuilder::origin_target`].
    pub fn origin_target(mut self, origin_target: Bytes) -> Self {
        self.origin.target = Some(origin_target);
        self
    }

    /// See [`BebopFeedBuilder::origin_source`].
    pub fn origin_source(mut self, origin_source: String) -> Self {
        self.origin.source = Some(origin_source);
        self
    }

    /// See [`BebopFeedBuilder::with_fallback_router`].
    pub fn with_fallback_router(mut self) -> Self {
        self.protocol_system = FALLBACK_PROTOCOL_SYSTEM;
        self
    }

    /// Fails for chains Bebop does not serve.
    pub fn build(self) -> Result<BebopBookAssembler, FeedError> {
        let chain =
            BebopSupportedChain::try_from(self.book_config.chain).map_err(FeedError::Fatal)?;
        Ok(BebopBookAssembler {
            protocol_system: self.protocol_system,
            book_config: self.book_config,
            usd_quote_tokens: Arc::new(
                self.usd_quote_tokens
                    .iter()
                    .cloned()
                    .collect(),
            ),
            client: Arc::new(BebopClient::new(
                chain,
                self.api_key,
                self.quote_timeout,
                self.origin,
            )),
            all_pairs: self.all_pairs,
        })
    }
}

impl SnapshotFeed for BebopFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_ws_feed(self.feed_config, publisher, self.source)
    }
}

/// Streams Bebop's pricing WebSocket and publishes each binary frame as [`BebopPrices`]; text
/// frames are skipped. Requires only the chain and API key. [`BebopBookAssembler`] applies the
/// token list and TVL floor. `Debug` output omits the API key.
#[derive(Clone, Debug)]
pub struct BebopPricesFeed {
    feed_config: WsFeedConfig,
    source: BebopPricesSource,
}

/// Builds a [`BebopPricesFeed`] from the chain and Bebop's API key.
pub struct BebopPricesFeedBuilder {
    chain: Chain,
    ws_key: String,
    feed_config: WsFeedConfig,
}

impl BebopPricesFeedBuilder {
    /// The feed tuning starts from [`BebopFeedBuilder::default_feed_config`].
    pub fn new(chain: Chain, ws_key: String) -> Self {
        Self { chain, ws_key, feed_config: BebopFeedBuilder::default_feed_config() }
    }

    /// Tune the connection loop (timeouts, backoff, failure limit).
    pub fn feed_config(mut self, feed_config: WsFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails with [`FeedError::Fatal`] for a chain Bebop does not serve.
    pub fn build(self) -> Result<BebopPricesFeed, FeedError> {
        let chain = BebopSupportedChain::try_from(self.chain).map_err(FeedError::Fatal)?;
        Ok(BebopPricesFeed {
            feed_config: self.feed_config,
            source: BebopPricesSource::new(BebopPricingClient::new(chain, self.ws_key)),
        })
    }
}

impl SnapshotFeed for BebopPricesFeed {
    type Snapshot = BebopPrices;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_ws_feed(self.feed_config, publisher, self.source)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr, time::Duration};

    use dotenv::dotenv;
    use num_bigint::BigUint;
    use tokio::time::timeout;
    use tycho_common::models::{protocol::GetAmountOutParams, Chain};

    use super::*;
    use crate::{
        book::{
            assert_same_books,
            levels::{Levels, PriceLevel},
            test_token_map, Book,
        },
        rfq::protocols::bebop::models::BebopBook,
        snapshot_feed::SnapshotFeed,
    };

    /// BebopSettlement.swapSingle
    const SWAP_SINGLE_SELECTOR: [u8; 4] = [0x4d, 0xce, 0xbc, 0xba];
    /// BebopSettlement.swapAggregate
    const SWAP_AGGREGATE_SELECTOR: [u8; 4] = [0xa2, 0xf7, 0x48, 0x93];
    /// BebopRouter.swap
    const ROUTER_SWAP_SELECTOR: [u8; 4] = [0x95, 0x86, 0xd0, 0xe8];

    /// Waits until the receiver holds a snapshot and returns its books.
    async fn next_books(
        rx: &mut tokio::sync::watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
    ) -> Arc<HashMap<String, Book>> {
        loop {
            rx.changed().await.expect("feed ended");
            if let Some(snapshot) = &*rx.borrow_and_update() {
                return Arc::clone(&snapshot.books);
            }
        }
    }

    #[tokio::test]
    #[ignore = "hits Bebop's live API; requires BEBOP_KEY"]
    async fn bebop_websocket_connection() {
        // We test with quote tokens that are not USDC in order to ensure our normalization works
        // fine
        let wbtc = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

        dotenv().expect("Missing .env file");
        let key = std::env::var("BEBOP_KEY").expect("BEBOP_KEY not set");

        let quote_tokens = HashSet::from([
            // Use addresses we forgot to checksum (to test checksumming)
            Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(), // USDC
            Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(), // USDT
        ]);

        let book_config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&wbtc, "WBTC", 8)])),
            min_tvl_usd: 10.0,
        };
        let feed = BebopFeedBuilder::new(book_config, quote_tokens, key)
            .quote_timeout(Duration::from_secs(30))
            .build()
            .unwrap();

        let (publisher, mut rx) = Publisher::channel();
        let _feed = tokio::spawn(feed.run(publisher));

        // Receiving a single decodable book is enough to prove the authenticated handshake and
        // protobuf decoding work. Bebop only pushes on price changes, so requiring more makes
        // the test flaky against market cadence.
        let pairs = timeout(Duration::from_secs(10), next_books(&mut rx))
            .await
            .expect("no book received within 10 seconds");

        assert!(!pairs.is_empty());
        println!("Received {} components in this book", pairs.len());
        for pair in pairs.values() {
            assert_eq!(pair.component.protocol_system, "rfq:bebop");
            assert_eq!(pair.component.protocol_type_name, "bebop_pool");
            assert_eq!(pair.component.chain, Chain::Ethereum);

            // A streamed state carries price levels: it can quote a spot price.
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
    #[ignore = "hits Bebop's live API; requires BEBOP_KEY"]
    async fn bebop_quote_single_order() {
        let token_in = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let token_out = Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap();
        dotenv().expect("Missing .env file");
        let key = std::env::var("BEBOP_KEY").expect("BEBOP_KEY not set");

        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        let book_config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[
                (&token_in, "TOKEN_IN", 18),
                (&token_out, "TOKEN_OUT", 18),
            ])),
            min_tvl_usd: 10.0,
        };
        let feed = BebopFeedBuilder::new(book_config, HashSet::new(), key)
            .quote_timeout(Duration::from_secs(30))
            .origin_address(Bytes::from_str("0x00000000219ab540356cBB839Cbe05303d7705Fa").unwrap())
            .origin_target(router.clone())
            .origin_source("tycho-test".to_string())
            .build()
            .unwrap();

        let params = GetAmountOutParams {
            amount_in: BigUint::from(1_000000000000000000u64),
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            sender: router.clone(),
            receiver: router,
            origin: None,
        };
        let quote = feed
            .source
            .assembler
            .client
            .request_binding_quote(&params)
            .await
            .unwrap();

        assert_eq!(quote.base_token, token_in);
        assert_eq!(quote.quote_token, token_out);
        assert_eq!(quote.amount_in, BigUint::from(1_000000000000000000u64));

        // Conservative sanity bound (0.01 WBTC for 1 WETH) — proves a real, non-dust quote came
        // back without depending closely on the live WETH/WBTC price.
        assert!(quote.amount_out > BigUint::from(1_000_000u64));

        // The settlement mode depends on the API account configuration behind BEBOP_KEY:
        // settlement-mode accounts get BebopSettlement.swapSingle calldata, router-mode
        // accounts get BebopRouter.swap calldata.
        let selector = &quote
            .quote_attributes
            .get("calldata")
            .unwrap()[..4];
        if selector == SWAP_SINGLE_SELECTOR {
            let partial_fill_offset_slice = quote
                .quote_attributes
                .get("partial_fill_offset")
                .unwrap()
                .as_ref();
            let mut partial_fill_offset_array = [0u8; 8];
            partial_fill_offset_array.copy_from_slice(partial_fill_offset_slice);

            assert_eq!(u64::from_be_bytes(partial_fill_offset_array), 12);
        } else {
            assert_eq!(selector, ROUTER_SWAP_SELECTOR);
        }
    }

    #[tokio::test]
    #[ignore = "hits Bebop's live API; requires BEBOP_KEY"]
    async fn bebop_quote_aggregate_order() {
        // This will make a quote request similar to the previous test but with a very big amount
        // We expect the Bebop Quote to have an aggregate order (split between different mms)
        let token_in = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let token_out = Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap();
        dotenv().expect("Missing .env file");
        let key = std::env::var("BEBOP_KEY").expect("BEBOP_KEY not set");

        let router = Bytes::from_str("0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap();

        let book_config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[
                (&token_in, "TOKEN_IN", 18),
                (&token_out, "TOKEN_OUT", 18),
            ])),
            min_tvl_usd: 10.0,
        };
        let feed = BebopFeedBuilder::new(book_config, HashSet::new(), key)
            .quote_timeout(Duration::from_secs(30))
            .origin_address(Bytes::from_str("0x00000000219ab540356cBB839Cbe05303d7705Fa").unwrap())
            .origin_target(router.clone())
            .origin_source("tycho-test".to_string())
            .build()
            .unwrap();

        let amount_in = BigUint::from_str("20_000_000_000").unwrap(); // 20k USDC
        let params = GetAmountOutParams {
            amount_in: amount_in.clone(),
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            sender: router.clone(),
            receiver: router,
            origin: None,
        };
        let quote = feed
            .source
            .assembler
            .client
            .request_binding_quote(&params)
            .await
            .unwrap();

        assert_eq!(quote.base_token, token_in);
        assert_eq!(quote.quote_token, token_out);
        assert_eq!(quote.amount_in, amount_in);

        // Assuming the USDC - ONDO price doesn't change too much at the time of running this
        assert!(quote.amount_out > BigUint::from_str("18000000000000000000000").unwrap()); // ~19k ONDO

        // The settlement mode depends on the API account configuration behind BEBOP_KEY:
        // settlement-mode accounts get BebopSettlement.swapAggregate calldata, router-mode
        // accounts get BebopRouter.swap calldata.
        let selector = &quote
            .quote_attributes
            .get("calldata")
            .unwrap()[..4];
        if selector == SWAP_AGGREGATE_SELECTOR {
            let partial_fill_offset_slice = quote
                .quote_attributes
                .get("partial_fill_offset")
                .unwrap()
                .as_ref();
            let mut partial_fill_offset_array = [0u8; 8];
            partial_fill_offset_array.copy_from_slice(partial_fill_offset_slice);

            // This is the only attribute that is significantly different for the Single and
            // Aggregate Order
            assert_eq!(u64::from_be_bytes(partial_fill_offset_array), 2);
        } else {
            assert_eq!(selector, ROUTER_SWAP_SELECTOR);
        }
    }

    #[test]
    fn building_for_a_chain_bebop_does_not_serve_is_fatal() {
        let config = BookFeedConfig {
            chain: Chain::ZkSync,
            tokens: Arc::new(HashMap::new()),
            min_tvl_usd: 0.0,
        };

        let error = BebopFeedBuilder::new(config, HashSet::new(), "key".to_string())
            .build()
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    #[test]
    fn building_a_book_assembler_for_a_chain_bebop_does_not_serve_is_fatal() {
        let config = BookFeedConfig {
            chain: Chain::ZkSync,
            tokens: Arc::new(HashMap::new()),
            min_tvl_usd: 0.0,
        };

        let error = BebopBookAssemblerBuilder::new(config, HashSet::new(), "key".to_string())
            .build()
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    fn weth_usdc_config() -> (BookFeedConfig, Arc<HashSet<Bytes>>, BebopPrices) {
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let config = BookFeedConfig {
            chain: Chain::Ethereum,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&usdc, "USDC", 6)])),
            min_tvl_usd: 0.0,
        };
        let levels = |price| Levels::new(vec![PriceLevel { price, quantity: 1.0 }]).unwrap();
        let prices = BebopPrices {
            pairs: vec![BebopBook {
                base: weth,
                quote: usdc.clone(),
                last_update_ts: 1752617378000,
                bids: levels(3000.0),
                asks: levels(3001.0),
            }],
        };
        (config, Arc::new(HashSet::from([usdc])), prices)
    }

    /// An assembler built on its own makes the books the feed builder's own assembler makes from
    /// the same settings.
    #[test]
    fn the_assembler_builder_makes_the_books_the_feed_builder_does() {
        let (config, usd_quote_tokens, prices) = weth_usdc_config();
        let feed = BebopFeedBuilder::new(config.clone(), usd_quote_tokens.clone(), "key".into())
            .all_pairs()
            .with_fallback_router()
            .build()
            .unwrap();
        let assembler = BebopBookAssemblerBuilder::new(config, usd_quote_tokens, "key".into())
            .all_pairs()
            .with_fallback_router()
            .build()
            .unwrap();

        let books = assembler.books(prices.clone());

        assert_eq!(books.len(), 1);
        assert_same_books(&books, &feed.source.assembler.books(prices));
    }

    #[test]
    fn the_feed_makes_its_books_with_the_assembler_it_is_given() {
        let (config, usd_quote_tokens, prices) = weth_usdc_config();
        let assembler =
            BebopBookAssemblerBuilder::new(config.clone(), usd_quote_tokens.clone(), "key".into())
                .with_fallback_router()
                .build()
                .unwrap();

        let feed = BebopFeedBuilder::new(config, usd_quote_tokens, "key".into())
            .with_assembler(assembler.clone())
            .build()
            .unwrap();

        assert_same_books(
            &feed
                .source
                .assembler
                .books(prices.clone()),
            &assembler.books(prices),
        );
        assert_eq!(feed.source.assembler.protocol_system, FALLBACK_PROTOCOL_SYSTEM);
    }

    #[test]
    fn the_prices_feed_refuses_a_chain_bebop_does_not_serve() {
        let error = BebopPricesFeedBuilder::new(Chain::ZkSync, "key".to_string())
            .build()
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    #[test]
    fn the_prices_feed_runs_on_the_feed_tuning_it_is_given() {
        let tuned = WsFeedConfig {
            read_idle_timeout: Duration::from_secs(7),
            ..BebopFeedBuilder::default_feed_config()
        };

        let feed = BebopPricesFeedBuilder::new(Chain::Ethereum, "key".to_string())
            .feed_config(tuned)
            .build()
            .unwrap();

        assert_eq!(feed.feed_config.read_idle_timeout, Duration::from_secs(7));
    }

    #[test]
    fn the_prices_feed_debug_output_omits_the_api_key() {
        let feed = BebopPricesFeedBuilder::new(Chain::Ethereum, "secret_key".to_string())
            .build()
            .unwrap();

        assert!(!format!("{feed:?}").contains("secret_key"));
    }
}
