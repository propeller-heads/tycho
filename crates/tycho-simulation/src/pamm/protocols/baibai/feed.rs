//! BaiBai's book feed: polls the Base deployment over JSON-RPC and publishes every poll as a
//! complete [`BookSnapshot`].

use std::{future::Future, time::Duration};

use alloy::primitives::Address;
use chrono::DateTime;
use reqwest::Url;
use tracing::{debug, warn};
use tycho_common::{models::Chain, Bytes};

use crate::{
    book::{
        component::{pair_book, pair_component_id},
        sim, BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    evm::protocol::u256_num::u256_to_biguint,
    pamm::protocols::baibai::{
        reader::{BaibaiReader, BookRead, PairRead},
        state::BaibaiState,
        Deployment, BASE_ATTRIBUTE, BASE_DEPLOYMENT, BASE_LISTED_BASES, PROTOCOL_SYSTEM,
        PROTOCOL_TYPE, QUOTE_ATTRIBUTE, ROUTER_FEE_PIPS,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig, HttpSource},
        Publisher, SnapshotFeed,
    },
};

/// Base's block time. A curve that expires before the next block can no longer be filled by a
/// swap priced from it, so it is not published.
const NEXT_BLOCK_SECONDS: u64 = 2;

/// BaiBai's book feed. Built through [`BaibaiFeedBuilder`]. Neither its `Debug` output nor its
/// errors carry the RPC URL, which commonly carries a key.
#[derive(Debug, Clone)]
pub struct BaibaiFeed {
    feed_config: HttpFeedConfig,
    source: BaibaiBookSource,
}

/// Builds a [`BaibaiFeed`] from the shared [`BookFeedConfig`] and a Base JSON-RPC URL. Every poll
/// makes two `eth_call`s against recent state, so a full node is enough.
pub struct BaibaiFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    bases: Vec<Bytes>,
    feed_config: HttpFeedConfig,
}

impl BaibaiFeedBuilder {
    pub fn new(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self {
            book_config,
            rpc_url,
            bases: BASE_LISTED_BASES
                .iter()
                .map(|base| Bytes::from(base.to_vec()))
                .collect(),
            feed_config: Self::default_feed_config(),
        }
    }

    /// The base tokens to serve, replacing the bases listed when this integration was written
    /// (WETH and cbBTC). The CurveBook cannot list its pairs, so this is how a newly listed base
    /// is served; one the book has not listed is skipped every poll.
    pub fn bases(mut self, bases: Vec<Bytes>) -> Self {
        self.bases = bases;
        self
    }

    /// Tune the poll loop (poll cadence, request timeout, failure limit).
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// The feed tuning a new builder starts from.
    ///
    /// Makers refresh their curves every Base block (2 s) and a curve expires a few seconds after
    /// its update (the earlier of its `validUntil` and the book's TTL, both read every poll). The
    /// feed polls every second to pick up each block soon after it lands, gives a poll 2 s, and
    /// withdraws a book 4 s after its last refresh. A poll publishes only curves still live in
    /// the next block, but while polls fail a book can outlive its curve by up to the withdrawal
    /// age; a swap priced from an expired curve reverts at the venue.
    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            max_snapshot_age: Some(Duration::from_secs(4)),
            ..default_http_feed_config()
        }
    }

    /// Fails for a chain other than Base, the only one BaiBai is deployed on, for an RPC URL
    /// that is not one, and for bases that are not addresses.
    pub fn build(self) -> Result<BaibaiFeed, FeedError> {
        if self.book_config.chain != Chain::Base {
            return Err(FeedError::InvalidInput(format!(
                "BaiBai is deployed on Base only, not {}",
                self.book_config.chain
            )));
        }
        let rpc_url = Url::parse(&self.rpc_url)
            .map_err(|e| FeedError::InvalidInput(format!("BaiBai RPC URL: {e}")))?;
        if self.bases.is_empty() {
            return Err(FeedError::InvalidInput("BaiBai feed has no bases to serve".into()));
        }
        let bases = self
            .bases
            .iter()
            .map(|base| address(base, "base"))
            .collect::<Result<Vec<_>, _>>()?;
        let deployment = BASE_DEPLOYMENT;
        Ok(BaibaiFeed {
            feed_config: self.feed_config,
            source: BaibaiBookSource {
                reader: BaibaiReader::new(rpc_url, deployment, bases),
                assembler: BaibaiBookAssembler { book_config: self.book_config, deployment },
            },
        })
    }
}

