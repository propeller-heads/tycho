use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use crate::{
    book::{BookFeedConfig, BookSnapshot, ReceivedAt},
    rfq::{
        constants::DEFAULT_QUOTE_TIMEOUT,
        protocols::euclid::{
            client::{EuclidClient, EuclidLevelsClient, EUCLID_API_URL},
            models::{EuclidPrices, EuclidSupportedChain},
            source::{EuclidBookAssembler, EuclidBookSource, EuclidPricesSource},
        },
    },
    snapshot_feed::{
        errors::FeedError,
        ws::{default_ws_feed_config, run_ws_feed, WsFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// Euclid's book feed: streams the price levels Euclid's RFQ gateway publishes over its
/// WebSocket and publishes every frame as a complete [`BookSnapshot`]. Built through
/// [`EuclidFeedBuilder`].
#[derive(Clone, Debug)]
pub struct EuclidFeed {
    feed_config: WsFeedConfig,
    source: EuclidBookSource,
}

/// Builds an [`EuclidFeed`] from the shared [`BookFeedConfig`]. The book settings are those of
/// an [`EuclidBookAssemblerBuilder`]; [`with_assembler`](Self::with_assembler) supplies a built
/// assembler instead.
///
/// Euclid's gateway is public: the levels stream and binding quotes need no credentials, so a
/// solver enables the venue without asking for a key. A deployment that gates binding quotes
/// takes one through [`api_key`](Self::api_key).
pub struct EuclidFeedBuilder {
    assembler_builder: EuclidBookAssemblerBuilder,
    assembler: Option<EuclidBookAssembler>,
    feed_config: WsFeedConfig,
}

impl EuclidFeedBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
    ) -> Self {
        Self {
            assembler_builder: EuclidBookAssemblerBuilder::new(book_config, usd_quote_tokens),
            assembler: None,
            feed_config: Self::default_feed_config(),
        }
    }

    /// Makes the feed's books with `assembler` instead of one built from this builder's book
    /// settings, which are then unused. The levels stream still opens at this builder's
    /// [`api_url`](Self::api_url).
    pub fn with_assembler(mut self, assembler: EuclidBookAssembler) -> Self {
        self.assembler = Some(assembler);
        self
    }

    /// Publishes Euclid as one component for every pair it prices, which a route may take a
    /// single quote from: every quote is signed against one maker inventory, so a route that
    /// filled against two of Euclid's pairs would count that inventory twice.
    ///
    /// One component per pair otherwise.
    pub fn all_pairs(mut self) -> Self {
        self.assembler_builder = self.assembler_builder.all_pairs();
        self
    }

    /// Sent as `x-api-key` with binding quote requests, for a gateway that requires one.
    pub fn api_key(mut self, api_key: String) -> Self {
        self.assembler_builder = self.assembler_builder.api_key(api_key);
        self
    }

    /// The gateway to stream levels from and request quotes of, in place of Euclid's public
    /// one. The levels stream and binding quotes live at `/levels` and `/firm` under it.
    pub fn api_url(mut self, api_url: String) -> Self {
        self.assembler_builder = self.assembler_builder.api_url(api_url);
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.assembler_builder = self
            .assembler_builder
            .quote_timeout(timeout);
        self
    }

    /// The feed tuning a new builder starts from: the set every WebSocket feed shares. Spread
    /// from it to change one field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::rfq::protocols::euclid::feed::EuclidFeedBuilder;
    /// # use tycho_simulation::snapshot_feed::ws::WsFeedConfig;
    /// let tuned = WsFeedConfig {
    ///     read_idle_timeout: Duration::from_secs(30),
    ///     ..EuclidFeedBuilder::default_feed_config()
    /// };
    /// ```
    pub fn default_feed_config() -> WsFeedConfig {
        default_ws_feed_config()
    }

    /// Tune the price feed loop (timeouts, backoff, failure limit).
    pub fn feed_config(mut self, feed_config: WsFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails for chains Euclid does not serve.
    pub fn build(self) -> Result<EuclidFeed, FeedError> {
        let EuclidPricesFeed { feed_config, source: prices } =
            EuclidPricesFeedBuilder::new(self.assembler_builder.book_config.chain)
                .api_url(self.assembler_builder.api_url.clone())
                .feed_config(self.feed_config)
                .build()?;
        let assembler = match self.assembler {
            Some(assembler) => assembler,
            None => self.assembler_builder.build()?,
        };
        Ok(EuclidFeed { feed_config, source: EuclidBookSource { prices, assembler } })
    }
}

/// Builds an [`EuclidBookAssembler`] from the book settings alone: the shared
/// [`BookFeedConfig`], the USD quote tokens, the firm-quote client's gateway, key and timeout,
/// and the component layout. Opens no connection; pass the result to
/// [`EuclidFeedBuilder::with_assembler`] or use it on its own with prices from an
/// [`EuclidPricesFeed`].
pub struct EuclidBookAssemblerBuilder {
    book_config: BookFeedConfig,
    usd_quote_tokens: Arc<HashSet<Bytes>>,
    api_url: String,
    api_key: Option<String>,
    quote_timeout: Duration,
    all_pairs: bool,
}

impl EuclidBookAssemblerBuilder {
    pub fn new(
        book_config: BookFeedConfig,
        usd_quote_tokens: impl Into<Arc<HashSet<Bytes>>>,
    ) -> Self {
        Self {
            book_config,
            usd_quote_tokens: usd_quote_tokens.into(),
            api_url: EUCLID_API_URL.to_string(),
            api_key: None,
            quote_timeout: DEFAULT_QUOTE_TIMEOUT,
            all_pairs: false,
        }
    }

    /// See [`EuclidFeedBuilder::all_pairs`].
    pub fn all_pairs(mut self) -> Self {
        self.all_pairs = true;
        self
    }

    /// See [`EuclidFeedBuilder::api_key`].
    pub fn api_key(mut self, api_key: String) -> Self {
        self.api_key = Some(api_key);
        self
    }

    /// The gateway binding quotes are requested of, in place of Euclid's public one. See
    /// [`EuclidFeedBuilder::api_url`].
    pub fn api_url(mut self, api_url: String) -> Self {
        self.api_url = api_url;
        self
    }

    /// Deadline for binding quote requests.
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Fails for chains Euclid does not serve.
    pub fn build(self) -> Result<EuclidBookAssembler, FeedError> {
        let chain =
            EuclidSupportedChain::try_from(self.book_config.chain).map_err(FeedError::Fatal)?;
        Ok(EuclidBookAssembler {
            chain,
            book_config: self.book_config,
            usd_quote_tokens: self.usd_quote_tokens,
            client: Arc::new(EuclidClient::new(
                chain,
                &self.api_url,
                self.api_key,
                self.quote_timeout,
            )),
            all_pairs: self.all_pairs,
        })
    }
}

impl SnapshotFeed for EuclidFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_ws_feed(self.feed_config, publisher, self.source)
    }
}

