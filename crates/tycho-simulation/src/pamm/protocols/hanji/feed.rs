//! Hanji's book feed: polls every listed market over JSON-RPC and publishes every poll as a
//! complete [`BookSnapshot`].

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Address;
use chrono::DateTime;
use futures::future::join_all;
use reqwest::Url;
use tracing::{debug, error, info, warn};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::{
    book::{
        component::{maker_component_id, pair_book},
        levels::{Levels, PriceLevel},
        quote_tokens::usd_stablecoins_for_chain,
        tvl::tvl_in_usd,
        BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    evm::protocol::u256_num::u256_to_f64,
    pamm::protocols::hanji::{
        market::{fetch_markets, Market},
        reader::{BookRead, HanjiReader, MarketRead, ProbeOutcome},
        state::{HanjiState, Level, Probe},
        BASE_DEPLOYMENT, BASE_ROUTER, ORDERBOOK_ATTRIBUTE, PROTOCOL_SYSTEM, PROTOCOL_TYPE,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig, HttpSource},
        Publisher, SnapshotFeed,
    },
};

/// How many milliseconds a read's block timestamp may trail the wall clock read just before the
/// read is sent. Base block timestamps are whole seconds, shared by about five 200 ms blocks, so
/// a synced node's latest block already trails the clock by up to a second; past that, the node
/// is lagging. The quoters' books empty about 3 s after their keepers last pushed a price, and
/// they push several times a second. With [`MAX_SNAPSHOT_AGE`], a book is withdrawn at most
/// 3.5 s, plus the read's round trip, after its block's timestamp: up to about half a second past
/// the point where a book whose keepers stopped at that block would have emptied.
const MAX_READ_LAG_MS: u64 = 1_500;

/// How long a book is served after the poll that read it, unless a later poll replaces it.
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(2);

/// How often the market list is fetched again by default.
const DEFAULT_MARKET_REFRESH: Duration = Duration::from_secs(300);

/// How long a failed market-list refresh waits before the API is asked again. The list fetched
/// before keeps being read meanwhile.
const MARKET_RETRY: Duration = Duration::from_secs(30);

/// How much of a poll's request timeout the market API gets. A fetch runs inside the poll, so it
/// must give up early enough to leave the poll time for its chain read, and for the fallback to the
/// last list to run at all.
const MARKET_API_SHARE_OF_POLL: u32 = 2;

/// How often each served market is sent a fill check (see [`FillChecks`]).
const FILL_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// How long a market waits for its next fill check after one that could not reach the node.
const FILL_CHECK_RETRY: Duration = Duration::from_secs(10);
/// How long one poll's fill checks may take; the poll's request timeout also covers its read.
const FILL_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

/// Hanji's book feed. Built through [`HanjiFeedBuilder`]. Neither its `Debug` output nor its
/// errors carry the RPC URL, which commonly carries a key.
#[derive(Debug, Clone)]
pub struct HanjiFeed {
    feed_config: HttpFeedConfig,
    source: HanjiBookSource,
}

/// Builds a [`HanjiFeed`] from the shared [`BookFeedConfig`] and a Base JSON-RPC URL. Every poll
/// sends one `eth_call` at the latest block, so a full node is enough; the market list comes from
/// Hanji's aggregator API.
pub struct HanjiFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    taker: Bytes,
    market_refresh: Duration,
    feed_config: HttpFeedConfig,
}

impl HanjiFeedBuilder {
    pub fn new(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self {
            book_config,
            rpc_url,
            taker: Bytes::from(BASE_ROUTER.to_vec()),
            market_refresh: DEFAULT_MARKET_REFRESH,
            feed_config: Self::default_feed_config(),
        }
    }

    /// The address that calls the proxy orderbooks, replacing Tycho's router on Base. A proxy
    /// fills a contract against a wider book than the one it fills a whitelisted caller against,
    /// and the books are read for this one, so set it when swaps go through another router.
    pub fn taker(mut self, taker: Bytes) -> Self {
        self.taker = taker;
        self
    }

    /// How long a fetched market list is used before the API is asked again. A refresh that
    /// fails, or that the API does not answer within half the poll's request timeout, keeps the
    /// list fetched before it and is retried 30 s later. The first fetch, with no list to fall
    /// back on, gets the poll's whole request timeout.
    pub fn market_refresh(mut self, market_refresh: Duration) -> Self {
        self.market_refresh = market_refresh;
        self
    }

    /// Tune the poll loop (poll cadence, request timeout, failure limit).
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// The feed tuning a new builder starts from, set by the quoters' freshness: a poll every
    /// second, given 2 s, and a book withdrawn [`MAX_SNAPSHOT_AGE`] after the poll that read it,
    /// so that with the read lag allowed it is gone about when the quoters stop quoting without a
    /// newer price (see [`MAX_READ_LAG_MS`]).
    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            max_snapshot_age: Some(MAX_SNAPSHOT_AGE),
            ..default_http_feed_config()
        }
    }

    /// The feed. Fails for a chain other than Base, the only Tycho chain Hanji is deployed on, for
    /// an RPC URL that is not one, for a zero `market_refresh`, and for a taker that is not an
    /// address. The node's chain is checked on every poll, which fails fatally when the node
    /// serves another one.
    pub fn build(self) -> Result<HanjiFeed, FeedError> {
        let deployment = BASE_DEPLOYMENT;
        if self.book_config.chain != deployment.chain {
            return Err(FeedError::InvalidInput(format!(
                "Hanji is integrated on Base only, not {}",
                self.book_config.chain
            )));
        }
        let rpc_url = Url::parse(&self.rpc_url)
            .map_err(|e| FeedError::InvalidInput(format!("Hanji RPC URL: {e}")))?;
        if self.market_refresh.is_zero() {
            return Err(FeedError::InvalidInput("Hanji market_refresh must not be zero".into()));
        }
        // The slice error says only that the length is not 20 bytes, which the message states.
        let taker = Address::try_from(self.taker.as_ref()).map_err(|_wrong_length| {
            FeedError::InvalidInput(format!("Hanji taker {} is not a 20-byte address", self.taker))
        })?;
        let api_timeout = self.feed_config.request_timeout / MARKET_API_SHARE_OF_POLL;
        Ok(HanjiFeed {
            source: HanjiBookSource {
                markets: MarketList::new(
                    deployment.markets_url.to_string(),
                    deployment.wrapped_native,
                    self.market_refresh,
                    api_timeout,
                    self.feed_config.request_timeout,
                ),
                reader: HanjiReader::new(rpc_url, deployment, taker),
                assembler: HanjiBookAssembler { book_config: self.book_config },
                fill_checks: FillChecks::new(FILL_CHECK_INTERVAL),
                clock: Arc::new(unix_now_ms),
                last_block: Arc::new(AtomicU64::new(0)),
            },
            feed_config: self.feed_config,
        })
    }
}

