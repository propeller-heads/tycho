//! The Efficient pools book feed: polls the configured factories' pools over JSON-RPC and
//! publishes every poll as a complete [`BookSnapshot`].
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{Address, B256, U256},
    providers::RootProvider,
};
use chrono::Utc;
use reqwest::Url;
use tracing::debug;
use tycho_common::{models::Chain, Bytes};

use crate::{
    book::{
        chain_id, component::pair_book, quote_tokens::usd_stablecoins_for_chain, Book,
        BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    pamm::protocols::flamm::{
        gas::{GasTable, Legs, SideGas},
        guard,
        ladder::{settle, Ladder},
        reader::{
            Admission, BookRead, DirectionRead, FlammReader, HookShape, PoolRead, ReaderSettings,
            TokenInfo, SAMPLER_RESERVE_GAS,
        },
        router,
        state::FlammPairState,
        POOL_ATTRIBUTE, PROTOCOL_SYSTEM, PROTOCOL_TYPE,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig, HttpSource},
        Publisher, SnapshotFeed,
    },
};

/// The haircut every quoted output takes, in basis points: an allowance for rounding and small
/// drift between the block a ladder was quoted at and the block a swap lands in. It bounds
/// nothing: a move of the hook's anchor, an oracle update or an earlier fill can take a pool
/// further, and through failing polls a book stands for up to `max_snapshot_age`. The executor
/// gives the pool no minimum output, so only the router's minimum for the whole route stops a
/// worse fill. Where the pool fills at its own answer, the router's fee receiver takes the
/// haircut as positive slippage (see the module's doc).
const DEFAULT_HAIRCUT_BPS: u64 = 2;
/// The most a haircut can be configured to.
const MAX_HAIRCUT_BPS: u64 = 1_000;
/// Rungs a direction is quoted at, doubling from the smallest: 10 USD to 5.2 million USD.
pub(super) const DEFAULT_RUNG_COUNT: usize = 20;
pub(super) const DEFAULT_SMALLEST_RUNG_USD: f64 = 10.0;
/// The gas cap of every view discovery reads. The sampler's call has its own.
const DEFAULT_CALL_GAS: u64 = 5_000_000;
/// The gas of one sampler call, which walks every direction of a pool. At the default cap the
/// sampler's gate keeps about 5.4 million of it back, so about 39.6 million goes to probes.
pub(super) const DEFAULT_SAMPLE_GAS: u64 = 45_000_000;
/// The cap on each of the sampler's calls to a pool or its hook, eight times the most a quote was
/// measured to take: 227,000 to 601,000 on pools over one Morpho Blue or Aave V3 venue.
pub(super) const DEFAULT_PROBE_GAS: u64 = 5_000_000;
/// The most a pool's cap is raised to where its probes run short of gas.
pub(super) const MAX_PROBE_GAS: u64 = 20_000_000;
/// The least gas a sampler call is sent with: the gate a probe passes at [`MAX_PROBE_GAS`], and
/// 100,000 for what the call spends before its first probe, its intrinsic and entry costs.
pub(super) const SAMPLE_GAS_FLOOR: u64 = MAX_PROBE_GAS * 64 / 63 + SAMPLER_RESERVE_GAS + 100_000;
/// The probes a direction may take, then one more just above a tier-labelled top.
pub(super) const MAX_PROBES_PER_DIRECTION: usize = 24;
/// How often a pool's views are read again.
const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// The most new entries of a factory's array a round of discovery reads, each a storage read and
/// a call: the feed learns a very long factory over several rounds, with its first page served.
const DEFAULT_DISCOVERY_PAGE: usize = 1_000;
/// The most calls in flight at once.
const DEFAULT_MAX_IN_FLIGHT: usize = 32;

/// Polls Efficient pools and publishes their pairs' books; made by [`FlammFeedBuilder`].
pub struct FlammFeed {
    feed_config: HttpFeedConfig,
    source: FlammBookSource,
}

/// Builds a [`FlammFeed`]. Nothing is served until [`Self::factories`] names at least one
/// factory, as Efficient pools are deployed per chain, and until something admits its pools, as
/// anyone can create one with a hook and a price feed of their own: [`Self::hook_classes`] with
/// [`Self::price_feeds`] or [`Self::price_feed_code_hashes`], or by name [`Self::pools`].
pub struct FlammFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    factories: Vec<Bytes>,
    pools: Option<Vec<Bytes>>,
    hook_classes: Vec<(B256, HookShape)>,
    price_feeds: Vec<Bytes>,
    price_feed_code_hashes: Vec<B256>,
    taker: Option<Bytes>,
    haircut_bps: u64,
    rung_count: usize,
    smallest_rung_usd: f64,
    call_gas: u64,
    sample_gas: u64,
    probe_gas: u64,
    gas: GasTable,
    max_block_lag: Option<Duration>,
    discovery_interval: Duration,
    discovery_page: usize,
    max_in_flight: usize,
    feed_config: HttpFeedConfig,
}

impl FlammFeedBuilder {
    pub fn new(book_config: BookFeedConfig, rpc_url: String) -> Self {
        let chain = book_config.chain;
        Self {
            book_config,
            rpc_url,
            factories: Vec::new(),
            pools: None,
            hook_classes: Vec::new(),
            price_feeds: Vec::new(),
            price_feed_code_hashes: Vec::new(),
            taker: None,
            haircut_bps: DEFAULT_HAIRCUT_BPS,
            rung_count: DEFAULT_RUNG_COUNT,
            smallest_rung_usd: DEFAULT_SMALLEST_RUNG_USD,
            call_gas: DEFAULT_CALL_GAS,
            sample_gas: DEFAULT_SAMPLE_GAS,
            probe_gas: DEFAULT_PROBE_GAS,
            gas: GasTable::default(),
            max_block_lag: Some(default_max_block_lag(chain)),
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            discovery_page: DEFAULT_DISCOVERY_PAGE,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            feed_config: Self::default_feed_config(chain),
        }
    }

    /// The factories whose pools are served: those the lists admit, or those [`Self::pools`]
    /// names.
    pub fn factories(mut self, factories: Vec<Bytes>) -> Self {
        self.factories = factories;
        self
    }

    /// Serves only these pools of the factories, whatever their hook and price feed: the caller
    /// vouches for them, and the lists are not asked. A pool a front prices is still not served. A
    /// pool whose hook is of no listed class is quoted by steps, each span at its lower rung, where
    /// the hook's `params()` show no leverage row, and not at all where they show one, as no
    /// listed shape says how its leverage answers fall.
    pub fn pools(mut self, pools: Vec<Bytes>) -> Self {
        self.pools = Some(pools);
        self
    }

    /// The classes ([`super::hook_class`]) of the hooks that may price a served pool, each with
    /// the shape of its answers, which decides how they are settled into ladders. A hook of
    /// another class is not admitted, and neither is a front its factory lists (`isListedFront`),
    /// whatever its class.
    pub fn hook_classes(mut self, classes: Vec<(B256, HookShape)>) -> Self {
        self.hook_classes = classes;
        self
    }

    /// The price feeds a served pool may read, by address. Prefer this to
    /// [`Self::price_feed_code_hashes`] for a feed configured in storage or at construction.
    pub fn price_feeds(mut self, feeds: Vec<Bytes>) -> Self {
        self.price_feeds = feeds;
        self
    }

    /// The price feeds a served pool may read, by the keccak-256 hash of their runtime code. A
    /// hash vouches for every contract with that code, so it suits only code that fixes the prices
    /// it reports: a feed told what to read at construction can be deployed again told otherwise,
    /// and shares the hash.
    pub fn price_feed_code_hashes(mut self, hashes: Vec<B256>) -> Self {
        self.price_feed_code_hashes = hashes;
        self
    }

    /// The caller quotes are asked for. Defaults to Tycho's router on the chain, which is the
    /// caller a pool and its hooks see when the router swaps.
    pub fn taker(mut self, taker: Bytes) -> Self {
        self.taker = Some(taker);
        self
    }

    /// Does nothing, kept so that code which sets it still builds: a pool a front prices, one
    /// whose hook its factory lists as a front or whose cheap lane is on, is never served,
    /// whatever this says. Its pairs trade through the front's listing doors.
    pub fn serve_front_priced(self, _serve: bool) -> Self {
        self
    }

    /// The share of every quoted output kept back, in basis points, at most 1,000. An allowance
    /// for drift, not a bound on it: raise it for a pool that moves more between blocks.
    pub fn haircut_bps(mut self, bps: u64) -> Self {
        self.haircut_bps = bps;
        self
    }

    /// How many doubling rungs a direction is quoted at, from `smallest_usd` of its input (or a
    /// thousandth of a whole token where the pool's feed does not price it): at most 24, the
    /// probes a direction may take.
    pub fn rungs(mut self, count: usize, smallest_usd: f64) -> Self {
        self.rung_count = count;
        self.smallest_rung_usd = smallest_usd;
        self
    }

    /// The gas cap of every view the feed reads to find and value pools. Quotes are read by the
    /// sampler, whose gas is [`Self::sample_gas`] and [`Self::probe_gas`].
    pub fn call_gas(mut self, gas: u64) -> Self {
        self.call_gas = gas;
        self
    }

    /// The gas of each pool's sampler call, which walks every direction of the pool: 45 million
    /// by default, and at least 20,717,460, what a call spends before its first probe at the most
    /// cap a pool is raised to. A node that gives the call less gas than asked gets a shorter
    /// walk, not a failed one.
    pub fn sample_gas(mut self, gas: u64) -> Self {
        self.sample_gas = gas;
        self
    }

    /// The cap on each of the sampler's calls to a pool or its hook, 5 million by default and 20
    /// million at most. A pool whose answers run short of it has it doubled for the next poll, up
    /// to 20 million, and back once every probe of an answer fits well under it.
    pub fn probe_gas(mut self, gas: u64) -> Self {
        self.probe_gas = gas;
        self
    }

    /// Does nothing: a swap is charged from the measured table compiled into this module, by
    /// what the feed reads of its pool, so that no fill is charged more than it costs, which a
    /// figure set here could be. Kept so that code which sets one still builds.
    pub fn gas(self, _hook: B256, _venue_kinds: Vec<u8>, _gas: u64) -> Self {
        self
    }

    /// The table swaps are charged by, in place of the measured one.
    #[cfg(test)]
    pub(super) fn gas_table(mut self, gas: GasTable) -> Self {
        self.gas = gas;
        self
    }

    /// How far the node's latest block may trail the wall clock before a poll fails; `None` for
    /// a node whose clock is not the wall clock's, as a local fork's.
    pub fn max_block_lag(mut self, lag: Option<Duration>) -> Self {
        self.max_block_lag = lag;
        self
    }

    /// How often each pool's views are read again, and each factory asked for new pools.
    pub fn discovery_interval(mut self, interval: Duration) -> Self {
        self.discovery_interval = interval;
        self
    }

    /// The most new pools of a factory one round of discovery takes in.
    pub fn discovery_page(mut self, pools: usize) -> Self {
        self.discovery_page = pools;
        self
    }

    /// The most calls the feed has in flight at once: a quarter of them, at least one, are
    /// discovery's, and the rest, at least one, the polls'. Each poll makes one sampler call per
    /// pool, so a poll of `pools` ends in time where, in seconds,
    /// `head_time + ceil(pools / poll_permits) * call_time <= poll_deadline`, the deadline being
    /// three quarters of `request_timeout`. On Base, 3 s and the default 24 permits hold 288 pools
    /// at 240 ms a call, which needs a head read under 120 ms, and at 500 ms 144 pools with no head
    /// time, 120 once it counts. A call's time is the node's execution of the sampler as well as
    /// the round trip. A pool given no permit by the deadline leaves that snapshot with no strike,
    /// and the next poll starts with it. A discovery round of `pools` takes about
    /// `17 * pools * view_time / discovery_permits`: 17 calls a pool of one venue once its hook's
    /// code and row are known, 8 permits by default.
    pub fn max_in_flight(mut self, calls: usize) -> Self {
        self.max_in_flight = calls;
        self
    }

