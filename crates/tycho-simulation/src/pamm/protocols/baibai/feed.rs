//! BaiBai's book feed: polls the Base deployment over JSON-RPC and publishes every poll as a
//! complete [`BookSnapshot`].

use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Address;
use chrono::DateTime;
use reqwest::Url;
use tracing::{debug, warn};
use tycho_common::Bytes;

use crate::{
    book::{
        component::{pair_book, pair_component_id},
        sim, BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    evm::protocol::u256_num::u256_to_biguint,
    pamm::protocols::baibai::{
        reader::{BaibaiReader, BookRead, PairRead},
        state::BaibaiState,
        Deployment, BASE_ATTRIBUTE, BASE_DEPLOYMENT, BASE_LISTED_BASES, BASE_ROUTER,
        PROTOCOL_SYSTEM, PROTOCOL_TYPE, QUOTE_ATTRIBUTE,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig, HttpSource},
        Publisher, SnapshotFeed,
    },
};

/// How many blocks a read's block may trail the wall clock. Past it the node is lagging, and
/// curves it reports live may already have expired on chain.
const MAX_BLOCK_LAG: u64 = 2;

/// BaiBai's book feed. Built through [`BaibaiFeedBuilder`]. Neither its `Debug` output nor its
/// errors carry the RPC URL, which commonly carries a key.
#[derive(Debug, Clone)]
pub struct BaibaiFeed {
    feed_config: HttpFeedConfig,
    source: BaibaiBookSource,
}