fn address(bytes: &Bytes, what: &str) -> Result<Address, FeedError> {
    Address::try_from(bytes.as_ref())
        .map_err(|_| FeedError::InvalidInput(format!("BaiBai {what} {bytes} is not an address")))
}

impl SnapshotFeed for BaibaiFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}

/// One poll: every configured pair read from the chain and turned into books.
#[derive(Debug, Clone)]
pub(super) struct BaibaiBookSource {
    pub(super) reader: BaibaiReader,
    pub(super) assembler: BaibaiBookAssembler,
}

impl HttpSource for BaibaiBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A read that fails fails the poll. Pairs the read returns but no book can be made of are
    /// left out of it, so a poll whose every curve has expired withdraws every pair.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let read = self.reader.read().await?;
        Ok(BookSnapshot::received_now(self.assembler.books(&read)))
    }
}

/// Turns one [`BookRead`] into books: one per pair between served tokens that clears the TVL
/// floor and can still be filled in the next block.
#[derive(Debug, Clone)]
pub(super) struct BaibaiBookAssembler {
    pub(super) book_config: BookFeedConfig,
    pub(super) deployment: Deployment,
}

impl BaibaiBookAssembler {
    pub(super) fn books(&self, read: &BookRead) -> Books {
        let mut books = Books::new();
        for pair in &read.pairs {
            let Some(state) = self.state(read, pair) else {
                continue;
            };
            let base = state.base_token.address.clone();
            let quote = state.quote_token.address.clone();
            let component_id = pair_component_id(PROTOCOL_SYSTEM, &base, &quote);
            if !self
                .book_config
                .clears_min_tvl(tvl_usd(&state), &component_id)
            {
                continue;
            }
            let updated_at = i64::try_from(pair.last_update_at)
                .ok()
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
            let mut book = pair_book(
                PROTOCOL_SYSTEM,
                PROTOCOL_TYPE,
                self.book_config.chain,
                component_id,
                state,
                updated_at,
            );
            let attributes = &mut book.component.static_attributes;
            attributes.insert(BASE_ATTRIBUTE.to_string(), base);
            attributes.insert(QUOTE_ATTRIBUTE.to_string(), quote);
            books.insert(book);
        }
        books
    }

    /// The pair's state, or `None`, logged, when its tokens are not served, its curve expires
    /// before the next block or the chain reports it malformed.
    fn state(&self, read: &BookRead, pair: &PairRead) -> Option<BaibaiState> {
        let base = Bytes::from(pair.base.to_vec());
        let quote = Bytes::from(self.deployment.quote.to_vec());
        let (base_token, quote_token) = self
            .book_config
            .pair_tokens(&base, &quote)?;
        let expires_at = expiry(pair.valid_until, pair.last_update_at, read.ttl);
        if expires_at <
            read.timestamp
                .saturating_add(NEXT_BLOCK_SECONDS)
        {
            debug!(%base, expires_at, block_timestamp = read.timestamp, "skipping expiring curve");
            return None;
        }
        let state = BaibaiState {
            base_token: base_token.clone(),
            quote_token: quote_token.clone(),
            mid: pair.mid,
            q_unit: pair.q_unit,
            c_unit: read.c_unit,
            ask: pair.ask.clone(),
            bid: pair.bid.clone(),
            available: [pair.available_base, read.available_quote],
            fee_pips: ROUTER_FEE_PIPS,
        };
        match state.validated() {
            Ok(state) => Some(state),
            Err(error) => {
                warn!(
                    %base,
                    block = read.block_number,
                    %error,
                    "skipping pair the chain reports malformed"
                );
                None
            }
        }
    }
}

/// The custody the pair can pay out, valued at its mid price, in USD: the deployment quotes in
/// USDC. Every pair counts the custodian's whole quote balance, which all pairs share.
fn tvl_usd(state: &BaibaiState) -> f64 {
    let (base, quote) = (&state.base_token, &state.quote_token);
    let mid = sim::to_human(&u256_to_biguint(state.mid), 18) *
        10f64.powi(base.decimals as i32 - quote.decimals as i32);
    sim::to_human(&u256_to_biguint(state.available[1]), quote.decimals) +
        sim::to_human(&u256_to_biguint(state.available[0]), base.decimals) * mid
}