    /// The poll loop's tuning; [`Self::default_feed_config`] is the chain's default.
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// One poll a second on Base and BSC and every two seconds on Ethereum, each poll's requests
    /// due in four and six seconds, and a book withdrawn after three missed polls' worth of time,
    /// so one poll that hangs to its deadline does not blank it.
    ///
    /// A poll reads the head, then makes one sampler call per pool unless the head is the block
    /// last read in full: at most `(pools + 1) / poll_interval` calls a second, in seconds, each
    /// walking up to 50 probes, some 5 to 8 million gas at the node for a typical pool. Where
    /// blocks come slower than the polls, most polls are served from the last read: about
    /// `pools / block_time + 1 / poll_interval`, or `pools / 12 + 0.5` on Ethereum, whose blocks
    /// come every 12 s (`tycho_common::models::Chain::block_time_secs`). `poll_interval` is the
    /// lever: doubling it halves the most a poll can cost a second. A failing poll costs up to
    /// `2 * pools` calls, as each is asked again once. Discovery adds 17 calls a pool of one venue,
    /// and up to four more for each further venue, every `discovery_interval`.
    pub fn default_feed_config(chain: Chain) -> HttpFeedConfig {
        let (poll_interval, request_timeout, max_snapshot_age) = match chain {
            Chain::Ethereum => (2, 6, 20),
            Chain::Starknet |
            Chain::ZkSync |
            Chain::Arbitrum |
            Chain::Base |
            Chain::Bsc |
            Chain::Unichain |
            Chain::Polygon |
            Chain::Plasma |
            Chain::Robinhood |
            Chain::Arc |
            Chain::Custom(_) |
            _ => (1, 4, 12),
        };
        HttpFeedConfig {
            poll_interval: Duration::from_secs(poll_interval),
            request_timeout: Duration::from_secs(request_timeout),
            max_snapshot_age: Some(Duration::from_secs(max_snapshot_age)),
            ..default_http_feed_config()
        }
    }

    pub fn build(self) -> Result<FlammFeed, FeedError> {
        let chain = self.book_config.chain;
        let Some(default_taker) = router(chain) else {
            return Err(FeedError::Fatal(format!(
                "Efficient pools are served on Ethereum, Base and BSC, not on {chain}"
            )));
        };
        if self.factories.is_empty() {
            return Err(FeedError::InvalidInput("no Efficient pools factory configured".into()));
        }
        let admits_any = match &self.pools {
            Some(pools) => !pools.is_empty(),
            None => {
                !self.hook_classes.is_empty() &&
                    !(self.price_feeds.is_empty() && self.price_feed_code_hashes.is_empty())
            }
        };
        if !admits_any {
            return Err(FeedError::InvalidInput(
                "nothing admits a pool: name pools, or list hook classes and price feeds".into(),
            ));
        }
        if self.haircut_bps > MAX_HAIRCUT_BPS {
            return Err(FeedError::InvalidInput(format!(
                "a haircut of {} bps is above the {MAX_HAIRCUT_BPS} bps allowed",
                self.haircut_bps
            )));
        }
        if self.rung_count == 0 ||
            !(self.smallest_rung_usd > 0.0 && self.smallest_rung_usd.is_finite())
        {
            return Err(FeedError::InvalidInput(
                "rungs need a count and a positive smallest size".into(),
            ));
        }
        if self.rung_count > MAX_PROBES_PER_DIRECTION {
            return Err(FeedError::InvalidInput(format!(
                "{} rungs are more than the {MAX_PROBES_PER_DIRECTION} probes a direction may take",
                self.rung_count
            )));
        }
        if self.probe_gas == 0 || self.probe_gas > MAX_PROBE_GAS {
            return Err(FeedError::InvalidInput(format!(
                "probe_gas {} is not between 1 and {MAX_PROBE_GAS}",
                self.probe_gas
            )));
        }
        if self.sample_gas < SAMPLE_GAS_FLOOR {
            return Err(FeedError::InvalidInput(format!(
                "sample_gas {} is under {SAMPLE_GAS_FLOOR}, what a sampler call spends before its \
                 first probe at the most cap",
                self.sample_gas
            )));
        }
        if self.discovery_interval.is_zero() {
            return Err(FeedError::InvalidInput("discovery_interval must not be zero".into()));
        }
        if self.discovery_page == 0 {
            return Err(FeedError::InvalidInput("discovery_page must not be zero".into()));
        }
        if self.max_in_flight == 0 {
            return Err(FeedError::InvalidInput("max_in_flight must not be zero".into()));
        }
        let address = |bytes: &Bytes, what: &str| {
            (bytes.len() == 20)
                .then(|| Address::from_slice(bytes))
                .filter(|a| !a.is_zero())
                .ok_or_else(|| FeedError::InvalidInput(format!("{what} {bytes} is not an address")))
        };
        let mut factories = self
            .factories
            .iter()
            .map(|f| address(f, "factory"))
            .collect::<Result<Vec<_>, _>>()?;
        factories.sort_unstable();
        factories.dedup();
        let allowed_pools = self
            .pools
            .as_ref()
            .map(|pools| {
                pools
                    .iter()
                    .map(|p| address(p, "pool"))
                    .collect::<Result<HashSet<_>, _>>()
            })
            .transpose()?;
        let price_feeds = self
            .price_feeds
            .iter()
            .map(|f| address(f, "price feed"))
            .collect::<Result<HashSet<_>, _>>()?;
        let taker = self
            .taker
            .as_ref()
            .map(|t| address(t, "taker"))
            .transpose()?
            .unwrap_or(default_taker);
        let url: Url = self
            .rpc_url
            .parse()
            .map_err(|e| FeedError::InvalidInput(format!("rpc url: {e}")))?;
        let to_addresses = |tokens: &mut dyn Iterator<Item = &Bytes>| -> HashSet<Address> {
            tokens
                .filter(|t| t.len() == 20)
                .map(|t| Address::from_slice(t))
                .collect()
        };
        let universe = to_addresses(&mut self.book_config.tokens.keys());
        let stablecoins = usd_stablecoins_for_chain(chain).unwrap_or_default();
        let stablecoins = to_addresses(&mut stablecoins.iter());
        let settings = ReaderSettings {
            taker,
            factories,
            allowed_pools,
            admission: Admission {
                hook_classes: self.hook_classes.into_iter().collect(),
                price_feeds,
                price_feed_codes: self
                    .price_feed_code_hashes
                    .into_iter()
                    .collect(),
            },
            call_gas: self.call_gas,
            call_timeout: self.feed_config.request_timeout,
            sample_gas: self.sample_gas,
            probe_gas: self.probe_gas,
            max_probes_per_direction: MAX_PROBES_PER_DIRECTION,
            smallest_rung_usd: self.smallest_rung_usd,
            rung_count: self.rung_count,
            discovery_interval: self.discovery_interval,
            discovery_page: self.discovery_page,
            min_nav_usd: self.book_config.min_tvl_usd,
            universe,
            stablecoins,
            chain_id: chain_id(chain)?,
            gas: self.gas,
        };
        Ok(FlammFeed {
            source: FlammBookSource {
                poll_deadline: self.feed_config.request_timeout * 3 / 4,
                chain_id: chain_id(chain)?,
                chain_checked: AtomicBool::new(false),
                reader: Arc::new(FlammReader::new(
                    RootProvider::new_http(url),
                    settings,
                    self.max_in_flight,
                )),
                assembler: FlammBookAssembler {
                    book_config: self.book_config,
                    haircut_bps: self.haircut_bps,
                },
                max_block_lag: self.max_block_lag,
                last_block: AtomicU64::new(0),
                last_books: Mutex::new(None),
            },
            feed_config: self.feed_config,
        })
    }
}

/// How far a healthy node's latest block trails the wall clock at most: two Base blocks and a
/// second, five BSC blocks, or one Ethereum slot and a half.
fn default_max_block_lag(chain: Chain) -> Duration {
    Duration::from_secs(match chain {
        Chain::Ethereum => 18,
        Chain::Starknet |
        Chain::ZkSync |
        Chain::Arbitrum |
        Chain::Base |
        Chain::Bsc |
        Chain::Unichain |
        Chain::Polygon |
        Chain::Plasma |
        Chain::Robinhood |
        Chain::Arc |
        Chain::Custom(_) |
        _ => 5,
    })
}

impl SnapshotFeed for FlammFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    /// Polls with discovery running beside the polls, on this one task: it lives as long as they
    /// do, and only they can end the feed.
    async fn run(self, publisher: Publisher<Self::Snapshot>) -> Result<(), FeedError> {
        let reader = Arc::clone(&self.source.reader);
        tokio::select! {
            ended = run_http_poll_feed(self.feed_config, publisher, self.source) => ended,
            never = reader.keep_discovering() => match never {},
        }
    }
}

#[cfg(test)]
impl FlammFeed {
    /// The reader this feed polls with.
    pub(super) fn reader(&self) -> &FlammReader {
        &self.source.reader
    }

    /// The books this feed publishes of `read`.
    pub(super) fn books(&self, read: &BookRead) -> Books {
        self.source.assembler.books(read)
    }
}

struct FlammBookSource {
    /// How long a poll's pools may take from the poll's start: less than the poll loop allows, so
    /// that the pools that answered in time are served.
    poll_deadline: Duration,
    chain_id: u64,
    chain_checked: AtomicBool,
    reader: Arc<FlammReader>,
    assembler: FlammBookAssembler,
    max_block_lag: Option<Duration>,
    last_block: AtomicU64,
    /// The books of the last block read in full, served again while the node's latest block is
    /// that one.
    last_books: Mutex<Option<BlockBooks>>,
}

/// A block and the books read at it.
type BlockBooks = (u64, Arc<std::collections::HashMap<String, Book>>);

impl HttpSource for FlammBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A poll fails on a node on another chain (for good), on a node that cannot be read, on a
    /// block older than one already read, on one further behind the wall clock than
    /// `max_block_lag`, while discovery has found nothing to serve and cannot read on, where the
    /// node runs the sampler for under half the pools, and where most pools miss it. A pool with
    /// no read drops out of the snapshot, with every book it had, and a poll short of a pool is
    /// not served again for its block: the next poll reads it anew.
    async fn fetch(&self) -> Result<Self::Snapshot, FeedError> {
        let started = tokio::time::Instant::now();
        if !self
            .chain_checked
            .load(Ordering::Relaxed)
        {
            let chain_id = self.reader.chain_id().await?;
            if chain_id != self.chain_id {
                return Err(FeedError::Fatal(format!(
                    "the node serves chain {chain_id}, not chain {}",
                    self.chain_id
                )));
            }
            self.chain_checked
                .store(true, Ordering::Relaxed);
        }
        let (block, timestamp) = self.reader.head().await?;
        if let Some(lag) = self.max_block_lag {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            if now > timestamp + lag.as_secs() {
                return Err(FeedError::Connection(format!(
                    "the node's latest block {block} is {} s old",
                    now - timestamp
                )));
            }
        }
        let newest = self
            .last_block
            .fetch_max(block, Ordering::Relaxed);
        if block < newest {
            return Err(FeedError::Connection(format!(
                "the node answered from block {block}, older than block {newest}"
            )));
        }
        if let Some((read_at, books)) = self
            .last_books
            .lock()
            .expect("books lock")
            .as_ref()
        {
            if *read_at == block {
                return Ok(BookSnapshot {
                    anchor: ReceivedAt(Utc::now()),
                    books: Arc::clone(books),
                });
            }
        }
        let read = self
            .reader
            .read(block, started + self.poll_deadline)
            .await?;
        let snapshot = BookSnapshot::received_now(self.assembler.books(&read));
        if !read.partial {
            *self
                .last_books
                .lock()
                .expect("books lock") = Some((block, Arc::clone(&snapshot.books)));
        }
        Ok(snapshot)
    }
}

struct FlammBookAssembler {
    book_config: BookFeedConfig,
    haircut_bps: u64,
}

impl FlammBookAssembler {
    /// One book per pool and unordered pair the pool quotes at least one way, between tokens of
    /// the universe, above the TVL floor. Its id is the pool and the two tokens, the lower
    /// address first; its `pamm_address` is the pool, the address the executor calls.
    fn books(&self, read: &BookRead) -> Books {
        let mut books = Books::new();
        for pool in &read.pools {
            for (lo, hi) in pairs(pool) {
                let tokens = &pool.info.tokens;
                let (a, b) = (&tokens[lo], &tokens[hi]);
                let (a_addr, b_addr) =
                    (Bytes::from(a.address.to_vec()), Bytes::from(b.address.to_vec()));
                let Some((base, quote)) = self
                    .book_config
                    .pair_tokens(&a_addr, &b_addr)
                else {
                    continue
                };
                let (sell_base, sell_base_gas) = self.ladder(pool, lo, hi);
                let (sell_quote, sell_quote_gas) = self.ladder(pool, hi, lo);
                if sell_base.is_empty() && sell_quote.is_empty() {
                    continue;
                }
                let id = Bytes::from(
                    [pool.info.pool.as_slice(), a.address.as_slice(), b.address.as_slice()]
                        .concat(),
                );
                let tvl = match tvl_usd(a, b, &sell_base, &sell_quote) {
                    Some(tvl) => tvl,
                    None if self.book_config.min_tvl_usd > 0.0 => {
                        debug!(book = %id, "neither token of the pair has a USD price: not served");
                        continue;
                    }
                    None => 0.0,
                };
                if !self
                    .book_config
                    .clears_min_tvl(tvl, &id)
                {
                    continue;
                }
                let state = FlammPairState {
                    base_token: base.clone(),
                    quote_token: quote.clone(),
                    sell_base,
                    sell_quote,
                    block: read.block,
                    sell_base_gas,
                    sell_quote_gas,
                    moved: false,
                };
                let mut book = pair_book(
                    PROTOCOL_SYSTEM,
                    PROTOCOL_TYPE,
                    self.book_config.chain,
                    id,
                    state,
                    None,
                );
                book.component
                    .static_attributes
                    .insert(POOL_ATTRIBUTE.to_string(), Bytes::from(pool.info.pool.to_vec()));
                books.insert(book);
            }
        }
        books
    }