/// Streams Euclid's levels WebSocket and publishes each frame as [`EuclidPrices`]; binary
/// frames are skipped. Requires only the chain, since the stream is public.
/// [`EuclidBookAssembler`] applies the token list and TVL floor.
#[derive(Clone, Debug)]
pub struct EuclidPricesFeed {
    feed_config: WsFeedConfig,
    source: EuclidPricesSource,
}

/// Builds an [`EuclidPricesFeed`] from the chain.
pub struct EuclidPricesFeedBuilder {
    chain: Chain,
    api_url: String,
    feed_config: WsFeedConfig,
}

impl EuclidPricesFeedBuilder {
    /// The feed tuning starts from [`EuclidFeedBuilder::default_feed_config`].
    pub fn new(chain: Chain) -> Self {
        Self {
            chain,
            api_url: EUCLID_API_URL.to_string(),
            feed_config: EuclidFeedBuilder::default_feed_config(),
        }
    }

    /// The gateway to stream levels from, in place of Euclid's public one. See
    /// [`EuclidFeedBuilder::api_url`].
    pub fn api_url(mut self, api_url: String) -> Self {
        self.api_url = api_url;
        self
    }

    /// Tune the connection loop (timeouts, backoff, failure limit).
    pub fn feed_config(mut self, feed_config: WsFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// Fails with [`FeedError::Fatal`] for a chain Euclid does not serve.
    pub fn build(self) -> Result<EuclidPricesFeed, FeedError> {
        let chain = EuclidSupportedChain::try_from(self.chain).map_err(FeedError::Fatal)?;
        Ok(EuclidPricesFeed {
            feed_config: self.feed_config,
            source: EuclidPricesSource::new(EuclidLevelsClient::new(chain, &self.api_url)),
        })
    }
}

impl SnapshotFeed for EuclidPricesFeed {
    type Snapshot = EuclidPrices;
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
    use std::{net::SocketAddr, str::FromStr};

    use futures::{SinkExt, StreamExt};
    use num_bigint::BigUint;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    use tycho_common::models::protocol::GetAmountOutParams;