impl SnapshotFeed for HanjiFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}

/// The market list, fetched from the API and kept for `refresh`.
#[derive(Clone, derive_more::Debug)]
pub(super) struct MarketList {
    url: String,
    wrapped_native: Address,
    refresh: Duration,
    /// How long a refresh may take: shorter than the poll's timeout, so that a refresh that does
    /// not answer fails inside the poll and the last list is read instead.
    api_timeout: Duration,
    /// How long the first fetch may take: there is no list to fall back on yet.
    first_fetch_timeout: Duration,
    #[debug(skip)]
    client: reqwest::Client,
    #[debug(skip)]
    state: Arc<Mutex<ListState>>,
}

/// The last list fetched, if any, and when the API is next asked.
#[derive(Default)]
struct ListState {
    markets: Option<Vec<Market>>,
    next_fetch: Option<Instant>,
}

impl MarketList {
    pub(super) fn new(
        url: String,
        wrapped_native: Address,
        refresh: Duration,
        api_timeout: Duration,
        first_fetch_timeout: Duration,
    ) -> Self {
        Self {
            url,
            wrapped_native,
            refresh,
            api_timeout,
            first_fetch_timeout,
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(ListState::default())),
        }
    }

    /// The markets to read. The API is asked when there is no list yet, `refresh` after the last
    /// fetch, and [`MARKET_RETRY`] after a refresh that failed; otherwise the last list is read. A
    /// refresh that fails or does not answer within `api_timeout` falls back to the last list,
    /// logged; the first fetch gets `first_fetch_timeout`, and its failure fails the poll.
    pub(super) async fn current(&self) -> Result<Vec<Market>, FeedError> {
        let (cached, due) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let due = state
                .next_fetch
                .is_none_or(|next| Instant::now() >= next);
            (state.markets.clone(), due)
        };
        if let (Some(markets), false) = (&cached, due) {
            return Ok(markets.clone());
        }
        let timeout = if cached.is_some() { self.api_timeout } else { self.first_fetch_timeout };
        let fetched = fetch_markets(&self.client, &self.url, timeout, self.wrapped_native).await;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match (fetched, cached) {
            (Ok(markets), _) => {
                state.markets = Some(markets.clone());
                state.next_fetch = Some(Instant::now() + self.refresh);
                Ok(markets)
            }
            (Err(error), Some(markets)) => {
                warn!(%error, retry_in = ?MARKET_RETRY, "keeping the last Hanji market list");
                state.next_fetch = Some(Instant::now() + MARKET_RETRY);
                Ok(markets)
            }
            (Err(error), None) => Err(error),
        }
    }
}

/// One poll: the market list, every listed market read from the chain and turned into books.
#[derive(Clone, derive_more::Debug)]
pub(super) struct HanjiBookSource {
    pub(super) markets: MarketList,
    pub(super) reader: HanjiReader,
    pub(super) assembler: HanjiBookAssembler,
    pub(super) fill_checks: FillChecks,
    /// The current Unix time in milliseconds.
    #[debug(skip)]
    pub(super) clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The newest block a poll has read, so that a node answering from an older one is caught.
    pub(super) last_block: Arc<AtomicU64>,
}

impl HttpSource for HanjiBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A read that fails fails the poll, and so does one from a block older than a previous
    /// poll's or more than [`MAX_READ_LAG_MS`] behind the wall clock: a load-balanced or
    /// lagging node would otherwise roll the books back or publish books the quoters no longer
    /// quote.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let markets = self.markets.current().await?;
        // Read before the call, so the lag measures the node and not the round trip.
        let now_ms = (self.clock)();
        let read = self.reader.read(&markets).await?;
        let block_ms = read.timestamp.saturating_mul(1_000);
        if now_ms > block_ms.saturating_add(MAX_READ_LAG_MS) {
            return Err(FeedError::Connection(format!(
                "Hanji node is {} ms behind: its latest block {} is from {}",
                now_ms.saturating_sub(block_ms),
                read.block_number,
                read.timestamp
            )));
        }
        let newest = self
            .last_block
            .fetch_max(read.block_number, Ordering::Relaxed);
        if read.block_number < newest {
            return Err(FeedError::Connection(format!(
                "Hanji node answered from block {}, older than block {newest} already read",
                read.block_number
            )));
        }
        let states = self.assembler.states(&read)?;
        self.fill_checks
            .run(&self.reader, &read, &states)
            .await;
        let books = self
            .assembler
            .books_of(&read, states, |orderbook| !self.fill_checks.withheld(orderbook));
        Ok(BookSnapshot::received_now(books))
    }
}

/// Checks each served market's state against its proxy, as a guard against an upgrade that
/// changes how the proxy fills. Every [`FILL_CHECK_INTERVAL`], and at once for a market not yet
/// checked, each side of the book is sent a swap past its best level ([`HanjiState::probe`]) as
/// an `eth_call` at the read's block, from the taker (see [`HanjiReader::probe`]). A market whose
/// proxy fills other shares, value or fee than its state predicts, or reverts, is withheld until
/// a later check agrees. A check that cannot reach the node changes nothing and is retried after
/// [`FILL_CHECK_RETRY`]; a market no check has decided is published.
#[derive(Clone, Debug)]
pub(super) struct FillChecks {
    interval: Duration,
    verdicts: Arc<Mutex<HashMap<Address, Verdict>>>,
}