    /// The ladder selling token `i` for token `j`, and what a swap that way is charged; an empty
    /// ladder, charged nothing, where the pool quotes nothing that way. Where a venue holding a
    /// minimum position lends token `j`, the settled ladder keeps what [`guard::keep`] allows
    /// before its haircut.
    fn ladder(&self, pool: &PoolRead, i: usize, j: usize) -> (Ladder, SideGas) {
        pool.directions
            .iter()
            .find(|d| d.plan.token_in == i && d.plan.token_out == j)
            .map_or_else(
                || (Ladder::default(), SideGas::default()),
                |d| {
                    let read = legs(pool, i, j);
                    let terms = d.terms(&pool.info);
                    let ladder = match d.plan.guard {
                        None => Ladder::settle(&d.probes, &terms, self.haircut_bps),
                        Some(held_repay) => {
                            let settled = settle(&d.probes, &terms).ladder;
                            let kept = guard::keep(
                                settled.rungs(),
                                &d.probes,
                                read.as_ref().map(|(legs, _)| legs),
                                read.map_or(U256::ZERO, |(_, band)| band),
                                held_repay,
                            );
                            if kept.len() < settled.rungs().len() {
                                debug!(pool = %pool.info.pool, token_in = i, token_out = j, ?kept, ?held_repay, "the Efficient pool's minimum position keeps part of its ladder");
                            }
                            settled
                                .keep(kept)
                                .haircut(self.haircut_bps)
                        }
                    };
                    let gas = d.plan.gas.side_gas(
                        read.as_ref().map(|(legs, _)| legs),
                        pool.info.tokens.len() == 2,
                    );
                    (ladder, gas)
                },
            )
    }
}

/// The pool's legs as a swap selling token `i` for token `j` reads them, the input's first, and
/// the band word beside them, from a walk of the pool at this block that read them, its band word
/// not zero: this direction's, or the other's with its two tokens swapped, as both are the pool's
/// positions at the block. `None` where neither read them.
fn legs(pool: &PoolRead, i: usize, j: usize) -> Option<(Legs, U256)> {
    let read = |d: &&DirectionRead| !d.band_wad.is_zero();
    if let Some(d) = pool
        .directions
        .iter()
        .filter(read)
        .find(|d| d.plan.token_in == i && d.plan.token_out == j)
    {
        return Some((d.legs, d.band_wad));
    }
    pool.directions
        .iter()
        .filter(read)
        .find(|d| d.plan.token_in == j && d.plan.token_out == i)
        .map(|d| {
            let mut legs = [U256::ZERO; 8];
            legs[..4].copy_from_slice(&d.legs[4..]);
            legs[4..].copy_from_slice(&d.legs[..4]);
            (legs, d.band_wad)
        })
}