    use super::*;
    use crate::{
        book::{
            assert_same_books,
            component::pair_component_id,
            levels::{Levels, PriceLevel},
            test_token_map, BookFeedEvent, BookFeedStream,
        },
        rfq::protocols::euclid::{models::EuclidBook, PROTOCOL_SYSTEM},
        snapshot_feed::{SnapshotFeedEvent, SnapshotFeedStream},
    };

    /// A levels frame recorded from Euclid's gateway on Ethereum: WETH/USDC.
    const LEVELS_FRAME: &str = include_str!("test_responses/levels_frame.json");

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn book_config(chain: Chain) -> BookFeedConfig {
        let (weth, usdc) = (Bytes::from_str(WETH).unwrap(), Bytes::from_str(USDC).unwrap());
        BookFeedConfig {
            chain,
            tokens: Arc::new(test_token_map(&[(&weth, "WETH", 18), (&usdc, "USDC", 6)])),
            min_tvl_usd: 0.0,
        }
    }

    fn usd_quote_tokens() -> HashSet<Bytes> {
        HashSet::from([Bytes::from_str(USDC).unwrap()])
    }

    /// Prices for one WETH/USDC book.
    fn weth_usdc_prices() -> EuclidPrices {
        let levels = |price| Levels::new(vec![PriceLevel { price, quantity: 1.0 }]).unwrap();
        EuclidPrices {
            chain_id: 1,
            pairs: vec![EuclidBook {
                base: Bytes::from_str(WETH).unwrap(),
                quote: Bytes::from_str(USDC).unwrap(),
                updated_at_ms: 1_757_500_000_000,
                bids: levels(3000.0),
                asks: levels(3001.0),
            }],
        }
    }

    /// A local WebSocket server that pushes the recorded frame to its first client and keeps
    /// the connection open while the test reads.
    async fn serve_one_frame() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(LEVELS_FRAME.into()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        address
    }

    #[test]
    fn building_for_a_chain_euclid_does_not_serve_is_fatal() {
        let error = EuclidFeedBuilder::new(book_config(Chain::Base), usd_quote_tokens())
            .build()
            .unwrap_err();
        assert!(matches!(error, FeedError::Fatal(ref message) if message.contains("Euclid")));
    }

    #[test]
    fn building_a_book_assembler_for_a_chain_euclid_does_not_serve_is_fatal() {
        let error = EuclidBookAssemblerBuilder::new(book_config(Chain::Base), usd_quote_tokens())
            .build()
            .unwrap_err();
        assert!(error.is_fatal(), "{error}");
    }

    #[test]
    fn the_prices_feed_refuses_a_chain_euclid_does_not_serve() {
        let error = EuclidPricesFeedBuilder::new(Chain::Base)
            .build()
            .unwrap_err();
        assert!(error.is_fatal(), "{error}");
    }

    #[test]
    fn the_prices_feed_runs_on_the_feed_tuning_it_is_given() {
        let tuned = WsFeedConfig {
            read_idle_timeout: Duration::from_secs(7),
            ..EuclidFeedBuilder::default_feed_config()
        };
        let feed = EuclidPricesFeedBuilder::new(Chain::Ethereum)
            .feed_config(tuned)
            .build()
            .unwrap();
        assert_eq!(feed.feed_config.read_idle_timeout, Duration::from_secs(7));
    }

    /// The feed builder's `api_url` points both halves at the same gateway: the levels stream
    /// and the quote client.
    #[test]
    fn the_api_url_reaches_the_levels_stream_and_the_quote_client() {
        let feed = EuclidFeedBuilder::new(book_config(Chain::Ethereum), usd_quote_tokens())
            .api_url("https://rfq.example.com/tycho".to_string())
            .build()
            .unwrap();
        let rendered = format!("{feed:?}");
        assert!(rendered.contains("wss://rfq.example.com/tycho/levels?chainId=1"), "{rendered}");
        assert!(rendered.contains("https://rfq.example.com/tycho/firm"), "{rendered}");
    }

    /// An assembler built on its own makes the books the feed builder's own assembler makes from
    /// the same settings.
    #[test]
    fn the_assembler_builder_makes_the_books_the_feed_builder_does() {
        let feed = EuclidFeedBuilder::new(book_config(Chain::Ethereum), usd_quote_tokens())
            .all_pairs()
            .build()
            .unwrap();
        let assembler =
            EuclidBookAssemblerBuilder::new(book_config(Chain::Ethereum), usd_quote_tokens())
                .all_pairs()
                .build()
                .unwrap();

        let books = assembler.books(weth_usdc_prices());
        assert_eq!(books.len(), 1);
        assert_same_books(
            &books,
            &feed
                .source
                .assembler
                .books(weth_usdc_prices()),
        );
    }