/// When a curve stops filling: at `validUntil`, or `ttl` seconds after its last update when that
/// comes first. A zero TTL leaves only `validUntil`.
fn expiry(valid_until: u64, last_update_at: u64, ttl: u64) -> u64 {
    if ttl == 0 {
        return valid_until;
    }
    valid_until.min(last_update_at.saturating_add(ttl))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use alloy::primitives::Address;
    use rstest::rstest;
    use tycho_common::models::token::Token;

    use super::*;
    use crate::{
        book::{chain_without_id, Book},
        pamm::protocols::baibai::test_support::{
            book_config, bytes, cbbtc, state, token, weth, FakeNode,
        },
    };

    fn feed(node_url: &str, book_config: BookFeedConfig) -> BaibaiFeed {
        BaibaiFeedBuilder::new(book_config, node_url.to_string())
            .build()
            .unwrap()
    }

    async fn fetch(
        node: FakeNode,
        book_config: BookFeedConfig,
        bases: Option<Vec<Address>>,
    ) -> Result<HashMap<String, Book>, FeedError> {
        let server = node.spawn().await;
        let mut builder = BaibaiFeedBuilder::new(book_config, server.url());
        if let Some(bases) = bases {
            builder = builder.bases(bases.into_iter().map(bytes).collect());
        }
        let snapshot = builder
            .build()
            .unwrap()
            .source
            .fetch()
            .await?;
        Ok(Arc::try_unwrap(snapshot.books).unwrap())
    }

    async fn books(node: FakeNode, book_config: BookFeedConfig) -> HashMap<String, Book> {
        fetch(node, book_config, None)
            .await
            .unwrap()
    }

    fn quote_token() -> Token {
        token(BASE_DEPLOYMENT.quote)
    }

    /// One book per listed pair, labelled for BaiBai's encoder, holding the recorded curve priced
    /// with the configured router fee.
    #[tokio::test]
    async fn every_listed_pair_is_published_as_a_book() {
        let node = FakeNode::new();
        let fixture = node.fixture.clone();

        let books = books(node, book_config()).await;

        assert_eq!(books.len(), fixture.pairs.len());
        for pair in &fixture.pairs {
            let base = bytes(pair.base);
            let id = pair_component_id(PROTOCOL_SYSTEM, &base, &quote_token().address);
            let book = &books[&id.to_string()];
            assert_eq!(book.component.protocol_system, "baibai");
            assert_eq!(book.component.static_attributes["base"], base);
            assert_eq!(book.component.static_attributes["quote"], quote_token().address);
            assert_eq!(book.updated_at.unwrap().timestamp() as u64, pair.pair.last_update_at);
            let expected = BaibaiState { fee_pips: ROUTER_FEE_PIPS, ..state(&fixture, pair) };
            assert!(book.state.eq(&expected), "{:?}", book.state);
        }
    }

    /// A base the book never listed reads as an empty pair, and one that is no token at all makes
    /// the custodian revert; either way only that base is left out.
    #[rstest]
    #[case::an_unlisted_token(false)]
    #[case::an_address_that_is_no_token(true)]
    #[tokio::test]
    async fn a_base_without_a_pair_is_skipped_and_the_rest_served(#[case] no_token: bool) {
        let stranger = Address::repeat_byte(0x77);
        let mut node = FakeNode::new();
        if no_token {
            node.not_tokens.push(stranger);
        }

        let books = fetch(node, book_config(), Some(vec![stranger, weth(), cbbtc()]))
            .await
            .unwrap();

        assert_eq!(books.len(), 2);
    }

    /// Contracts that no longer point at each other are a deployment this integration was not
    /// built for, which no later poll fixes.
    #[tokio::test]
    async fn a_rewired_deployment_ends_the_feed() {
        let mut node = FakeNode::new();
        node.entrypoint_curve_book = Address::repeat_byte(0x99);

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    /// A curve that expires before the next block cannot fill a swap priced from it. The
    /// recording's curves run until `validUntil`, six seconds after their update.
    #[rstest]
    #[case::fresh(0, 2)]
    #[case::expiring_before_the_next_block(5, 0)]
    #[tokio::test]
    async fn curves_expiring_before_the_next_block_are_withdrawn(
        #[case] seconds_later: u64,
        #[case] expected: usize,
    ) {
        let mut node = FakeNode::new();
        node.timestamp += seconds_later;

        assert_eq!(books(node, book_config()).await.len(), expected);
    }

    #[rstest]
    #[case::valid_until_comes_first(100, 90, 20, 100)]
    #[case::the_ttl_comes_first(100, 90, 5, 95)]
    #[case::a_zero_ttl_leaves_valid_until(100, 90, 0, 100)]
    fn a_curve_expires_at_the_earlier_of_valid_until_and_its_ttl(
        #[case] valid_until: u64,
        #[case] last_update_at: u64,
        #[case] ttl: u64,
        #[case] expected: u64,
    ) {
        assert_eq!(expiry(valid_until, last_update_at, ttl), expected);
    }

    #[tokio::test]
    async fn pairs_outside_the_token_list_or_under_the_floor_are_skipped() {
        let mut config = book_config();
        let mut tokens = (*config.tokens).clone();
        tokens.remove(&bytes(cbbtc()));
        config.tokens = Arc::new(tokens);
        assert_eq!(
            books(FakeNode::new(), config)
                .await
                .len(),
            1
        );

        let mut config = book_config();
        config.min_tvl_usd = 1e12;
        assert!(books(FakeNode::new(), config)
            .await
            .is_empty());
    }

    #[rstest]
    #[case::another_chain(Some(Chain::Ethereum), "http://node", None)]
    #[case::a_chain_with_no_id(Some(chain_without_id()), "http://node", None)]
    #[case::not_a_url(None, "node", None)]
    #[case::a_base_that_is_not_an_address(None, "http://node", Some(vec![Bytes::from(vec![1u8; 4])]))]
    #[case::no_bases(None, "http://node", Some(vec![]))]
    fn the_builder_refuses_what_it_cannot_run(
        #[case] chain: Option<Chain>,
        #[case] url: &str,
        #[case] bases: Option<Vec<Bytes>>,
    ) {
        let mut config = book_config();
        if let Some(chain) = chain {
            config.chain = chain;
        }
        let mut builder = BaibaiFeedBuilder::new(config, url.to_string());
        if let Some(bases) = bases {
            builder = builder.bases(bases);
        }

        let error = builder.build().unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[test]
    fn debug_output_omits_the_rpc_url() {
        let feed = feed("https://base.example/secret_key", book_config());

        assert!(!format!("{feed:?}").contains("secret_key"));
    }

    /// The HTTP client quotes the URL a request failed on; the poll's error, which the feed logs,
    /// does not.
    #[tokio::test]
    async fn a_failed_poll_does_not_report_the_rpc_url() {
        let feed = feed("http://127.0.0.1:1/secret_key", book_config());

        let error = feed.source.fetch().await.unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        assert!(!error.to_string().contains("secret_key"), "{error}");
    }

    /// Polls the live Base deployment: each poll fits the request timeout and publishes both listed
    /// pairs.
    #[tokio::test]
    #[ignore = "reads Base mainnet; set BASE_RPC_URL"]
    async fn live_poll_publishes_the_listed_pairs() {
        let url = std::env::var("BASE_RPC_URL").expect("BASE_RPC_URL not set");
        let feed = feed(&url, book_config());
        let budget = BaibaiFeedBuilder::default_feed_config().request_timeout;
        let mut snapshot = None;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            snapshot = Some(feed.source.fetch().await.unwrap());
            let elapsed = started.elapsed();
            println!("poll took {elapsed:?} of a {budget:?} budget");
            assert!(elapsed < budget);
        }
        let snapshot = snapshot.unwrap();

        for book in snapshot.books.values() {
            let tokens = &book.component.tokens;
            let limits = book
                .state
                .get_limits(tokens[0].address.clone(), tokens[1].address.clone())
                .unwrap();
            println!(
                "{} {}/{}: sell limits {limits:?}, fee {}",
                book.component.id,
                tokens[0].symbol,
                tokens[1].symbol,
                book.state.fee()
            );
        }
        assert_eq!(snapshot.books.len(), 2, "WETH and cbBTC are listed and live");
    }
}