/// The unordered pairs `pool` quotes at least one way, the lower token address first.
fn pairs(pool: &PoolRead) -> Vec<(usize, usize)> {
    let tokens = &pool.info.tokens;
    let mut out: Vec<(usize, usize)> = pool
        .directions
        .iter()
        .map(|d| {
            let (i, j) = (d.plan.token_in, d.plan.token_out);
            if tokens[i].address < tokens[j].address {
                (i, j)
            } else {
                (j, i)
            }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The depth of a book in USD: the larger of its two ladders' largest fills, each valued on a
/// side whose token has a price. `None` where neither token has one.
fn tvl_usd(a: &TokenInfo, b: &TokenInfo, sell_a: &Ladder, sell_b: &Ladder) -> Option<f64> {
    let value = |token: &TokenInfo, amount: U256| -> Option<f64> {
        let amount = u128::try_from(amount).map_or(f64::MAX, |v| v as f64);
        Some(amount / 10f64.powi(i32::from(token.decimals)) * token.usd?)
    };
    // a ladder is valued at its input where the input has a price, else at its output
    let depth = |ladder: &Ladder, input: &TokenInfo, output: &TokenInfo| -> Option<f64> {
        let Some(last) = ladder.last() else { return Some(0.0) };
        value(input, last.input).or_else(|| value(output, last.output))
    };
    if a.usd.is_none() && b.usd.is_none() {
        return None;
    }
    Some(depth(sell_a, a, b)?.max(depth(sell_b, b, a)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pamm::protocols::flamm::ladder::{Rung, Segment};

    fn token(byte: u8, decimals: u8, usd: Option<f64>) -> TokenInfo {
        TokenInfo { address: Address::repeat_byte(byte), decimals, usd }
    }

    fn ladder(rungs: &[(u64, u64)]) -> Ladder {
        Ladder::from_rungs(
            rungs
                .iter()
                .map(|(input, output)| Rung {
                    input: U256::from(*input),
                    output: U256::from(*output),
                    segment: Segment::Chord,
                })
                .collect(),
        )
    }

    #[test]
    fn depth_is_valued_on_whichever_side_has_a_price() {
        let btc = token(1, 8, None);
        let usdc = token(2, 6, Some(1.0));
        // 1 BTC sells for 80,000 USDC; 40,000 USDC buys 0.5 BTC
        let sell_btc = ladder(&[(100_000_000, 80_000_000_000)]);
        let sell_usdc = ladder(&[(40_000_000_000, 50_000_000)]);
        assert_eq!(tvl_usd(&btc, &usdc, &sell_btc, &sell_usdc), Some(80_000.0));
        let unpriced = token(3, 6, None);
        assert_eq!(tvl_usd(&btc, &unpriced, &sell_btc, &sell_usdc), None);
    }
}

#[cfg(test)]
mod node_tests {
    use std::{sync::Arc, time::Instant};

    use alloy::primitives::keccak256;
    use futures::StreamExt;
    use num_bigint::BigUint;
    use rstest::rstest;

    use super::*;
    use crate::{
        book::test_token_map,
        pamm::protocols::flamm::{
            gas::tests::{table_serving, table_serving_on},
            hook_class,
            ladder::{Rung, Segment, DUST_OUT},
            reader::{Catalogue, LEV_STATE_SELECTOR, QUOTE_PAIR_SELECTOR, SAMPLER_RUNTIME},
            test_support::{
                self as fake, FakeHook, FakeNode, FakePool, Fault, Limit, BLOCK, CHAIN_ID,
                CURVE_FEE, FACTORY, FEED, FEED_CODE, FRONT, HOOK, HOOK_CODE, NO_CODE, ROGUE_FEED,
                ROGUE_HOOK, TIER_DOWN, TIER_UP, TWIN_FEED, UNLISTED_FRONT,
            },
        },
        snapshot_feed::{SnapshotFeedEvent, SnapshotFeedStream},
    };

    fn bytes(address: Address) -> Bytes {
        Bytes::from(address.to_vec())
    }

    fn config(min_tvl_usd: f64) -> BookFeedConfig {
        let (eth, usd) = (bytes(fake::eth().address), bytes(fake::usd().address));
        let tether = bytes(fake::tether().address);
        BookFeedConfig {
            chain: Chain::Base,
            tokens: Arc::new(test_token_map(&[
                (&eth, "ETH", 18),
                (&usd, "USD", 6),
                (&tether, "USDT", 6),
            ])),
            min_tvl_usd,
        }
    }

    /// The fake hook's class, as an Efficient pools hook.
    fn listed_hook() -> (B256, HookShape) {
        (hook_class(&HOOK_CODE), HookShape::Everlong)
    }

    /// A feed of the factory with the fake hook's class and the fake feed on its lists.
    fn builder(node: &FakeNode, config: BookFeedConfig) -> FlammFeedBuilder {
        unlisted_builder(node, config)
            .hook_classes(vec![listed_hook()])
            .price_feeds(vec![bytes(FEED)])
    }

    fn wad(value: u64) -> U256 {
        U256::from(value)
    }

    fn ether(whole: u64) -> U256 {
        U256::from(whole) * U256::from(10u64).pow(U256::from(18))
    }

    /// A feed of the factory whose swaps are charged by a table that serves the fake hook's class
    /// over kinds 0 and 1: 100,000 a swap from custody, 300,000 on any other path.
    fn unlisted_builder(node: &FakeNode, config: BookFeedConfig) -> FlammFeedBuilder {
        FlammFeedBuilder::new(config, node.server.url())
            .factories(vec![bytes(FACTORY)])
            .max_block_lag(None)
            .gas_table(table_serving(hook_class(&HOOK_CODE)))
    }

    /// The pools the node was asked to quote, in order of address.
    fn pools_quoted(node: &FakeNode) -> Vec<Address> {
        let seen = node.seen.lock().unwrap();
        let quoted: HashSet<Address> = seen
            .quoted_pools
            .iter()
            .copied()
            .collect();
        let mut quoted: Vec<Address> = quoted.into_iter().collect();
        quoted.sort();
        quoted
    }

    /// One round of discovery, then a poll.
    async fn first_poll(feed: &FlammFeed) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        feed.source
            .reader
            .discover(&mut Catalogue::default())
            .await;
        feed.source.fetch().await
    }

    /// A poll of the next block, after a round of discovery where `catalogue` is given.
    async fn next_poll(
        feed: &FlammFeed,
        node: &FakeNode,
        catalogue: Option<&mut Catalogue>,
    ) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        if let Some(catalogue) = catalogue {
            feed.source
                .reader
                .discover(catalogue)
                .await;
        }
        node.chain.lock().unwrap().blocks_ahead += 1;
        feed.source.fetch().await
    }

    fn state(book: &Book) -> &FlammPairState {
        book.state
            .as_any()
            .downcast_ref::<FlammPairState>()
            .unwrap()
    }

    #[tokio::test]
    async fn a_pool_is_quoted_to_the_wei_at_its_rungs_as_the_router() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();

        assert_eq!(snapshot.books.len(), 1);
        let book = snapshot.books.values().next().unwrap();
        let (pool, eth, usd) = (Address::repeat_byte(0x01), fake::eth(), fake::usd());
        let id = [pool.as_slice(), eth.address.as_slice(), usd.address.as_slice()].concat();
        assert_eq!(book.component.id, Bytes::from(id));
        assert_eq!(book.component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(book.component.static_attributes[POOL_ATTRIBUTE], bytes(pool));

        // 10 USD of ether at 2,000 USD sells for 9.99 USD, less the 2 bps haircut
        let tokens = config(0.0).tokens;
        let (eth_t, usd_t) = (&tokens[&bytes(eth.address)], &tokens[&bytes(usd.address)]);
        let first = state(book).sell_base.first().unwrap();
        assert_eq!(first.input, U256::from(5_000_000_000_000_000u64));
        let out = book
            .state
            .get_amount_out(BigUint::from(5_000_000_000_000_000u64), eth_t, usd_t)
            .unwrap();
        assert_eq!(out.amount, BigUint::from(9_988_002u64));
        // the walk read no legs, so the swap is charged the least its venue's kind allows
        assert_eq!(out.gas, BigUint::from(100_000u64));

        // the 50 ETH cap clips the ladder there, and the limit offers 99 % of it
        let last = state(book).sell_base.last().unwrap();
        assert_eq!(last.input, U256::from(50u64) * U256::from(10u64).pow(U256::from(18)));
        let (limit, _) = book
            .state
            .get_limits(bytes(eth.address), bytes(usd.address))
            .unwrap();
        assert_eq!(limit, BigUint::from(495u64) * BigUint::from(10u64).pow(17));

        let seen = node.seen.lock().unwrap();
        assert!(!seen.quote_senders.is_empty());
        assert!(seen
            .quote_senders
            .iter()
            .all(|sender| *sender == router(Chain::Base)));
    }

    /// A pool is served only where the table charges a swap over every live venue on its pair: not
    /// with no live venue, which no fill was measured over, nor with a live venue of a kind the
    /// chain does not route, nor with a record past those read, whose kind is unknown.
    #[rstest]
    #[case::no_venue(Vec::new(), 0)]
    #[case::a_retired_venue(vec![0], 1)]
    #[case::a_fenced_venue(vec![1], 2)]
    #[case::a_kind_the_chain_does_not_route(vec![0, 2], 0)]
    #[case::more_records_than_are_read(vec![0; 33], 0)]
    #[tokio::test]
    async fn a_pool_whose_swaps_the_table_cannot_charge_is_not_served(
        #[case] venue_kinds: Vec<u8>,
        #[case] venue_status: u8,
    ) {
        let mut pool = fake::pool(0x01);
        pool.venue_kinds = venue_kinds;
        pool.venue_status = venue_status;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool, fake::pool(0x02)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x02)]);
        assert_eq!(snapshot.books.len(), 1);
    }

    /// A venue that does not say it lets the pool repay leaves a buy's repay unsure, which only
    /// lowers what a swap into a pool that owes is charged; it serves the pool as before.
    #[rstest]
    #[case::repays(Some(0x3f))]
    #[case::refuses_repays(Some(0x3d))]
    #[case::does_not_say(None)]
    #[tokio::test]
    async fn a_venues_actions_do_not_decide_whether_a_pool_is_served(#[case] actions: Option<u8>) {
        let mut pool = fake::pool(0x01);
        pool.venue_actions = actions;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        let book = snapshot.books.values().next().unwrap();
        assert_eq!(
            state(book)
                .sell_quote_gas
                .limit_addition(),
            1_000_000
        );
    }

    #[tokio::test]
    async fn the_same_block_is_served_again_without_quoting_it() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let first = first_poll(&feed).await.unwrap();
        let asked = node.server.request_count();
        let second = feed.source.fetch().await.unwrap();
        assert!(Arc::ptr_eq(&first.books, &second.books));
        // the head of the chain, and nothing else
        assert_eq!(node.server.request_count(), asked + 1);
    }

    #[tokio::test]
    async fn a_pool_that_fails_its_views_is_set_aside_and_the_others_served() {
        let mut broken = fake::pool(0x02);
        broken.fault = Fault::Views;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), broken]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(feed
            .source
            .reader
            .quarantined(&Address::repeat_byte(0x02), Instant::now()));
    }

    /// An error reads the same from a pool out of gas as from a node in trouble, so only the
    /// other pools answering tells them apart.
    #[tokio::test]
    async fn a_quote_that_runs_out_of_gas_is_a_miss_and_a_second_sets_its_pool_aside() {
        let mut heavy = fake::pool(0x03);
        heavy.fault = Fault::OutOfGas;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), heavy]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(!reader.quarantined(&Address::repeat_byte(0x03), Instant::now()));

        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(reader.quarantined(&Address::repeat_byte(0x03), Instant::now()));
    }

    /// Views that read well forgive nothing: a pool whose quotes keep failing goes on to the
    /// longest time aside, and only its quotes coming back clear it.
    #[tokio::test]
    async fn strikes_outlast_discovery_and_end_with_a_good_quote() {
        let address = Address::repeat_byte(0x05);
        let mut failing = fake::pool(0x05);
        failing.fault = Fault::QuotesUnanswered;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), failing]).await;
        let feed = builder(&node, config(0.0))
            .discovery_interval(Duration::from_millis(1))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let mut catalogue = Catalogue::default();
        reader.discover(&mut catalogue).await;
        feed.source.fetch().await.unwrap();
        for strikes in 1..=3 {
            // out of its time aside, its views read well and its quotes miss again
            tokio::time::sleep(Duration::from_millis(10)).await;
            next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap();
            assert_eq!(reader.strikes(&address), strikes);
        }

        node.chain.lock().unwrap().pools[1].fault = Fault::None;
        tokio::time::sleep(Duration::from_millis(10)).await;
        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 2);
        assert_eq!(reader.strikes(&address), 0);
    }

    /// One pool's trouble at the node costs that pool its books, not the others theirs, and at
    /// first not even a strike: the fault may be gone by the next poll.
    #[tokio::test]
    async fn a_pool_the_node_fails_drops_out_and_the_others_are_served() {
        let mut failing = fake::pool(0x05);
        failing.fault = Fault::QuotesUnanswered;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), failing]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(!reader.quarantined(&Address::repeat_byte(0x05), Instant::now()));

        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(reader.quarantined(&Address::repeat_byte(0x05), Instant::now()));
    }

    #[tokio::test]
    async fn a_pool_back_from_a_miss_is_served_with_no_strike() {
        let mut failing = fake::pool(0x05);
        failing.fault = Fault::QuotesUnanswered;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), failing]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        first_poll(&feed).await.unwrap();
        node.chain.lock().unwrap().pools[1].fault = Fault::None;
        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 2);

        node.chain.lock().unwrap().pools[1].fault = Fault::QuotesUnanswered;
        next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert!(!feed
            .source
            .reader
            .quarantined(&Address::repeat_byte(0x05), Instant::now()));
    }

    /// Most pools at once is the node's moment of trouble until it repeats: the poll fails, which
    /// keeps the last books. If the same pools miss the next poll, the fault is theirs.
    #[tokio::test]
    async fn most_pools_missing_fails_the_poll_until_they_miss_again() {
        let failing = |byte: u8| {
            let mut pool = fake::pool(byte);
            pool.fault = Fault::QuotesUnanswered;
            pool
        };
        let pools = vec![fake::pool(0x01), failing(0x05), failing(0x06)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert!(matches!(first_poll(&feed).await, Err(FeedError::Connection(_))));
        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
    }

    /// Nobody answered, so nobody is to blame: when the node is back every pool is too.
    #[tokio::test]
    async fn a_poll_no_pool_answers_sets_no_pool_aside() {
        let failing = |byte: u8| {
            let mut pool = fake::pool(byte);
            pool.fault = Fault::QuotesUnanswered;
            pool
        };
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![failing(0x05), failing(0x06)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        for _ in 0..3 {
            assert!(matches!(
                next_poll(&feed, &node, Some(&mut Catalogue::default())).await,
                Err(FeedError::Connection(_))
            ));
        }
        let reader = &feed.source.reader;
        assert!(!reader.quarantined(&Address::repeat_byte(0x05), Instant::now()));
        assert!(!reader.quarantined(&Address::repeat_byte(0x06), Instant::now()));
    }

    #[tokio::test]
    async fn a_pool_slow_to_quote_is_cut_off_and_the_others_served() {
        let mut heavy = fake::pool(0x05);
        heavy.fault = Fault::SlowNode(Duration::from_secs(60));
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), heavy]).await;
        let feed = builder(&node, config(0.0))
            .feed_config(HttpFeedConfig {
                request_timeout: Duration::from_millis(800),
                ..FlammFeedBuilder::default_feed_config(Chain::Base)
            })
            .build()
            .unwrap();
        feed.source
            .reader
            .discover(&mut Catalogue::default())
            .await;
        // the poll loop's own deadline, which must not be what ends it
        let snapshot = tokio::time::timeout(Duration::from_millis(800), feed.source.fetch())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
    }

    #[tokio::test]
    async fn a_call_the_node_fails_once_is_asked_again() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        node.chain
            .lock()
            .unwrap()
            .flaky_quote
            .store(true, Ordering::SeqCst);
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
    }

    fn faulty(byte: u8, fault: Fault) -> FakePool {
        let mut pool = fake::pool(byte);
        pool.fault = fault;
        pool
    }

    /// `count` pools at addresses of their own, each with `fault`.
    fn many_pools(count: u16, fault: Fault) -> Vec<FakePool> {
        (0..count)
            .map(|i| {
                let mut address = [0x7a; 20];
                address[18..].copy_from_slice(&i.to_be_bytes());
                FakePool { address: Address::from(address), ..faulty(0x01, fault) }
            })
            .collect()
    }

    /// How many sampler calls the node had for `pool`.
    fn samples(node: &FakeNode, pool: Address) -> usize {
        node.seen
            .lock()
            .unwrap()
            .samples_of(pool)
            .len()
    }

    /// The cap of each sampler call for `pool`, in order.
    fn caps_asked(node: &FakeNode, pool: Address) -> Vec<u64> {
        node.seen
            .lock()
            .unwrap()
            .samples_of(pool)
            .iter()
            .map(|sample| u64::try_from(sample.call.limits.probeGas).unwrap())
            .collect()
    }

    fn pool_of(book: &Book) -> Bytes {
        book.component.static_attributes[POOL_ATTRIBUTE].clone()
    }

    /// One call per pool, to the router, at the poll's block: the sampler's code put there by
    /// state override, no sender, the sampler's own gas, and a walk per direction of the hook pair
    /// with the hook's leverage labels, so that the sampler reads what they need.
    #[tokio::test]
    async fn a_pool_is_read_by_one_sampler_call_at_the_router_at_the_polls_block() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        first_poll(&feed).await.unwrap();

        let seen = node.seen.lock().unwrap();
        let [sample] = seen.samples.as_slice() else {
            panic!("one sampler call, not {}", seen.samples.len());
        };
        assert_eq!(Some(sample.to), router(Chain::Base));
        assert_eq!(sample.from, None);
        assert_eq!(sample.block, Some(BLOCK));
        assert_eq!(sample.gas, Some(DEFAULT_SAMPLE_GAS));
        assert_eq!(sample.code.as_deref(), Some(SAMPLER_RUNTIME));
        let call = &sample.call;
        assert_eq!(call.pool, Address::repeat_byte(0x01));
        assert_eq!(call.limits.probeGas, U256::from(DEFAULT_PROBE_GAS));
        assert_eq!(call.limits.reserveGas, U256::from(SAMPLER_RESERVE_GAS));
        let (eth, usd) = (fake::eth().address, fake::usd().address);
        let pairs: Vec<_> = call
            .asks
            .iter()
            .map(|ask| (ask.tokenIn, ask.tokenOut))
            .collect();
        assert_eq!(pairs, vec![(eth, usd), (usd, eth)]);
        for ask in &call.asks {
            assert_eq!((ask.hook, ask.tierA, ask.tierB), (HOOK, wad(TIER_UP), wad(TIER_DOWN)));
            assert_eq!(ask.maxProbes, U256::from(MAX_PROBES_PER_DIRECTION));
            assert!(!ask.seeds.is_empty());
            assert!(ask
                .seeds
                .windows(2)
                .all(|pair| pair[0] < pair[1]));
            assert!(ask
                .seeds
                .iter()
                .all(|seed| seed.bit_len() <= 120));
        }
        // every quote the pool answered came from the router, inside that call
        assert!(seen
            .quote_senders
            .iter()
            .all(|sender| *sender == router(Chain::Base)));
    }

    /// What a node's error to one pool's call costs: the pool leaves the snapshot with no strike
    /// and no miss, and is asked again only where one more call may answer otherwise. One pool of
    /// three the node does not run is one backend's trouble, not the node's.
    #[rstest]
    #[case::the_override_dropped(Fault::OverrideDropped, 2)]
    #[case::the_override_refused(Fault::OverrideRefused, 1)]
    #[case::not_the_samplers_answer(Fault::BadTag, 2)]
    #[case::a_block_the_node_lacks(Fault::NoBlock, 2)]
    #[case::an_answer_from_an_older_block(Fault::OlderBlock, 1)]
    #[case::a_rate_limit(Fault::RateLimited(Limit::Code(429)), 1)]
    #[tokio::test]
    async fn a_nodes_error_for_one_pool_is_asked_again_only_where_it_may_answer_otherwise(
        #[case] fault: Fault,
        #[case] calls: usize,
    ) {
        let failing = Address::repeat_byte(0x05);
        let pools = vec![fake::pool(0x01), fake::pool(0x02), faulty(0x05, fault)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let mut catalogue = Catalogue::default();
        for poll in 1..=2 {
            let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap();
            assert_eq!(snapshot.books.len(), 2);
            assert_eq!(samples(&node, failing), calls * poll);
            assert_eq!(reader.strikes(&failing), 0);
        }
        // a miss is the one class that counts against a pool, and none of these is one
        assert!(reader.missed_last().is_empty());
    }

    /// A node that does not run the sampler fails the poll, which keeps the last books, and
    /// blames no pool. A revert is the router's own code, so the node dropped the override, and
    /// is asked again once in case only one backend of several did; a refusal is not.
    #[rstest]
    #[case::the_override_dropped(Fault::OverrideDropped, 2)]
    #[case::the_override_refused(Fault::OverrideRefused, 1)]
    #[case::not_the_samplers_answer(Fault::BadTag, 2)]
    #[tokio::test]
    async fn a_node_that_does_not_run_the_sampler_fails_the_poll_and_blames_no_pool(
        #[case] fault: Fault,
        #[case] calls: usize,
    ) {
        let pools = vec![faulty(0x01, fault), faulty(0x02, fault), fake::pool(0x03)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let mut catalogue = Catalogue::default();
        for poll in 1..=2 {
            let error = next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap_err();
            assert!(
                matches!(error, FeedError::Connection(ref m) if m.contains("did not run the sampler")),
                "{error:?}"
            );
            for byte in [0x01, 0x02] {
                let pool = Address::repeat_byte(byte);
                assert_eq!(samples(&node, pool), calls * poll);
                assert_eq!(reader.strikes(&pool), 0);
            }
        }
    }

    /// A node that gives the call too little gas for one probe at the default cap did not run it
    /// as asked, which no pool is to blame for.
    #[tokio::test]
    async fn a_node_that_gives_no_gas_for_a_probe_fails_the_poll() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        node.chain.lock().unwrap().gas_cap = Some(DEFAULT_PROBE_GAS);
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let error = first_poll(&feed).await.unwrap_err();
        assert!(
            matches!(error, FeedError::Connection(ref m) if m.contains("did not run the sampler")),
            "{error:?}"
        );
        assert_eq!(samples(&node, Address::repeat_byte(0x01)), 1);
        assert_eq!(
            feed.source
                .reader
                .strikes(&Address::repeat_byte(0x01)),
            0
        );
    }

    /// A rate limit, in every form a node sends one, is the node's leave and no pool's fault: the
    /// pool leaves the snapshot with no strike however often, and is not asked again in the poll.
    /// Where no pool answered, the poll fails, which keeps the last books.
    #[rstest]
    #[case::code_429(Limit::Code(429))]
    #[case::code_32005(Limit::Code(-32005))]
    #[case::code_32007(Limit::Code(-32007))]
    #[case::code_32012(Limit::Code(-32012))]
    #[case::code_32016(Limit::Code(-32016))]
    #[case::code_32055(Limit::Code(-32055))]
    #[case::http_429(Limit::Http(429))]
    #[case::http_503(Limit::Http(503))]
    #[case::a_rate_limit(Limit::Wording("Rate limit exceeded"))]
    #[case::a_rate_exceeded(Limit::Wording("request rate exceeded"))]
    #[case::too_many_requests(Limit::Wording("Too Many Requests"))]
    #[case::credits_limited(Limit::Wording("your credits limited"))]
    #[case::a_request_limit(Limit::Wording("daily request limit hit"))]
    #[case::a_limit_reached(Limit::Wording("limit reached"))]
    #[case::concurrency(Limit::Wording("maximum number of concurrent requests exceeded"))]
    #[case::a_null_answer(Limit::Null)]
    #[tokio::test]
    async fn a_rate_limit_in_any_form_is_no_strike(#[case] limit: Limit) {
        let limited = Address::repeat_byte(0x05);
        let pools = vec![fake::pool(0x01), faulty(0x05, Fault::RateLimited(limit))];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let mut catalogue = Catalogue::default();
        for poll in 1..=3 {
            let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap();
            assert_eq!(snapshot.books.len(), 1);
            assert_eq!(samples(&node, limited), poll);
            assert_eq!(reader.strikes(&limited), 0);
            assert!(reader.missed_last().is_empty());
        }

        node.chain.lock().unwrap().pools[0].fault = Fault::RateLimited(limit);
        let error = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap_err();
        assert!(matches!(error, FeedError::Connection(_)), "{error:?}");
        assert_eq!(reader.strikes(&Address::repeat_byte(0x01)), 0);
    }

    /// A pool with no read this poll leaves the snapshot: its last books are not carried. A poll
    /// short of a pool is not served again for its block, so the next poll asks the pool anew.
    #[tokio::test]
    async fn a_skipped_pool_leaves_the_snapshot_and_its_block_is_read_again() {
        let skipped = Address::repeat_byte(0x05);
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), fake::pool(0x05)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert_eq!(
            first_poll(&feed)
                .await
                .unwrap()
                .books
                .len(),
            2
        );

        node.chain.lock().unwrap().pools[1].fault = Fault::RateLimited(Limit::Code(429));
        let short = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(short.books.len(), 1);
        assert!(short
            .books
            .values()
            .all(|book| pool_of(book) != bytes(skipped)));

        // the same block, read again, in full this time
        node.chain.lock().unwrap().pools[1].fault = Fault::None;
        let full = feed.source.fetch().await.unwrap();
        assert_eq!(full.books.len(), 2);
        assert_eq!(samples(&node, skipped), 3);
        // and served again for its block without a call
        let asked = node.server.request_count();
        let again = feed.source.fetch().await.unwrap();
        assert!(Arc::ptr_eq(&full.books, &again.books));
        assert_eq!(node.server.request_count(), asked + 1);
    }

    /// A pool whose first answer ran short of its cap says nothing of itself: it leaves the
    /// snapshot with no miss, and its cap is doubled for the next poll. Once an answer's probes
    /// all fit well under the default, the cap is the default again.
    #[tokio::test]
    async fn a_pool_short_of_its_cap_is_asked_with_twice_as_much_and_then_the_default_again() {
        let heavy = Address::repeat_byte(0x05);
        // a quote of 6 million gas: over 7/8 of 5 million, under 7/8 of 10 million
        let pools = vec![fake::pool(0x01), faulty(0x05, Fault::Starved(6_000_000))];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        assert_eq!(
            first_poll(&feed)
                .await
                .unwrap()
                .books
                .len(),
            1
        );
        assert!(reader.missed_last().is_empty());
        assert_eq!(reader.probe_cap(&heavy), 10_000_000);
        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 2);
        // its quotes still need more than the default
        assert_eq!(reader.probe_cap(&heavy), 10_000_000);

        node.chain.lock().unwrap().pools[1].fault = Fault::None;
        let snapshot = next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 2);
        assert_eq!(reader.probe_cap(&heavy), DEFAULT_PROBE_GAS);
        next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(caps_asked(&node, heavy), vec![5_000_000, 10_000_000, 10_000_000, 5_000_000]);
        assert_eq!(reader.strikes(&heavy), 0);
    }

    /// At the most cap, an answer short of gas is a miss, and a second in a row sets the pool
    /// aside.
    #[tokio::test]
    async fn a_pool_short_of_the_most_cap_misses() {
        let heavy = Address::repeat_byte(0x05);
        let pools = vec![fake::pool(0x01), faulty(0x05, Fault::Starved(30_000_000))];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        first_poll(&feed).await.unwrap();
        next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert!(reader.missed_last().is_empty());
        next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert_eq!(reader.missed_last(), HashSet::from([heavy]));
        assert_eq!(reader.strikes(&heavy), 0);
        next_poll(&feed, &node, None)
            .await
            .unwrap();
        assert!(reader.quarantined(&heavy, Instant::now()));
        assert_eq!(caps_asked(&node, heavy), vec![5_000_000, 10_000_000, 20_000_000, 20_000_000]);
    }

    /// A node may give a call less gas than it asks for. Where that leaves no probe at a raised
    /// cap, the cap goes back to the default and the pool is asked again at once, which is
    /// neither the node's failure nor the pool's; the other pools' walks end where the gas does,
    /// and are served as far as they reached.
    #[tokio::test]
    async fn a_raised_cap_the_node_gives_no_gas_for_is_put_back_and_asked_again() {
        let heavy = Address::repeat_byte(0x05);
        let pools = vec![fake::pool(0x01), faulty(0x05, Fault::Starved(6_000_000))];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        // enough for a probe at the default cap, and not at twice it
        node.chain.lock().unwrap().gas_cap = Some(8_000_000);
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let mut catalogue = Catalogue::default();
        for _ in 0..3 {
            let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap();
            let books: Vec<&Book> = snapshot.books.values().collect();
            let [book] = books.as_slice() else {
                panic!("one book, not {}", books.len());
            };
            // 81.92 ETH would be clipped to 50; the walk ends nine rungs in, at 1.28 ETH
            assert_eq!(
                state(book)
                    .sell_base
                    .last()
                    .unwrap()
                    .input,
                U256::from(128u64) * U256::from(10u64).pow(U256::from(16))
            );
            assert_eq!(reader.strikes(&heavy), 0);
            assert!(reader.missed_last().is_empty());
        }
        assert_eq!(
            caps_asked(&node, heavy),
            vec![5_000_000, 10_000_000, 5_000_000, 10_000_000, 5_000_000]
        );
    }

    /// A Venus Core pool on BSC whose probes paying past custody read its markets and run short of
    /// the cap: the sale stops there, publishing only what custody pays, and the pool stays served
    /// with no miss, its cap doubled. At twice the cap the sale reaches past custody. No rung
    /// quotes over the pool's own answer.
    #[tokio::test]
    async fn a_venus_walk_starved_past_custody_publishes_custody_and_doubles_the_cap() {
        let mut pool = fake::pool(0x05);
        pool.venue_kinds = vec![2];
        pool.tokens[0].held = ether(100);
        pool.tokens[1].held = U256::from(100_000_000u64);
        pool.fault = Fault::StarvedPastCustody(6_000_000);
        let held = pool.tokens[1].held;
        let node = FakeNode::spawn(56, 0, vec![pool.clone()]).await;
        let feed = builder(&node, BookFeedConfig { chain: Chain::Bsc, ..config(0.0) })
            .gas_table(table_serving_on(hook_class(&HOOK_CODE), 56, &[1, 2, 3]))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let sale = |snapshot: &BookSnapshot<ReceivedAt>| {
            let books: Vec<&Book> = snapshot.books.values().collect();
            let [book] = books.as_slice() else {
                panic!("one book, not {}", books.len());
            };
            let sale = state(book).sell_base.clone();
            for rung in sale.rungs() {
                let paid = pool
                    .quote(fake::eth().address, fake::usd().address, rung.input)
                    .unwrap()
                    .1;
                assert!(rung.output <= paid, "{rung:?} over the pool's {paid}");
            }
            sale.last().unwrap().output
        };

        let starved = sale(&first_poll(&feed).await.unwrap());
        assert!(starved <= held);
        assert!(reader.missed_last().is_empty());
        assert_eq!(reader.probe_cap(&pool.address), 10_000_000);

        let full = sale(
            &next_poll(&feed, &node, None)
                .await
                .unwrap(),
        );
        assert!(full > held);
        assert_eq!(caps_asked(&node, pool.address), vec![5_000_000, 10_000_000]);
        assert_eq!(reader.strikes(&pool.address), 0);
    }

    /// An answer that cannot be a fill, first in a direction where the pool refused nothing, is
    /// the pool's: it is set aside and the others are served.
    #[tokio::test]
    async fn a_pool_whose_first_answer_cannot_be_a_fill_is_set_aside() {
        let broken = Address::repeat_byte(0x05);
        let pools = vec![fake::pool(0x01), faulty(0x05, Fault::Malformed)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert_eq!(
            first_poll(&feed)
                .await
                .unwrap()
                .books
                .len(),
            1
        );
        assert!(feed
            .source
            .reader
            .quarantined(&broken, Instant::now()));
    }

    /// Where every pool asked is set aside, or none is left to ask, the poll serves an empty book
    /// at once, so the books of the pools set aside come down.
    #[tokio::test]
    async fn a_poll_with_no_pool_left_to_serve_serves_an_empty_book() {
        let pools = vec![faulty(0x05, Fault::Malformed), faulty(0x06, Fault::Malformed)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert!(first_poll(&feed)
            .await
            .unwrap()
            .books
            .is_empty());
        let asked = node.seen.lock().unwrap().samples.len();
        assert_eq!(asked, 2);
        assert!(next_poll(&feed, &node, None)
            .await
            .unwrap()
            .books
            .is_empty());
        assert_eq!(node.seen.lock().unwrap().samples.len(), asked);
    }

    /// A pool whose turn comes at the poll's deadline is skipped before its call, as a waiter can
    /// be handed a permit there and a call started then could only overrun into a miss.
    #[tokio::test]
    async fn a_turn_at_the_deadline_starts_no_call() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), fake::pool(0x02)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        reader
            .discover(&mut Catalogue::default())
            .await;
        let Err(error) = reader
            .read(BLOCK, tokio::time::Instant::now())
            .await
        else {
            panic!("a read at its deadline served books");
        };
        assert!(matches!(error, FeedError::Connection(_)), "{error:?}");
        assert!(node
            .seen
            .lock()
            .unwrap()
            .samples
            .is_empty());
        assert!(reader.missed_last().is_empty());
    }

    /// More pools than a poll's deadline holds on a slow node: each poll starts past the last
    /// one's answers, so every pool takes its turn; a pool given no turn is no miss, so none is
    /// set aside; and no call reaches the node past the deadline, beyond the time a request
    /// takes to arrive.
    #[tokio::test]
    async fn many_pools_on_a_slow_node_take_turns_and_none_is_set_aside() {
        const POOLS: u16 = 200;
        let pools = many_pools(POOLS, Fault::SlowNode(Duration::from_millis(100)));
        let addresses: Vec<Address> = pools
            .iter()
            .map(|pool| pool.address)
            .collect();
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .feed_config(HttpFeedConfig {
                request_timeout: Duration::from_millis(800),
                ..FlammFeedBuilder::default_feed_config(Chain::Base)
            })
            .build()
            .unwrap();
        let deadline = Duration::from_millis(600);
        let reader = &feed.source.reader;
        reader
            .discover(&mut Catalogue::default())
            .await;
        let mut served = HashSet::new();
        for _ in 0..3 {
            let before = node.seen.lock().unwrap().samples.len();
            let started = Instant::now();
            let snapshot = next_poll(&feed, &node, None)
                .await
                .unwrap();
            assert!(!snapshot.books.is_empty());
            assert!(snapshot.books.len() < usize::from(POOLS));
            served.extend(snapshot.books.values().map(pool_of));

            // a miss is a call in flight at the deadline, one a permit at most, and never a pool
            // that got no turn
            assert!(reader.missed_last().len() <= 24);
            let late = started + deadline + Duration::from_millis(100);
            let seen = node.seen.lock().unwrap();
            assert!(seen.samples[before..]
                .iter()
                .all(|sample| sample.at < late));
        }
        assert_eq!(served.len(), usize::from(POOLS));
        assert!(addresses
            .iter()
            .all(|pool| reader.strikes(pool) == 0));
    }

    /// Discovery has calls of its own, so a poll holding every one of the polls' does not hold up
    /// a round. A round of known codes and rows makes 17 calls a pool of one venue, 8 of them the
    /// pool's own views, and reads the factory's length once.
    #[tokio::test]
    async fn discovery_keeps_its_own_calls_beside_a_full_poll() {
        const POOLS: u16 = 30;
        let pools = many_pools(POOLS, Fault::SlowNode(Duration::from_secs(2)));
        let first = pools[0].address;
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .feed_config(HttpFeedConfig {
                request_timeout: Duration::from_secs(4),
                ..FlammFeedBuilder::default_feed_config(Chain::Base)
            })
            .build()
            .unwrap();
        let reader = Arc::clone(&feed.source.reader);
        reader
            .discover(&mut Catalogue::default())
            .await;
        let poll = tokio::spawn(async move { feed.source.fetch().await.map(|_| ()) });
        // the poll's first 24 calls take every one of its permits for two seconds
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(node.seen.lock().unwrap().samples.len(), 24);

        let (asked, views) = (node.server.request_count(), node.seen.lock().unwrap().views[&first]);
        tokio::time::timeout(Duration::from_secs(1), reader.discover(&mut Catalogue::default()))
            .await
            .unwrap();
        assert!(!poll.is_finished());
        assert_eq!(node.seen.lock().unwrap().views[&first] - views, 8);
        assert_eq!(node.server.request_count() - asked, 1 + 17 * usize::from(POOLS));
        poll.abort();
    }

    /// Two books of one pool would both promise its inventory, as no book moves another. Every
    /// pair here is tradable and every token in the universe: the hook pair is what decides.
    #[tokio::test]
    async fn a_pool_of_three_tokens_is_served_its_hook_pair_only() {
        let mut pool = fake::pool(0x01);
        pool.tokens.push(fake::tether());
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();

        assert_eq!(snapshot.books.len(), 1);
        let book = snapshot.books.values().next().unwrap();
        let (eth, usd) = (fake::eth().address, fake::usd().address);
        let id = [Address::repeat_byte(0x01).as_slice(), eth.as_slice(), usd.as_slice()].concat();
        assert_eq!(book.component.id, Bytes::from(id));
        let seen = node.seen.lock().unwrap();
        assert!(!seen.quoted_pairs.is_empty());
        assert!(seen
            .quoted_pairs
            .iter()
            .all(|pair| *pair == (eth, usd) || *pair == (usd, eth)));
    }

    /// The third token's legs still count: the pool is worth what it holds of all of them.
    #[tokio::test]
    async fn a_pool_of_three_tokens_is_valued_at_all_its_legs() {
        let mut pool = fake::pool(0x01);
        pool.tokens[0].usd_wad = None;
        let mut tether = fake::tether();
        tether.held = U256::from(60_000_000_000u64);
        pool.tokens.push(tether);
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(50_000.0))
            .build()
            .unwrap();
        assert_eq!(
            first_poll(&feed)
                .await
                .unwrap()
                .books
                .len(),
            1
        );
    }

    /// No knob serves a pool a front prices: not the one that once did, not a listed class, not a
    /// name.
    #[rstest]
    #[case::a_lane_on_pool(HOOK, true)]
    #[case::a_listed_front(FRONT, false)]
    #[case::a_listed_front_with_its_lane_on(FRONT, true)]
    #[tokio::test]
    async fn a_listed_front_is_refused_whatever_the_knobs_say(
        #[case] hook: Address,
        #[case] lane_on: bool,
    ) {
        let mut front = fake::pool(0x04);
        front.hook = hook;
        front.lane_on = lane_on;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![front, fake::pool(0x01)]).await;
        let front_class = (hook_class(&fake::code_at(FRONT)), HookShape::Everlong);
        let knobs = [
            builder(&node, config(0.0)).serve_front_priced(true),
            builder(&node, config(0.0)).hook_classes(vec![listed_hook(), front_class]),
            builder(&node, config(0.0))
                .pools(vec![bytes(Address::repeat_byte(0x04)), bytes(Address::repeat_byte(0x01))]),
        ];
        for knob in knobs {
            let feed = knob.build().unwrap();
            let snapshot = first_poll(&feed).await.unwrap();
            assert_eq!(snapshot.books.len(), 1);
        }
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x01)]);
    }

    /// Anyone can create a pool with a hook of their own, whose quote need not be what it fills.
    #[tokio::test]
    async fn a_pool_priced_by_an_unlisted_hook_is_not_served() {
        let mut rogue = fake::pool(0x09);
        rogue.hook = ROGUE_HOOK;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![rogue, fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x01)]);
    }

    /// A pool's feed prices its floor and, through its hook, its fills: an honest hook over a
    /// hostile feed is not served either.
    #[tokio::test]
    async fn a_pool_on_an_unlisted_feed_is_not_served() {
        let mut rogue = fake::pool(0x09);
        rogue.feed = ROGUE_FEED;
        let mut feedless = fake::pool(0x0a);
        feedless.feed = Address::ZERO;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![rogue, feedless, fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .price_feed_code_hashes(vec![keccak256(FEED_CODE)])
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x01)]);
    }

    #[tokio::test]
    async fn a_feed_is_admitted_by_the_hash_of_its_code() {
        let mut twin = fake::pool(0x0b);
        twin.feed = TWIN_FEED;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![twin]).await;
        let by_address = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert!(first_poll(&by_address)
            .await
            .unwrap()
            .books
            .is_empty());
        let by_code = builder(&node, config(0.0))
            .price_feed_code_hashes(vec![keccak256(FEED_CODE)])
            .build()
            .unwrap();
        assert_eq!(
            first_poll(&by_code)
                .await
                .unwrap()
                .books
                .len(),
            1
        );
    }

    /// A front its factory does not list is a hook of no listed class, so it is not served either,
    /// whatever its lane says.
    #[rstest]
    #[case::its_lane_off(false)]
    #[case::its_lane_on(true)]
    #[tokio::test]
    async fn a_front_the_factory_does_not_list_is_a_hook_of_no_listed_class(#[case] lane_on: bool) {
        let mut pool = fake::pool(0x0d);
        pool.hook = UNLISTED_FRONT;
        pool.lane_on = lane_on;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool, fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x01)]);
    }

    /// A table serving the class of [`ROGUE_HOOK`]'s code.
    fn rogue_table() -> GasTable {
        table_serving(hook_class(&fake::code_at(ROGUE_HOOK)))
    }

    /// `pools` is the opt-in: the caller vouches for these, and the lists are not asked. A hook of
    /// no listed class that shows no leverage row is quoted by steps, each span at its lower rung,
    /// where the gas table charges its class.
    #[tokio::test]
    async fn a_named_pool_is_served_whatever_its_hook_and_feed() {
        let mut named = fake::pool(0x0e);
        named.hook = ROGUE_HOOK;
        named.feed = ROGUE_FEED;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![named, fake::pool(0x01)]).await;
        let feed = unlisted_builder(&node, config(0.0))
            .pools(vec![bytes(Address::repeat_byte(0x0e))])
            .gas_table(rogue_table())
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x0e)]);
        let book = snapshot.books.values().next().unwrap();
        let rungs = state(book).sell_base.rungs();
        assert!(rungs.len() > 1);
        assert!(rungs[1..]
            .iter()
            .all(|rung| rung.segment == Segment::Flat));
    }

    /// Naming a pool does not charge its swaps: one whose hook's class has no cells in the gas
    /// table is not served.
    #[tokio::test]
    async fn a_named_pool_of_a_class_the_table_cannot_charge_is_not_served() {
        let mut named = fake::pool(0x0e);
        named.hook = ROGUE_HOOK;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![named]).await;
        let feed = unlisted_builder(&node, config(0.0))
            .pools(vec![bytes(Address::repeat_byte(0x0e))])
            .build()
            .unwrap();
        assert!(first_poll(&feed)
            .await
            .unwrap()
            .books
            .is_empty());
        assert!(pools_quoted(&node).is_empty());
    }

    /// No listed shape says how the leverage answers of a hook of no listed class fall, so a
    /// named pool whose hook shows a leverage row is not served.
    #[tokio::test]
    async fn a_named_pool_whose_hook_shows_a_leverage_row_of_no_listed_shape_is_not_served() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = unlisted_builder(&node, config(0.0))
            .pools(vec![bytes(Address::repeat_byte(0x01))])
            .build()
            .unwrap();
        assert!(first_poll(&feed)
            .await
            .unwrap()
            .books
            .is_empty());
        assert!(pools_quoted(&node).is_empty());
    }

    /// A hook of no listed class that refuses `params()` shows no row this round only: the
    /// refusal is never kept, so the next round asks again.
    #[tokio::test]
    async fn a_named_pool_whose_hook_refuses_its_row_is_quoted_by_steps_and_asked_again() {
        let mut named = fake::pool(0x0e);
        named.hook = ROGUE_HOOK;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![named]).await;
        node.chain
            .lock()
            .unwrap()
            .hooks
            .get_mut(&ROGUE_HOOK)
            .unwrap()
            .params = None;
        let feed = unlisted_builder(&node, config(0.0))
            .pools(vec![bytes(Address::repeat_byte(0x0e))])
            .gas_table(rogue_table())
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
        let reads = || node.seen.lock().unwrap().params_reads[&ROGUE_HOOK];
        // asked once more on the refusal
        assert_eq!(reads(), 2);
        next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(reads(), 4);
    }

    /// A listed class's code answers `params()`, so a refusal is the node's: the pool is unread,
    /// with no strike, and nothing is served until its row reads. A row read is kept for good.
    #[tokio::test]
    async fn a_listed_hook_whose_row_is_refused_leaves_its_pool_unread() {
        let address = Address::repeat_byte(0x01);
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let set_hook = |hook: FakeHook| {
            node.chain
                .lock()
                .unwrap()
                .hooks
                .insert(HOOK, hook);
        };
        set_hook(FakeHook { params: None, ..FakeHook::levered() });
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let mut catalogue = Catalogue::default();
        let error = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap_err();
        assert!(matches!(error, FeedError::Connection(_)), "{error:?}");
        assert!(!reader.quarantined(&address, Instant::now()));
        assert_eq!(reader.strikes(&address), 0);

        set_hook(FakeHook::levered());
        let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
        next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        // the refusal, asked again once, then the one read that is kept
        assert_eq!(node.seen.lock().unwrap().params_reads[&HOOK], 3);
    }

    /// A hook may keep its tuning row in a parameter record that a keeper's `setTuning`
    /// publishes (an idle-entry hook: the record's address in slot 13's low 160 bits, zero until
    /// a `setTuning` lands, and always on a locked hook, whose row in force is then the
    /// construction's, from code). The feed reads neither the record nor `tuning()`: it reads
    /// `params()`, whose leverage words 27-34 a record never moves, and the hook's answers to
    /// `quotePair` and `levState`, which price on the row in force. So with no record, a record
    /// that lands between two polls, or one that landed before the first, the feed reads the row
    /// once and publishes what it publishes for a hook whose tuning words never move, with no
    /// more storage read and no other call to the hook.
    #[tokio::test]
    async fn a_hooks_parameter_record_moves_no_read_of_the_feed() {
        let baseline = record_polls(None, None).await;
        for (first, second) in [(Some(1), Some(1)), (Some(1), Some(2)), (Some(2), Some(2))] {
            let got = record_polls(first, second).await;
            assert_eq!(got.0, baseline.0, "the books, tuning words {first:?} then {second:?}");
            assert_eq!(got.1, 1, "params() read once");
            assert_eq!(got.2, baseline.2, "no more storage read");
            assert_eq!(got.3, 0, "no other call to the hook");
        }
    }

    /// Two polls of one pool whose hook answers `params()` with tuning words of `first`, then of
    /// `second` (`None`: zero, else distinct words of that seed), its leverage row the levered one
    /// throughout: the states published, the `params()` reads, the storage reads and the other
    /// calls to the hook.
    async fn record_polls(
        first: Option<u64>,
        second: Option<u64>,
    ) -> ([Vec<FlammPairState>; 2], usize, usize, usize) {
        let hook = |seed: Option<u64>| {
            let mut hook = FakeHook::levered();
            if let (Some(seed), Some(params)) = (seed, hook.params.as_mut()) {
                for (k, word) in params[..27].iter_mut().enumerate() {
                    *word = U256::from(seed * 1_000_000 + k as u64 + 1);
                }
            }
            hook
        };
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        node.chain
            .lock()
            .unwrap()
            .hooks
            .insert(HOOK, hook(first));
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        let mut states = Vec::new();
        for seed in [first, second] {
            node.chain
                .lock()
                .unwrap()
                .hooks
                .insert(HOOK, hook(seed));
            let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap();
            assert_eq!(snapshot.books.len(), 1);
            states.push(
                snapshot
                    .books
                    .values()
                    .map(|book| state(book).clone())
                    .collect::<Vec<_>>(),
            );
        }
        let seen = node.seen.lock().unwrap();
        let second_states = states.pop().unwrap();
        let first_states = states.pop().unwrap();
        (
            [first_states, second_states],
            seen.params_reads[&HOOK],
            seen.storage_reads,
            seen.other_hook_calls,
        )
    }

    /// Where answers carry a leverage label, the direction reads the pool's request once, as the
    /// router, at the first such answer that pays more than dust; the hook's curve fee with the
    /// same words, the input zeroed, where every answer carries that one label; and, last, one
    /// quote just above its top. A sale reads no bid.
    #[tokio::test]
    async fn a_tier_labelled_sale_reads_its_request_and_curve_fee_once_and_asks_above_its_top() {
        let mut pool = fake::pool(0x01);
        pool.labels[0] = vec![(U256::ZERO, wad(TIER_UP))];
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();

        let seen = node.seen.lock().unwrap();
        let [request] = seen.pair_requests.as_slice() else {
            panic!("one request, not {:?}", seen.pair_requests);
        };
        assert_eq!(request.token_in, fake::eth().address);
        assert_eq!(request.amount, U256::from(5_000_000_000_000_000u64));
        assert_eq!(request.from, router(Chain::Base));
        let [fee_read] = seen.hook_reads.as_slice() else {
            panic!("one hook read, not {:?}", seen.hook_reads);
        };
        assert_eq!((fee_read.hook, fee_read.selector), (HOOK, QUOTE_PAIR_SELECTOR));
        assert_eq!(fee_read.from, router(Chain::Base));
        let mut at_zero = request.words;
        at_zero[13] = U256::ZERO;
        assert_eq!(fee_read.words, at_zero.to_vec());
        // 81.92 ETH is clipped to the 50 ETH cap, so the top priced answer is 40.96 ETH's
        let top = ether(4_096) / U256::from(100);
        let above = top + (top >> 10) + U256::from(1);
        assert_eq!(
            seen.quoted_amounts
                .iter()
                .filter(|amount| **amount == above)
                .count(),
            1
        );
        drop(seen);

        // the curve's fee reads apart from the tier, so every span is a chord; the clip, with no
        // priced answer above it, goes
        let book = snapshot.books.values().next().unwrap();
        let sell = &state(book).sell_base;
        assert_eq!(sell.last().unwrap().input, top);
        assert!(sell
            .rungs()
            .iter()
            .all(|rung| rung.segment == Segment::Chord));
    }

    /// A pool's venue lends its token 1 and its account holds a partial repay at a minimum
    /// position. The sale reads its legs, as its answers carry a leverage label: 100 USD held, so
    /// payouts past it and under 100 USD and the minimum of 50 USD with its margin can refuse.
    /// Read, the minimum keeps the rungs from the first paying 150.5 USD, 0.08 ETH's; unread or
    /// not ok, the rungs whose payouts and parts fit custody, to 0.04 ETH's 79.92 USD. The buy,
    /// whose output no such venue lends, keeps its ladder.
    #[rstest]
    #[case::read(Some((true, U256::from(50_000_000u64))), Some(80_000_000_000_000_000), None)]
    #[case::unread(None, None, Some(40_000_000_000_000_000))]
    #[case::not_ok(Some((false, U256::from(50_000_000u64))), None, Some(40_000_000_000_000_000))]
    #[tokio::test]
    async fn a_sale_drawing_on_a_minimum_position_publishes_only_what_passes_it(
        #[case] minimum: Option<(bool, U256)>,
        #[case] first: Option<u64>,
        #[case] last: Option<u64>,
    ) {
        let unguarded = {
            let mut pool = fake::pool(0x01);
            pool.labels[0] = vec![(U256::ZERO, wad(TIER_UP))];
            pool.tokens[1].held = U256::from(100_000_000u64);
            pool
        };
        let mut pool = unguarded.clone();
        pool.venue_capabilities = Some(560);
        pool.venue_minimum = minimum;
        let books = |pool: FakePool| async move {
            let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
            let feed = builder(&node, config(0.0))
                .build()
                .unwrap();
            let snapshot = first_poll(&feed).await.unwrap();
            let book = snapshot.books.values().next().unwrap();
            (state(book).sell_base.clone(), state(book).sell_quote.clone())
        };
        let (sale, buy) = books(pool).await;
        let (whole_sale, whole_buy) = books(unguarded).await;
        assert_eq!(buy, whole_buy);
        let first = first.map_or_else(|| whole_sale.first().unwrap().input, U256::from);
        let last = last.map_or_else(|| whole_sale.last().unwrap().input, U256::from);
        let kept: Vec<Rung> = whole_sale
            .rungs()
            .iter()
            .filter(|rung| rung.input >= first && rung.input <= last)
            .enumerate()
            .map(|(k, rung)| if k == 0 { Rung { segment: Segment::Chord, ..*rung } } else { *rung })
            .collect();
        assert!(kept.len() < whole_sale.rungs().len());
        assert_eq!(sale.rungs(), kept.as_slice());
    }

    /// Where no walk of the pair read its legs, the sale that draws on a minimum position is not
    /// published, and the buy is; an account that does not say what it holds leaves its venue's
    /// record unread, and the pool unserved.
    #[rstest]
    #[case::says_it_holds_one(Some(560), Some(1))]
    #[case::says_nothing(None, None)]
    #[tokio::test]
    async fn a_sale_drawing_on_a_minimum_position_needs_its_legs(
        #[case] capabilities: Option<u64>,
        #[case] books: Option<usize>,
    ) {
        let mut pool = fake::pool(0x01);
        pool.venue_capabilities = capabilities;
        pool.venue_minimum = Some((true, U256::from(50_000_000u64)));
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(Some(snapshot.books.len()).filter(|n| *n > 0), books);
        if let Some(book) = snapshot.books.values().next() {
            assert!(state(book).sell_base.is_empty());
            assert!(!state(book).sell_quote.is_empty());
        }
    }

    /// A buy also reads the hook's top bid, with the very words of its request; where the bid
    /// reads zero the buy's leverage answers go.
    #[rstest]
    #[case::a_bid(wad(1_000_000_000_000_000_000), false)]
    #[case::no_bid(U256::ZERO, true)]
    #[tokio::test]
    async fn a_tier_labelled_buy_reads_the_hooks_bid_with_its_request(
        #[case] bid: U256,
        #[case] dark: bool,
    ) {
        let mut pool = fake::pool(0x01);
        pool.tokens[0].held = ether(100);
        pool.labels[1] = vec![(U256::ZERO, wad(TIER_DOWN))];
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        node.chain
            .lock()
            .unwrap()
            .hooks
            .insert(HOOK, FakeHook { bid, ..FakeHook::levered() });
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();

        let seen = node.seen.lock().unwrap();
        let [request] = seen.pair_requests.as_slice() else {
            panic!("one request, not {:?}", seen.pair_requests);
        };
        assert_eq!(request.token_in, fake::usd().address);
        // the input is token 1
        assert_eq!(request.words[3], U256::from(1));
        let bid_reads: Vec<_> = seen
            .hook_reads
            .iter()
            .filter(|read| read.selector == LEV_STATE_SELECTOR)
            .collect();
        let [bid_read] = bid_reads.as_slice() else {
            panic!("one bid read, not {bid_reads:?}");
        };
        assert_eq!(bid_read.words, request.words.to_vec());
        drop(seen);

        let book = snapshot.books.values().next().unwrap();
        assert_eq!(state(book).sell_quote.is_empty(), dark);
        assert!(!state(book).sell_base.is_empty());
    }

    /// The request is read only for a leverage label on an answer that pays more than dust:
    /// the rule drops dust, and with it any label it carries.
    #[tokio::test]
    async fn a_tier_label_on_dust_alone_reads_no_request() {
        let mut pool = fake::pool(0x01);
        // 0.001 USD of ether pays 999 units, and the first answer over dust is 0.016 USD's
        pool.labels[0] =
            vec![(U256::ZERO, wad(TIER_UP)), (U256::from(8_000_000_000_000u64), wad(CURVE_FEE))];
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(0.0))
            .rungs(20, 0.001)
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        let seen = node.seen.lock().unwrap();
        assert!(seen.pair_requests.is_empty(), "{:?}", seen.pair_requests);
        assert!(seen.hook_reads.is_empty(), "{:?}", seen.hook_reads);
        drop(seen);
        let book = snapshot.books.values().next().unwrap();
        let first = state(book).sell_base.first().unwrap();
        assert!(first.output >= U256::from(DUST_OUT));
    }

    /// Leverage answers stand only with the words that test them and on a pool trusted with
    /// them: two tokens, and a row whose caps fit the band the pair trades at now. Otherwise the
    /// ladder ends before its first leverage answer, here past 1 ETH.
    #[rstest]
    #[case::a_request_refused(Fault::RequestRefused, Vec::new(), fake::BAND)]
    #[case::a_third_token(Fault::None, vec![fake::tether()], fake::BAND)]
    #[case::a_band_the_rows_caps_do_not_fit(Fault::None, Vec::new(), 10_000_000_000_000_000)]
    #[tokio::test]
    async fn a_ladder_ends_before_leverage_answers_it_cannot_test(
        #[case] fault: Fault,
        #[case] more_tokens: Vec<fake::FakeToken>,
        #[case] band_wad: u64,
    ) {
        let mut pool = fake::pool(0x01);
        pool.fault = fault;
        pool.tokens.extend(more_tokens);
        pool.band_wad = band_wad;
        pool.labels[0] = vec![(ether(1), wad(TIER_UP))];
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        let book = snapshot.books.values().next().unwrap();
        let last = state(book).sell_base.last().unwrap();
        assert!(last.input < ether(1), "{last:?}");
    }

    /// The array is read from storage: `pools()` is one call whose gas grows with it, and the
    /// node here does not even answer it.
    #[tokio::test]
    async fn a_factory_is_read_a_page_at_a_time() {
        let pools = vec![fake::pool(0x01), fake::pool(0x02), fake::pool(0x03)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        let feed = builder(&node, config(0.0))
            .discovery_page(2)
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        let first = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(first.books.len(), 2);
        let second = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(second.books.len(), 3);
    }

    /// Only what is new is read: a known entry never changes, as the array only grows.
    #[tokio::test]
    async fn the_next_round_reads_only_the_new_pools() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), fake::pool(0x02)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        assert_eq!(
            next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap()
                .books
                .len(),
            2
        );
        // the length and two entries
        assert_eq!(node.seen.lock().unwrap().storage_reads, 3);

        node.chain
            .lock()
            .unwrap()
            .pools
            .push(fake::pool(0x03));
        assert_eq!(
            next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap()
                .books
                .len(),
            3
        );
        // the length and the one new entry
        assert_eq!(node.seen.lock().unwrap().storage_reads, 5);
    }

    #[tokio::test]
    async fn a_hooks_code_is_read_once() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        for _ in 0..3 {
            next_poll(&feed, &node, Some(&mut catalogue))
                .await
                .unwrap();
        }
        assert_eq!(node.seen.lock().unwrap().code_reads[&HOOK], 1);
    }

    /// Anyone can create pools by the thousand: one the lists refuse is not read every round.
    #[tokio::test]
    async fn a_pool_with_nothing_to_serve_rests_between_rounds() {
        let mut rogue = fake::pool(0x09);
        rogue.hook = ROGUE_HOOK;
        let address = rogue.address;
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![rogue]).await;
        let feed = builder(&node, config(0.0))
            .discovery_interval(Duration::from_millis(100))
            .build()
            .unwrap();
        let reader = &feed.source.reader;
        let views = || node.seen.lock().unwrap().views[&address];
        let mut catalogue = Catalogue::default();
        reader.discover(&mut catalogue).await;
        let read_once = views();
        assert!(read_once <= 3, "{read_once} views of a pool the lists refuse");
        reader.discover(&mut catalogue).await;
        assert_eq!(views(), read_once);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        reader.discover(&mut catalogue).await;
        assert!(views() > read_once);
    }

    #[tokio::test]
    async fn a_factory_that_cannot_be_read_keeps_the_last_list() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        node.chain.lock().unwrap().factory_down = true;
        let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
    }

    /// Before any good round there is nothing to serve, but nothing to give up on either.
    #[tokio::test]
    async fn a_factory_that_cannot_be_read_at_first_fails_polls_until_it_can() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        node.chain.lock().unwrap().factory_down = true;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        let error = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap_err();
        assert!(matches!(error, FeedError::Connection(_)), "{error:?}");
        node.chain.lock().unwrap().factory_down = false;
        let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
    }

    /// A wrong address is told apart from a factory with no pools, and still ends nothing: it can
    /// be a factory not deployed yet.
    #[tokio::test]
    async fn an_address_with_no_code_fails_polls_and_not_the_feed() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .factories(vec![bytes(NO_CODE)])
            .build()
            .unwrap();
        let error = first_poll(&feed).await.unwrap_err();
        assert!(
            matches!(error, FeedError::Connection(ref m) if m.contains("holds no code")),
            "{error:?}"
        );

        node.chain.lock().unwrap().pools.clear();
        let no_pools_yet = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert!(first_poll(&no_pools_yet)
            .await
            .unwrap()
            .books
            .is_empty());
    }

    /// `isPool` guards the storage layout: past an entry the factory disowns, nothing is taken.
    #[tokio::test]
    async fn an_entry_the_factory_disowns_stops_the_page() {
        let pools = vec![fake::pool(0x01), fake::pool(0x02), fake::pool(0x03)];
        let node = FakeNode::spawn(CHAIN_ID, 0, pools).await;
        node.chain
            .lock()
            .unwrap()
            .not_pools
            .insert(Address::repeat_byte(0x02));
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert_eq!(pools_quoted(&node), vec![Address::repeat_byte(0x01)]);
    }

    #[tokio::test]
    async fn a_named_pool_is_checked_with_its_factory_and_the_array_is_not_read() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01), fake::pool(0x02)]).await;
        let feed = builder(&node, config(0.0))
            .pools(vec![bytes(Address::repeat_byte(0x01))])
            .build()
            .unwrap();
        let snapshot = first_poll(&feed).await.unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert_eq!(node.seen.lock().unwrap().storage_reads, 0);
    }

    /// A pool no factory created is not served by name; it is asked for again, as it may be yet
    /// to come.
    #[tokio::test]
    async fn a_named_pool_no_factory_created_is_not_served_until_one_did() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        node.chain
            .lock()
            .unwrap()
            .not_pools
            .insert(Address::repeat_byte(0x01));
        let feed = builder(&node, config(0.0))
            .pools(vec![bytes(Address::repeat_byte(0x01))])
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        let error = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap_err();
        assert!(matches!(error, FeedError::Connection(_)), "{error:?}");
        assert!(pools_quoted(&node).is_empty());

        node.chain
            .lock()
            .unwrap()
            .not_pools
            .clear();
        let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
    }

    /// Views only decide what to quote, so one the node fails to answer costs nothing: the pool's
    /// quotes still come from the chain at each block.
    #[tokio::test]
    async fn a_pool_whose_views_the_node_fails_keeps_its_last_read() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        let mut catalogue = Catalogue::default();
        next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        node.chain.lock().unwrap().pools[0].fault = Fault::ViewsUnanswered;
        let snapshot = next_poll(&feed, &node, Some(&mut catalogue))
            .await
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(!feed
            .source
            .reader
            .quarantined(&Address::repeat_byte(0x01), Instant::now()));
    }

    #[tokio::test]
    async fn a_slow_round_does_not_hold_up_the_polls() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        next_poll(&feed, &node, Some(&mut Catalogue::default()))
            .await
            .unwrap();

        node.chain.lock().unwrap().storage_delay = Duration::from_secs(60);
        let reader = Arc::clone(&feed.source.reader);
        let round = tokio::spawn(async move {
            reader
                .discover(&mut Catalogue::default())
                .await;
        });
        let snapshot = tokio::time::timeout(Duration::from_secs(5), next_poll(&feed, &node, None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.books.len(), 1);
        assert!(!round.is_finished());
        round.abort();
    }

    /// A node that hangs must not hold a round for good: the client sets no deadline of its own.
    #[tokio::test]
    async fn a_read_the_node_never_answers_ends_at_the_call_deadline() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        node.chain.lock().unwrap().storage_delay = Duration::from_secs(60);
        let feed = builder(&node, config(0.0))
            .feed_config(HttpFeedConfig {
                request_timeout: Duration::from_millis(50),
                ..FlammFeedBuilder::default_feed_config(Chain::Base)
            })
            .build()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), first_poll(&feed))
            .await
            .unwrap()
            .unwrap_err();
    }

    /// The whole of it through `run`: the round beside the polls, and the books out of the stream.
    #[tokio::test]
    async fn the_feed_publishes_with_discovery_beside_its_polls() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .feed_config(HttpFeedConfig {
                poll_interval: Duration::from_millis(5),
                ..FlammFeedBuilder::default_feed_config(Chain::Base)
            })
            .build()
            .unwrap();
        let mut stream = SnapshotFeedStream::spawn("flamm", feed);
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap();
        let SnapshotFeedEvent::Published(snapshot) = event else {
            panic!("expected a snapshot, got {event:?}");
        };
        assert_eq!(snapshot.books.len(), 1);
    }

    #[tokio::test]
    async fn a_book_under_the_floor_is_not_served() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        // each side's largest fill is worth 100,000 USD
        let feed = builder(&node, config(150_000.0))
            .build()
            .unwrap();
        assert!(first_poll(&feed)
            .await
            .unwrap()
            .books
            .is_empty());
    }

    /// Anyone can create a pool, so each is valued before it is quoted: where its pool token has no
    /// price, at its legs in the tokens that have one.
    #[tokio::test]
    async fn a_pool_whose_pool_token_has_no_price_is_valued_at_its_priced_legs() {
        let pool = |byte: u8, usd_held: u64| {
            let mut pool = fake::pool(byte);
            pool.tokens[0].usd_wad = None;
            pool.tokens[1].held = U256::from(usd_held) * U256::from(1_000_000u64);
            pool
        };
        // each side's largest fill is worth 100,000 USD, over the floor
        let floor = config(50_000.0);
        let rich = FakeNode::spawn(CHAIN_ID, 0, vec![pool(0x06, 60_000)]).await;
        let served = builder(&rich, floor.clone())
            .build()
            .unwrap();
        assert_eq!(
            first_poll(&served)
                .await
                .unwrap()
                .books
                .len(),
            1
        );

        let poor = FakeNode::spawn(CHAIN_ID, 0, vec![pool(0x07, 40_000)]).await;
        let skipped = builder(&poor, floor).build().unwrap();
        assert!(first_poll(&skipped)
            .await
            .unwrap()
            .books
            .is_empty());
        assert!(poor
            .seen
            .lock()
            .unwrap()
            .quote_senders
            .is_empty());
    }

    /// A pool whose NAV cannot be read is valued at its priced legs rather than set aside, as it
    /// may still fill.
    #[tokio::test]
    async fn a_pool_that_refuses_its_nav_is_valued_at_its_priced_legs() {
        let mut pool = fake::pool(0x08);
        pool.fault = Fault::Nav;
        pool.tokens[1].held = U256::from(60_000_000_000u64);
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![pool]).await;
        let feed = builder(&node, config(50_000.0))
            .build()
            .unwrap();
        assert_eq!(
            first_poll(&feed)
                .await
                .unwrap()
                .books
                .len(),
            1
        );
        assert!(!feed
            .source
            .reader
            .quarantined(&Address::repeat_byte(0x08), Instant::now()));
    }

    #[tokio::test]
    async fn a_node_on_another_chain_ends_the_feed() {
        let node = FakeNode::spawn(1, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .build()
            .unwrap();
        assert!(matches!(first_poll(&feed).await, Err(FeedError::Fatal(_))));
    }

    #[tokio::test]
    async fn a_lagging_node_fails_the_poll() {
        let node = FakeNode::spawn(CHAIN_ID, 0, vec![fake::pool(0x01)]).await;
        let feed = builder(&node, config(0.0))
            .max_block_lag(Some(Duration::from_secs(5)))
            .build()
            .unwrap();
        assert!(matches!(first_poll(&feed).await, Err(FeedError::Connection(_))));
    }

    #[test]
    fn the_builder_refuses_what_it_cannot_serve() {
        let base = || FlammFeedBuilder::new(config(0.0), "http://127.0.0.1:1".into());
        let listed = || {
            base()
                .hook_classes(vec![listed_hook()])
                .price_feeds(vec![bytes(FEED)])
        };
        assert!(matches!(listed().build(), Err(FeedError::InvalidInput(_))));
        let mut other = config(0.0);
        other.chain = Chain::Arbitrum;
        assert!(matches!(
            FlammFeedBuilder::new(other, "http://127.0.0.1:1".into())
                .factories(vec![bytes(FACTORY)])
                .build(),
            Err(FeedError::Fatal(_))
        ));
        assert!(matches!(
            listed()
                .factories(vec![bytes(FACTORY)])
                .haircut_bps(2_000)
                .build(),
            Err(FeedError::InvalidInput(_))
        ));
        assert!(matches!(
            listed()
                .factories(vec![Bytes::from(vec![1u8; 19])])
                .build(),
            Err(FeedError::InvalidInput(_))
        ));
        assert!(matches!(
            listed()
                .factories(vec![bytes(FACTORY)])
                .max_in_flight(0)
                .build(),
            Err(FeedError::InvalidInput(_))
        ));
        assert!(matches!(
            listed()
                .factories(vec![bytes(FACTORY)])
                .discovery_page(0)
                .build(),
            Err(FeedError::InvalidInput(_))
        ));
        assert!(matches!(
            listed()
                .factories(vec![bytes(FACTORY)])
                .price_feeds(vec![Bytes::from(vec![1u8; 19])])
                .build(),
            Err(FeedError::InvalidInput(_))
        ));

        // a direction takes 24 probes at most, and a call needs the gas of a probe at the most cap
        let served = || listed().factories(vec![bytes(FACTORY)]);
        let refused =
            |builder: FlammFeedBuilder| matches!(builder.build(), Err(FeedError::InvalidInput(_)));
        assert!(served()
            .rungs(MAX_PROBES_PER_DIRECTION, DEFAULT_SMALLEST_RUNG_USD)
            .build()
            .is_ok());
        assert!(refused(served().rungs(MAX_PROBES_PER_DIRECTION + 1, DEFAULT_SMALLEST_RUNG_USD)));
        assert_eq!(SAMPLE_GAS_FLOOR, 20_717_460);
        assert!(served()
            .sample_gas(SAMPLE_GAS_FLOOR)
            .build()
            .is_ok());
        assert!(refused(served().sample_gas(SAMPLE_GAS_FLOOR - 1)));
        assert!(refused(served().probe_gas(0)));
        assert!(refused(served().probe_gas(MAX_PROBE_GAS + 1)));
        assert!(served()
            .probe_gas(MAX_PROBE_GAS)
            .build()
            .is_ok());
    }

    /// A feed that could never serve a pool is a mistake to refuse here, not a quiet empty feed.
    #[test]
    fn the_builder_needs_something_that_admits_a_pool() {
        let base = || {
            FlammFeedBuilder::new(config(0.0), "http://127.0.0.1:1".into())
                .factories(vec![bytes(FACTORY)])
        };
        let class = || vec![listed_hook()];
        let feeds = || vec![bytes(FEED)];
        let refused = |builder: FlammFeedBuilder| matches!(builder.build(), Err(FeedError::InvalidInput(ref m)) if m.contains("admits"));
        assert!(refused(base()));
        assert!(refused(base().hook_classes(class())));
        assert!(refused(base().price_feeds(feeds())));
        assert!(refused(base().pools(Vec::new())));
        assert!(base()
            .hook_classes(class())
            .price_feeds(feeds())
            .build()
            .is_ok());
        assert!(base()
            .hook_classes(class())
            .price_feed_code_hashes(vec![keccak256(FEED_CODE)])
            .build()
            .is_ok());
        // a front is never served, so the knob that once served one admits nothing
        assert!(refused(
            base()
                .serve_front_priced(true)
                .price_feeds(feeds())
        ));
        assert!(base()
            .pools(vec![bytes(Address::repeat_byte(0x01))])
            .build()
            .is_ok());
    }
}