    #[test]
    fn the_feed_makes_its_books_with_the_assembler_it_is_given() {
        let assembler =
            EuclidBookAssemblerBuilder::new(book_config(Chain::Ethereum), usd_quote_tokens())
                .all_pairs()
                .build()
                .unwrap();
        let feed = EuclidFeedBuilder::new(book_config(Chain::Ethereum), usd_quote_tokens())
            .with_assembler(assembler.clone())
            .build()
            .unwrap();

        assert!(feed.source.assembler.all_pairs);
        assert_same_books(
            &feed
                .source
                .assembler
                .books(weth_usdc_prices()),
            &assembler.books(weth_usdc_prices()),
        );
    }

    /// The feed connects to the levels stream it is pointed at and publishes what the stream
    /// pushes, end to end through the shared WebSocket loop.
    #[tokio::test]
    async fn a_pushed_frame_is_published_as_a_snapshot() {
        let address = serve_one_frame().await;
        let feed = EuclidFeedBuilder::new(book_config(Chain::Ethereum), usd_quote_tokens())
            .api_url(format!("http://{address}/tycho"))
            .build()
            .unwrap();
        let mut stream = BookFeedStream::spawn(PROTOCOL_SYSTEM, feed);

        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a snapshot within five seconds")
            .expect("the feed is still running");
        let BookFeedEvent::Published(snapshot) = event else {
            panic!("expected a published snapshot, got {event:?}");
        };
        let id = pair_component_id(
            PROTOCOL_SYSTEM,
            &Bytes::from_str(WETH).unwrap(),
            &Bytes::from_str(USDC).unwrap(),
        );
        let book = &snapshot.books[&id.to_string()];
        let [base, quote] = book.component.tokens.as_slice() else {
            panic!("expected exactly two tokens")
        };
        assert!(
            book.state
                .spot_price(base, quote)
                .unwrap() >
                0.0
        );
    }

    /// The prices feed publishes the frame itself, for a reader that assembles books elsewhere.
    #[tokio::test]
    async fn the_prices_feed_publishes_the_frame_it_receives() {
        let address = serve_one_frame().await;
        let feed = EuclidPricesFeedBuilder::new(Chain::Ethereum)
            .api_url(format!("http://{address}/tycho"))
            .build()
            .unwrap();
        let mut stream = SnapshotFeedStream::spawn(PROTOCOL_SYSTEM, feed);

        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("prices within five seconds")
            .expect("the feed is still running");
        let SnapshotFeedEvent::Published(prices) = event else {
            panic!("expected published prices, got {event:?}");
        };
        assert_eq!(prices.chain_id, 1);
        assert_eq!(prices.pairs.len(), 1);
        assert_eq!(prices.pairs[0].base, Bytes::from_str(WETH).unwrap());
    }

    /// Against Euclid's live gateway: one snapshot off the levels stream, and a binding quote
    /// for a book in it.
    #[tokio::test]
    #[ignore = "hits Euclid's live gateway"]
    async fn euclid_live_levels_and_quote() {
        let config = book_config(Chain::Ethereum);
        let feed = EuclidFeedBuilder::new(config.clone(), usd_quote_tokens())
            .build()
            .unwrap();
        let mut stream = BookFeedStream::spawn(PROTOCOL_SYSTEM, feed);
        let event = tokio::time::timeout(Duration::from_secs(15), stream.next())
            .await
            .expect("a snapshot within fifteen seconds")
            .expect("the feed is still running");
        let BookFeedEvent::Published(snapshot) = event else {
            panic!("expected a published snapshot, got {event:?}");
        };
        let book = snapshot
            .books
            .values()
            .next()
            .expect("Euclid prices at least one pair");

        let router = Bytes::from_str("0x1644D2477f809cc2C71bCCFd6Dc9497E3F83210d").unwrap();
        let quote = book
            .state
            .as_indicatively_priced()
            .unwrap()
            .request_signed_quote(GetAmountOutParams {
                amount_in: BigUint::from(3_000_000_000_000_000u64),
                token_in: book.component.tokens[0].address.clone(),
                token_out: book.component.tokens[1].address.clone(),
                sender: router.clone(),
                receiver: router,
                origin: None,
            })
            .await
            .unwrap();
        assert!(quote.amount_out > BigUint::ZERO);
        assert_eq!(quote.quote_attributes["partial_fill_offset"].as_ref(), 8u64.to_be_bytes());
    }
}