/// Builds a [`BaibaiFeed`] from the shared [`BookFeedConfig`] and a Base JSON-RPC URL. Every poll
/// makes one `eth_call` against the latest state, so a full node is enough.
pub struct BaibaiFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    bases: Vec<Bytes>,
    taker: Bytes,
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
            taker: Bytes::from(BASE_ROUTER.to_vec()),
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

    /// The address that calls BaiBai's entrypoint, replacing Tycho's router on Base. The
    /// entrypoint charges each caller its own fee and the books are priced with this one's, so
    /// set it when swaps go through another router.
    pub fn taker(mut self, taker: Bytes) -> Self {
        self.taker = taker;
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
    /// that is not one, and for bases or a taker that are not addresses. The node's chain is
    /// checked on every poll, which fails fatally when the node serves another one.
    pub fn build(self) -> Result<BaibaiFeed, FeedError> {
        let deployment = BASE_DEPLOYMENT;
        if self.book_config.chain != deployment.chain {
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
        let taker = address(&self.taker, "taker")?;
        Ok(BaibaiFeed {
            feed_config: self.feed_config,
            source: BaibaiBookSource {
                reader: BaibaiReader::new(rpc_url, deployment, bases, taker),
                assembler: BaibaiBookAssembler { book_config: self.book_config, deployment },
                clock: Arc::new(unix_now),
                last_block: Arc::new(AtomicU64::new(0)),
            },
        })
    }
}

fn address(bytes: &Bytes, what: &str) -> Result<Address, FeedError> {
    Address::try_from(bytes.as_ref()).map_err(|e| {
        FeedError::InvalidInput(format!("BaiBai {what} {bytes} is not an address: {e}"))
    })
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
#[derive(Clone, derive_more::Debug)]
pub(super) struct BaibaiBookSource {
    pub(super) reader: BaibaiReader,
    pub(super) assembler: BaibaiBookAssembler,
    /// The current Unix time in seconds.
    #[debug(skip)]
    pub(super) clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The newest block a poll has read, so that a node answering from an older one is caught.
    pub(super) last_block: Arc<AtomicU64>,
}

impl HttpSource for BaibaiBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A read that fails fails the poll, and so does one from a block older than a previous
    /// poll's or more than [`MAX_BLOCK_LAG`] blocks behind the wall clock: a load-balanced or
    /// lagging node would otherwise roll the books back or publish curves expired on chain.
    /// Pairs the read returns but no book can be made of are left out of it, so a poll whose
    /// every curve has expired withdraws every pair.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let read = self.reader.read().await?;
        let now = (self.clock)();
        if now > read.timestamp + MAX_BLOCK_LAG * self.assembler.block_time() {
            return Err(FeedError::Connection(format!(
                "BaiBai node is {} s behind: its latest block {} is from {}",
                now - read.timestamp,
                read.block_number,
                read.timestamp
            )));
        }
        let newest = self
            .last_block
            .fetch_max(read.block_number, Ordering::Relaxed);
        if read.block_number < newest {
            return Err(FeedError::Connection(format!(
                "BaiBai node answered from block {}, older than block {newest} already read",
                read.block_number
            )));
        }
        Ok(BookSnapshot::received_now(self.assembler.books(&read, now)))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Turns one [`BookRead`] into books: one per pair between served tokens that clears the TVL
/// floor and can still be filled in the next block.
#[derive(Debug, Clone)]
pub(super) struct BaibaiBookAssembler {
    pub(super) book_config: BookFeedConfig,
    pub(super) deployment: Deployment,
}

impl BaibaiBookAssembler {
    /// The chain's block time in seconds. [`BaibaiFeedBuilder::build`] admits Base only, whose
    /// block time is always registered.
    fn block_time(&self) -> u64 {
        self.book_config.chain.block_time_secs()
    }

    /// The books of `read`, judging expiry at `now` (Unix seconds) or the read's block timestamp,
    /// whichever is later.
    pub(super) fn books(&self, read: &BookRead, now: u64) -> Books {
        let mut books = Books::new();
        for pair in &read.pairs {
            let Some(state) = self.state(read, pair, now.max(read.timestamp)) else {
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
    /// before the block after `now` or the chain reports it malformed.
    fn state(&self, read: &BookRead, pair: &PairRead, now: u64) -> Option<BaibaiState> {
        let base = Bytes::from(pair.base.to_vec());
        let quote = Bytes::from(self.deployment.quote.to_vec());
        let (base_token, quote_token) = self
            .book_config
            .pair_tokens(&base, &quote)?;
        let expires_at = expiry(pair.valid_until, pair.last_update_at, read.ttl);
        if expires_at < now.saturating_add(self.block_time()) {
            debug!(%base, expires_at, now, "skipping expiring curve");
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
            fee_pips: pair.fee_pips,
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
        10f64.powf(f64::from(base.decimals) - f64::from(quote.decimals));
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

    use alloy::{primitives::Address, sol_types::SolCall};
    use rstest::rstest;
    use tycho_common::models::{token::Token, Chain};

    use super::*;
    use crate::{
        book::{chain_without_id, Book},
        pamm::protocols::baibai::{
            math::MAX_KNOTS,
            reader::{knotCall, pairCall, ttlCall, validUntilCall, IMPLEMENTATION_SLOT},
            test_support::{
                book_config, bytes, cbbtc, state, token, weth, FakeNode, TAKER_FEE_SELECTOR,
            },
        },
    };

    fn feed(node_url: &str, book_config: BookFeedConfig) -> BaibaiFeed {
        BaibaiFeedBuilder::new(book_config, node_url.to_string())
            .build()
            .unwrap()
    }

    /// The books one poll of `node` publishes, with the wall clock at the node's block timestamp.
    async fn fetch(
        node: FakeNode,
        book_config: BookFeedConfig,
        bases: Option<Vec<Address>>,
    ) -> Result<HashMap<String, Book>, FeedError> {
        let now = node.timestamp;
        let server = node.spawn().await;
        let mut builder = BaibaiFeedBuilder::new(book_config, server.url());
        if let Some(bases) = bases {
            builder = builder.bases(bases.into_iter().map(bytes).collect());
        }
        let mut source = builder.build().unwrap().source;
        source.clock = Arc::new(move || now);
        let snapshot = source.fetch().await?;
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
    /// with the fee the entrypoint reports for Tycho's router: zero since its whitelist.
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
            let updated_at = book.updated_at.unwrap().timestamp();
            assert_eq!(u64::try_from(updated_at).unwrap(), pair.pair.last_update_at);
            assert!(book.state.eq(&state(&fixture, pair)), "{:?}", book.state);
            assert_eq!(book.state.fee(), 0.0);
        }
    }

    /// The fee is read for each pair every poll, so a change the venue makes reaches the books.
    #[tokio::test]
    async fn the_books_carry_the_fee_the_entrypoint_reports() {
        let mut node = FakeNode::new();
        node.fixture.pairs[0].fee_pips = 50;

        let books = books(node, book_config()).await;

        let weth_book = pair_component_id(PROTOCOL_SYSTEM, &bytes(weth()), &quote_token().address);
        assert_eq!(
            books[&weth_book.to_string()]
                .state
                .fee(),
            0.00005
        );
    }

    /// The books are priced with the fee the entrypoint charges the configured taker: a router
    /// other than Tycho's pays the venue default.
    #[tokio::test]
    async fn the_books_carry_the_fee_of_the_configured_taker() {
        let taker = Address::repeat_byte(0x11);
        let mut node = FakeNode::new();
        node.taker = taker;
        let now = node.timestamp;
        let server = node.spawn().await;
        let mut source = BaibaiFeedBuilder::new(book_config(), server.url())
            .taker(bytes(taker))
            .build()
            .unwrap()
            .source;
        source.clock = Arc::new(move || now);

        let books = source.fetch().await.unwrap().books;

        assert!(!books.is_empty());
        for book in books.values() {
            assert_eq!(book.state.fee(), 0.00005);
        }
    }

    /// After an upgrade that removes a getter the feed reads, no later poll can succeed.
    #[rstest]
    #[case::a_settings_getter(ttlCall::SELECTOR)]
    #[case::the_pair_getter(pairCall::SELECTOR)]
    #[case::the_valid_until_getter(validUntilCall::SELECTOR)]
    #[case::the_knot_getter(knotCall::SELECTOR)]
    #[case::the_taker_fee_getter(TAKER_FEE_SELECTOR)]
    #[tokio::test]
    async fn a_reverting_getter_ends_the_feed(#[case] selector: [u8; 4]) {
        let mut node = FakeNode::new();
        node.reverting.push(selector);

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
    }

    #[tokio::test]
    async fn a_node_on_another_chain_ends_the_feed() {
        let mut node = FakeNode::new();
        node.chain_id = 1;

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        assert!(error.to_string().contains("chain 1"), "{error}");
    }

    /// Settings, pairs and knots come from one `eth_call`, so they are all from one block.
    #[tokio::test]
    async fn a_poll_reads_the_chain_in_one_request() {
        let node = FakeNode::new();
        let now = node.timestamp;
        let server = node.spawn().await;
        let mut source = feed(&server.url(), book_config()).source;
        source.clock = Arc::new(move || now);

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.len(), 2);
        assert_eq!(server.request_count(), 1);
    }

    /// A read asks for the first [`MAX_KNOTS`] knots of each side; a side holding more is a
    /// deployment this integration was not built for, which reading part of its curve would
    /// misprice.
    #[tokio::test]
    async fn a_side_with_more_knots_than_read_ends_the_feed() {
        let mut node = FakeNode::new();
        let knots = &mut node.fixture.pairs[0].pair.ask.knots;
        let last = *knots.last().unwrap();
        knots.resize(MAX_KNOTS + 1, last);

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        assert!(error.to_string().contains("37 knots"), "{error}");
    }

    /// A load-balanced provider can answer from a block older than one already read; publishing
    /// it would roll the books back.
    #[tokio::test]
    async fn a_read_older_than_the_last_one_fails_the_poll() {
        let node = FakeNode::new();
        let now = node.timestamp;
        let mut older = node.clone();
        older.block -= 1;
        let (newer, older) = (node.spawn().await, older.spawn().await);
        let mut source = feed(&newer.url(), book_config()).source;
        source.clock = Arc::new(move || now);
        source.fetch().await.unwrap();
        let mut replica = feed(&older.url(), book_config()).source;
        replica.clock = Arc::new(move || now);
        replica.last_block = Arc::clone(&source.last_block);

        let error = replica.fetch().await.unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        source.fetch().await.unwrap();
    }

    /// The books of one poll of `node` with the wall clock `lag` seconds past its block.
    async fn fetch_behind(node: FakeNode, lag: u64) -> Result<usize, FeedError> {
        let now = node.timestamp + lag;
        let server = node.spawn().await;
        let mut source = feed(&server.url(), book_config()).source;
        source.clock = Arc::new(move || now);
        Ok(source.fetch().await?.books.len())
    }

    /// A node whose latest block trails the wall clock by more than two blocks is lagging: its
    /// curves may have expired on chain.
    #[tokio::test]
    async fn a_lagging_node_fails_the_poll() {
        assert_eq!(
            fetch_behind(FakeNode::new(), 4)
                .await
                .unwrap(),
            2
        );

        let error = fetch_behind(FakeNode::new(), 5)
            .await
            .unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
    }

    /// Within the lag a node is allowed, a curve is judged at the wall clock: cbBTC's, cut to
    /// five seconds, is still live in the block after the node's but not in the one after now.
    #[tokio::test]
    async fn curves_are_judged_at_the_wall_clock_when_the_node_trails_it() {
        let mut node = FakeNode::new();
        node.fixture.pairs[1].valid_until = node.timestamp + 5;

        assert_eq!(fetch_behind(node, 4).await.unwrap(), 1);
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

    /// The custodian reverts only for an address that is no token, which the book cannot have
    /// listed: for a listed base it means the deployment changed.
    #[tokio::test]
    async fn a_listed_base_whose_custody_reverts_ends_the_feed() {
        let mut node = FakeNode::new();
        node.not_tokens.push(weth());

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        assert!(
            error
                .to_string()
                .contains("availableLiquidity"),
            "{error}"
        );
    }

    /// A proxy upgraded to an implementation this integration was not verified against may price
    /// differently with every getter still answering, so it ends the feed.
    #[rstest]
    #[case::the_entrypoint("Entrypoint", BASE_DEPLOYMENT.entrypoint)]
    #[case::the_curve_book("CurveBook", BASE_DEPLOYMENT.curve_book)]
    #[case::the_custodian("Custodian", BASE_DEPLOYMENT.custodian)]
    #[tokio::test]
    async fn an_upgraded_proxy_ends_the_feed(#[case] name: &str, #[case] proxy: Address) {
        let upgraded = Address::repeat_byte(0x99);
        let mut node = FakeNode::new();
        node.implementations
            .insert(proxy, upgraded);

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        let message = error.to_string();
        assert!(message.contains(&format!("{name} proxy {proxy}")), "{message}");
        assert!(message.contains(&upgraded.to_string()), "{message}");
    }

    /// An upgrade often breaks a getter too; the error names the upgrade, not the getter.
    #[tokio::test]
    async fn an_upgrade_is_reported_before_the_getters_it_broke() {
        let mut node = FakeNode::new();
        node.implementations
            .insert(BASE_DEPLOYMENT.entrypoint, Address::repeat_byte(0x99));
        node.reverting.push(ttlCall::SELECTOR);

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Entrypoint proxy"),
            "{error}"
        );
    }

    /// A node that cannot read a proxy's storage this poll may next poll: the feed retries.
    #[tokio::test]
    async fn an_unreadable_implementation_slot_fails_only_the_poll() {
        let mut node = FakeNode::new();
        node.storage_fails = true;

        let error = fetch(node, book_config(), None)
            .await
            .unwrap_err();

        assert!(!error.is_fatal(), "{error}");
        assert!(
            error
                .to_string()
                .contains("implementation slot"),
            "{error}"
        );
    }

    #[test]
    fn the_implementation_slot_is_eip_1967s() {
        use alloy::primitives::{keccak256, B256, U256};

        let hash = keccak256("eip1967.proxy.implementation");
        let slot = U256::from_be_bytes(hash.0) - U256::from(1);

        assert_eq!(IMPLEMENTATION_SLOT, B256::from(slot));
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
    /// recording's curves run until `validUntil`, six (cbBTC) and seven (WETH) seconds after
    /// their update.
    #[rstest]
    #[case::fresh(0, 2)]
    #[case::one_expiring_before_the_next_block(5, 1)]
    #[case::both_expiring_before_the_next_block(6, 0)]
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
    fn the_builder_refuses_a_taker_that_is_not_an_address() {
        let error = BaibaiFeedBuilder::new(book_config(), "http://node".to_string())
            .taker(Bytes::from(vec![1u8; 4]))
            .build()
            .unwrap_err();

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