/// A market's last fill check: whether its fills agreed with its state (`None` until a check
/// decides), and when it is checked next.
#[derive(Clone, Copy, Debug)]
struct Verdict {
    agrees: Option<bool>,
    next_check: Instant,
}

/// What a market's fill checks found in one poll.
enum Finding {
    Agrees,
    Disagrees(String),
    /// The node could not be asked, or no side could be funded.
    Undecided {
        retry: bool,
        reason: String,
    },
}

impl FillChecks {
    pub(super) fn new(interval: Duration) -> Self {
        Self { interval, verdicts: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Whether the last check of `orderbook` found its fills disagreeing with its state.
    pub(super) fn withheld(&self, orderbook: Address) -> bool {
        self.verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&orderbook)
            .is_some_and(|verdict| verdict.agrees == Some(false))
    }

    /// Checks the markets of `states` that are due, within [`FILL_CHECK_TIMEOUT`].
    async fn run(
        &self,
        reader: &HanjiReader,
        read: &BookRead,
        states: &[(&MarketRead, HanjiState)],
    ) {
        let now = Instant::now();
        let due: Vec<_> = {
            let verdicts = self
                .verdicts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            states
                .iter()
                .filter(|(market_read, _)| {
                    verdicts
                        .get(&market_read.market.orderbook)
                        .is_none_or(|verdict| now >= verdict.next_check)
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        let checks = join_all(
            due.iter()
                .map(|(market_read, state)| check_market(reader, read, market_read, state)),
        );
        let Ok(findings) = tokio::time::timeout(FILL_CHECK_TIMEOUT, checks).await else {
            warn!(markets = due.len(), timeout = ?FILL_CHECK_TIMEOUT, "Hanji fill checks timed out");
            self.postpone(
                due.iter()
                    .map(|(market_read, _)| market_read.market.orderbook),
                now,
            );
            return;
        };
        let mut verdicts = self
            .verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for ((market_read, _), finding) in due.iter().zip(findings) {
            let orderbook = market_read.market.orderbook;
            let previous = verdicts
                .get(&orderbook)
                .and_then(|verdict| verdict.agrees);
            let verdict = match finding {
                Finding::Agrees => {
                    if previous == Some(false) {
                        info!(%orderbook, "Hanji market's fills agree with its book again");
                    }
                    Verdict { agrees: Some(true), next_check: now + self.interval }
                }
                Finding::Disagrees(reason) => {
                    error!(%orderbook, block = read.block_number, %reason, "withholding Hanji market whose fills disagree with its book");
                    Verdict { agrees: Some(false), next_check: now + self.interval }
                }
                Finding::Undecided { retry, reason } => {
                    warn!(%orderbook, %reason, "Hanji fill check undecided");
                    let wait = if retry { FILL_CHECK_RETRY } else { self.interval };
                    Verdict { agrees: previous, next_check: now + wait }
                }
            };
            verdicts.insert(orderbook, verdict);
        }
    }

    /// Whether the last check that decided found `orderbook`'s fills agreeing with its state.
    #[cfg(test)]
    pub(super) fn agrees(&self, orderbook: Address) -> Option<bool> {
        self.verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&orderbook)
            .and_then(|verdict| verdict.agrees)
    }

    /// Leaves the verdicts of `orderbooks` as they are and checks them again after
    /// [`FILL_CHECK_RETRY`].
    fn postpone(&self, orderbooks: impl Iterator<Item = Address>, now: Instant) {
        let mut verdicts = self
            .verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for orderbook in orderbooks {
            let agrees = verdicts
                .get(&orderbook)
                .and_then(|verdict| verdict.agrees);
            verdicts.insert(orderbook, Verdict { agrees, next_check: now + FILL_CHECK_RETRY });
        }
    }
}

/// Sends each side of `state` its probe and compares the fills with the state's.
async fn check_market(
    reader: &HanjiReader,
    read: &BookRead,
    market_read: &MarketRead,
    state: &HanjiState,
) -> Finding {
    let mut probes: Vec<Probe> = Vec::new();
    for sell_base in [true, false] {
        match state.probe(sell_base) {
            Ok(Some(probe)) => probes.push(probe),
            Ok(None) => {}
            Err(error) => {
                return Finding::Disagrees(format!("the state cannot price a probe: {error}"))
            }
        }
    }
    let outcomes = join_all(
        probes
            .iter()
            .map(|probe| reader.probe(read.block_number, read.timestamp, market_read, probe)),
    )
    .await;
    let mut checked = false;
    for (probe, outcome) in probes.iter().zip(outcomes) {
        let side = if probe.sell_base { "sell" } else { "buy" };
        match outcome {
            Ok(ProbeOutcome::Filled(report)) if report == probe.expected => checked = true,
            Ok(ProbeOutcome::Filled(report)) => {
                return Finding::Disagrees(format!(
                    "a {side} of {} filled {report:?}, the book predicts {:?}",
                    probe.units, probe.expected
                ))
            }
            Ok(ProbeOutcome::Reverted(reason)) => {
                return Finding::Disagrees(format!("a {side} of {} reverted: {reason}", probe.units))
            }
            Ok(ProbeOutcome::Unfunded) => {}
            Err(error) => {
                return Finding::Undecided { retry: true, reason: format!("a {side}: {error}") }
            }
        }
    }
    if checked {
        Finding::Agrees
    } else {
        Finding::Undecided { retry: false, reason: "no side of the book can be funded".into() }
    }
}

/// The wall clock in Unix milliseconds. A clock set before 1970 reads as `u64::MAX`, which fails
/// every read's lag check rather than passing it.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// Turns one [`BookRead`] into books: one per market between served tokens with a quotable side
/// that clears the TVL floor.
#[derive(Debug, Clone)]
pub(super) struct HanjiBookAssembler {
    pub(super) book_config: BookFeedConfig,
}

impl HanjiBookAssembler {
    /// Every market of `read` that is served, with its state: those between served tokens whose
    /// book has a level and reads as one the contracts could report. Fails when every market
    /// read is malformed, which is a change in what the chain reports rather than in its books.
    pub(super) fn states<'r>(
        &self,
        read: &'r BookRead,
    ) -> Result<Vec<(&'r MarketRead, HanjiState)>, FeedError> {
        let native = self
            .book_config
            .chain
            .native_token()
            .address;
        let mut states = Vec::new();
        let mut malformed = Vec::new();
        for market_read in &read.markets {
            match self.state(market_read, &native) {
                Ok(Some(state)) => states.push((market_read, state)),
                Ok(None) => {}
                Err(error) => malformed.push((market_read.market.orderbook, error)),
            }
        }
        if let Some((orderbook, error)) = malformed.first() {
            if malformed.len() == read.markets.len() {
                return Err(FeedError::Parsing(format!(
                    "Hanji: all {} markets read are malformed; {orderbook}: {error}",
                    malformed.len()
                )));
            }
            warn!(skipped = malformed.len(), %orderbook, %error, "skipping Hanji markets the chain reports malformed");
        }
        Ok(states)
    }

    /// The books of `read`'s served markets that clear the TVL floor.
    #[cfg(test)]
    pub(super) fn books(&self, read: &BookRead) -> Books {
        let states = self
            .states(read)
            .expect("a read with a well-formed market");
        self.books_of(read, states, |_| true)
    }

    /// The books of `states` whose proxy `publish` accepts and that clear the TVL floor. Every
    /// state, published or not, can value another's quote token in USD.
    pub(super) fn books_of(
        &self,
        read: &BookRead,
        states: Vec<(&MarketRead, HanjiState)>,
        publish: impl Fn(Address) -> bool,
    ) -> Books {
        let usd_tokens = usd_stablecoins_for_chain(self.book_config.chain).unwrap_or_default();
        let bids: Vec<(Bytes, Bytes, Levels)> = states
            .iter()
            .filter_map(|(_, state)| {
                let levels = human_levels(state, &state.bids)?;
                Some((state.base_token.address.clone(), state.quote_token.address.clone(), levels))
            })
            .collect();
        let updated_at = i64::try_from(read.timestamp)
            .ok()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
        let mut books = Books::new();
        for (market_read, state) in states {
            let orderbook = market_read.market.orderbook;
            if !publish(orderbook) {
                continue;
            }
            // A pair could be listed on two proxies, each its own market, so the proxy is part of
            // the id.
            let component_id = maker_component_id(
                PROTOCOL_SYSTEM,
                &state.base_token.address,
                &state.quote_token.address,
                &orderbook.to_string(),
            );
            let Some(tvl) = tvl_usd(&state, &usd_tokens, &bids) else {
                debug!(%component_id, "skipping Hanji market whose quote token has no USD price");
                continue;
            };
            if !self
                .book_config
                .clears_min_tvl(tvl, &component_id)
            {
                continue;
            }
            let mut book = pair_book(
                PROTOCOL_SYSTEM,
                PROTOCOL_TYPE,
                self.book_config.chain,
                component_id,
                state,
                updated_at,
            );
            book.component
                .static_attributes
                .insert(ORDERBOOK_ATTRIBUTE.to_string(), Bytes::from(orderbook.to_vec()));
            books.insert(book);
        }
        books
    }

    /// The market's state; `None`, logged, when its tokens are not served or neither side of its
    /// book holds a level; an error when the chain reports it malformed.
    fn state(
        &self,
        market_read: &MarketRead,
        native: &Bytes,
    ) -> Result<Option<HanjiState>, SimulationError> {
        let market = &market_read.market;
        let (base, quote) = market.published_tokens(native);
        let Some((base_token, quote_token)) = self
            .book_config
            .pair_tokens(&base, &quote)
        else {
            debug!(orderbook = %market.orderbook, %base, %quote, "skipping Hanji market of unserved tokens");
            return Ok(None);
        };
        if market_read.asks.is_empty() && market_read.bids.is_empty() {
            debug!(orderbook = %market.orderbook, "skipping Hanji market with an empty book");
            return Ok(None);
        }
        let state = HanjiState {
            orderbook: market.orderbook,
            base_token: base_token.clone(),
            quote_token: quote_token.clone(),
            scaling_x: market_read.config.scaling_x,
            scaling_y: market_read.config.scaling_y,
            fee_rate: market_read.config.fee_rate,
            asks: market_read.asks.clone(),
            bids: market_read.bids.clone(),
        };
        state.validated().map(Some)
    }
}

/// One side of `state`'s book in whole tokens: base quantities, priced in quote per base. `None`
/// for an empty side or one that does not convert.
fn human_levels(state: &HanjiState, side: &[Level]) -> Option<Levels> {
    let base_unit = 10f64.powf(f64::from(state.base_token.decimals));
    let quote_unit = 10f64.powf(f64::from(state.quote_token.decimals));
    let scaling_x = u256_to_f64(state.scaling_x).ok()?;
    let scaling_y = u256_to_f64(state.scaling_y).ok()?;
    let levels = side
        .iter()
        .map(|level| {
            Some(PriceLevel {
                quantity: u256_to_f64(level.shares).ok()? * scaling_x / base_unit,
                price: u256_to_f64(level.price).ok()? * scaling_y / quote_unit * base_unit /
                    scaling_x,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Levels::new(levels)
        .ok()
        .filter(|levels| !levels.is_empty())
}

/// The book's depth on both sides, in USD: valued in its quote token, then converted through the
/// bids of a market that sells the quote token for a USD stablecoin (see [`tvl_in_usd`]). `None`
/// when the quote token is neither a stablecoin nor sold for one in `bids`.
fn tvl_usd(
    state: &HanjiState,
    usd_tokens: &HashSet<Bytes>,
    bids: &[(Bytes, Bytes, Levels)],
) -> Option<f64> {
    let notional =
        |side: &[Level]| human_levels(state, side).map_or(0.0, |levels| levels.notional());
    let raw_tvl = notional(&state.asks) + notional(&state.bids);
    tvl_in_usd(
        raw_tvl,
        &state.quote_token.address,
        usd_tokens,
        bids.iter()
            .map(|(base, quote, levels)| (base, quote, levels)),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use rstest::rstest;
    use tycho_common::models::{token::Token, Chain};

    use super::*;
    use crate::pamm::protocols::hanji::{
        state::Level,
        test_support::{
            book_config, cbbtc, eth, fixture, levels, source, state, usdc, Caller, FakeNode, Rates,
            CBBTC_ETH, CBBTC_USDC, ETH_USDC, NVDA_USDC, TEST_API_TIMEOUT,
        },
    };

    const REFRESH: Duration = Duration::from_secs(300);

    fn component_id(base: &Token, quote: &Token, orderbook: Address) -> String {
        maker_component_id(PROTOCOL_SYSTEM, &base.address, &quote.address, &orderbook.to_string())
            .to_string()
    }

    fn recorded_bids(orderbook: Address, caller: Caller) -> Vec<Level> {
        let fixture = fixture();
        levels(
            &fixture
                .market(orderbook)
                .books
                .of(caller)
                .unwrap()
                .bids,
        )
    }

    fn published_bids(snapshot: &BookSnapshot<ReceivedAt>, id: &str) -> Vec<Level> {
        snapshot.books[id]
            .state
            .as_any()
            .downcast_ref::<HanjiState>()
            .unwrap()
            .bids
            .clone()
    }

    /// A node, a source reading it for Tycho's router at the recorded block, and the server they
    /// talk through.
    async fn served(
        node: FakeNode,
        market_refresh: Duration,
    ) -> (Arc<FakeNode>, HanjiBookSource, crate::snapshot_feed::http::test_support::MockHttpServer)
    {
        let node = Arc::new(node);
        let server = Arc::clone(&node).serve().await;
        let source = source(&server, node.fixture.timestamp * 1_000, market_refresh);
        (node, source, server)
    }

    /// A poll at the recorded block publishes the three markets whose tokens the universe holds,
    /// with ETH as the native token on whichever side it is, the proxy each trades through, and
    /// the book of the contract quoter, which Tycho's router gets while it is not whitelisted.
    #[tokio::test]
    async fn a_poll_publishes_the_markets_of_served_tokens() {
        let (_node, source, _server) = served(FakeNode::new(), REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        let expected = [
            (component_id(&eth(), &usdc(), ETH_USDC), ETH_USDC),
            (component_id(&cbbtc(), &usdc(), CBBTC_USDC), CBBTC_USDC),
            (component_id(&cbbtc(), &eth(), CBBTC_ETH), CBBTC_ETH),
        ];
        assert_eq!(snapshot.books.len(), expected.len());
        for (id, orderbook) in &expected {
            let book = &snapshot.books[id];
            assert_eq!(book.component.protocol_system, PROTOCOL_SYSTEM);
            assert_eq!(
                book.component.static_attributes[ORDERBOOK_ATTRIBUTE],
                Bytes::from(orderbook.to_vec())
            );
            assert_eq!(published_bids(&snapshot, id), recorded_bids(*orderbook, Caller::Contract));
        }
        let eth_usdc = &snapshot.books[&expected[0].0].component;
        assert_eq!(eth_usdc.tokens, vec![eth(), usdc()]);
    }

    /// Each published state is the market's config and the book its proxy fills the taker
    /// against, field for field: both scalings, the taker's rate (the aggressive commission plus
    /// the passive payout, not the passive commission), and both sides from the same quoter.
    #[rstest]
    #[case::contract_taker(false, Caller::Contract)]
    #[case::whitelisted_taker(true, Caller::Direct)]
    #[tokio::test]
    async fn a_published_state_is_the_config_and_the_takers_book(
        #[case] whitelisted: bool,
        #[case] book: Caller,
    ) {
        let node = FakeNode { rates: Some(Rates::DISTINCT), ..FakeNode::new() };
        if whitelisted {
            node.whitelist_the_router_everywhere();
        }
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        for (base, quote, orderbook) in
            [(eth(), usdc(), ETH_USDC), (cbbtc(), usdc(), CBBTC_USDC), (cbbtc(), eth(), CBBTC_ETH)]
        {
            let published = snapshot.books[&component_id(&base, &quote, orderbook)]
                .state
                .as_any()
                .downcast_ref::<HanjiState>()
                .unwrap()
                .clone();
            let mut expected = state(orderbook, book);
            expected.fee_rate = Rates::DISTINCT.taker();
            assert_eq!(published, expected, "{orderbook}");
        }
    }

    /// The TVL floor applies in USD: cbBTC/ETH's depth, valued in ETH, is converted through
    /// ETH/USDC's bids. At the recorded block every book is worth more than 100k USD and less
    /// than 100M.
    #[test]
    fn the_tvl_floor_is_applied_in_usd() {
        let fixture = fixture();
        let read = fixture.read(Caller::Contract);
        let published = |min_tvl_usd| {
            HanjiBookAssembler { book_config: BookFeedConfig { min_tvl_usd, ..book_config() } }
                .books(&read)
                .into_map()
                .len()
        };

        assert_eq!(published(100_000.0), 3);
        assert_eq!(published(100_000_000.0), 0);
    }

    /// A read whose every market is malformed fails rather than publishing nothing; one malformed
    /// market among others is left out.
    #[test]
    fn a_read_of_malformed_markets_fails() {
        let fixture = fixture();
        let assembler = HanjiBookAssembler { book_config: book_config() };
        let mut read = fixture.read(Caller::Contract);
        read.markets[0].config.scaling_x = alloy::primitives::U256::ZERO;
        let served = assembler.states(&read).unwrap().len();
        for market in &mut read.markets {
            market.config.scaling_x = alloy::primitives::U256::ZERO;
        }

        let all_malformed = assembler.states(&read);

        assert_eq!(served, 2);
        assert!(matches!(all_malformed, Err(FeedError::Parsing(_))));
    }

    #[test]
    fn a_market_with_an_empty_book_is_not_published() {
        let fixture = fixture();
        let mut read = fixture.read(Caller::Contract);
        for market in &mut read.markets {
            market.asks.clear();
            market.bids.clear();
        }

        let books = HanjiBookAssembler { book_config: book_config() }.books(&read);

        assert!(books.into_map().is_empty());
    }

    /// A read whose block trails the clock by the allowed lag is published; one more millisecond
    /// fails the poll.
    #[rstest]
    #[case::at_the_allowed_lag(MAX_READ_LAG_MS, true)]
    #[case::past_the_allowed_lag(MAX_READ_LAG_MS + 1, false)]
    #[tokio::test]
    async fn a_lagging_node_fails_the_poll(#[case] lag_ms: u64, #[case] published: bool) {
        let node = Arc::new(FakeNode::new());
        let server = Arc::clone(&node).serve().await;
        let source = source(&server, node.fixture.timestamp * 1_000 + lag_ms, REFRESH);

        let result = source.fetch().await;

        assert_eq!(result.is_ok(), published);
        assert!(result.is_ok() || matches!(result, Err(FeedError::Connection(_))));
    }

    /// The lag is measured from before the read is sent, so a slow answer from a node that was
    /// in time is not taken for a lagging node.
    #[tokio::test]
    async fn a_slow_answer_does_not_count_as_lag() {
        let node = Arc::new(FakeNode::new());
        *node.node_delay.lock().unwrap() = Duration::from_millis(300);
        let server = Arc::clone(&node).serve().await;
        let started = Instant::now();
        let start_ms = node.fixture.timestamp * 1_000 + MAX_READ_LAG_MS - 100;
        let mut source = source(&server, start_ms, REFRESH);
        source.clock =
            Arc::new(move || start_ms + u64::try_from(started.elapsed().as_millis()).unwrap());

        let result = source.fetch().await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn a_node_answering_from_an_older_block_fails_the_poll() {
        let (node, source, _server) = served(FakeNode::new(), REFRESH).await;
        source.fetch().await.unwrap();

        node.block
            .fetch_sub(1, Ordering::SeqCst);
        let result = source.fetch().await;

        assert!(matches!(result, Err(FeedError::Connection(_))));
    }

    #[tokio::test]
    async fn a_node_on_another_chain_ends_the_feed() {
        let (_node, source, _server) =
            served(FakeNode { chain_id: 1, ..FakeNode::new() }, REFRESH).await;

        let result = source.fetch().await;

        assert!(matches!(result, Err(FeedError::Fatal(_))));
    }

    /// One market's proxy reverting leaves it out; the others are still published.
    #[tokio::test]
    async fn a_reverting_market_is_left_out() {
        let mut node = FakeNode::new();
        node.reverting.insert(CBBTC_USDC);
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 2);
        assert!(!snapshot
            .books
            .contains_key(&component_id(&cbbtc(), &usdc(), CBBTC_USDC)));
    }

    /// A proxy whose config puts native ETH on the other side than the market list is left out.
    #[tokio::test]
    async fn a_market_whose_config_moves_the_native_side_is_left_out() {
        let mut node = FakeNode::new();
        node.native_flipped.insert(CBBTC_ETH);
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 2);
        assert!(!snapshot
            .books
            .contains_key(&component_id(&cbbtc(), &eth(), CBBTC_ETH)));
    }

    /// A poll where no listed market is served, or none reads cleanly, fails instead of
    /// publishing nothing.
    #[tokio::test]
    async fn a_poll_with_nothing_to_serve_fails() {
        let node = FakeNode::new();
        {
            let mut rewired = node.rewired.lock().unwrap();
            for market in &node.fixture.markets {
                rewired.insert(market.market().orderbook, Address::ZERO);
            }
        }
        let (_node, unserved, _server) = served(node, REFRESH).await;
        let mut node = FakeNode::new();
        node.reverting = node
            .fixture
            .markets
            .iter()
            .map(|market| market.market().orderbook)
            .collect();
        let (_other, unreadable, _other_server) = served(node, REFRESH).await;

        assert!(matches!(unserved.fetch().await, Err(FeedError::Parsing(_))));
        assert!(matches!(unreadable.fetch().await, Err(FeedError::Parsing(_))));
    }

    /// A proxy whose config names other tokens than the market list is left out.
    #[tokio::test]
    async fn a_market_whose_config_disagrees_with_the_list_is_left_out() {
        let mut node = FakeNode::new();
        node.misreporting.insert(CBBTC_USDC);
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 2);
        assert!(!snapshot
            .books
            .contains_key(&component_id(&cbbtc(), &usdc(), CBBTC_USDC)));
    }

    #[tokio::test]
    async fn the_api_down_at_start_fails_the_poll() {
        let node = FakeNode::new();
        node.api_down
            .store(true, Ordering::SeqCst);
        let (_node, source, _server) = served(node, REFRESH).await;

        let result = source.fetch().await;

        assert!(matches!(result, Err(FeedError::Connection(_))));
    }

    /// Once a list was fetched, a refresh the API fails keeps it, and the API is not asked again
    /// before the retry interval.
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_list_and_waits_to_retry() {
        let (node, source, _server) = served(FakeNode::new(), Duration::from_nanos(1)).await;
        source.fetch().await.unwrap();

        node.api_down
            .store(true, Ordering::SeqCst);
        for _ in 0..3 {
            let snapshot = source.fetch().await.unwrap();
            assert_eq!(snapshot.books.len(), 3);
        }

        assert_eq!(node.api_requests.load(Ordering::SeqCst), 2, "the first fetch and one refresh");
    }

    /// The first fetch has no list to fall back on, so it waits past the refresh timeout for a
    /// slow API.
    #[tokio::test]
    async fn a_slow_api_still_gives_the_first_list() {
        let node = FakeNode::new();
        *node.api_delay.lock().unwrap() = TEST_API_TIMEOUT * 2;
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 3);
    }

    /// An API that does not answer is given up on within its timeout, so the poll still reads the
    /// last list in time.
    #[tokio::test]
    async fn a_hanging_api_falls_back_to_the_last_list_in_time() {
        let (node, source, _server) = served(FakeNode::new(), Duration::from_nanos(1)).await;
        source.fetch().await.unwrap();

        *node.api_delay.lock().unwrap() = Duration::from_secs(10);
        let poll = tokio::time::timeout(TEST_API_TIMEOUT * 5, source.fetch()).await;

        let snapshot = poll
            .expect("the poll gives up on the API")
            .unwrap();
        assert_eq!(snapshot.books.len(), 3);
    }

    /// Within the refresh interval the list is not fetched again: the API is asked once, and the
    /// node once for the wiring, once per poll, and once per side of each market for the fill
    /// checks of the first poll, which are not due again within their interval.
    #[tokio::test]
    async fn the_market_list_is_fetched_once_per_refresh() {
        let (node, source, server) = served(FakeNode::new(), REFRESH).await;

        for _ in 0..3 {
            source.fetch().await.unwrap();
        }

        assert_eq!(node.api_requests.load(Ordering::SeqCst), 1);
        assert_eq!(server.request_count(), 1 + 1 + 3 + 3 * 2);
    }

    /// Once the proxies whitelist the taker, they fill it against FastQuoter's book, and the feed
    /// publishes that one from the next poll.
    #[tokio::test]
    async fn a_whitelisted_taker_gets_the_fastquoter_book() {
        let (node, source, _server) = served(FakeNode::new(), REFRESH).await;
        let id = component_id(&eth(), &usdc(), ETH_USDC);
        let before = source.fetch().await.unwrap();

        node.whitelist_the_router_everywhere();
        node.block
            .fetch_add(1, Ordering::SeqCst);
        let after = source.fetch().await.unwrap();

        assert_eq!(published_bids(&before, &id), recorded_bids(ETH_USDC, Caller::Contract));
        assert_eq!(published_bids(&after, &id), recorded_bids(ETH_USDC, Caller::Direct));
    }

    /// A market whose proxy names no contract quoter (the tokenized equities) is not read, even
    /// for a whitelisted taker: its fills and gas were not recorded.
    #[tokio::test]
    async fn a_market_without_a_contract_quoter_is_not_served() {
        let (node, mut source, _server) = served(FakeNode::new(), REFRESH).await;
        let nvda = Token::new(
            &Bytes::from("0xb20000000000000000000078ee7ce2fe4908108c"),
            "NVDAc",
            8,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        );
        let mut tokens = (*book_config().tokens).clone();
        tokens.insert(nvda.address.clone(), nvda.clone());
        source.assembler.book_config.tokens = Arc::new(tokens);
        let id = component_id(&nvda, &usdc(), NVDA_USDC);

        let contract = source.fetch().await.unwrap();
        node.whitelist_the_router_everywhere();
        node.block
            .fetch_add(1, Ordering::SeqCst);
        let whitelisted = source.fetch().await.unwrap();

        assert!(!contract.books.contains_key(&id));
        assert!(!whitelisted.books.contains_key(&id));
    }

    /// A paused proxy fills nothing, though its quoters still report a book: it is left out, and
    /// the others are still published.
    #[tokio::test]
    async fn a_paused_market_is_left_out() {
        let mut node = FakeNode::new();
        node.paused.insert(CBBTC_USDC);
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 2);
        assert!(!snapshot
            .books
            .contains_key(&component_id(&cbbtc(), &usdc(), CBBTC_USDC)));
    }

    /// A market whose wiring does not resolve, whether a getter reverts or it names no contract
    /// quoter, is not asked for it again on the next poll.
    #[rstest]
    #[case::getter_reverts(true)]
    #[case::no_contract_quoter(false)]
    #[tokio::test]
    async fn an_unresolved_market_is_not_asked_again_at_once(#[case] reverts: bool) {
        let mut node = FakeNode::new();
        if reverts {
            node.unwired.insert(CBBTC_USDC);
        } else {
            node.rewired
                .get_mut()
                .unwrap()
                .insert(CBBTC_USDC, Address::ZERO);
        }
        let (node, source, server) = served(node, REFRESH).await;

        source.fetch().await.unwrap();
        let after_first = server.request_count();
        node.block
            .fetch_add(1, Ordering::SeqCst);
        let snapshot = source.fetch().await.unwrap();

        assert_eq!(server.request_count(), after_first + 1, "one read, no wiring request");
        assert!(!snapshot
            .books
            .contains_key(&component_id(&cbbtc(), &usdc(), CBBTC_USDC)));
    }

    /// A proxy that starts naming another contract quoter is left out of the read that sees the
    /// change, and served again once its wiring is resolved anew.
    #[tokio::test]
    async fn a_rewired_market_is_skipped_then_resolved_again() {
        let (node, source, _server) = served(FakeNode::new(), REFRESH).await;
        let id = component_id(&cbbtc(), &usdc(), CBBTC_USDC);
        source.fetch().await.unwrap();
        let fastquoter = node
            .fixture
            .market(CBBTC_USDC)
            .wiring
            .quoter;

        node.rewired
            .lock()
            .unwrap()
            .insert(CBBTC_USDC, fastquoter);
        node.block
            .fetch_add(1, Ordering::SeqCst);
        let changed = source.fetch().await.unwrap();
        node.block
            .fetch_add(1, Ordering::SeqCst);
        let resolved = source.fetch().await.unwrap();

        assert!(!changed.books.contains_key(&id));
        // The proxy now sends contracts to FastQuoter, so they get its book.
        assert_eq!(published_bids(&resolved, &id), recorded_bids(CBBTC_USDC, Caller::Direct));
    }

    /// The first poll checks both sides of every served market, sent as the taker at the read's
    /// block, and publishes them all when their fills agree with their books.
    #[tokio::test]
    async fn fills_that_agree_keep_the_markets_published() {
        let (node, source, _server) = served(FakeNode::new(), REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 3);
        let mut fills = node.fills.lock().unwrap().clone();
        fills.sort();
        let mut expected: Vec<_> = [ETH_USDC, CBBTC_USDC, CBBTC_ETH]
            .into_iter()
            .flat_map(|orderbook| [(orderbook, false), (orderbook, true)])
            .collect();
        expected.sort();
        assert_eq!(fills, expected);
        for orderbook in [ETH_USDC, CBBTC_USDC, CBBTC_ETH] {
            assert_eq!(source.fill_checks.agrees(orderbook), Some(true));
        }
    }

    /// A market whose proxy fills other than its book predicts is withheld, and published again
    /// once a later check agrees.
    #[tokio::test]
    async fn a_market_whose_fills_disagree_is_withheld_until_they_agree() {
        let (node, mut source, _server) = served(FakeNode::new(), REFRESH).await;
        source.fill_checks = FillChecks::new(Duration::ZERO);
        node.misfilling
            .lock()
            .unwrap()
            .insert(CBBTC_USDC);
        let id = component_id(&cbbtc(), &usdc(), CBBTC_USDC);

        let withheld = source.fetch().await.unwrap();
        node.misfilling.lock().unwrap().clear();
        let restored = source.fetch().await.unwrap();

        assert_eq!(withheld.books.len(), 2);
        assert!(!withheld.books.contains_key(&id));
        assert!(restored.books.contains_key(&id));
    }

    #[tokio::test]
    async fn a_market_whose_fills_revert_is_withheld() {
        let mut node = FakeNode::new();
        node.fill_reverting.insert(ETH_USDC);
        let (_node, source, _server) = served(node, REFRESH).await;

        let snapshot = source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 2);
        assert_eq!(source.fill_checks.agrees(ETH_USDC), Some(false));
    }

    /// A check the node cannot answer decides nothing: an unchecked market is still published,
    /// and a withheld one stays withheld.
    #[tokio::test]
    async fn an_unreachable_check_leaves_the_verdicts_as_they_were() {
        let (node, mut source, _server) = served(FakeNode::new(), REFRESH).await;
        source.fill_checks = FillChecks::new(Duration::ZERO);
        node.misfilling
            .lock()
            .unwrap()
            .insert(CBBTC_USDC);
        source.fetch().await.unwrap();

        node.misfilling.lock().unwrap().clear();
        node.fills_unreachable
            .store(true, Ordering::SeqCst);
        let (other, fresh, _other_server) = served(FakeNode::new(), REFRESH).await;
        other
            .fills_unreachable
            .store(true, Ordering::SeqCst);
        let withheld = source.fetch().await.unwrap();
        let unchecked = fresh.fetch().await.unwrap();

        assert_eq!(withheld.books.len(), 2);
        assert_eq!(source.fill_checks.agrees(CBBTC_USDC), Some(false));
        assert_eq!(unchecked.books.len(), 3);
        assert_eq!(fresh.fill_checks.agrees(CBBTC_USDC), None);
    }

    /// Against Base mainnet: a poll publishes the three crypto markets, and each one's fills, sent
    /// as Tycho's router, agree with its published state.
    #[tokio::test]
    #[ignore = "reads Base mainnet and Hanji's API; set BASE_RPC_URL"]
    async fn live_fills_agree_with_the_published_books() {
        let url = std::env::var("BASE_RPC_URL").expect("BASE_RPC_URL not set");
        let feed = HanjiFeedBuilder::new(book_config(), url)
            .build()
            .unwrap();

        let snapshot = feed.source.fetch().await.unwrap();

        assert_eq!(snapshot.books.len(), 3, "{:?}", snapshot.books.keys());
        for orderbook in [ETH_USDC, CBBTC_USDC, CBBTC_ETH] {
            assert_eq!(
                feed.source
                    .fill_checks
                    .agrees(orderbook),
                Some(true),
                "{orderbook}"
            );
        }
    }

    #[test]
    fn the_builder_refuses_other_chains_and_a_zero_refresh() {
        let rpc = "http://localhost:8545".to_string();
        let on_ethereum = BookFeedConfig { chain: Chain::Ethereum, ..book_config() };

        let other_chain = HanjiFeedBuilder::new(on_ethereum, rpc.clone()).build();
        let zero_refresh = HanjiFeedBuilder::new(book_config(), rpc.clone())
            .market_refresh(Duration::ZERO)
            .build();
        let bad_url = HanjiFeedBuilder::new(book_config(), "not a url".into()).build();
        let bad_taker = HanjiFeedBuilder::new(book_config(), rpc.clone())
            .taker(Bytes::from("0x1234"))
            .build();

        assert!(matches!(other_chain, Err(FeedError::InvalidInput(_))));
        assert!(matches!(zero_refresh, Err(FeedError::InvalidInput(_))));
        assert!(matches!(bad_url, Err(FeedError::InvalidInput(_))));
        assert!(matches!(bad_taker, Err(FeedError::InvalidInput(_))));
        assert!(HanjiFeedBuilder::new(book_config(), rpc)
            .build()
            .is_ok());
    }

    /// The RPC URL commonly carries a key: neither the feed's `Debug` output nor a failed poll's
    /// error repeats it.
    #[tokio::test]
    async fn the_rpc_url_stays_out_of_debug_output_and_errors() {
        let rpc = "http://127.0.0.1:1/secret-key".to_string();
        let feed = HanjiFeedBuilder::new(book_config(), rpc)
            .build()
            .unwrap();
        let reader = feed.source.reader.clone();
        let markets = vec![fixture().market(ETH_USDC).market()];

        let debug = format!("{feed:?}");
        let error = reader
            .read(&markets)
            .await
            .unwrap_err()
            .to_string();

        assert!(!debug.contains("secret-key"), "{debug}");
        assert!(!error.contains("secret-key"), "{error}");
    }
}
