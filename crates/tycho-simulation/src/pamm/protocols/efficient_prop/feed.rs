//! The listing doors' book feed: polls one front's doors over JSON-RPC and publishes every poll as
//! a complete [`BookSnapshot`].

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    future::Future,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::primitives::{address, Address, B256, U256};
use reqwest::Url;
use tracing::debug;
use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim as _, Bytes};

use crate::{
    book::{
        component::{pair_book, pair_component_id},
        quote_tokens::usd_stablecoins_for_chain,
        sim, BookFeedConfig, BookSnapshot, Books, ReceivedAt,
    },
    pamm::protocols::efficient_prop::{
        desks::{ApprovedDesks, DEFAULT_MAX_AGE},
        ladder::Ladder,
        overrides::{Cadence, NextBlockReader},
        reader::{lagging, DoorRead, DoorsRead, DoorsReader, Hold, Samples},
        state::{CreditGas, Deadline, EfficientPropState, GasChain},
        telemetry::{self, PollOutcome},
        EfficientPropDeployment, ASK_VALID_UNTIL_MS_ATTRIBUTE, BASE_PRICE_SOURCES,
        BID_VALID_UNTIL_MS_ATTRIBUTE, ETHEREUM_PRICE_SOURCES, EXPECTED_GAS_ATTRIBUTE,
        FALLBACK_PROTOCOL_SYSTEM, GAS_LIMIT_ADDITION_ATTRIBUTE, PAMM_ADDRESS_ATTRIBUTE,
        PROTOCOL_SYSTEM, PROTOCOL_TYPE, QUOTE_LAYOUT_BUILDER, TIP_CAP_WEI_ATTRIBUTE,
        VALID_FROM_BLOCK_ATTRIBUTE,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig, HttpSource},
        Publisher, SnapshotFeed,
    },
};

/// How many seconds a read's block timestamp may trail the wall clock. Past it the node is
/// lagging, and the words its quotes priced may already have been replaced.
const MAX_READ_LAG_SECS: u64 = 4;

/// How many listing ids past the highest one seen each poll reads, so a new listing is found the
/// poll after it is reserved.
const LISTING_LOOKAHEAD: u32 = 16;

/// The highest listing id the front can hand out: ids are `uint24`, and the last is never issued.
const MAX_LISTING: u32 = (1 << 24) - 2;

/// The sizes each side is sampled at by default: the most it fills and eleven halvings of it,
/// down to 1/2048 of it.
const DEFAULT_RUNGS: u32 = 12;

/// USD1 on BNB Chain, the quote token of the first pool listed there, outside the chain's curated
/// stablecoins: [`EfficientPropFeedBuilder::bnb`] values books in it as well.
const BNB_USD1: Address = address!("8d0D000Ee44948FC98c9B98A4FA4921476f08B0d");

/// How long a route priced from a book may take to land after the book stops being served, by
/// default on Base: ten of its 200 ms blocks.
const DEFAULT_LANDING_ALLOWANCE: Duration = Duration::from_secs(2);

/// The same on Ethereum: how long before the next block's time a route priced from its book must
/// be sent to reach a builder of that block.
const ETHEREUM_LANDING_ALLOWANCE: Duration = Duration::from_secs(1);

/// The same on BNB Chain, whose blocks come every 450 ms: 48 Club, which carries the makers'
/// updates there, takes them up to about 100 ms before its block's end. Unmeasured for a taker's
/// route.
const BNB_LANDING_ALLOWANCE: Duration = Duration::from_millis(100);

/// The least time, ms, a side whose print lapses within the book's life is served for: one that
/// would quote for less from the poll's end, a route priced from it then landing while the print
/// lives, is left out.
pub(super) const MIN_SIDE_SERVED_MS: u64 = 200;

/// The same on BNB Chain, where a block lasts 450 ms and a side lives through one or two of them:
/// 200 ms would leave out most sides a poll finds.
pub(super) const BNB_MIN_SIDE_SERVED_MS: u64 = 50;

/// The most sizes a side may be sampled at.
const MAX_RUNGS: u32 = 64;

/// The listing doors' book feed. Built through [`EfficientPropFeedBuilder`]. Neither its `Debug`
/// output, its errors nor its logs, at any level, carry the RPC URL, which commonly carries a key
/// in its query or its userinfo.
#[derive(Debug, Clone)]
pub struct EfficientPropFeed {
    feed_config: HttpFeedConfig,
    source: EfficientPropBookSource,
}

/// Builds an [`EfficientPropFeed`] from the shared [`BookFeedConfig`], a JSON-RPC URL of its
/// chain and the front and factory to serve. Every poll sends three JSON-RPC batches, and a fourth
/// where a print lapses within the book's life (see the reader), all reads of a full node.
pub struct EfficientPropFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    deployment: EfficientPropDeployment,
    rungs: u32,
    min_tip_cap_wei: U256,
    price_sources: BTreeSet<u32>,
    credit_listings: Vec<(u32, CreditGas)>,
    approved_desks: Vec<Bytes>,
    approved_desks_url: Option<String>,
    approved_desks_max_age: Duration,
    door_code_hash: Option<Bytes>,
    landing_allowance: Option<Duration>,
    state_urls: Vec<String>,
    usd_quote_tokens: HashSet<Bytes>,
    feed_config: HttpFeedConfig,
    protocol_system: &'static str,
}

impl EfficientPropFeedBuilder {
    /// A feed of the front deployed on Ethereum ([`EfficientPropDeployment::ethereum`]), with
    /// the price sources of its deployment approved ([`ETHEREUM_PRICE_SOURCES`]): `book_config`
    /// is Ethereum's and `rpc_url` an Ethereum node. What is left to set is where the next
    /// block's state comes from ([`Self::state_urls`], one URL at least) and whose listings to
    /// serve ([`Self::with_approved_desks`]): without the first the feed does not build, and
    /// without the second it serves nothing.
    ///
    /// Approving a price source is a judgement of its code ([`Self::approved_price_sources`]):
    /// these four were read for it at the deployment. A caller that judges otherwise replaces the
    /// set, or starts from [`Self::new`]. A credit listing, the first pool's wstETH/WETH among
    /// them, is served only where [`Self::credit_listing`] names its gas, which is its pool's
    /// settings' and no constant.
    pub fn ethereum(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self::new(book_config, rpc_url, EfficientPropDeployment::ethereum())
            .approved_price_sources(ETHEREUM_PRICE_SOURCES)
    }

    /// A feed of the front deployed on Base ([`EfficientPropDeployment::base`]), with the price
    /// source of its deployment approved ([`BASE_PRICE_SOURCES`]): `book_config` is Base's and
    /// `rpc_url` a Base node. Base reads no state URL. What is left to set is whose listings to
    /// serve ([`Self::with_approved_desks`]) and what each one's fills cost
    /// ([`Self::credit_listing`]): no fill of the deployed build was measured on Base, so a
    /// listing whose gas is not named is not served there.
    pub fn base(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self::new(book_config, rpc_url, EfficientPropDeployment::base())
            .approved_price_sources(BASE_PRICE_SOURCES)
    }

    /// A feed of the front as it is to be deployed on BNB Chain ([`EfficientPropDeployment::bnb`]),
    /// valuing books in USD1 beside the chain's curated stablecoins: `book_config` is BNB Chain's
    /// and `rpc_url` a BNB Chain node. What is left to set is where the next block's state comes
    /// from ([`Self::state_urls`], required), whose listings to serve
    /// ([`Self::with_approved_desks`]), what each one's fills cost ([`Self::credit_listing`], as
    /// on Base), and which price sources to approve ([`Self::approved_price_sources`]): none is
    /// approved here, since the deployment's record has not landed and anyone may register a
    /// source, so an id names a price contract only once the deployment's record shows it.
    pub fn bnb(book_config: BookFeedConfig, rpc_url: String) -> Self {
        let mut builder = Self::new(book_config, rpc_url, EfficientPropDeployment::bnb());
        builder
            .usd_quote_tokens
            .insert(Bytes::from(BNB_USD1.0.to_vec()));
        builder
    }

    pub fn new(
        book_config: BookFeedConfig,
        rpc_url: String,
        deployment: EfficientPropDeployment,
    ) -> Self {
        let usd_quote_tokens = usd_stablecoins_for_chain(book_config.chain).unwrap_or_default();
        Self {
            book_config,
            rpc_url,
            deployment,
            rungs: DEFAULT_RUNGS,
            min_tip_cap_wei: U256::ZERO,
            price_sources: BTreeSet::new(),
            credit_listings: Vec::new(),
            approved_desks: Vec::new(),
            approved_desks_url: None,
            approved_desks_max_age: DEFAULT_MAX_AGE,
            door_code_hash: None,
            landing_allowance: None,
            state_urls: Vec::new(),
            usd_quote_tokens,
            feed_config: Self::default_feed_config(),
            protocol_system: PROTOCOL_SYSTEM,
        }
    }

    /// How many sizes each side is sampled at from the top: the most the listing fills and
    /// `rungs - 1` halvings of it. Below those, each side is also sampled at quarterings of the
    /// smallest down to the listing's least fill, and at that, so a small swap is quoted however
    /// deep the listing. More sizes price between them more closely and cost a larger read.
    pub fn rungs(mut self, rungs: u32) -> Self {
        self.rungs = rungs;
        self
    }

    /// The lowest taker tip cap, wei per gas above the base fee, of a listing served (0 by
    /// default, every listing). A listing refuses a swap sent with a priority fee above its cap,
    /// which no book can enforce, so a caller who sends at a known priority fee sets it here to
    /// leave out the listings that would refuse its swaps. A listing with no cap is always served.
    pub fn min_tip_cap_wei(mut self, min_tip_cap_wei: U256) -> Self {
        self.min_tip_cap_wei = min_tip_cap_wei;
        self
    }

    /// The front's price sources, by the ids its `registerSource` gave them, that a listing reading
    /// a price print may be bound to and be served: none by default, so only the listings that read
    /// no print (fence kind WORD or NONE) are served until the caller names the protocol's own.
    ///
    /// A PRINT or SESSION listing fills at its source's answer. A source is any contract
    /// registered on the front, which anyone may register, and the listing's operator binds it.
    /// The source's code runs inside the quote and again inside the fill, and it can tell the two
    /// apart: by what the call reads of its transaction and block, and by state the fill has
    /// already changed when the source runs. On this route the front holds its lock through the
    /// fill, and the input is already at the door. A source that tells them apart can fill a real
    /// transaction for less than the book quoted, down to the route's minimum output. So approve a
    /// source only where its code cannot change and its answer comes from its own stored prices
    /// and the clock alone; a guarded source needs approval as any other. Such a listing is served
    /// only where every source it is bound to, its primary and its backup (record word 10 of the
    /// front's storage, read with its settings at each poll's block, so a rebinding is seen on the
    /// next poll), is one of these.
    ///
    /// The same holds of a listing's two tokens. Their code runs inside the fill and can tell it
    /// from a quote in the same ways, and the feed does not screen them: it serves the listings
    /// between the tokens the book configuration names, whatever their code. Name only tokens
    /// whose code cannot change and moves exactly the amounts asked.
    pub fn approved_price_sources(mut self, sources: impl IntoIterator<Item = u32>) -> Self {
        self.price_sources = sources.into_iter().collect();
        self
    }

    /// Serves listing `listing` while its credit mode is set (1 to 3), or while its pair is an
    /// asset-module pair (a stock token, whose module's rules run inside every fill and cost what
    /// its token's reads cost: such a listing in no credit mode is charged `gas`'s custody pair),
    /// charging its fills `gas`. A listing of either kind not named here is not served. Any other
    /// listing named here is charged `gas`'s custody pair rather than a figure of this crate's:
    /// on Base every listing must be, since no fill of the deployed build was measured there, and
    /// on Ethereum every listing whose pair was not measured. A listing with a credit mode is
    /// otherwise not served: past its pool's custody it fills through the pool's credit route,
    /// whose output may step down from custody's and whose gas is its pool's own (a bid that
    /// borrows costs several times a custody fill), so no figure of this crate can charge it.
    /// Measure `gas` on that listing's pool with real transactions through the chain's
    /// `PropAMMExecutor` (see [`CreditGas`]), and name the listing again where its pool changes.
    ///
    /// Each size of such a listing is then sampled as any other, and also asked of the front's
    /// own plan (`quoteFrom` with `PLAN_ONLY`), which tells a size its pool's custody fills from
    /// one the route fills; each fill is charged its path's figure. The state quotes nothing
    /// between the largest custody size and the smallest routed size. What the route refuses (a
    /// debt cap, a rate ceiling, a darkened print, a pause) the door refuses in the sample, and
    /// the size is not quoted; what changes between the read and the fill (another taker's borrow
    /// lifting the market's rate past its ceiling, a feed update) reverts the route, held only by
    /// the route's minimum output. On Base, whose words stand in storage, every size is asked
    /// both at each moment of the book's life sampled (the read's block, the life's end, a
    /// premium's edges, a guarded lane's hold, a lapsing print's blocks): a size whose path
    /// differs between two of them is not quoted, and its side's run ends there.
    ///
    /// The four charges are the least seen, so they size no gas limit. Name the dearest fill
    /// measured on the listing before refunds as well ([`CreditGas::most_before_refunds`]) and
    /// its book carries what a swap's call adds to a gas limit; a listing with a credit mode
    /// carries none otherwise, on a pool of its own or on one it shares, where it is served its
    /// custody sizes alone. On a listing in no credit mode that figure sizes the limit where it
    /// is over the custody fill and its refund.
    pub fn credit_listing(mut self, listing: u32, gas: CreditGas) -> Self {
        self.credit_listings
            .push((listing, gas));
        self
    }

    /// The desk contracts whose listings are served, replacing any given before. Anyone can
    /// reserve a listing and post quotes to it, so a listing is served only where its maker's
    /// desk contract (the front's `deskCellOf` of the listing's desk id) is one of these. Empty by
    /// default: a feed built without them, and without a list from
    /// [`Self::with_approved_desks_url`], serves nothing.
    pub fn with_approved_desks(mut self, desks: impl IntoIterator<Item = Bytes>) -> Self {
        self.approved_desks = desks.into_iter().collect();
        self
    }

    /// A URL answering a GET with the approved desk contracts,
    /// `{"chainId": 1, "updatedAt": <Unix seconds>, "desks": ["0x…"]}` (`updatedAt` when the list
    /// last changed; other fields are ignored). The list is read every 15 s at most, each read
    /// given half the request timeout, and the last one taken replaces
    /// [`Self::with_approved_desks`]'s set, an empty list approving nobody, until its read is ten
    /// minutes old ([`Self::with_approved_desks_max_age`]). Past that age that set rules less
    /// every desk the list left out, and before any answer that set alone: a URL that fails
    /// brings no desk in. An answer is refused whole where its status is not 200, it is for
    /// another chain, over 64 KiB or 1,024 desks, names the zero address, or is stamped zero,
    /// before the one held or ahead of the clock.
    ///
    /// No such list is served at any URL yet. Until one is, give the desks to
    /// [`Self::with_approved_desks`]: they are the whole set, and they rule.
    pub fn with_approved_desks_url(mut self, url: impl Into<String>) -> Self {
        self.approved_desks_url = Some(url.into());
        self
    }

    /// How old the read of [`Self::with_approved_desks_url`]'s list may be while the list rules.
    /// Ten minutes by default.
    pub fn with_approved_desks_max_age(mut self, max_age: Duration) -> Self {
        self.approved_desks_max_age = max_age;
        self
    }

    /// Pins the listing doors' creation code hash. Without it the factory's own `DOOR_CODEHASH`
    /// answer is taken each poll, and a door's code is only as trusted as the configured factory
    /// address; with it, a poll that finds the factory answering another hash ends the feed.
    pub fn with_door_code_hash(mut self, hash: Bytes) -> Self {
        self.door_code_hash = Some(hash);
        self
    }

    /// Settles the listings' swaps through Tycho's `PropAMMFallbackRouter`, which quotes the door
    /// in the block the swap lands in and runs a solver-named fallback pool where the door quotes
    /// less or cannot fill, as in a block without the maker's update, where the direct path
    /// reverts the route. Components then carry
    /// [`FALLBACK_PROTOCOL_SYSTEM`](super::FALLBACK_PROTOCOL_SYSTEM) rather than
    /// [`PROTOCOL_SYSTEM`](super::PROTOCOL_SYSTEM), which is what selects the fallback encoder.
    /// Off by default, and on Ethereum only: its figures were measured through Ethereum's router.
    ///
    /// The books and their gas are the direct path's, and tycho-execution adds the router's own
    /// work to a `fallback:` swap. A credit listing is refused with it: the router's quote runs
    /// the listing's route once more, which no figure named for the direct path covers.
    pub fn with_fallback_router(mut self) -> Self {
        self.protocol_system = FALLBACK_PROTOCOL_SYSTEM;
        self
    }

    /// The USD stablecoins a book's liquidity is valued in, replacing the chain's curated set: a
    /// listing quoted in one is valued at its notional, one quoted in another token through a
    /// listing that sells that token for one.
    pub fn usd_quote_tokens(mut self, usd_quote_tokens: HashSet<Bytes>) -> Self {
        self.usd_quote_tokens = usd_quote_tokens;
        self
    }

    /// How long a route priced from a book may take to land after the book stops being served:
    /// a side is published only while its word and depth stand that long past the snapshot's
    /// life. Two seconds by default on Base, ten of its blocks.
    ///
    /// On Ethereum a side quotes until its life's last block's time less this, one second by
    /// default: a shorter allowance quotes a block's book for longer, and leaves a route sent
    /// after a builder sealed the block to land in a later one, where the maker's next update
    /// prices it (or none does and it reverts), held only by the route's minimum output; a longer
    /// one serves each book for less of its slot. On BNB Chain the same, 100 ms by default, before
    /// the next block's milliseconds (its header's latest and 450 ms), or the block after's where
    /// the listing carries: a route must reach 48 Club, which carries the makers' updates, before
    /// it seals the block.
    pub fn landing_allowance(mut self, landing_allowance: Duration) -> Self {
        self.landing_allowance = Some(landing_allowance);
        self
    }

    /// The state endpoints to read the next block's state map from, on Ethereum and BNB Chain
    /// (required there, refused on Base): URLs answering the JSON-RPC method
    /// `titan_getPammStateOverrides`, such as the protocol api's `https://<api host>/v1/1/state`
    /// and a builder's data hosts. On BNB Chain a map names its block (`blockNumber`) and no
    /// slot; no URL serving chain 56 is known yet, and until one answers the feed serves nothing
    /// there. Every poll asks each and keeps only the maps for the block after the latest one. Each
    /// listing is quoted from one answer, the first URL's in this order that writes its quote's
    /// stamp slot with a stamp that block fills, and only when that answer also writes the price
    /// contract it reads. A URL may carry a key, in its query or its userinfo; it is never logged,
    /// nor is any part of it.
    pub fn state_urls(mut self, state_urls: impl IntoIterator<Item = String>) -> Self {
        self.state_urls = state_urls.into_iter().collect();
        self
    }

    /// Tune the poll loop (poll cadence, request timeout, failure limit).
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = feed_config;
        self
    }

    /// The feed tuning a new builder starts from, set by how often operators post. An operator
    /// posts a listing's price every few seconds, and its quotes move with each post, so the feed
    /// polls every 250 ms, gives a poll's requests 2 s, and withdraws a snapshot 4 s
    /// (`MAX_READ_LAG_SECS`) after its last refresh. A poll slower than the interval is followed by
    /// the next as soon as it answers, so a slow node is read as often as it answers and no more.
    ///
    /// # The load on the node
    ///
    /// A poll is three HTTP requests (four where a print lapses within the book's life), but a node
    /// meters each JSON-RPC call inside them. For `n` listings, each side sampled at about 16 sizes
    /// (32 door quotes a listing), a poll makes:
    /// - `2 + 5n` `eth_getStorageAt` calls: the front's implementation slot twice, and each
    ///   listing's settings, controls, session, source and live-source words;
    /// - `eth_call`s of `Multicall3.aggregate3`: `ceil((n + 24) / 48)` reading the listings,
    ///   `ceil(5n / 48)` asking each door's front, listing, base and fills, and for the quotes
    ///   `ceil(32n / 48)` at the read's block and `ceil(32n / 45)` under each block override: the
    ///   life's end, and for each distinct moment a premium window closes within the life two, the
    ///   last block before it and the first at or after it (only the sides it closes on);
    /// - where `p` of the listings read a price print, `ceil(4p / 45)` more under each block
    ///   override at the last block of each second of the life between the read's block and its
    ///   last, asking each of their sides two quotes: `4p` quotes for each second asked. That is
    ///   seven or eight such seconds where the poll starts at the read block's time, as the read
    ///   lands in its second, and about one more for each second the poll starts after it (a node's
    ///   latest block trails the wall clock by a few hundred milliseconds on Base), at most twelve,
    ///   since a read more than four seconds behind fails at the first request: so 7 to 12 more
    ///   `eth_call`s a poll for up to 11 such listings, and 28 to 48 more quotes a poll for each.
    ///   At eight, one such listing adds 32 calls and 128 quotes a second at 250 ms. A listing that
    ///   reads no print adds none;
    /// - where `g` of those listings read a guarded source, `ceil(32g / 45)` more under each block
    ///   override at each block of its lane's hold after the read's block, through the first at or
    ///   past its end, asking every size of both sides: two blocks where the lane holds 400 ms on a
    ///   front stepping 200 ms, or 2 s on one stepping a second, and ten on the first poll of a
    ///   front stepping 200 ms, which samples 2 s until the lane answers its `HOLD_MS`, asked once
    ///   a lane in the at-block call;
    /// - where `q` of those listings' prints lapse within the life, a fourth request: `ceil(32q /
    ///   45)` under each block override at the last second's ask the print lived through and at
    ///   each block between it and the next ask, five blocks where the front's clock steps 200 ms
    ///   and one where it steps a second, each asking every size of their sides.
    ///
    /// A credit listing served asks the front's plan beside each of its quotes, twice its calls,
    /// and where a side of it may route, a request before the doors' asks its door at the read's
    /// block across its token's scale, how far the route reaches. Every listing's pool is asked
    /// its two balances beside the at-block quotes.
    ///
    /// One listing with a premium window closing within the life, reading no print, makes 13 calls
    /// a poll, 7 `eth_getStorageAt` and 6 `eth_call`, which
    /// run 128 door quotes of about 70k to 110k gas each: at 250 ms, 52 calls and 512 quotes a
    /// second; reading a print, at eight seconds asked, 21 calls a poll (84 a second, 640 quotes).
    /// Ten listings make 70 calls a poll (280 a second, 2,560 quotes), and up to 90 (360 a second,
    /// 5,120 quotes) when each has its own premium window closing; 98 (392 a second, 6,400 quotes)
    /// when each also reads a print. A print living four seconds and posted every second lapses
    /// within each poll's life: such a listing makes 26 calls a poll on Base (104 a second, 1,280
    /// quotes). The default therefore needs a node that allows that rate: a rate-limited endpoint
    /// answers with errors, the polls fail, and the snapshots are withdrawn 4 s after the last
    /// refresh. For such an endpoint, set a longer `poll_interval` with [`Self::feed_config`].
    ///
    /// On Ethereum a poll is four requests to the node and one POST to each state URL, sent with
    /// the first: four POSTs a second to each URL at 250 ms. The node is asked as on Base, less
    /// BaseTime, and, before the doors, each listing's quote contract (`ceil(n / 48)` `eth_call`s)
    /// and its settings and live-source words (`2n` `eth_getStorageAt` calls, and one for each desk
    /// contract and two a listing from it) at the
    /// latest block, from which the accounts each listing is asked under are picked. Every quote
    /// is asked at one block, the next, and again at the block after for the listings that carry:
    /// `ceil(32n / 46)` `eth_call`s for the quotes, and `ceil(32c / 46)` more for `c` carrying
    /// listings. No premium, print or hold asks are made. A credit listing served asks the front's
    /// plan beside each of its quotes, twice its calls. The second request also reads its block's
    /// header (`eth_getBlockByNumber`), for its hash, and the requests after it name their block
    /// by that hash where the node answers it (EIP-1898), by its number where it does not.
    ///
    /// A poll that finds nothing the doors read changed since the last poll that sampled them
    /// asks no door: it is the first two requests and the POSTs alone, and publishes that poll's
    /// samples (see the module doc of `efficient_prop`, "On Ethereum"). Between two blocks that
    /// is every poll but the ones a state URL answers a new update on.
    pub fn default_feed_config() -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_millis(250),
            request_timeout: Duration::from_secs(2),
            max_snapshot_age: Some(Duration::from_secs(MAX_READ_LAG_SECS)),
            ..default_http_feed_config()
        }
    }

    /// The feed of the chain's own path.
    ///
    /// Fails for a chain other than Base, Ethereum and BNB Chain, for Ethereum or BNB Chain without
    /// a state URL or Base with one, for a quote layout other than [`QUOTE_LAYOUT_BUILDER`] on
    /// Ethereum and BNB Chain or any on Base, for an RPC URL or a state URL that is not one, for a
    /// front, factory or implementation that is not an address, for a desk contract code hash
    /// missing or not 32 bytes, for an empty allow-list, for a feed config that never withdraws a
    /// snapshot, for a number of sizes outside 1 to 64, for the fallback router off Ethereum, and
    /// for a credit listing of an id the front cannot hand out, or named twice. The node's chain is
    /// checked on every poll, which fails fatally when the node serves another one.
    pub fn build(self) -> Result<EfficientPropFeed, FeedError> {
        self.build_path()
    }

    /// [`Self::build`]'s body.
    pub(super) fn build_path(self) -> Result<EfficientPropFeed, FeedError> {
        telemetry::init();
        let chain = self.book_config.chain;
        let (gas_chain, default_landing_allowance) = if chain == Chain::Base {
            (GasChain::Base, DEFAULT_LANDING_ALLOWANCE)
        } else if chain == Chain::Ethereum {
            (GasChain::Ethereum, ETHEREUM_LANDING_ALLOWANCE)
        } else if chain == Chain::Bsc {
            (GasChain::Bnb, BNB_LANDING_ALLOWANCE)
        } else {
            return Err(FeedError::InvalidInput(format!(
                "Efficient PropAMM listing doors are served on Ethereum, Base and BNB Chain, not \
                 {chain}"
            )));
        };
        let next_block = match (gas_chain, self.state_urls.is_empty()) {
            (GasChain::Base, true) => None,
            (GasChain::Base, false) => {
                return Err(FeedError::InvalidInput(
                    "Efficient PropAMM state URLs are read on Ethereum and BNB Chain only".into(),
                ))
            }
            (GasChain::Ethereum | GasChain::Bnb, true) => {
                return Err(FeedError::InvalidInput(format!(
                    "Efficient PropAMM on {chain} needs a state URL: its makers' quotes are in \
                         the next block's state map"
                )))
            }
            (GasChain::Ethereum | GasChain::Bnb, false) => {
                let urls = self
                    .state_urls
                    .iter()
                    .enumerate()
                    .map(|(i, url)| {
                        Url::parse(url).map_err(|e| {
                            FeedError::InvalidInput(format!(
                                "Efficient PropAMM state URL {} is not one: {e}",
                                i + 1
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // Half the poll's own timeout, so a URL that hangs leaves the poll time
                // to read the doors from the others' maps.
                let cadence =
                    if gas_chain == GasChain::Bnb { Cadence::BNB } else { Cadence::Slots };
                Some(NextBlockReader::new_in(urls, self.feed_config.request_timeout / 2, cadence)?)
            }
        };
        if self.protocol_system == FALLBACK_PROTOCOL_SYSTEM && gas_chain != GasChain::Ethereum {
            return Err(FeedError::InvalidInput(format!(
                "Efficient PropAMM through the fallback router is served on Ethereum only, where \
                 its gas was measured, not {chain}"
            )));
        }
        let mut credit = BTreeMap::new();
        for (listing, gas) in self.credit_listings {
            if self.protocol_system == FALLBACK_PROTOCOL_SYSTEM {
                return Err(FeedError::InvalidInput(format!(
                    "Efficient PropAMM credit listing {listing} is not served through the fallback \
                     router, whose quote of the door runs the listing's route once more: its gas \
                     is measured on the direct path"
                )));
            }
            if !(1..=MAX_LISTING).contains(&listing) {
                return Err(FeedError::InvalidInput(format!(
                    "Efficient PropAMM credit listing {listing} is not an id the front hands out"
                )));
            }
            if credit.insert(listing, gas).is_some() {
                return Err(FeedError::InvalidInput(format!(
                    "Efficient PropAMM credit listing {listing} is named twice"
                )));
            }
        }
        let desks = ApprovedDesks::new(
            self.approved_desks
                .iter()
                .map(|desk| address(desk, "approved desk"))
                .collect::<Result<_, _>>()?,
            self.approved_desks_url
                .as_deref()
                .map(Url::parse)
                .transpose()
                .map_err(|e| {
                    FeedError::InvalidInput(format!(
                        "Efficient PropAMM approved desks URL is not one: {e}"
                    ))
                })?,
            chain.id(),
            self.approved_desks_max_age,
            self.feed_config.request_timeout / 2,
        )?;
        let landing_allowance = self
            .landing_allowance
            .unwrap_or(default_landing_allowance);
        let rpc_url = Url::parse(&self.rpc_url)
            .map_err(|e| FeedError::InvalidInput(format!("Efficient PropAMM RPC URL: {e}")))?;
        if !(1..=MAX_RUNGS).contains(&self.rungs) {
            return Err(FeedError::InvalidInput(format!(
                "Efficient PropAMM sizes per side must be 1 to {MAX_RUNGS}, not {}",
                self.rungs
            )));
        }
        let EfficientPropDeployment {
            quote_layout,
            front,
            factory,
            cell_codehash,
            implementations,
        } = &self.deployment;
        match (gas_chain, quote_layout.as_deref()) {
            (GasChain::Base, None) => {}
            (GasChain::Base, Some(layout)) => {
                return Err(FeedError::InvalidInput(format!(
                    "Efficient PropAMM quote contract layout {layout:?} is set on Base, where the \
                     feed samples the doors and reads no quote contract: leave it unset"
                )))
            }
            (GasChain::Ethereum | GasChain::Bnb, Some(QUOTE_LAYOUT_BUILDER)) => {}
            (GasChain::Ethereum | GasChain::Bnb, other) => {
                return Err(FeedError::InvalidInput(format!(
                    "Efficient PropAMM quote contract layout {other:?} is not one this \
                     integration reads: {chain} needs {QUOTE_LAYOUT_BUILDER:?}, the layout on \
                     Ethereum and BNB Chain"
                )))
            }
        }
        // Every chain's front places each maker's desk contract by this hash: a listing is
        // served only where that contract is approved, and on Ethereum it also places the quote.
        let Some(cell_codehash) = cell_codehash else {
            return Err(FeedError::InvalidInput(
                "Efficient PropAMM needs the desk contract code hash: it places each maker's desk \
                 contract, which a listing is served by only where approved"
                    .into(),
            ));
        };
        let cell_codehash = Some(B256::try_from(cell_codehash.as_ref()).map_err(|e| {
            FeedError::InvalidInput(format!(
                "Efficient PropAMM desk contract code hash {cell_codehash} is not 32 bytes: {e}"
            ))
        })?);
        if implementations.is_empty() {
            return Err(FeedError::InvalidInput(
                "Efficient PropAMM has no front implementation on its allow-list".into(),
            ));
        }
        let implementations = implementations
            .iter()
            .map(|implementation| address(implementation, "front implementation"))
            .collect::<Result<Vec<_>, _>>()?;
        // A poll's books are published when its fetch ends, at most a request timeout after it
        // starts; a published snapshot is served until the next one or until it is withdrawn, at
        // most its age limit; and a route priced from it then needs time to land. A book must
        // hold for all of that.
        let Some(max_snapshot_age) = self.feed_config.max_snapshot_age else {
            return Err(FeedError::InvalidInput(
                "Efficient PropAMM books must be withdrawn once stale: set max_snapshot_age".into(),
            ));
        };
        let served = max_snapshot_age + landing_allowance;
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let hold_ms = millis(self.feed_config.request_timeout + served);
        let door_codehash = self
            .door_code_hash
            .map(|hash| {
                B256::try_from(hash.as_ref()).map_err(|e| {
                    FeedError::InvalidInput(format!(
                        "Efficient PropAMM door code hash {hash} is not a hash of 32 bytes: {e}"
                    ))
                })
            })
            .transpose()?;
        let mut reader = DoorsReader::new(
            rpc_url,
            self.book_config.chain,
            address(front, "front")?,
            address(factory, "factory")?,
            cell_codehash,
            implementations,
            self.rungs,
            self.min_tip_cap_wei,
            self.price_sources,
            credit,
            desks,
            next_block,
        );
        reader.door_codehash = door_codehash;
        Ok(EfficientPropFeed {
            feed_config: self.feed_config,
            source: EfficientPropBookSource {
                reader,
                assembler: EfficientPropBookAssembler {
                    protocol_system: self.protocol_system,
                    book_config: self.book_config,
                    usd_quote_tokens: Arc::new(self.usd_quote_tokens),
                    landing_ms: millis(landing_allowance),
                    gas_chain,
                },
                hold_ms,
                served_ms: millis(served),
                clock: Arc::new(unix_now_ms),
                last_block: Arc::new(AtomicU64::new(0)),
                highest_listing: Arc::new(AtomicU32::new(0)),
            },
        })
    }
}

fn address(bytes: &Bytes, what: &str) -> Result<Address, FeedError> {
    Address::try_from(bytes.as_ref()).map_err(|e| {
        FeedError::InvalidInput(format!("Efficient PropAMM {what} {bytes} is not an address: {e}"))
    })
}

impl SnapshotFeed for EfficientPropFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}

/// One poll: every door read from the chain and turned into books.
#[derive(Clone, derive_more::Debug)]
pub(super) struct EfficientPropBookSource {
    pub(super) reader: DoorsReader,
    pub(super) assembler: EfficientPropBookAssembler,
    /// The current Unix time in milliseconds.
    #[debug(skip)]
    pub(super) clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// How long past a poll's start, in ms, its books must hold: the request timeout, the
    /// snapshot's age limit and the landing allowance.
    pub(super) hold_ms: u64,
    /// How long past a poll's end, in ms, its books may be served and a route priced from them
    /// land: the snapshot's age limit and the landing allowance.
    pub(super) served_ms: u64,
    /// The newest block a poll has read, so that a node answering from an older one is caught.
    pub(super) last_block: Arc<AtomicU64>,
    /// The highest listing id a poll has found reserved, which the next poll scans past.
    pub(super) highest_listing: Arc<AtomicU32>,
}

impl HttpSource for EfficientPropBookSource {
    type Snapshot = BookSnapshot<ReceivedAt>;

    /// A read that fails fails the poll, and so does one from a block older than a previous
    /// poll's or more than [`MAX_READ_LAG_SECS`] behind the wall clock, at the poll's start (at
    /// the read's first request, before any door is asked) or at its end: a load-balanced or
    /// lagging node would otherwise roll the books back or publish quotes of replaced words. On
    /// Ethereum, where a poll with no map for the next block serves nothing without failing, a
    /// node whose latest block is more than two slots and that lag behind the poll's start fails
    /// it too, so a stalled node is warned of rather than silent. Doors whose sides quoted nothing
    /// are left out, so a poll where no listing fills publishes no book; so does a front with no
    /// door open yet. The read samples the doors for the books' whole life, [`Self::hold_ms`] from
    /// the poll's start; a poll that ends too late for its books to be served and land within it
    /// fails.
    async fn fetch(&self) -> Result<BookSnapshot<ReceivedAt>, FeedError> {
        let polled = self.poll().await;
        telemetry::record_poll(match &polled {
            Ok((_, outcome)) => *outcome,
            Err(_) => PollOutcome::Failed,
        });
        polled.map(|(snapshot, _)| snapshot)
    }
}

impl EfficientPropBookSource {
    /// One poll, and how it ended where it publishes: see [`HttpSource::fetch`] of this source.
    async fn poll(&self) -> Result<(BookSnapshot<ReceivedAt>, PollOutcome), FeedError> {
        let started_ms = (self.clock)();
        let scan_to = self
            .highest_listing
            .load(Ordering::Relaxed)
            .saturating_add(LISTING_LOOKAHEAD)
            .min(MAX_LISTING);
        let tokens = &self.assembler.book_config.tokens;
        let read = self
            .reader
            .read(
                scan_to,
                |token| {
                    tokens
                        .get(&Bytes::from(token.to_vec()))
                        .map(|token| token.decimals)
                },
                Hold {
                    from_ms: started_ms,
                    for_ms: self.hold_ms,
                    max_lag_secs: MAX_READ_LAG_SECS,
                    min_side_ms: self
                        .assembler
                        .landing_ms
                        .saturating_add(self.assembler.min_side_ms()),
                },
            )
            .await?;
        let now_ms = (self.clock)();
        let now = now_ms / 1000;
        if now > read.timestamp + MAX_READ_LAG_SECS {
            return Err(lagging(now, read.block_number, read.timestamp));
        }
        // A builder chain's poll that found no map for the next block serves nothing: its block
        // is the node's latest, which may lag a block served before, and is not counted as read.
        let newest = if read.answered_at_block {
            self.last_block
                .fetch_max(read.block_number, Ordering::Relaxed)
        } else {
            read.block_number
        };
        if read.block_number < newest {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM node answered from block {}, older than block {newest} already \
                 read",
                read.block_number
            )));
        }
        self.highest_listing
            .fetch_max(read.highest_listing, Ordering::Relaxed);
        if !read.block_lives && now_ms.saturating_add(self.served_ms) > read.until_ms {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM read ended at {now_ms} ms, too late for books sampled to hold \
                 until {} ms to be served and land",
                read.until_ms
            )));
        }
        Ok((BookSnapshot::received_now(self.assembler.books(&read, now_ms)), read.outcome))
    }
}

/// The current Unix time in milliseconds.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// Turns one [`DoorsRead`] into books: one per door between served tokens that quotes on at least
/// one side and clears the TVL floor.
#[derive(Debug, Clone)]
pub(super) struct EfficientPropBookAssembler {
    /// The protocol system stamped on the components: the listings' own, or the fallback-router
    /// one for a feed built with `with_fallback_router()`.
    pub(super) protocol_system: &'static str,
    pub(super) book_config: BookFeedConfig,
    pub(super) usd_quote_tokens: Arc<HashSet<Bytes>>,
    /// The landing allowance, ms: how long a route sent takes to land.
    pub(super) landing_ms: u64,
    /// The chain whose measured gas the states charge.
    pub(super) gas_chain: GasChain,
}

impl EfficientPropBookAssembler {
    /// The least time a side is served for on this chain ([`MIN_SIDE_SERVED_MS`],
    /// [`BNB_MIN_SIDE_SERVED_MS`]).
    pub(super) fn min_side_ms(&self) -> u64 {
        if self.gas_chain == GasChain::Bnb {
            BNB_MIN_SIDE_SERVED_MS
        } else {
            MIN_SIDE_SERVED_MS
        }
    }

    /// The books of `read`, each side judged against the life its samples were read for, the wall
    /// clock reading `now_ms`.
    pub(super) fn books(&self, read: &DoorsRead, now_ms: u64) -> Books {
        let states: Vec<_> = read
            .doors
            .iter()
            .filter_map(|door| self.state(door, read, now_ms))
            .collect();
        let mut books = Books::new();
        for state in &states {
            let (base, quote) = (&state.base_token.address, &state.quote_token.address);
            let component_id = component_id(self.protocol_system, base, quote, &state.door);
            let Some(usd_per_quote) = self
                .usd_per_token(quote, &states)
                .or_else(|| self.usd_per_quote_through_base(state, &states))
            else {
                debug!(
                    %component_id,
                    "skipping listing, no listing prices its quote token or its base in a USD \
                     quote token"
                );
                continue;
            };
            let tvl_usd = sim::to_human(&state.notional_in_quote(), state.quote_token.decimals) *
                usd_per_quote;
            if !self
                .book_config
                .clears_min_tvl(tvl_usd, &component_id)
            {
                continue;
            }
            let door = state.door.clone();
            let door_read = read
                .doors
                .iter()
                .find(|read| read.door.as_slice() == door.as_ref());
            let tip_cap_wei = door_read.and_then(|read| read.tip_cap_wei);
            // What its fills are measured to cost, beside each swap's charge: each figure only
            // where one measured on the listing states it. By the listing's credit mode, not by
            // the sizes this book serves: a credit listing cut to its custody sizes (it shares
            // its pool) still fills through the route where custody falls short at the fill.
            let outlook = state.gas_outlook(door_read.is_some_and(|read| read.credit.is_some()));
            let mut book = pair_book(
                self.protocol_system,
                PROTOCOL_TYPE,
                self.book_config.chain,
                component_id,
                state.clone(),
                None,
            );
            book.component
                .static_attributes
                .insert(PAMM_ADDRESS_ATTRIBUTE.to_string(), door);
            if let Some(cap) = tip_cap_wei {
                book.component.static_attributes.insert(
                    TIP_CAP_WEI_ATTRIBUTE.to_string(),
                    Bytes::from(cap.to_be_bytes_trimmed_vec()),
                );
            }
            let number = |number: u64| Bytes::from(U256::from(number).to_be_bytes_trimmed_vec());
            // On a builder chain the read's block is the one its quotes were sampled for: the
            // block after the node's latest.
            if read.block_lives {
                book.component
                    .static_attributes
                    .insert(VALID_FROM_BLOCK_ATTRIBUTE.to_string(), number(read.block_number));
            }
            for (attribute, until) in [
                (BID_VALID_UNTIL_MS_ATTRIBUTE, state.bid_until),
                (ASK_VALID_UNTIL_MS_ATTRIBUTE, state.ask_until),
            ] {
                if let Some(until) = until {
                    book.component
                        .static_attributes
                        .insert(attribute.to_string(), number(until.unix_ms));
                }
            }
            for (attribute, gas) in [
                (EXPECTED_GAS_ATTRIBUTE, outlook.expected),
                (GAS_LIMIT_ADDITION_ATTRIBUTE, outlook.limit),
            ] {
                if let Some(gas) = gas {
                    book.component
                        .static_attributes
                        .insert(attribute.to_string(), number(gas));
                }
            }
            books.insert(book);
        }
        books
    }

    /// The door's state, or `None` when its tokens are not served or neither side quotes. A side
    /// quotes nothing when its word or depth runs out before the end of its life on the front's
    /// clock (the book's, or sooner where its print lapses within it), since a route priced from
    /// it could not land before the listing stops filling; or when its samples do not hold until
    /// then.
    ///
    /// A side whose print lapses within the book's life quotes only until the last moment a route
    /// sent then lands while the print lives: its life's end, less the landing allowance and how
    /// far the front's clock leads the wall clock. A side that would quote for less than
    /// [`MIN_SIDE_SERVED_MS`] from now quotes nothing.
    fn state(&self, door: &DoorRead, read: &DoorsRead, now_ms: u64) -> Option<EfficientPropState> {
        let base = Bytes::from(door.base.to_vec());
        let quote = Bytes::from(door.quote.to_vec());
        let (base_token, quote_token) = self
            .book_config
            .pair_tokens(&base, &quote)?;
        let mut deadlines = [None, None];
        let mut side = |samples: &Samples,
                        route_from: Option<U256>,
                        runs_out_ms: u64,
                        holds: bool,
                        side_until_ms: u64,
                        name: &str| {
            let quotes = samples
                .iter()
                .any(|(_, output)| output.is_some());
            // The reader ends a side's own life no later than the book's.
            let until_ms = side_until_ms;
            if quotes && (read.block_lives || until_ms < read.until_ms) {
                let deadline_ms = until_ms
                    .saturating_sub(read.lead_ms)
                    .saturating_sub(self.landing_ms);
                if deadline_ms < now_ms.saturating_add(self.min_side_ms()) {
                    debug!(
                        listing = door.listing,
                        door = %door.door,
                        side = name,
                        until_ms,
                        "skipping side whose life ends too soon for a route to be sent and land: its \
                         print lapses, or its word or depth runs out"
                    );
                    return Ladder::from_samples(&[], None);
                }
                deadlines[usize::from(name == "ask")] =
                    Some(Deadline::at_unix_ms(deadline_ms, now_ms));
            }
            if quotes && runs_out_ms < until_ms {
                debug!(
                    listing = door.listing,
                    door = %door.door,
                    side = name,
                    runs_out_ms,
                    until_ms,
                    "skipping side whose word or depth runs out before a route could land"
                );
                return Ladder::from_samples(&[], None);
            }
            if quotes && !holds {
                debug!(
                    listing = door.listing,
                    door = %door.door,
                    side = name,
                    "skipping side whose later quotes the node did not show where its levels age \
                     or it reads a print, or whose print the door refuses within the book's life"
                );
                return Ladder::from_samples(&[], None);
            }
            Ladder::from_samples(samples, route_from)
        };
        let credit = door.credit.as_ref();
        let bid = side(
            &door.bid,
            credit.and_then(|credit| credit.bid_route_from),
            door.bid_runs_out_ms,
            door.bid_holds,
            door.bid_until_ms,
            "bid",
        );
        let ask = side(
            &door.ask,
            credit.and_then(|credit| credit.ask_route_from),
            door.ask_runs_out_ms,
            door.ask_holds,
            door.ask_until_ms,
            "ask",
        );
        let [bid_until, ask_until] = deadlines;
        if bid.is_empty() && ask.is_empty() {
            debug!(
                listing = door.listing,
                door = %door.door,
                "skipping listing that fills nothing"
            );
            return None;
        }
        let state = EfficientPropState::new(
            base_token.clone(),
            quote_token.clone(),
            Bytes::from(door.door.to_vec()),
            bid,
            ask,
            door.reads,
            door.pair,
            self.gas_chain,
            credit
                .map(|credit| credit.gas)
                .or(door.named_gas),
        )
        .with_pool_lacking(door.pool_lacks.0, door.pool_lacks.1)
        .with_fallback_router(self.protocol_system == FALLBACK_PROTOCOL_SYSTEM)
        .with_deadlines(bid_until, ask_until);
        if !state.has_gas() {
            debug!(
                listing = door.listing,
                door = %door.door,
                "skipping listing whose pair's gas was not measured, with no gas configured"
            );
            return None;
        }
        Some(state)
    }

    /// What one whole `token` is worth in USD: one when it is a USD quote token, else
    /// the rate at which a listing among `states` sells it for one.
    fn usd_per_token(&self, token: &Bytes, states: &[EfficientPropState]) -> Option<f64> {
        if self.usd_quote_tokens.contains(token) {
            return Some(1.0);
        }
        states
            .iter()
            .filter(|state| {
                state.base_token.address == *token &&
                    self.usd_quote_tokens
                        .contains(&state.quote_token.address)
            })
            .find_map(|state| {
                state
                    .spot_price(&state.base_token, &state.quote_token)
                    .ok()
            })
    }

    /// What one whole quote token of `state` is worth in USD where no listing prices it: its
    /// base's worth ([`Self::usd_per_token`]) over the rate `state` itself trades the two at, the
    /// bid's where it quotes, else the ask's. A listing quoted in a token no listing sells for a
    /// USD quote token (wstETH/WETH beside wstETH/USDC) is priced so.
    fn usd_per_quote_through_base(
        &self,
        state: &EfficientPropState,
        states: &[EfficientPropState],
    ) -> Option<f64> {
        let usd_per_base = self.usd_per_token(&state.base_token.address, states)?;
        let quote_per_base = state
            .spot_price(&state.base_token, &state.quote_token)
            .ok()
            .or_else(|| {
                let base_per_quote = state
                    .spot_price(&state.quote_token, &state.base_token)
                    .ok()?;
                (base_per_quote > 0.0).then_some(1.0 / base_per_quote)
            })?;
        (quote_per_base > 0.0 && quote_per_base.is_finite())
            .then_some(usd_per_base / quote_per_base)
    }
}

/// The id of a listing door's book: the protocol system its feed stamps on its components, the
/// pair's base and quote, then the door, since two listings may trade one pair.
fn component_id(protocol_system: &str, base: &Bytes, quote: &Bytes, door: &Bytes) -> Bytes {
    let pair = pair_component_id(protocol_system, base, quote);
    Bytes::from([pair.as_ref(), door.as_ref()].concat())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::simulation::errors::SimulationError;

    use super::*;
    use crate::{
        book::{
            chain_without_id, component::PairState as _, sim::SwapDirection, Book,
            SWAP_DIRECTIONS_ATTRIBUTE,
        },
        pamm::protocols::efficient_prop::{
            state::PairPlace,
            test_support::{
                approved_desks, book_config, bytes, cbbtc_address, credit_gas, deployment,
                desk_cell, door_of, named_gas, spawn_recording_proxy, token, usdc, usdc_address,
                weth, weth_address, DoorCheck, FakeAging, FakeClock, FakeHold, FakeListing,
                FakeNode, FakeSide, Overrides, RecordedNode, RecordedPoll, APPROVED_SOURCE,
                FACTORY, FENCE_PRINT, FENCE_SESSION, FENCE_WORD, FRONT, MAX_TTL_MS, NODE_TIMESTAMP,
            },
        },
    };

    /// A source polling `node_url`, its wall clock stopped at `now_ms`, Unix milliseconds, with
    /// the fake front's price source approved.
    fn source(node_url: &str, book_config: BookFeedConfig, now_ms: u64) -> EfficientPropBookSource {
        source_approving(node_url, book_config, now_ms, &[APPROVED_SOURCE])
    }

    /// The same, approving the price sources `approved`.
    fn source_approving(
        node_url: &str,
        book_config: BookFeedConfig,
        now_ms: u64,
        approved: &[u32],
    ) -> EfficientPropBookSource {
        let mut source = base_builder(book_config, node_url.to_string())
            .approved_price_sources(approved.iter().copied())
            .build_path()
            .unwrap()
            .source;
        source.clock = Arc::new(move || now_ms);
        source
    }

    /// A builder of the fake front on Base approving the desk contracts of its first 64 desk
    /// ids and naming [`named_gas`] for its first 64 listings: on Base a listing is served only
    /// with its desk approved and its gas named.
    fn base_builder(book_config: BookFeedConfig, node_url: String) -> EfficientPropFeedBuilder {
        (1..=64).fold(
            EfficientPropFeedBuilder::new(book_config, node_url, deployment())
                .with_approved_desks(approved_desks()),
            |builder, listing| builder.credit_listing(listing, named_gas()),
        )
    }

    /// The books one poll of `node` publishes, with the wall clock at the node's block timestamp.
    async fn fetch(
        node: FakeNode,
        book_config: BookFeedConfig,
    ) -> Result<HashMap<String, Book>, FeedError> {
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let snapshot = source(&server.url(), book_config, now)
            .fetch()
            .await?;
        Ok(Arc::try_unwrap(snapshot.books).unwrap())
    }

    async fn books(node: FakeNode) -> HashMap<String, Book> {
        fetch(node, book_config())
            .await
            .unwrap()
    }

    /// A door's book id on the direct path.
    fn door_component_id(base: &Bytes, quote: &Bytes, door: &Bytes) -> Bytes {
        component_id(PROTOCOL_SYSTEM, base, quote, door)
    }

    fn id(listing: u32) -> Bytes {
        door_component_id(&bytes(weth_address()), &bytes(usdc_address()), &bytes(door_of(listing)))
    }

    fn eth(value: u64) -> BigUint {
        BigUint::from(value) * BigUint::from(10u64).pow(15)
    }

    /// The fallback router's figures were measured through Ethereum's router alone, so a feed on
    /// Base refuses it.
    #[test]
    fn the_builder_refuses_the_fallback_router_on_base() {
        let error = base_builder(book_config(), "http://node".to_string())
            .with_fallback_router()
            .build_path()
            .unwrap_err();

        assert!(
            matches!(&error, FeedError::InvalidInput(m) if m.contains("Ethereum only")),
            "{error}"
        );
    }

    /// One book per open door, labelled for the generic pAMM encoder with the door, priced
    /// from the door's own quotes.
    #[tokio::test]
    async fn every_open_door_is_published_as_a_book() {
        let books = books(FakeNode::new(vec![FakeListing::weth_usdc(1)])).await;

        assert_eq!(books.len(), 1);
        let book = &books[&id(1).to_string()];
        assert_eq!(book.component.protocol_system, "efficient_prop");
        assert_eq!(book.component.protocol_type_name, "efficient_prop_listing");
        assert_eq!(book.component.static_attributes["pamm_address"], bytes(door_of(1)));
        assert!(!book
            .component
            .static_attributes
            .contains_key(SWAP_DIRECTIONS_ATTRIBUTE));
        let tokens: Vec<_> = book
            .component
            .tokens
            .iter()
            .map(|token| token.address.clone())
            .collect();
        assert_eq!(tokens, [weth().address, usdc().address]);
        // The bid fills up to 1 WETH at 2,990 and the ask up to 3,010 USDC at 3,010.
        assert_eq!(
            book.state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (eth(1_000), BigUint::from(2_990_000_000u64))
        );
        assert_eq!(
            book.state
                .get_limits(usdc().address, weth().address)
                .unwrap(),
            (BigUint::from(3_010_000_000u64), eth(1_000))
        );
        let half = book
            .state
            .get_amount_out(eth(500), &weth(), &usdc())
            .unwrap();
        assert_eq!(half.amount, BigUint::from(1_495_000_000u64));
    }

    /// The listing fills from 3 USDC: an ask from 3 USDC in, a bid from the WETH that pays 3 USDC
    /// at 2,990, 1,003,344,481,605,352 wei. Each side is sampled from there up, however deep the
    /// listing, so the state quotes from there; one unit less the door refuses, and so does the
    /// state.
    #[rstest]
    #[case::one_weth_deep(1)]
    #[case::a_thousand_weth_deep(1_000)]
    #[tokio::test]
    async fn a_side_is_quoted_from_the_listings_least_fill(#[case] weth_deep: u64) {
        let mut listing = FakeListing::weth_usdc(1);
        listing.bid.max_in = U256::from(weth_deep) * U256::from(10u64).pow(U256::from(18));
        listing.ask.max_in = U256::from(weth_deep * 3_010_000_000);
        let books = books(FakeNode::new(vec![listing])).await;
        let state = &books[&id(1).to_string()].state;

        let bid_floor = BigUint::from(1_003_344_481_605_352u64);
        assert!(state
            .get_amount_out(bid_floor.clone(), &weth(), &usdc())
            .is_ok());
        assert!(state
            .get_amount_out(bid_floor - 1u8, &weth(), &usdc())
            .is_err());
        let ask_floor = BigUint::from(3_000_000u64);
        assert!(state
            .get_amount_out(ask_floor.clone(), &usdc(), &weth())
            .is_ok());
        assert!(state
            .get_amount_out(ask_floor - 1u8, &usdc(), &weth())
            .is_err());
    }

    /// A poll may take two seconds, a published snapshot may then be served for four, and a route
    /// priced from it needs two more to land, so a word that stops standing sooner than eight
    /// seconds after the wall clock at the poll's start publishes no book; one that stands
    /// exactly that long does. The wall clock counts to the millisecond, however far into the
    /// block's second the read lands.
    #[rstest]
    #[case::standing_past_the_books_life(0, 0, true)]
    #[case::running_out_within_it(0, 1, false)]
    #[case::read_late_in_the_second_standing(999, 0, true)]
    #[case::read_late_in_the_second_running_out(999, 1, false)]
    #[tokio::test]
    async fn a_word_running_out_before_a_route_could_land_is_left_out(
        #[case] into_the_second_ms: u64,
        #[case] older_ms: u64,
        #[case] served: bool,
    ) {
        let now_ms = NODE_TIMESTAMP * 1000 + into_the_second_ms;
        let mut listing = FakeListing::weth_usdc(1);
        // The word's first millisecond out is its maker time plus its life plus one.
        listing.maker_ms = now_ms + BOOK_LIFE_MS - MAX_TTL_MS - 1 - older_ms;
        let server = FakeNode::new(vec![listing])
            .spawn()
            .await;

        let books = source(&server.url(), book_config(), now_ms)
            .fetch()
            .await
            .unwrap()
            .books;

        assert_eq!(books.len(), usize::from(served));
    }

    /// How long past the read, by default, a book must hold: a poll's request timeout, the
    /// snapshot's age limit and the landing allowance, 2 + 4 + 2 seconds.
    const BOOK_LIFE_MS: u64 = 8_000;

    /// The book of `listing` that `books` publishes.
    fn state_of(books: &HashMap<String, Book>, listing: u32) -> &EfficientPropState {
        books[&id(listing).to_string()]
            .state
            .as_any()
            .downcast_ref::<EfficientPropState>()
            .unwrap()
    }

    /// Holds every size a book quotes on both sides, at its samples and seven between each two,
    /// to the door's quote at every block of the book's life, `life` (the front's clock at each):
    /// never above it, and never a size the door refuses.
    fn assert_holds(listing: &FakeListing, state: &EfficientPropState, life: &[u64]) {
        use crate::book::sim::SwapDirection;
        // Whether a side quotes yet on this process's clock is not what is held here.
        let state = &state.clone().with_deadlines(None, None);
        let (base, quote) = (token(listing.base), token(listing.quote));
        for (direction, token_in, token_out) in [
            (SwapDirection::BaseToQuote, base.clone(), quote.clone()),
            (SwapDirection::QuoteToBase, quote, base),
        ] {
            let points: Vec<(U256, U256)> = state.side(direction).clone().into();
            assert!(!points.is_empty(), "{direction:?} quotes nothing");
            let mut sizes = Vec::new();
            for pair in points.windows(2) {
                let ((a0, _), (a1, _)) = (pair[0], pair[1]);
                sizes.extend((0u8..8).map(|i| a0 + (a1 - a0) * U256::from(i) / U256::from(8u8)));
            }
            sizes.push(points[points.len() - 1].0);
            for &now_ms in life {
                for &amount in &sizes {
                    let book = state
                        .get_amount_out(
                            BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
                            &token_in,
                            &token_out,
                        )
                        .unwrap()
                        .amount;
                    let door = listing
                        .quote_at(
                            Address::from_slice(&token_in.address),
                            Address::from_slice(&token_out.address),
                            amount,
                            now_ms,
                        )
                        .unwrap_or_else(|| {
                            panic!("{direction:?}: the door refuses {amount} at {now_ms} ms")
                        });
                    assert!(
                        book <= BigUint::from_bytes_be(&door.to_be_bytes::<32>()),
                        "{direction:?} at {amount}, at {now_ms} ms: the book quotes {book}, the \
                         door {door}"
                    );
                }
            }
        }
    }

    /// The book of `listing`, of any pair, that `books` publishes.
    fn state_for<'a>(
        books: &'a HashMap<String, Book>,
        listing: &FakeListing,
    ) -> &'a EfficientPropState {
        let id = door_component_id(
            &bytes(listing.base),
            &bytes(listing.quote),
            &bytes(door_of(listing.id)),
        );
        books[&id.to_string()]
            .state
            .as_any()
            .downcast_ref::<EfficientPropState>()
            .unwrap()
    }

    /// A cbBTC/USDC listing: its base is the pair's higher address, its token 1, and its pool's
    /// token `base_index`, which `doorView` reports, 0 on a cbBTC pool, 2 on a WETH pool holding
    /// USDC and cbBTC beside it. And WETH/USDC, its base the lower address, on its own pool and as
    /// a further listing of a USDC pool, where WETH is the pool's token 1.
    fn pair_cases() -> [(Address, u8); 4] {
        [(weth_address(), 0), (weth_address(), 1), (cbbtc_address(), 0), (cbbtc_address(), 2)]
    }

    /// Listing 1 of base `base` against USDC, the base its pool's token `base_index`.
    fn listing_of(base: Address, base_index: u8) -> FakeListing {
        let mut listing = FakeListing::weth_usdc(1);
        listing.base = base;
        listing.base_index = base_index;
        listing
    }

    /// Each side's levels age from its own depth: here the bid's depth was posted 2 s after the
    /// word and the ask's with it, so the bid's premium closes 2 s after the ask's, both within the
    /// book's life. The book holds at every block of its life whichever of the pair's addresses is
    /// the base and wherever it sits in its pool.
    #[tokio::test]
    async fn a_side_is_sampled_at_its_own_premium_edge_whichever_token_is_the_base() {
        for (base, base_index) in pair_cases() {
            let mut listing = listing_of(base, base_index);
            listing.aging = FakeAging {
                bid_drift: 100,
                ask_drift: 100,
                premium: 1_000,
                window_ms: 3_300,
                bid_nonce_ms: listing.maker_ms + 2_000,
                ..FakeAging::default()
            };
            let mut node = FakeNode::new(vec![listing.clone()]);
            node.clock = FakeClock::counted(0);
            let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

            let books = books(node).await;

            assert_holds(&listing, state_for(&books, &listing), &life);
        }
    }

    /// A premium holds from the latest of the word's maker time, the side's depth nonce and the
    /// controls' nonce: here the controls were posted 2 s after the word, so both sides' premium
    /// closes 2 s later than the word alone would close it, within the book's life.
    #[tokio::test]
    async fn a_premium_is_sampled_from_the_controls_post_where_it_is_the_latest() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.aging = FakeAging {
            bid_drift: 100,
            ask_drift: 100,
            premium: 1_000,
            window_ms: 3_300,
            controls_nonce_ms: listing.maker_ms + 2_000,
            ..FakeAging::default()
        };
        let mut node = FakeNode::new(vec![listing.clone()]);
        node.clock = FakeClock::counted(0);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// Without the node's overrides, a side whose levels drift is left out and one whose levels do
    /// not is served, whichever of the pair's addresses is the base and wherever it sits in its
    /// pool; so is a side whose depth expires within the book's life, beside one whose does not.
    #[rstest]
    #[case::the_bid_drifting(|listing: &mut FakeListing| listing.aging.bid_drift = 100)]
    #[case::the_bids_depth_expiring(|listing: &mut FakeListing| {
        listing.bid_depth_expires = NODE_TIMESTAMP + 3;
    })]
    #[tokio::test]
    async fn without_overrides_the_side_that_ages_or_runs_out_is_the_one_left_out(
        #[case] change: fn(&mut FakeListing),
    ) {
        use crate::book::sim::SwapDirection;
        for (base, base_index) in pair_cases() {
            let mut listing = listing_of(base, base_index);
            change(&mut listing);
            let mut node = FakeNode::new(vec![listing.clone()]);
            node.clock = FakeClock::counted(600);
            node.overrides = Overrides::Refused;

            let books = books(node).await;

            let state = state_for(&books, &listing);
            let case = format!("base {base}, pool token {base_index}");
            assert!(
                state
                    .side(SwapDirection::BaseToQuote)
                    .is_empty(),
                "{case}: the bid is served"
            );
            assert!(
                !state
                    .side(SwapDirection::QuoteToBase)
                    .is_empty(),
                "{case}: the ask is out"
            );
        }
    }

    /// A listing's levels may widen or drift with its word's age, and a premium may hold them back
    /// for a window after a post, so a door quotes less later in a book's life than at the read, or
    /// more after the window: the book holds at every block of its life, on each clock a front is
    /// served on. On Base a block comes every 200 ms and the front counts them; where it does not
    /// count them (in its mode 1, or where the count is a second or more off) it reads BaseTime at
    /// each block's own milliseconds; on a chain a block a second the count leaves the block's
    /// second and the front reads BaseTime, part way into each second; with neither, the start of
    /// each second. A premium may close at any millisecond, early in a second or between two
    /// blocks.
    #[rstest]
    #[case::drifting(FakeAging { bid_drift: 100, ask_drift: 100, ..FakeAging::default() })]
    #[case::widening(FakeAging { widen: 200, ..FakeAging::default() })]
    #[case::widening_past_a_grace(FakeAging { widen: 200, grace_ms: 3_000, ..FakeAging::default() })]
    #[case::drifting_past_a_premium(FakeAging {
        bid_drift: 10,
        ask_drift: 10,
        premium: 100,
        window_ms: 5_000,
        ..FakeAging::default()
    })]
    #[case::a_premium_closing_early_in_a_second(FakeAging {
        bid_drift: 100,
        ask_drift: 100,
        premium: 1_000,
        window_ms: 3_300,
        ..FakeAging::default()
    })]
    #[case::a_premium_closing_between_blocks(FakeAging {
        bid_drift: 100,
        ask_drift: 100,
        premium: 1_000,
        window_ms: 3_150,
        ..FakeAging::default()
    })]
    #[case::widening_past_a_premium_closing_late(FakeAging {
        widen: 300,
        premium: 2_000,
        window_ms: 7_550,
        ..FakeAging::default()
    })]
    #[tokio::test]
    async fn a_book_holds_for_its_life_on_a_listing_whose_levels_age(
        #[case] aging: FakeAging,
        #[values(
            FakeClock::seal(),
            FakeClock::base_time(600),
            FakeClock::base_time(0),
            FakeClock::counted(600),
            FakeClock::counted(0),
            FakeClock::base_time_blocks(1, 0),
            FakeClock::base_time_blocks(2, 600)
        )]
        clock: FakeClock,
    ) {
        let mut listing = FakeListing::weth_usdc(1);
        listing.aging = aging;
        let mut node = FakeNode::new(vec![listing.clone()]);
        node.clock = clock;
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A bid whose largest fill is bound by its output (a maker's bucket, or the pool's room): with
    /// a premium of 2,000 ppm on, 1 WETH pays 2,984.02 USDC, under the 2,985 USDC bound; once the
    /// premium's window closes 3 s after the read it pays more, past the bound, and the door
    /// refuses it until its drift of 400 ppm a second brings it back under, about 1.2 s later. The
    /// read, the last block before the window closes and the life's last all fill it, so the side
    /// is also sampled at the first block at or after the window closes, and the size is not
    /// quoted.
    fn output_bound_listing() -> FakeListing {
        let mut listing = FakeListing::weth_usdc(1);
        listing.aging = FakeAging {
            bid_drift: 400,
            ask_drift: 400,
            premium: 2_000,
            window_ms: 3_000,
            ..FakeAging::default()
        };
        listing.bid_out_cap = Some(U256::from(2_985_000_000u64));
        listing
    }

    #[rstest]
    #[tokio::test]
    async fn a_size_refused_once_a_premium_ends_is_not_quoted(
        #[values(
            FakeClock::counted(0),
            FakeClock::counted(600),
            FakeClock::base_time_blocks(2, 200),
            FakeClock::base_time(0),
            FakeClock::seal()
        )]
        clock: FakeClock,
    ) {
        let listing = output_bound_listing();
        let mut node = FakeNode::new(vec![listing.clone()]);
        node.clock = clock;
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let read = NODE_TIMESTAMP * 1000;
        let top = U256::from(10u64.pow(18));
        for (at, fills) in [(read, true), (read + 2_999, true), (read + 3_000, false)] {
            assert_eq!(
                listing
                    .quote_at(weth_address(), usdc_address(), top, at)
                    .is_some(),
                fills
            );
        }

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A premium window may close exactly at the life's last block: the last block before it,
    /// where the premium last holds the levels back and the drift has run longest under it, is
    /// sampled. Here, read at the start of a second on Base's clock, the life's last block is
    /// 7.8 s after the read and the window closes there; at 7.6 s the door takes 2,000 ppm and
    /// 760 ppm of drift off, more than at the read or at the last block.
    #[tokio::test]
    async fn a_premium_closing_at_the_lifes_last_block_is_sampled_before_it() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.aging = FakeAging {
            bid_drift: 100,
            ask_drift: 100,
            premium: 2_000,
            window_ms: 7_800,
            ..FakeAging::default()
        };
        let mut node = FakeNode::new(vec![listing.clone()]);
        node.clock = FakeClock::counted(0);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let read = NODE_TIMESTAMP * 1000;
        assert_eq!(life[life.len() - 1], read + 7_800);
        let top = U256::from(10u64.pow(18));
        let quote = |at: u64| {
            listing
                .quote_at(weth_address(), usdc_address(), top, at)
                .unwrap()
        };
        assert!(quote(read + 7_600) < quote(read) && quote(read + 7_600) < quote(read + 7_800));

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A node that hides later blocks cannot show what a side quotes once its premium's window
    /// closes within the book's life, where an output-bound size is refused: such a side is left
    /// out, though its levels do not otherwise age. One whose window closed before the read is
    /// served.
    #[rstest]
    #[case::closing_within_the_life(3_000, false)]
    #[case::closed_before_the_read(100, true)]
    #[tokio::test]
    async fn without_overrides_a_side_whose_premium_ends_within_the_life_is_left_out(
        #[case] window_ms: u64,
        #[case] served: bool,
    ) {
        let mut listing = output_bound_listing();
        listing.aging = FakeAging { premium: 2_000, window_ms, ..FakeAging::default() };
        let mut node = FakeNode::new(vec![listing.clone()]);
        node.clock = FakeClock::counted(600);
        node.overrides = Overrides::Refused;
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), usize::from(served));
        if served {
            assert_holds(&listing, state_of(&books, 1), &life);
        }
    }

    /// What makes a door quote less where the front's clock reads earlier: nothing, a price print
    /// fed 100 ms before the read's reading (the front refuses a print from its future), or an ask
    /// bucket at its read's room and refilling at 301 USDC a second (less room earlier).
    #[derive(Clone, Copy, Debug)]
    enum Earlier {
        Nothing,
        PrintJustFed,
        AskRefilling,
    }

    /// Listing 1 of `node`, quoting less where its clock reads earlier than at its block, as
    /// `earlier` says.
    fn listing_on(node: &mut FakeNode, earlier: Earlier) -> FakeListing {
        let read_ms = node
            .clock
            .ms(node.block, node.timestamp);
        let mut listing = FakeListing::weth_usdc(1);
        match earlier {
            Earlier::Nothing => {}
            Earlier::PrintJustFed => {
                listing.fence = FENCE_PRINT;
                listing.print_fed_ms = Some(read_ms - 100);
                listing.print_ends_ms = Some(read_ms + 60_000);
            }
            Earlier::AskRefilling => listing.ask_refill = Some((read_ms, 301_000_000)),
        }
        node.listings = vec![listing.clone()];
        listing
    }

    /// Off its anchor by less than a second, behind or ahead, the front's block count lands in the
    /// block's second in some blocks and BaseTime is read in the others, so a block after the read
    /// may read earlier than it: there the door refuses a print fed just before the read, and a
    /// refilling bucket has less room. Such a front serves no book, whatever the node shows.
    #[rstest]
    #[tokio::test]
    async fn a_front_whose_clock_may_step_back_serves_no_book(
        #[values(Earlier::Nothing, Earlier::PrintJustFed, Earlier::AskRefilling)] earlier: Earlier,
        #[values(
            FakeClock::behind_its_anchor(600, 200),
            FakeClock::behind_its_anchor(200, 0),
            FakeClock::ahead_of_its_anchor(600, 200),
            FakeClock::ahead_of_its_anchor(400, 0)
        )]
        clock: FakeClock,
        #[values(Overrides::Honoured, Overrides::Refused)] overrides: Overrides,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        node.overrides = overrides;
        listing_on(&mut node, earlier);

        let books = books(node).await;

        assert!(books.is_empty());
    }

    /// A front whose clock may step back is warned of when the poll first finds it so, and not
    /// on the polls after while it stays so; once its clock is found not to step back, the next
    /// poll that finds it may is warned of again.
    #[tokio::test]
    async fn a_front_whose_clock_may_step_back_is_warned_of_once_until_that_changes() {
        let mut stepping = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        stepping.clock = FakeClock::behind_its_anchor(600, 200);
        let mut steady = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        steady.clock = FakeClock::counted(200);
        let now = stepping.timestamp * 1000;
        let (stepping, steady) = (stepping.spawn().await, steady.spawn().await);
        let stepping = source(&stepping.url(), book_config(), now);

        for _ in 0..3 {
            assert!(stepping
                .fetch()
                .await
                .unwrap()
                .books
                .is_empty());
        }

        let warnings = Arc::clone(&stepping.reader.step_back_warnings);
        assert_eq!(warnings.load(Ordering::SeqCst), 1);
        let mut steady = source(&steady.url(), book_config(), now);
        steady.reader.stepping_back = Arc::clone(&stepping.reader.stepping_back);
        assert_eq!(
            steady
                .fetch()
                .await
                .unwrap()
                .books
                .len(),
            1
        );
        for _ in 0..2 {
            assert!(stepping
                .fetch()
                .await
                .unwrap()
                .books
                .is_empty());
        }
        assert_eq!(warnings.load(Ordering::SeqCst), 2);
    }

    /// The two clocks Base's front reads never read earlier at a later block: its count on its
    /// anchor, a block every 200 ms, or BaseTime alone where the count is a second or more off it,
    /// on Base or on a fork making a block a second. There the same listings are served, and hold
    /// for their life, with or without the node showing later blocks; but for the listing reading
    /// a print, whose print's life only later blocks show, which is left out without them.
    #[rstest]
    #[tokio::test]
    async fn a_front_whose_clock_cannot_step_back_serves_its_books(
        #[values(Earlier::Nothing, Earlier::PrintJustFed, Earlier::AskRefilling)] earlier: Earlier,
        #[values(
            FakeClock::counted(200),
            FakeClock::counted(0),
            FakeClock::base_time_blocks(2, 200),
            FakeClock::base_time(200)
        )]
        clock: FakeClock,
        #[values(Overrides::Honoured, Overrides::Refused)] overrides: Overrides,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        node.overrides = overrides;
        let listing = listing_on(&mut node, earlier);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let served = !matches!((earlier, overrides), (Earlier::PrintJustFed, Overrides::Refused));

        let books = books(node).await;

        if served {
            assert_holds(&listing, state_of(&books, 1), &life);
        } else {
            assert!(books.is_empty());
        }
    }

    /// Without overrides, a side whose levels carry only a premium is served from the block's
    /// samples: as the front's clock reads later the premium only ends, so the door quotes no less
    /// at any later block of the book's life than at the read.
    #[rstest]
    #[case::on_its_anchor(FakeClock::counted(600))]
    #[case::from_base_time(FakeClock::base_time_blocks(2, 600))]
    #[tokio::test]
    async fn without_overrides_a_side_with_a_premium_alone_is_served(#[case] clock: FakeClock) {
        let mut listing = FakeListing::weth_usdc(1);
        listing.aging = FakeAging { premium: 1_000, window_ms: 100, ..FakeAging::default() };
        let mut node = FakeNode::new(vec![listing.clone()]);
        node.clock = clock;
        node.overrides = Overrides::Refused;
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// The front counts its clock from the block number, so a node that moves an override's time
    /// but not its number shows quotes at another moment than the one asked: its later quotes
    /// count as unknown, as a node's that ignores the override.
    #[tokio::test]
    async fn a_node_that_overrides_the_time_alone_shows_no_later_quotes() {
        let mut drifting = FakeListing::weth_usdc(1);
        drifting.aging.bid_drift = 100;
        let mut node = FakeNode::new(vec![drifting]);
        node.clock = FakeClock::counted(600);
        node.overrides = Overrides::TimeOnly;

        let books = books(node).await;

        use crate::book::sim::SwapDirection;
        let state = state_of(&books, 1);
        assert!(state
            .side(SwapDirection::BaseToQuote)
            .is_empty());
        assert!(!state
            .side(SwapDirection::QuoteToBase)
            .is_empty());
    }

    /// A listing whose price reads a print fills only while that print is younger than the
    /// listing allows, and no later print is assumed. A print that outlives the book's life is
    /// served for all of it; one that lapses within it is served with each side's deadline, from
    /// which it quotes nothing: the last moment a route priced from it can be sent and land while
    /// the print lives. Where that leaves less than 200 ms to send one in, the listing is left
    /// out: here a print lapsing 2.1 s after the read, with two seconds to land.
    #[rstest]
    #[case::the_print_outlives_the_book(BOOK_LIFE_MS, true, false)]
    #[case::the_print_lapses_at_the_books_last_moment(BOOK_LIFE_MS - 1, true, false)]
    #[case::the_print_lapses_within_the_book(BOOK_LIFE_MS - 1_001, true, true)]
    #[case::the_print_lapses_soon(4_000, true, true)]
    #[case::the_print_lapses_too_soon(2_100, false, true)]
    #[tokio::test]
    async fn a_listing_is_served_only_while_its_print_lives(
        #[case] print_left_ms: u64,
        #[case] served: bool,
        #[case] cut: bool,
        #[values(FakeClock::seal(), FakeClock::counted(0), FakeClock::counted(600))]
        clock: FakeClock,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let listing = print_listing_on(&mut node, FENCE_PRINT, print_left_ms, None);
        let wall = node.timestamp * 1000;
        let life = node.life(wall, BOOK_LIFE_MS);
        assert!(listing
            .quote_at(weth_address(), usdc_address(), U256::from(10u64.pow(17)), life[0])
            .is_some());

        let books = books(node).await;

        assert_eq!(books.len(), usize::from(served));
        if served {
            assert_serves_its_print(&listing, state_of(&books, 1), &life, wall, cut);
        }
    }

    /// The landing allowance by default, ms.
    const LANDING_MS: u64 = 2_000;

    /// Holds `state`, `listing`'s book read with the wall clock at `wall_ms`, to its print: each
    /// side has a deadline exactly where `cut`, and holds at every block of `life` at which a
    /// route sent before it may land, where the door fills it, so never past the print.
    fn assert_serves_its_print(
        listing: &FakeListing,
        state: &EfficientPropState,
        life: &[u64],
        wall_ms: u64,
        cut: bool,
    ) {
        use crate::book::sim::SwapDirection;
        // The front's clock leads the wall clock at the read by this much, or none.
        let lead = life[0].saturating_sub(wall_ms);
        let mut served = life.len();
        for direction in [SwapDirection::BaseToQuote, SwapDirection::QuoteToBase] {
            let until = state.side_until(direction);
            assert_eq!(until.is_some(), cut, "{direction:?}: {until:?}");
            if let Some(until) = until {
                assert!(until.unix_ms >= wall_ms + 200, "{direction:?}: {until:?}");
                let lands_by = until.unix_ms + lead + LANDING_MS;
                served = served.min(
                    life.iter()
                        .filter(|&&ms| ms < lands_by)
                        .count(),
                );
                let print_ends = listing.print_ends_ms.unwrap();
                assert!(lands_by <= print_ends + 1, "{direction:?}: lands by {lands_by}");
            }
        }
        assert_holds(listing, state, &life[..served]);
    }

    /// A print that lives 4 s, posted every second, is read between 0 and 1 s old: the side is
    /// served at the default settings, a book's life of 8 s, with a deadline that lets a route
    /// sent before it land while the print lives, never past it.
    #[rstest]
    #[tokio::test]
    async fn a_print_living_four_seconds_is_served_at_the_defaults(
        #[values(0, 400, 999)] age_ms: u64,
        #[values(
            FakeClock::seal(),
            FakeClock::counted(0),
            FakeClock::counted(600),
            FakeClock::base_time_blocks(1, 200)
        )]
        clock: FakeClock,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 3_999 - age_ms, None);
        let read_ms = listing.print_ends_ms.unwrap() + age_ms - 3_999;
        listing.print_fed_ms = Some(read_ms - age_ms);
        node.listings = vec![listing.clone()];
        let wall = node.timestamp * 1000;
        let life = node.life(wall, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_serves_its_print(&listing, state_of(&books, 1), &life, wall, true);
    }

    /// Where a print lapses between two seconds' asks, a fourth request asks every size of its
    /// sides at the last ask's block and at each block between it and the next, so the side's
    /// deadline falls at the last block the print lives, not up to a second sooner: on a counted
    /// clock read at the start of a second, a print lapsing 3.1 s after the read is asked at 2.8,
    /// 3.0, 3.2, 3.4 and 3.6 s. A print lapsing so soon that no route could be sent in and land
    /// is left out without it.
    #[rstest]
    #[case::lapsing_within_the_life(3_100, 1, 4, 16)]
    #[case::lapsing_too_soon(1_100, 0, 3, 11)]
    #[tokio::test]
    async fn a_print_lapsing_within_the_life_is_asked_block_by_block(
        #[case] print_left_ms: u64,
        #[case] books_served: usize,
        #[case] requests: usize,
        #[case] eth_calls: usize,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        print_listing_on(&mut node, FENCE_PRINT, print_left_ms, None);
        let calls = Arc::clone(&node.eth_calls);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;

        let books = source(&server.url(), book_config(), now)
            .fetch()
            .await
            .unwrap()
            .books;

        assert_eq!(books.len(), books_served);
        assert_eq!(server.request_count(), requests);
        assert_eq!(calls.load(Ordering::SeqCst), eth_calls);
        if books_served == 1 {
            use crate::book::sim::SwapDirection;
            let state = state_of(&books, 1);
            // The print's last block is 3.0 s after the read: a route sent by 1.0 s lands by it.
            let until = state
                .side_until(SwapDirection::BaseToQuote)
                .unwrap();
            assert_eq!(until.unix_ms, now + 3_001 - LANDING_MS);
        }
    }

    /// A side whose print lapses within the book's life quotes every size it would quote were its
    /// print to outlive the life: a size refused only at an ask after the print lapsed is not
    /// dropped, since the side is not served there.
    #[tokio::test]
    async fn a_side_cut_at_its_print_keeps_its_sizes() {
        let points = |books: &HashMap<String, Book>| {
            use crate::book::sim::SwapDirection;
            let state = state_of(books, 1);
            [SwapDirection::BaseToQuote, SwapDirection::QuoteToBase].map(|direction| {
                let points: Vec<(U256, U256)> = state.side(direction).clone().into();
                points
                    .into_iter()
                    .map(|(amount, _)| amount)
                    .collect::<Vec<_>>()
            })
        };
        let read = |print_left_ms: u64| async move {
            let mut node = FakeNode::new(Vec::new());
            node.clock = FakeClock::counted(0);
            print_listing_on(&mut node, FENCE_PRINT, print_left_ms, None);
            books(node).await
        };
        let outliving = read(60_000).await;
        let cut = read(3_100).await;

        assert!(!points(&outliving)[0].is_empty());
        assert_eq!(points(&cut), points(&outliving));
    }

    /// A side whose print lapses before its premium's window closes is not sampled at the window's
    /// edge: the print is refused there, and the side is not served there.
    #[tokio::test]
    async fn a_premium_closing_after_the_print_lapses_is_not_sampled() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 3_100, None);
        let read_ms = node
            .clock
            .ms(node.block, node.timestamp);
        listing.aging = FakeAging {
            premium: 300,
            window_ms: 5_000,
            controls_nonce_ms: read_ms,
            ..FakeAging::default()
        };
        node.listings = vec![listing.clone()];
        let wall = node.timestamp * 1000;
        let life = node.life(wall, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), 1);
        assert_serves_its_print(&listing, state_of(&books, 1), &life, wall, true);
    }

    /// A side cut at its print keeps the least the door quotes over its own life: its levels drift,
    /// so the door pays less at the print's last block than at the read.
    #[rstest]
    #[tokio::test]
    async fn a_side_cut_at_its_print_holds_as_its_levels_age(
        #[values(FakeClock::seal(), FakeClock::counted(0), FakeClock::counted(600))]
        clock: FakeClock,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 4_100, None);
        listing.aging = FakeAging { bid_drift: 100, ask_drift: 150, ..FakeAging::default() };
        node.listings = vec![listing.clone()];
        let wall = node.timestamp * 1000;
        let life = node.life(wall, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), 1);
        assert_serves_its_print(&listing, state_of(&books, 1), &life, wall, true);
    }

    /// A side cut at its print is sampled at a premium's edge within its own life: the levels
    /// drift under a premium whose window closes 1.5 s after the read, so the door pays the least
    /// at the last block before it closes, neither at the read nor at the print's last block.
    #[tokio::test]
    async fn a_side_cut_at_its_print_is_sampled_at_a_premium_edge_within_its_life() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 3_100, None);
        listing.aging = FakeAging {
            bid_drift: 100,
            ask_drift: 100,
            premium: 300,
            window_ms: 1_500,
            ..FakeAging::default()
        };
        node.listings = vec![listing.clone()];
        let wall = node.timestamp * 1000;
        let life = node.life(wall, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), 1);
        assert_serves_its_print(&listing, state_of(&books, 1), &life, wall, true);
    }

    /// A cut side stops quoting on this process's clock where its deadline says: a print lapsing
    /// 3.1 s after the read, with two seconds to land, quotes for one second more.
    #[tokio::test]
    async fn a_cut_sides_deadline_is_that_far_ahead_on_the_monotonic_clock() {
        use crate::book::sim::SwapDirection;
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        print_listing_on(&mut node, FENCE_PRINT, 3_100, None);

        let books = books(node).await;

        let until = state_of(&books, 1)
            .side_until(SwapDirection::BaseToQuote)
            .unwrap();
        let left = until
            .at
            .saturating_duration_since(std::time::Instant::now());
        assert!(left <= Duration::from_millis(1_001), "{left:?}");
        assert!(left >= Duration::from_millis(500), "{left:?}");
    }

    /// Each side keeps its own deadline: the bid's depth expires 4 s after the read, so the door
    /// refuses every bid from then, while the ask fills for the book's life.
    #[tokio::test]
    async fn each_side_keeps_its_own_deadline() {
        use crate::book::sim::SwapDirection;
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        listing.bid_depth_expires = node.timestamp + 4;
        node.listings = vec![listing.clone()];
        let now = node.timestamp * 1000;

        let books = books(node).await;

        let state = state_of(&books, 1);
        let bid = state
            .side_until(SwapDirection::BaseToQuote)
            .unwrap();
        assert_eq!(bid.unix_ms, now + 3_801 - LANDING_MS);
        assert_eq!(state.side_until(SwapDirection::QuoteToBase), None);
    }

    /// Where the node refuses one block of the fourth request, the side's life ends before it,
    /// even where the door fills at later blocks: no gap in what it shows is bridged. A print
    /// lapsing 3.5 s after the read lives at 2.8, 3.0, 3.2 and 3.4 s; with 3.0 s refused the bid
    /// lives to 2.8 s.
    #[tokio::test]
    async fn a_refused_block_ends_a_sides_life_before_it() {
        use crate::book::sim::SwapDirection;
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let listing = print_listing_on(&mut node, FENCE_PRINT, 3_500, None);
        node.refused_blocks = vec![node.block + 15];
        let now = node.timestamp * 1000;
        let wall = now;
        let life = node.life(wall, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), 1);
        let state = state_of(&books, 1);
        let until = state
            .side_until(SwapDirection::BaseToQuote)
            .unwrap();
        assert_eq!(until.unix_ms, now + 2_801 - LANDING_MS);
        assert_serves_its_print(&listing, state, &life, wall, true);
    }

    /// Listing 1 of `node`, a WETH/USDC listing of fence kind `fence` reading a print that the
    /// front refuses as stale `print_left_ms` after the read, and, where `closed_after_ms` is set,
    /// fills again in a SESSION listing's closed phase from that long after the read, its session
    /// word then holding a `closedAfter` of 60 s.
    fn print_listing_on(
        node: &mut FakeNode,
        fence: u64,
        print_left_ms: u64,
        closed_after_ms: Option<u64>,
    ) -> FakeListing {
        let read_ms = node
            .clock
            .ms(node.block, node.timestamp);
        let mut listing = FakeListing::weth_usdc(1);
        listing.fence = fence;
        listing.print_ends_ms = Some(read_ms + print_left_ms);
        listing.closed_from_ms = closed_after_ms.map(|after| read_ms + after);
        listing.closed_after_s = if closed_after_ms.is_some() { 60 } else { 0 };
        node.listings = vec![listing.clone()];
        listing
    }

    /// Whether `listing`'s door fills a tenth of a WETH at each of `life`'s blocks.
    fn fills_over(listing: &FakeListing, life: &[u64]) -> Vec<bool> {
        life.iter()
            .map(|&ms| {
                listing
                    .quote_at(weth_address(), usdc_address(), U256::from(10u64.pow(17)), ms)
                    .is_some()
            })
            .collect()
    }

    /// A listing that reads a price print fills while the print is young enough, and a SESSION
    /// listing fills again in its closed phase, once its print is stale for its closed-phase
    /// delay; between the two the door refuses every size. A print fresh at the read that the
    /// door refuses later in the book's life is served only until its deadline, and a SESSION
    /// listing with a closed phase not at all, though the door fills at the read and at the
    /// life's end. Here the print lapses 3 s after the read, and the SESSION listing's closed phase
    /// starts 5 s after it. A node that hides later blocks cannot show the print's life at all.
    #[rstest]
    #[case::a_session_listing_refusing_until_its_closed_phase(FENCE_SESSION, Some(5_000))]
    #[case::a_print_listing_going_stale(FENCE_PRINT, None)]
    #[tokio::test]
    async fn a_listing_whose_print_refuses_within_the_books_life_is_left_out(
        #[case] fence: u64,
        #[case] closed_after_ms: Option<u64>,
        #[values(FakeClock::counted(0), FakeClock::base_time_blocks(1, 200), FakeClock::seal())]
        clock: FakeClock,
        #[values(Overrides::Honoured, Overrides::Refused)] overrides: Overrides,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        node.overrides = overrides;
        let listing = print_listing_on(&mut node, fence, 3_000, closed_after_ms);
        let wall = node.timestamp * 1000;
        let life = node.life(wall, BOOK_LIFE_MS);
        let fills = fills_over(&listing, &life);
        assert!(fills[0] && fills.contains(&false));
        assert_eq!(fills[fills.len() - 1], closed_after_ms.is_some());

        let books = books(node).await;

        // A PRINT listing is served for as long as its print lives, which the node shows only
        // under its overrides; a SESSION listing with a closed phase never.
        let served = fence == FENCE_PRINT && overrides == Overrides::Honoured;
        assert_eq!(books.len(), usize::from(served));
        if served {
            assert_serves_its_print(&listing, state_of(&books, 1), &life, wall, true);
        }
    }

    /// A listing reading a print whose print outlives the book fills at every block of the life,
    /// so it is served and holds for it: a PRINT listing, and a SESSION listing with no closed
    /// phase, which reads its print as a PRINT listing does.
    #[rstest]
    #[case::a_print_listing(FENCE_PRINT)]
    #[case::a_session_listing_with_no_closed_phase(FENCE_SESSION)]
    #[tokio::test]
    async fn a_listing_whose_print_fills_for_the_books_life_is_served(
        #[case] fence: u64,
        #[values(FakeClock::counted(0), FakeClock::base_time_blocks(1, 200), FakeClock::seal())]
        clock: FakeClock,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let listing = print_listing_on(&mut node, fence, 60_000, None);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        assert!(!fills_over(&listing, &life).contains(&false));

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A SESSION listing with a closed phase (a nonzero `closedAfter` in its session word) is not
    /// served, whichever phase the read and the book's life fall in: its open phase, the gap
    /// between its print's lapse and its closed phase, its closed phase, or a switch from the open
    /// phase straight to the closed one. In its closed phase the band eases as the print ages
    /// while the word's levels tighten, so the door may quote less between two samples than at
    /// either, and the gap may be shorter than a second. Here the print lapses and the closed
    /// phase starts at the given offsets from the read, ms; in the open and closed phases and
    /// across the switch the door fills at every block of the life.
    #[rstest]
    #[case::in_its_open_phase(60_000, 120_000, true)]
    #[case::in_its_gap(-1_000, 3_000, false)]
    #[case::in_its_closed_phase(-10_000, -5_000, true)]
    #[case::switching_from_open_to_closed(3_000, 3_001, true)]
    #[tokio::test]
    async fn a_session_listing_with_a_closed_phase_is_left_out(
        #[case] print_ends_ms: i64,
        #[case] closed_from_ms: i64,
        #[case] fills_throughout: bool,
        #[values(FakeClock::counted(0), FakeClock::base_time_blocks(1, 200), FakeClock::seal())]
        clock: FakeClock,
        // `closedAfter` is 16 bits: 256 s and 3,840 s are nonzero only past its low 8 bits.
        #[values(60, 256, 3_840)] closed_after_s: u64,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let read_ms = node
            .clock
            .ms(node.block, node.timestamp);
        let at = |offset: i64| {
            read_ms
                .checked_add_signed(offset)
                .unwrap()
        };
        let mut listing = FakeListing::weth_usdc(1);
        listing.fence = FENCE_SESSION;
        listing.print_ends_ms = Some(at(print_ends_ms));
        listing.closed_from_ms = Some(at(closed_from_ms));
        listing.closed_after_s = closed_after_s;
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        assert_eq!(!fills_over(&listing, &life).contains(&false), fills_throughout);

        let books = books(node).await;

        assert!(books.is_empty());
    }

    /// A SESSION listing whose session word the node does not answer may have a closed phase, so
    /// it is left out; a PRINT or WORD listing does not read that word, and is served.
    #[rstest]
    #[case::session(FENCE_SESSION, false)]
    #[case::print(FENCE_PRINT, true)]
    #[case::word(FENCE_WORD, true)]
    #[tokio::test]
    async fn a_session_listing_whose_session_word_is_unread_is_left_out(
        #[case] fence: u64,
        #[case] served: bool,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.unread_words = vec![13];
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), usize::from(served));
        if served {
            assert_holds(&listing, state_of(&books, 1), &life);
        }
    }

    /// A listing reading a price print fills at its source's answer, read in the fill itself, and
    /// anyone may register a source and bind it to a listing of their own: one answering well where
    /// the transaction carries no gas price, as every quote here does, and worse in a real fill.
    /// So such a listing is served only where every source it is bound to, its primary and its
    /// backup, is one the integration's operator approved; a listing reading no print reads no
    /// source, whatever it is bound to.
    #[rstest]
    #[case::print_on_an_approved_source(FENCE_PRINT, false, (APPROVED_SOURCE, 0), true)]
    #[case::print_on_another(FENCE_PRINT, false, (9, 0), false)]
    #[case::print_with_an_approved_backup(FENCE_PRINT, false, (APPROVED_SOURCE, 2), true)]
    #[case::print_with_another_backup(FENCE_PRINT, false, (APPROVED_SOURCE, 9), false)]
    #[case::print_on_another_with_an_approved_backup(FENCE_PRINT, false, (9, 2), false)]
    #[case::print_on_none(FENCE_PRINT, false, (0, 0), false)]
    // Ids are 32 bits: 65,537 and 65,538 are not 1 and 2, which their low 16 bits read.
    #[case::print_on_an_id_past_16_bits(FENCE_PRINT, false, (65_536 + APPROVED_SOURCE, 0), false)]
    #[case::print_with_a_backup_past_16_bits(FENCE_PRINT, false, (APPROVED_SOURCE, 65_538), false)]
    #[case::session_on_another(FENCE_SESSION, false, (9, 0), false)]
    #[case::session_on_an_approved_source(FENCE_SESSION, false, (APPROVED_SOURCE, 0), true)]
    #[case::tied_on_another(FENCE_PRINT, true, (9, 0), false)]
    #[case::word_on_another(FENCE_WORD, false, (9, 0), true)]
    #[tokio::test]
    async fn a_listing_reading_a_print_is_served_only_on_approved_sources(
        #[case] fence: u64,
        #[case] tied: bool,
        #[case] sources: (u32, u32),
        #[case] served: bool,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.tied = tied;
        listing.sources = sources;
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;

        let books = source_approving(&server.url(), book_config(), now, &[APPROVED_SOURCE, 2])
            .fetch()
            .await
            .unwrap()
            .books;

        assert_eq!(books.len(), usize::from(served));
        if served {
            assert_holds(&listing, state_of(&books, 1), &life);
        }
    }

    /// A tied listing is not served, its source approved or not: it walks its maker's ladder from
    /// its print rather than its word, which no few samples bound over a book's life. A listing
    /// reading a print whose live source's copy the node does not answer may be tied to a source
    /// whose lane is unknown, and is left out too. A listing on a guarded source is served (see
    /// `a_listing_on_a_guarded_source_holds_through_its_lanes_hold`), and one reading no print
    /// reads no source, whatever its copy says.
    #[rstest]
    #[case::print_on_an_unguarded_source(FENCE_PRINT, false, false, None, true)]
    #[case::print_on_a_guarded_source(FENCE_PRINT, false, true, None, true)]
    #[case::session_on_a_guarded_source(FENCE_SESSION, false, true, None, true)]
    #[case::tied_on_a_guarded_source(FENCE_PRINT, true, true, None, false)]
    #[case::tied_on_an_unguarded_copy(FENCE_PRINT, true, false, None, false)]
    #[case::print_whose_copy_is_unread(FENCE_PRINT, false, false, Some(2), false)]
    #[case::word_with_a_guarded_copy(FENCE_WORD, false, true, None, true)]
    #[case::word_whose_copy_is_unread(FENCE_WORD, false, false, Some(2), true)]
    #[tokio::test]
    async fn a_tied_listing_or_one_whose_sources_copy_is_unread_is_left_out(
        #[case] fence: u64,
        #[case] tied: bool,
        #[case] guarded: bool,
        #[case] unread: Option<u64>,
        #[case] served: bool,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.tied = tied;
        listing.guarded = guarded;
        listing.unread_words = unread.into_iter().collect();
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(books.len(), usize::from(served));
        if served {
            assert_holds(&listing, state_of(&books, 1), &life);
        }
    }

    /// The most the bid of [`guarded_listing_on`] pays out, as a maker's bucket or the pool's room
    /// bounds it: 2,989.5 USDC, which 1 WETH passes at the held print and not at the kept one.
    const GUARDED_BID_CAP: u64 = 2_989_500_000;

    /// Listing 1 of `node`, a PRINT WETH/USDC listing on a guarded source whose lane holds
    /// `hold_ms` and answers `lane_hold_ms` for it, a post having replaced the print
    /// `posted_before_ms` before the read, with the maker's word: within the hold the bid pays
    /// `bid_ppm` less and the ask `ask_ppm` less. The bid drifts `bid_drift` ppm a second and pays
    /// out at most [`GUARDED_BID_CAP`], so 1 WETH (2,990 USDC at the kept print) fills within a
    /// hold of the bid, is refused once it ends, and fills again once the drift brings it under the
    /// cap, before the next second's ask of the print. Each side fills from one atom, so its
    /// smallest size shows its print living at every ask.
    fn guarded_listing_on(
        node: &mut FakeNode,
        hold_ms: u64,
        lane_hold_ms: Option<u64>,
        posted_before_ms: u64,
        (bid_ppm, ask_ppm): (u64, u64),
        bid_drift: u64,
    ) -> FakeListing {
        let read_ms = node
            .clock
            .ms(node.block, node.timestamp);
        let mut listing = print_listing_on(node, FENCE_PRINT, 60_000, None);
        listing.guarded = true;
        listing.maker_ms = read_ms - posted_before_ms;
        listing.lane_hold_ms = lane_hold_ms;
        listing.held = Some(FakeHold {
            from_ms: read_ms - posted_before_ms,
            for_ms: hold_ms,
            bid_ppm,
            ask_ppm,
        });
        listing.aging.bid_drift = bid_drift;
        listing.bid_out_cap = Some(U256::from(GUARDED_BID_CAP));
        listing.min_fill = 0;
        listing.bid.min_in = U256::from(1u8);
        listing.ask.min_in = U256::from(1u8);
        node.listings = vec![listing.clone()];
        listing
    }

    /// The books the second of two polls of `node` publishes, by one source: the first asks a
    /// guarded source's lane its hold, which the second samples.
    async fn books_after_a_poll(node: FakeNode) -> HashMap<String, Book> {
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let source = source(&server.url(), book_config(), now);
        source.fetch().await.unwrap();
        let snapshot = source.fetch().await.unwrap();
        Arc::try_unwrap(snapshot.books).unwrap()
    }

    /// A listing whose live source is guarded is served: its lane prices a fill's side at the
    /// worst of the prints it still holds, for its `HOLD_MS` after a post replaced one, so where a
    /// post moved the price at or just before the read the door quotes that side less within the
    /// hold and more once it ends, past an output bound it then refuses. So every block of the
    /// hold after the read's is sampled, through the first that is past it, and the book holds at
    /// every block of its life: here a post that moved the price up holds the bid 300 ppm down, at
    /// the read's block or the block before, and one that moved it down holds the ask, on a poll
    /// after the one that asked the lane its hold, and on that first poll too. A lane on
    /// Base holds 400 ms, and 2 s where the front's clock reads the block's second; the bid drifts
    /// 250 ppm a second there and 70 here, so 1 WETH is refused from the hold's end until the drift
    /// brings it under the cap, within a second.
    #[rstest]
    #[case::moved_up_at_the_read(0, (300, 0))]
    #[case::moved_up_a_block_before_the_read(200, (300, 0))]
    #[case::moved_down_at_the_read(0, (0, 300))]
    #[tokio::test]
    async fn a_listing_on_a_guarded_source_holds_through_its_lanes_hold(
        #[case] posted_before_ms: u64,
        #[case] held_ppm: (u64, u64),
        #[values(
            FakeClock::counted(0),
            FakeClock::counted(600),
            FakeClock::base_time_blocks(1, 200),
            FakeClock::seal()
        )]
        clock: FakeClock,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let (hold_ms, drift) = if clock.fast_blocks { (400, 250) } else { (2_000, 70) };
        let listing = guarded_listing_on(
            &mut node,
            hold_ms,
            Some(hold_ms),
            posted_before_ms,
            held_ppm,
            drift,
        );
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let one_weth = U256::from(10u64.pow(18));
        let fills = |ms: u64| {
            listing
                .quote_at(weth_address(), usdc_address(), one_weth, ms)
                .is_some()
        };
        assert_eq!(fills(life[0]), held_ppm.0 != 0);
        assert!(life.iter().any(|&ms| !fills(ms)));
        assert!(fills(life[life.len() - 1]));

        let first = books(node.clone()).await;
        let books = books_after_a_poll(node).await;

        assert_holds(&listing, state_of(&first, 1), &life);
        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// Where a guarded source's lane does not answer its `HOLD_MS`, the longest a lane holds, 2 s,
    /// is sampled: here a lane holding 2 s on a front counting 200 ms blocks, the bid drifting
    /// 70 ppm a second, so 1 WETH is refused from 2 s after the read to 2.4 s.
    #[tokio::test]
    async fn a_lanes_hold_it_does_not_answer_is_sampled_as_the_longest() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let listing = guarded_listing_on(&mut node, 2_000, None, 0, (300, 0), 70);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// The ask of a guarded listing, bound by its output (the pool's room), holds through its
    /// lane's hold as the bid does: a post that moved the price down holds the ask 300 ppm down at
    /// the read, so 3,010 USDC (1 WETH at the kept print) fills within the hold, is refused from
    /// its end, and fills again once the ask's drift brings it under the cap.
    #[tokio::test]
    async fn an_ask_bound_by_its_output_holds_through_its_lanes_hold() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = guarded_listing_on(&mut node, 400, Some(400), 0, (0, 300), 0);
        listing.bid_out_cap = None;
        listing.aging.ask_drift = 250;
        listing.ask_out_cap = Some(U256::from(999_833_000_000_000_000u64));
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let most = U256::from(3_010_000_000u64);
        let fills = |ms: u64| {
            listing
                .quote_at(usdc_address(), weth_address(), most, ms)
                .is_some()
        };
        assert!(fills(life[0]));
        assert!(life.iter().any(|&ms| !fills(ms)));
        assert!(fills(life[life.len() - 1]));

        let first = books(node.clone()).await;
        let books = books_after_a_poll(node).await;

        assert_holds(&listing, state_of(&first, 1), &life);
        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A size the door refuses only at the first block after the read is not quoted: the hold of a
    /// post made a block before the read ends there, and the bid's drift (300 ppm a second) brings
    /// 1 WETH back under the cap by the next block.
    #[tokio::test]
    async fn a_size_refused_only_at_the_first_block_after_the_read_is_not_quoted() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let listing = guarded_listing_on(&mut node, 400, Some(400), 200, (300, 0), 300);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let one_weth = U256::from(10u64.pow(18));
        let fills = |ms: u64| {
            listing
                .quote_at(weth_address(), usdc_address(), one_weth, ms)
                .is_some()
        };
        assert_eq!(
            life.iter()
                .take(4)
                .map(|&ms| fills(ms))
                .collect::<Vec<_>>(),
            [true, false, true, true]
        );

        let first = books(node.clone()).await;
        let books = books_after_a_poll(node).await;

        assert_holds(&listing, state_of(&first, 1), &life);
        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A lane's `HOLD_MS` is an immutable of its code: the first poll serving a listing on it asks
    /// for it, and later polls sample the hold it answered.
    #[tokio::test]
    async fn a_lanes_hold_is_read_once() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let listing = guarded_listing_on(&mut node, 400, Some(400), 0, (300, 0), 250);
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let (reads, calls) = (Arc::clone(&node.hold_ms_reads), Arc::clone(&node.eth_calls));
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let source = source(&server.url(), book_config(), now);

        source.fetch().await.unwrap();
        let first = calls.swap(0, Ordering::SeqCst);
        let books = source.fetch().await.unwrap().books;

        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(calls.load(Ordering::SeqCst) < first);
        let books = Arc::try_unwrap(books).unwrap();
        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A node that refuses or ignores the overrides cannot show a guarded listing's hold, nor its
    /// print's life: the listing is left out.
    #[rstest]
    #[case::refused(Overrides::Refused)]
    #[case::ignored(Overrides::Ignored)]
    #[tokio::test]
    async fn without_overrides_a_listing_on_a_guarded_source_is_left_out(
        #[case] overrides: Overrides,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        node.overrides = overrides;
        guarded_listing_on(&mut node, 400, Some(400), 0, (300, 0), 250);

        let books = books(node).await;

        assert!(books.is_empty());
    }

    /// A fill reading a price print spends more than one reading only the word, and a print from
    /// a guarded source more again, but no class of the deployed build was measured on Base: each
    /// listing is charged the gas named for it, whatever it reads, 95,825 selling base and 96,239
    /// buying it here.
    #[rstest]
    #[case::word(FENCE_WORD, false, false)]
    #[case::print(FENCE_PRINT, false, false)]
    #[case::session(FENCE_SESSION, false, false)]
    #[case::word_with_a_guarded_copy(FENCE_WORD, true, false)]
    #[case::print_on_a_guarded_source(FENCE_PRINT, true, false)]
    #[case::session_on_a_guarded_source(FENCE_SESSION, true, false)]
    #[case::print_on_a_guarded_source_past_the_hook_pair(FENCE_PRINT, true, true)]
    #[tokio::test]
    async fn a_listing_reading_a_source_is_charged_its_named_gas(
        #[case] fence: u64,
        #[case] guarded: bool,
        #[case] past_hook_pair: bool,
    ) {
        let (sell, buy) = (95_825u64, 96_239u64);
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.guarded = guarded;
        // Past the hook pair: the pool's asset, its token 0, still the base.
        if past_hook_pair {
            listing.quote_index = Some(2);
        }
        node.listings = vec![listing.clone()];

        let books = books(node).await;

        use tycho_common::models::token::Token;

        use crate::{book::sim::SwapDirection, evm::protocol::u256_num::u256_to_biguint};

        let state = state_for(&books, &listing);
        let gas = |direction, token_in: &Token, token_out: &Token| {
            let (smallest, _) = state.side(direction).first().unwrap();
            state
                .get_amount_out(u256_to_biguint(smallest), token_in, token_out)
                .unwrap()
                .gas
        };
        assert_eq!(gas(SwapDirection::BaseToQuote, &weth(), &usdc()), BigUint::from(sell));
        assert_eq!(gas(SwapDirection::QuoteToBase, &usdc(), &weth()), BigUint::from(buy));
    }

    /// A listing of an asset-module pair (a stock token) runs its module's rules inside every fill,
    /// the token's own reads, which no figure holds: with no gas configured for it it is left
    /// out, and with gas configured it is served and charged that.
    #[rstest]
    #[case::reading_only_its_word(FENCE_WORD)]
    #[case::reading_a_print(FENCE_PRINT)]
    #[tokio::test]
    async fn a_listing_of_an_asset_module_pair_is_served_with_its_gas_alone(
        #[case] fence: u64,
        #[values(true, false)] named: bool,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.asset_module = true;
        node.listings = vec![listing.clone()];
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let builder = EfficientPropFeedBuilder::new(book_config(), server.url(), deployment())
            .with_approved_desks(approved_desks())
            .approved_price_sources([APPROVED_SOURCE]);
        let builder = if named { builder.credit_listing(listing.id, named_gas()) } else { builder };
        let mut source = builder.build_path().unwrap().source;
        source.clock = Arc::new(move || now);

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.len(), usize::from(named));
    }

    /// A listing's pair is past its pool's hook pair wherever `doorView` places it at other indices
    /// than base 0 and quote 1, the pool's first listing's: a WETH/USDC listing of a USDC pool,
    /// WETH its token 1, a WETH/USDC listing of a pool whose token 0 is WETH and token 2 USDC, both
    /// with the pool's asset (its token 0), and a cbBTC/USDC listing of a pool holding cbBTC as its
    /// token 2 and USDC as its token 1, with neither token the asset. Such a fill settles past the
    /// pool's hook pair. On Base no fill of the deployed build was measured, so every listing is
    /// charged the gas its caller names for it, wherever its pair sits and whatever it reads.
    #[rstest]
    #[case::the_hook_pair(weth_address(), 0, None, PairPlace::HookPair)]
    #[case::weth_a_usdc_pools_token_1(weth_address(), 1, None, PairPlace::PastWithAssetAsQuote)]
    #[case::cbbtc_a_pools_token_2_beside_usdc_its_token_1(
        cbbtc_address(),
        2,
        Some(1),
        PairPlace::PastWithoutAsset
    )]
    #[case::usdc_a_pools_token_2(weth_address(), 0, Some(2), PairPlace::PastWithAsset)]
    #[tokio::test]
    async fn a_listing_on_base_is_charged_its_named_gas_wherever_its_pair_sits(
        #[case] base: Address,
        #[case] base_index: u8,
        #[case] quote_index: Option<u8>,
        #[case] pair: PairPlace,
        #[values(FENCE_WORD, FENCE_PRINT)] fence: u64,
    ) {
        use crate::book::sim::SwapDirection;
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.base = base;
        listing.base_index = base_index;
        listing.quote_index = quote_index;
        node.listings = vec![listing.clone()];

        let books = books(node).await;

        let state = state_for(&books, &listing);
        assert_eq!(state.pair, pair);
        for (direction, token_in, token_out) in [
            (SwapDirection::BaseToQuote, token(listing.base), token(listing.quote)),
            (SwapDirection::QuoteToBase, token(listing.quote), token(listing.base)),
        ] {
            let (smallest, _) = state.side(direction).first().unwrap();
            let gas = state
                .get_amount_out(
                    BigUint::from_bytes_be(&smallest.to_be_bytes::<32>()),
                    &token_in,
                    &token_out,
                )
                .unwrap()
                .gas;
            let named = named_gas().custody;
            let expected = if direction == SwapDirection::BaseToQuote {
                named.sell_base
            } else {
                named.buy_base
            };
            assert_eq!(gas, BigUint::from(expected.get()));
        }
    }

    /// On Base a listing whose gas the caller does not name is left out, whatever it reads: no
    /// figure of this crate's charges it.
    #[tokio::test]
    async fn a_listing_on_base_with_no_gas_named_is_left_out() {
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1), FakeListing::weth_usdc(2)]);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let mut source = EfficientPropFeedBuilder::new(book_config(), server.url(), deployment())
            .with_approved_desks(approved_desks())
            .credit_listing(2, named_gas())
            .build_path()
            .unwrap()
            .source;
        source.clock = Arc::new(move || now);

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(2).to_string()]);
    }

    /// On Base too a listing is served only where its maker's desk contract is approved, the
    /// front's CREATE2 child under the desk contracts' code hash.
    #[tokio::test]
    async fn a_listing_on_base_of_a_desk_not_approved_is_left_out() {
        let mut other = FakeListing::weth_usdc(2);
        other.desk = 7;
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1), other]);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let mut source = (1..=2)
            .fold(
                EfficientPropFeedBuilder::new(book_config(), server.url(), deployment())
                    .with_approved_desks([bytes(desk_cell(1))]),
                |builder, listing| builder.credit_listing(listing, named_gas()),
            )
            .build_path()
            .unwrap()
            .source;
        source.clock = Arc::new(move || now);

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(1).to_string()]);
    }

    /// No price source is approved unless the caller names one: then only the listings reading no
    /// print are served. One whose binding the node does not answer is left out too.
    #[rstest]
    #[case::none_approved(&[], false)]
    #[case::the_word_unread(&[APPROVED_SOURCE], true)]
    #[tokio::test]
    async fn without_an_approved_source_only_listings_reading_no_print_are_served(
        #[case] approved: &[u32],
        #[case] unread: bool,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut print = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        print.id = 2;
        print.door_listing = 2;
        let unread_words = if unread { vec![10] } else { Vec::new() };
        print.unread_words = unread_words.clone();
        let mut word = FakeListing::weth_usdc(1);
        word.unread_words = unread_words;
        node.listings = vec![word, print];
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let built = base_builder(book_config(), server.url())
            .build_path()
            .unwrap();
        assert!(built.source.reader.approves_none());

        let books = source_approving(&server.url(), book_config(), now, approved)
            .fetch()
            .await
            .unwrap()
            .books;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(1).to_string()]);
    }

    /// An operator may rebind a listing's source: each poll reads the binding at its own block, so
    /// a listing rebound to a source not approved is left out from the next poll.
    #[tokio::test]
    async fn a_listing_rebound_to_a_source_not_approved_is_left_out_from_the_next_poll() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let listing = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        let mut rebound = node.clone();
        rebound.listings[0].sources = (9, 0);
        let now = node.timestamp * 1000;
        let (before, after) = (node.spawn().await, rebound.spawn().await);
        let first = source(&before.url(), book_config(), now);
        assert_eq!(first.fetch().await.unwrap().books.len(), 1);
        let mut second = source(&after.url(), book_config(), now);
        second.last_block = Arc::clone(&first.last_block);

        let books = second.fetch().await.unwrap().books;

        assert!(books.is_empty(), "listing {} is served on a source not approved", listing.id);
    }

    /// A print's life is asked with the smallest and the largest size a side is sampled at. Where
    /// the door refuses the smallest throughout (here the halvings' smallest, under the door's
    /// minimum on a listing with no least fill), the largest shows the print's life, and the side
    /// is served from its other sizes, whether or not it reads a print.
    #[rstest]
    #[case::reading_no_print(FENCE_WORD)]
    #[case::reading_a_print(FENCE_PRINT)]
    #[tokio::test]
    async fn a_print_whose_smallest_size_the_door_always_refuses_is_shown_by_its_largest(
        #[case] fence: u64,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.min_fill = 0;
        node.listings = vec![listing.clone()];
        let smallest = U256::from(10u64.pow(18)) >> (DEFAULT_RUNGS - 1);
        assert!(listing
            .quote_at(weth_address(), usdc_address(), smallest, node.timestamp * 1000)
            .is_none());
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// The front refuses every size while it refuses a print, so a door that fills the smallest
    /// size at each ask took the print there, though it refused that size at the read: here a
    /// premium of 100 ppm, closing 500 ms after the read, takes the bid's least fill under the
    /// listing's minimum at the read alone. The bid is served from its larger sizes.
    #[tokio::test]
    async fn a_print_is_shown_by_its_smallest_size_where_the_read_alone_refuses_it() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        listing.aging = FakeAging { premium: 100, window_ms: 500, ..FakeAging::default() };
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let floor = U256::from(1_003_344_481_605_352u64);
        let read = node.timestamp * 1000;
        assert!(listing
            .quote_at(weth_address(), usdc_address(), floor, read)
            .is_none());
        assert!(listing
            .quote_at(weth_address(), usdc_address(), floor, read + 800)
            .is_some());

        let books = books(node).await;

        assert_holds(&listing, state_of(&books, 1), &life);
    }

    /// A gap between a print's lapse and a SESSION listing's closed phase within the read's own
    /// second is asked at that second's last block: the print lapses 500 ms after the read and the
    /// closed phase starts 900 ms after it, the door refusing the blocks between.
    #[rstest]
    #[tokio::test]
    async fn a_session_listing_refusing_within_the_reads_second_is_left_out(
        #[values(FakeClock::counted(0), FakeClock::base_time_blocks(1, 200))] clock: FakeClock,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = clock;
        let listing = print_listing_on(&mut node, FENCE_SESSION, 500, Some(900));
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let fills = fills_over(&listing, &life);
        assert!(fills[0] && fills[fills.len() - 1] && fills.contains(&false));

        let books = books(node).await;

        assert!(books.is_empty());
    }

    /// The front refuses every size while it refuses a print, so a side is left out only where
    /// the door refuses both its smallest and its largest size at an ask. A side whose least fill
    /// drifts under the listing's minimum within the life drops that size alone: here the bid's
    /// least fill, on stepped depth, fills until its drift takes its output under the minimum, at
    /// 14 ppm a second in the life's last second, past the last ask, and at 25 ppm a second from
    /// about 4.2 s, refused at the asks after.
    #[rstest]
    #[case::reading_no_print(FENCE_WORD, 14)]
    #[case::reading_a_print(FENCE_PRINT, 14)]
    #[case::reading_a_print_refused_at_the_asks(FENCE_PRINT, 25)]
    #[tokio::test]
    async fn a_print_side_whose_least_fill_drifts_under_the_minimum_keeps_its_larger_sizes(
        #[case] fence: u64,
        #[case] drift: u64,
    ) {
        use crate::book::sim::SwapDirection;
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, fence, 60_000, None);
        listing.bid = FakeSide::stepped_weth_bid();
        listing.aging.bid_drift = drift;
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        // The bid's least fill, 3 USDC at the full fill's average price, 105 ppm under the top
        // rung's; the asks fall 800 ms into each second after the read's, the last 6.8 s after
        // it, and the life's last block 7.8 s.
        let floor = U256::from(1_003_449_843_838_955u64);
        let read = node.timestamp * 1000;
        let refused_from = if drift == 14 { 7_800 } else { 4_800 };
        assert_eq!(life[life.len() - 1], read + 7_800);
        assert!(listing
            .quote_at(weth_address(), usdc_address(), floor, read + refused_from - 1_000)
            .is_some());
        assert!(listing
            .quote_at(weth_address(), usdc_address(), floor, read + refused_from)
            .is_none());

        let books = books(node).await;

        let state = state_of(&books, 1);
        let bid: Vec<(U256, U256)> = state
            .side(SwapDirection::BaseToQuote)
            .clone()
            .into();
        assert!(bid[0].0 > floor, "the bid quotes its least fill");
        assert_holds(&listing, state, &life);
    }

    /// A size the door refuses at any ask is not quoted, though the door fills it at the read and
    /// at the life's end: here the bid's least fill, or its largest size (1 WETH, its halving 0.5
    /// WETH not refused), is refused from 2 s to 3 s after the read, across the ask 2.8 s after
    /// it, while the other fills throughout.
    #[rstest]
    #[case::the_smallest(1_003_344_481_605_352, 1_003_344_481_605_352)]
    #[case::the_largest(600_000_000_000_000_000, 1_000_000_000_000_000_000)]
    #[tokio::test]
    async fn a_size_refused_at_an_ask_is_not_quoted(#[case] least: u64, #[case] most: u64) {
        use crate::book::sim::SwapDirection;
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        let (least, most) = (U256::from(least), U256::from(most));
        let read = node.timestamp * 1000;
        listing.bid_refused = Some((read + 2_000, read + 3_000, least, most));
        node.listings = vec![listing.clone()];
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        for (at, fills) in [(read, true), (read + 2_800, false), (read + 7_800, true)] {
            assert_eq!(
                listing
                    .quote_at(weth_address(), usdc_address(), most, at)
                    .is_some(),
                fills
            );
        }

        let books = books(node).await;

        let state = state_of(&books, 1);
        let bid: Vec<(U256, U256)> = state
            .side(SwapDirection::BaseToQuote)
            .clone()
            .into();
        assert!(
            bid.iter()
                .all(|&(amount, _)| amount < least || amount > most),
            "the bid quotes a size refused at an ask"
        );
        assert_holds(&listing, state, &life);
    }

    /// A node that refuses a block override, ignores it, or moves its time alone cannot show how
    /// long a print lives, so a listing reading one is left out, however long its print lasts
    /// and though its levels cannot age; a listing reading none beside it is served. So is one
    /// that answers some overrides and not others: here the life's end, at an odd block, and
    /// every other second's ask.
    #[rstest]
    #[case::refused(Overrides::Refused)]
    #[case::ignored(Overrides::Ignored)]
    #[case::time_only(Overrides::TimeOnly)]
    #[case::some_answered(Overrides::OddBlocksOnly)]
    #[tokio::test]
    async fn without_overrides_a_listing_reading_a_print_is_left_out(#[case] overrides: Overrides) {
        let word = FakeListing::weth_usdc(1);
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut print = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        print.id = 2;
        print.door_listing = 2;
        let mut session = print_listing_on(&mut node, FENCE_SESSION, 60_000, Some(120_000));
        session.id = 3;
        session.door_listing = 3;
        node.listings = vec![word.clone(), print, session];
        node.overrides = overrides;
        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);

        let books = books(node).await;

        assert_eq!(
            books
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            [id(1).to_string()]
        );
        assert_holds(&word, state_of(&books, 1), &life);
    }

    /// A side's samples hold only where the node answered every block they were asked at: here a
    /// listing reading a print, whose premium window closes 3 s after the read, its first block at
    /// or after that (no second's ask) refused by the node while the life's end and every ask are
    /// answered, is left out.
    #[tokio::test]
    async fn a_print_listing_whose_premium_edge_the_node_refuses_is_left_out() {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(0);
        let mut listing = print_listing_on(&mut node, FENCE_PRINT, 60_000, None);
        listing.aging = FakeAging {
            bid_drift: 100,
            ask_drift: 100,
            premium: 2_000,
            window_ms: 3_000,
            ..FakeAging::default()
        };
        node.listings = vec![listing];
        // The read's block reads the start of its second; the window closes 15 blocks later.
        node.refused_blocks = vec![node.block + 15];

        let books = books(node).await;

        assert!(books.is_empty());
    }

    /// A listing reading a print is probed at the last block of each second of the book's life
    /// after the read's and before its last, in the third request: on Base's clock, read at the
    /// start of a second, seven more `eth_call`s, and eight read 400 ms into it, its life then
    /// ending 400 ms into its eighth second after. One reading none adds nothing: four, the
    /// listings, the doors, and the quotes at the read and at the life's end.
    #[rstest]
    #[case::reading_no_print(FENCE_WORD, 0, 4)]
    #[case::reading_no_print_read_into_a_second(FENCE_WORD, 400, 4)]
    #[case::reading_a_print(FENCE_PRINT, 0, 11)]
    #[case::reading_a_print_read_into_a_second(FENCE_PRINT, 400, 12)]
    #[case::reading_a_session_print(FENCE_SESSION, 0, 11)]
    #[tokio::test]
    async fn a_print_is_probed_each_second_of_the_life_in_the_third_request(
        #[case] fence: u64,
        #[case] into_second_ms: u64,
        #[case] eth_calls: usize,
    ) {
        let mut node = FakeNode::new(Vec::new());
        node.clock = FakeClock::counted(into_second_ms);
        print_listing_on(&mut node, fence, 60_000, None);
        let calls = Arc::clone(&node.eth_calls);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;

        let books = source(&server.url(), book_config(), now)
            .fetch()
            .await
            .unwrap()
            .books;

        assert_eq!(books.len(), 1);
        assert_eq!(server.request_count(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), eth_calls);
    }

    /// A node that refuses a block override, or ignores it, cannot show what a door quotes later
    /// in a book's life. Then a side is served only where its levels cannot age within that life:
    /// no drift and no widening, or a word that stays within its grace for all of it. A tied
    /// listing is never served. A listing drifting on its bid alone still quotes its ask.
    #[rstest]
    #[case::refused(Overrides::Refused)]
    #[case::ignored(Overrides::Ignored)]
    #[tokio::test]
    async fn without_overrides_only_listings_whose_levels_cannot_age_are_served(
        #[case] overrides: Overrides,
    ) {
        let fresh = FakeListing::weth_usdc(1);
        let mut drifting = FakeListing::weth_usdc(2);
        drifting.aging.bid_drift = 100;
        let mut graced = FakeListing::weth_usdc(3);
        graced.aging = FakeAging { widen: 200, grace_ms: BOOK_LIFE_MS, ..FakeAging::default() };
        let mut tied = graced.clone();
        tied.id = 4;
        tied.door_listing = 4;
        tied.tied = true;
        let mut node = FakeNode::new(vec![fresh.clone(), drifting, graced.clone(), tied]);
        node.overrides = overrides;

        let life = node.life(node.timestamp * 1000, BOOK_LIFE_MS);
        let books = books(node).await;

        let mut ids: Vec<_> = books.keys().cloned().collect();
        ids.sort();
        let mut expected = vec![id(1).to_string(), id(2).to_string(), id(3).to_string()];
        expected.sort();
        assert_eq!(ids, expected);
        let state = |listing: u32| {
            books[&id(listing).to_string()]
                .state
                .as_any()
                .downcast_ref::<EfficientPropState>()
                .unwrap()
        };
        assert_holds(&fresh, state(1), &life);
        assert_holds(&graced, state(3), &life);
        use crate::book::sim::SwapDirection;
        assert!(state(2)
            .side(SwapDirection::BaseToQuote)
            .is_empty());
        assert!(!state(2)
            .side(SwapDirection::QuoteToBase)
            .is_empty());
    }

    /// The landing allowance is the caller's to set: with none, a word standing six seconds past
    /// the poll's start, its request timeout and the snapshot's age limit, is served.
    #[tokio::test]
    async fn the_landing_allowance_is_configurable() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.maker_ms = NODE_TIMESTAMP * 1000 + 6_000 - MAX_TTL_MS - 1;
        let node = FakeNode::new(vec![listing]);
        let server = node.spawn().await;
        let mut source = base_builder(book_config(), server.url())
            .landing_allowance(Duration::ZERO)
            .build_path()
            .unwrap()
            .source;
        source.clock = Arc::new(|| NODE_TIMESTAMP * 1000);

        assert_eq!(
            source
                .fetch()
                .await
                .unwrap()
                .books
                .len(),
            1
        );
    }

    /// The word's life is capped at the front's: a word posted with a longer one runs out as
    /// if it had the cap.
    #[tokio::test]
    async fn a_word_lives_no_longer_than_the_fronts_cap() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.maker_ms = NODE_TIMESTAMP * 1000 - MAX_TTL_MS + 3_000;
        listing.life_s = 600;

        assert!(books(FakeNode::new(vec![listing]))
            .await
            .is_empty());
    }

    /// Each side's depth expires on its own: a bid whose depth runs out within the snapshot's life
    /// leaves the book quoting the ask alone.
    #[tokio::test]
    async fn a_side_whose_depth_runs_out_is_left_out() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.bid_depth_expires = NODE_TIMESTAMP + 3;

        let books = books(FakeNode::new(vec![listing])).await;

        assert_eq!(
            books[&id(1).to_string()]
                .state
                .as_any()
                .downcast_ref::<EfficientPropState>()
                .unwrap()
                .directions(),
            vec![(usdc().address, weth().address)]
        );
    }

    /// The books one poll of `node` publishes on Base with its desks approved and `named` each
    /// configured with its gas, the wall clock at the node's block timestamp.
    async fn base_books_naming(
        node: FakeNode,
        named: &[(u32, CreditGas)],
    ) -> HashMap<String, Book> {
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let mut source = named
            .iter()
            .fold(
                EfficientPropFeedBuilder::new(book_config(), server.url(), deployment())
                    .with_approved_desks(approved_desks())
                    .approved_price_sources([APPROVED_SOURCE]),
                |builder, &(listing, gas)| builder.credit_listing(listing, gas),
            )
            .build_path()
            .unwrap()
            .source;
        source.clock = Arc::new(move || now);
        Arc::try_unwrap(source.fetch().await.unwrap().books).unwrap()
    }

    /// The gas a state charges a swap of `amount` in `direction` on WETH/USDC, or its refusal.
    fn charged(
        state: &EfficientPropState,
        direction: SwapDirection,
        amount: U256,
    ) -> Result<BigUint, SimulationError> {
        let (token_in, token_out) = match direction {
            SwapDirection::BaseToQuote => (weth(), usdc()),
            SwapDirection::QuoteToBase => (usdc(), weth()),
        };
        state
            .get_amount_out(
                BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
                &token_in,
                &token_out,
            )
            .map(|result| result.gas)
    }

    /// A credit listing fills an input inside its cheap prefix at the ladder's price and a larger
    /// one whole at the pool's preview, which may pay less than a smaller input: the samples' chord
    /// across that step quotes more than the door fills. Such a listing is not served on Base
    /// unless its gas is named.
    #[rstest]
    #[case::the_pool_route(1)]
    #[case::the_venue_route(2)]
    #[tokio::test]
    async fn a_listing_with_a_credit_mode_and_no_gas_named_is_not_served(#[case] credit_mode: u8) {
        let mut credit = FakeListing::weth_usdc(2);
        credit.credit_mode = credit_mode;
        credit.bid = FakeSide::credit_weth_bid();
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1), credit]);

        let books = base_books_naming(node, &[(1, named_gas())]).await;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(1).to_string()]);
    }

    /// A credit listing named with its gas is served on Base, its words standing in storage: each
    /// size is asked the door's quote and the front's plan at the read's block and at every later
    /// moment of the life sampled, and quoted at the least output where every moment agrees on
    /// its path. The bid's sizes inside the 0.1 WETH prefix fill from custody and those above it
    /// through the route, each charged its path's gas, nothing quoted between them; the ask, all
    /// custody, the custody figure buying base.
    #[rstest]
    #[case::the_pool_route(1)]
    #[case::the_venue_route(2)]
    #[case::every_size_the_switches_let(3)]
    #[tokio::test]
    async fn a_named_credit_listing_on_base_is_served_with_its_route_and_its_gas(
        #[case] credit_mode: u8,
    ) {
        let mut listing = FakeListing::weth_usdc(1);
        listing.credit_mode = credit_mode;
        listing.bid = FakeSide::credit_weth_bid();
        let node = FakeNode::new(vec![listing.clone()]);

        let books = base_books_naming(node, &[(1, credit_gas())]).await;

        let state = state_for(&books, &listing);
        assert_eq!(state.credit, Some(credit_gas()));
        let prefix = U256::from(100_000_000_000_000_000u64);
        let bid = state
            .side(SwapDirection::BaseToQuote)
            .points()
            .to_vec();
        for &(amount, output) in &bid {
            assert_eq!(Some(output), listing.bid.quote(amount), "the door's own answer");
        }
        let (custody, routed): (Vec<_>, Vec<_>) = bid
            .iter()
            .partition(|&&(amount, _)| amount <= prefix);
        let (Some(&(custody_most, custody_out)), Some(&(route_least, _))) =
            (custody.last(), routed.first())
        else {
            panic!("expected both paths in {bid:?}");
        };
        let gas = |amount| charged(state, SwapDirection::BaseToQuote, amount);
        assert_eq!(gas(custody[0].0).unwrap(), BigUint::from(160_877u64));
        // The largest custody size may empty the pool: charged less what that earns back.
        assert_eq!(gas(custody_most).unwrap(), BigUint::from(151_277u64));
        assert_eq!(gas(route_least).unwrap(), BigUint::from(610_501u64));
        let across = gas(custody_most + U256::from(1u8)).unwrap_err();
        let SimulationError::InvalidInput(_, Some(partial)) = across else {
            panic!("expected the custody's fill handed back, got {across}");
        };
        assert_eq!(partial.amount, BigUint::from_bytes_be(&custody_out.to_be_bytes::<32>()));
        let ask = state
            .side(SwapDirection::QuoteToBase)
            .points()
            .to_vec();
        assert_eq!(
            charged(state, SwapDirection::QuoteToBase, ask[0].0).unwrap(),
            BigUint::from(158_340u64)
        );
    }

    /// A size the front's plan routes at the read and fills from custody later in the life (the
    /// pool's custody grew past it) bounds neither its gas nor its output over the life: it is
    /// not quoted, and the run of the bid ends there, its custody sizes below it still served.
    #[tokio::test]
    async fn a_size_whose_path_changes_within_the_life_is_not_quoted_on_base() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.credit_mode = 2;
        listing.bid = FakeSide::credit_weth_bid();
        listing.bid_prefix_from =
            Some((NODE_TIMESTAMP * 1000 + 3_000, U256::from(300_000_000_000_000_000u64)));
        let node = FakeNode::new(vec![listing.clone()]);

        let books = base_books_naming(node, &[(1, credit_gas())]).await;

        let state = state_for(&books, &listing);
        let bid = state
            .side(SwapDirection::BaseToQuote)
            .points()
            .to_vec();
        assert!(!bid.is_empty());
        assert!(
            bid.iter()
                .all(|&(amount, _)| amount <= U256::from(100_000_000_000_000_000u64)),
            "{bid:?}"
        );
    }

    /// A credit listing sharing its pool with a door trading the token its route may leave owed
    /// is served on Base its custody sizes alone, as on Ethereum.
    #[tokio::test]
    async fn a_credit_listing_sharing_what_its_route_owes_is_served_custody_alone_on_base() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.credit_mode = 2;
        listing.bid = FakeSide::credit_weth_bid();
        let mut other = FakeListing::weth_usdc(2);
        other.pool = listing.pool;
        let node = FakeNode::new(vec![listing.clone(), other]);

        let books = base_books_naming(node, &[(1, credit_gas()), (2, named_gas())]).await;

        let state = state_for(&books, &listing);
        let bid = state
            .side(SwapDirection::BaseToQuote)
            .points()
            .to_vec();
        assert!(!bid.is_empty());
        assert!(
            bid.iter()
                .all(|&(amount, _)| amount <= U256::from(100_000_000_000_000_000u64)),
            "{bid:?}"
        );
        assert_eq!(books.len(), 2);
    }

    /// On depth whose rungs walk further off the price, the book quotes each sample exactly and
    /// never more than the door between them.
    #[tokio::test]
    async fn a_stepped_side_is_quoted_at_or_under_the_door() {
        let mut listing = FakeListing::weth_usdc(1);
        listing.bid = FakeSide::stepped_weth_bid();
        let door = listing.bid.clone();

        let books = books(FakeNode::new(vec![listing])).await;
        let state = &books[&id(1).to_string()].state;

        let (smallest, _) = state
            .as_any()
            .downcast_ref::<EfficientPropState>()
            .unwrap()
            .side(crate::book::sim::SwapDirection::BaseToQuote)
            .first()
            .unwrap();
        let top = U256::from(10u64).pow(U256::from(18));
        let steps = 997u64;
        for step in 0..=steps {
            let amount = smallest + (top - smallest) * U256::from(step) / U256::from(steps);
            let book = state
                .get_amount_out(
                    BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
                    &weth(),
                    &usdc(),
                )
                .unwrap()
                .amount;
            let fill = door.quote(amount).unwrap();
            assert!(
                book <= BigUint::from_bytes_be(&fill.to_be_bytes::<32>()),
                "at {amount} the book quotes {book} over the door's {fill}"
            );
        }
    }

    /// Every Base listing has a taker tip cap, which its book carries for a solver to send under.
    #[tokio::test]
    async fn a_book_carries_its_listings_tip_cap() {
        let mut uncapped = FakeListing::weth_usdc(2);
        uncapped.tip_cap = 0;

        let books = books(FakeNode::new(vec![FakeListing::weth_usdc(1), uncapped])).await;

        let attributes = |listing| {
            &books[&id(listing).to_string()]
                .component
                .static_attributes
        };
        assert_eq!(
            attributes(1)[TIP_CAP_WEI_ATTRIBUTE],
            // 10^8, big-endian, with no leading zeros.
            Bytes::from(vec![0x05, 0xf5, 0xe1, 0x00])
        );
        assert!(!attributes(2).contains_key(TIP_CAP_WEI_ATTRIBUTE));
    }

    /// A listing whose tip cap is under the configured floor is not served; one at the floor, or
    /// with no cap, is.
    #[rstest]
    #[case::under_the_floor(1 | (8 << 12), false)]
    #[case::at_the_floor(2 | (8 << 12), true)]
    #[case::no_cap(0, true)]
    #[tokio::test]
    async fn a_listing_capping_the_tip_under_the_floor_is_left_out(
        #[case] tip_cap: u16,
        #[case] served: bool,
    ) {
        let mut listing = FakeListing::weth_usdc(1);
        listing.tip_cap = tip_cap;
        let node = FakeNode::new(vec![listing]);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let mut source = base_builder(book_config(), server.url())
            .min_tip_cap_wei(U256::from(200_000_000u64))
            .build_path()
            .unwrap()
            .source;
        source.clock = Arc::new(move || now);

        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.len(), usize::from(served));
    }

    /// Two listings may trade one pair; each door is its own book.
    #[tokio::test]
    async fn two_doors_on_one_pair_are_two_books() {
        let books =
            books(FakeNode::new(vec![FakeListing::weth_usdc(1), FakeListing::weth_usdc(2)])).await;

        let mut ids: Vec<_> = books.keys().cloned().collect();
        ids.sort();
        let mut expected = vec![id(1).to_string(), id(2).to_string()];
        expected.sort();
        assert_eq!(ids, expected);
    }

    /// A side the listing does not fill is no direction of the book; a listing filling neither
    /// is no book.
    #[tokio::test]
    async fn sides_that_fill_nothing_are_left_out() {
        let mut one_sided = FakeListing::weth_usdc(1);
        one_sided.ask = FakeSide::closed();
        let mut closed = FakeListing::weth_usdc(2);
        closed.bid = FakeSide::closed();
        closed.ask = FakeSide::closed();

        let books = books(FakeNode::new(vec![one_sided, closed])).await;

        assert_eq!(books.len(), 1);
        assert_eq!(
            books[&id(1).to_string()]
                .state
                .as_any()
                .downcast_ref::<EfficientPropState>()
                .unwrap()
                .directions(),
            vec![(weth().address, usdc().address)]
        );
        assert!(books[&id(1).to_string()]
            .component
            .static_attributes
            .contains_key(SWAP_DIRECTIONS_ATTRIBUTE));
    }

    /// Doors the factory has not opened, and contracts at a door's address that name another
    /// front or listing, are not served; the rest are.
    #[rstest]
    #[case::not_opened(|listing: &mut FakeListing| listing.opened = false)]
    #[case::naming_another_front(|listing: &mut FakeListing| listing.door_front = FACTORY)]
    #[case::naming_another_listing(|listing: &mut FakeListing| listing.door_listing = 9)]
    #[tokio::test]
    async fn only_the_doors_of_the_front_and_listing_are_served(
        #[case] change: fn(&mut FakeListing),
    ) {
        let mut other = FakeListing::weth_usdc(2);
        change(&mut other);

        let books = books(FakeNode::new(vec![FakeListing::weth_usdc(1), other])).await;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(1).to_string()]);
    }

    /// A door is where its code lands under the configured factory and its listing id, whatever
    /// the factory's `doorOf` says.
    #[tokio::test]
    async fn a_door_is_found_at_its_create2_address_not_where_the_factory_says() {
        let mut node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        node.factory_door_of_lies = true;

        let books = books(node).await;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(1).to_string()]);
    }

    /// The doors' code hash is no setting: the reader takes it from the factory it is given, so a
    /// factory of another door code (and doors created from it) is served, at the CREATE2 address
    /// of the code the factory names.
    #[tokio::test]
    async fn a_doors_address_comes_from_the_factorys_own_code_hash() {
        let other = B256::repeat_byte(0x99);
        let mut node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        node.factory_door_codehash = other;

        let books = books(node).await;

        let door = FACTORY.create2(B256::from(U256::from(1u8)), other);
        assert_ne!(door, door_of(1));
        assert_eq!(books.len(), 1);
        assert_eq!(
            books
                .values()
                .next()
                .unwrap()
                .component
                .static_attributes[PAMM_ADDRESS_ATTRIBUTE],
            bytes(door)
        );
    }

    /// A listing whose tokens are outside the universe is not even quoted.
    #[tokio::test]
    async fn listings_outside_the_token_list_are_skipped() {
        let mut config = book_config();
        let mut tokens = (*config.tokens).clone();
        tokens.remove(&bytes(weth_address()));
        config.tokens = Arc::new(tokens);

        let books = fetch(FakeNode::new(vec![FakeListing::weth_usdc(1)]), config)
            .await
            .unwrap();

        assert!(books.is_empty());
    }

    /// A book is valued in USD through its quote token: USDC at par, cbBTC through a listing
    /// that sells it for USDC; one with no such listing is not published.
    #[tokio::test]
    async fn a_book_needs_a_usd_value_to_clear_the_floor() {
        let mut weth_cbbtc = FakeListing::weth_usdc(1);
        weth_cbbtc.quote = cbbtc_address();
        let node = FakeNode::new(vec![weth_cbbtc.clone()]);

        assert!(books(node).await.is_empty());

        let mut cbbtc_usdc = FakeListing::weth_usdc(2);
        cbbtc_usdc.base = cbbtc_address();
        assert_eq!(
            books(FakeNode::new(vec![weth_cbbtc, cbbtc_usdc]))
                .await
                .len(),
            2
        );

        let mut config = book_config();
        config.min_tvl_usd = 1e12;
        let books = fetch(FakeNode::new(vec![FakeListing::weth_usdc(1)]), config)
            .await
            .unwrap();
        assert!(books.is_empty());
    }

    /// A poll scans sixteen ids past the highest listing found, so a door on listing 20 is
    /// found once the first poll has seen listing 16.
    #[tokio::test]
    async fn listings_past_the_lookahead_are_found_on_later_polls() {
        let mut listings: Vec<_> = (1..=20)
            .map(|id| {
                let mut listing = FakeListing::weth_usdc(id);
                listing.opened = false;
                listing
            })
            .collect();
        listings[19].opened = true;
        let node = FakeNode::new(listings);
        let now = node.timestamp * 1000;
        let server = node.spawn().await;
        let source = source(&server.url(), book_config(), now);

        assert!(source
            .fetch()
            .await
            .unwrap()
            .books
            .is_empty());
        let books = source.fetch().await.unwrap().books;

        assert_eq!(books.keys().collect::<Vec<_>>(), [&id(20).to_string()]);
    }

    /// No later poll can succeed against a deployment that is not the one configured.
    #[rstest]
    #[case::an_upgraded_front(
        |node: &mut FakeNode| node.implementation = Address::repeat_byte(0x99),
        "implementation"
    )]
    #[case::another_chain(|node: &mut FakeNode| node.chain_id = 1, "chain 1")]
    #[case::a_factory_of_another_front(
        |node: &mut FakeNode| node.factory_front = Address::repeat_byte(0x99),
        "serves front"
    )]
    #[case::a_reverting_door_view(|node: &mut FakeNode| node.door_view_reverts = true, "doorView")]
    #[case::a_clock_mode_a_base_front_does_not_run(
        |node: &mut FakeNode| node.clock.mode = 3,
        "clock mode 3"
    )]
    #[case::an_upgraded_front_at_the_listings_block(
        |node: &mut FakeNode| node.pinned_implementation = Address::repeat_byte(0x99),
        "implementation"
    )]
    #[tokio::test]
    async fn a_deployment_other_than_the_configured_one_ends_the_feed(
        #[case] change: fn(&mut FakeNode),
        #[case] reason: &str,
    ) {
        let mut node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        change(&mut node);

        let error = fetch(node, book_config())
            .await
            .unwrap_err();

        assert!(error.is_fatal(), "{error}");
        assert!(error.to_string().contains(reason), "{error}");
    }

    /// A factory's address is known before it is deployed: until it is, polls fail and retry.
    #[tokio::test]
    async fn a_factory_with_no_code_fails_only_the_poll() {
        let mut node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        node.factory_deployed = false;

        let error = fetch(node, book_config())
            .await
            .unwrap_err();

        assert!(!error.is_fatal(), "{error}");
        assert!(
            error
                .to_string()
                .contains("has no code"),
            "{error}"
        );
    }

    /// A node whose latest block trails the wall clock by more than four seconds is lagging.
    #[rstest]
    #[case::within_the_lag(4, true)]
    #[case::past_it(5, false)]
    #[tokio::test]
    async fn a_lagging_node_fails_the_poll(#[case] lag: u64, #[case] served: bool) {
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        let now = (node.timestamp + lag) * 1000;
        let server = node.spawn().await;

        let result = source(&server.url(), book_config(), now)
            .fetch()
            .await;

        assert_eq!(result.is_ok(), served, "{result:?}");
    }

    /// The lag is judged again when the read is back: a poll that starts 3 s after its block's
    /// second, within the lag, and whose read is back 5 s after it fails as lagging, though its
    /// books would still hold for their life.
    #[tokio::test]
    async fn a_node_lagging_by_the_polls_end_fails_the_poll() {
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        let block_ms = node.timestamp * 1000;
        let server = node.spawn().await;
        let mut source = source(&server.url(), book_config(), block_ms);
        let reads = Arc::new(AtomicU32::new(0));
        source.clock = Arc::new(move || {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                block_ms + 3_000
            } else {
                block_ms + 5_000
            }
        });

        let error = source.fetch().await.unwrap_err();

        assert!(error.to_string().contains("behind"), "{error}");
    }

    /// A node whose latest block trails the wall clock at the poll's start by more than four
    /// seconds fails the poll at its first request, before any door is asked: a read of a listing
    /// reading a print would otherwise ask it at each second between the block and the book's
    /// end, one call more for every second the node lags.
    #[rstest]
    #[case::four_seconds_behind(4, true)]
    #[case::five_seconds_behind(5, false)]
    #[case::a_minute_behind(60, false)]
    #[case::ten_minutes_behind(600, false)]
    #[tokio::test]
    async fn a_lagging_node_is_sent_only_the_first_request(
        #[case] lag_s: u64,
        #[case] served: bool,
    ) {
        let mut listing = FakeListing::weth_usdc(1);
        listing.fence = FENCE_PRINT;
        listing.print_ends_ms = Some((NODE_TIMESTAMP + 86_400) * 1000);
        let mut node = FakeNode::new(vec![listing]);
        node.clock = FakeClock::counted(0);
        let calls = Arc::clone(&node.eth_calls);
        let server = node.spawn().await;

        let result = source(&server.url(), book_config(), (NODE_TIMESTAMP + lag_s) * 1000)
            .fetch()
            .await;

        assert_eq!(result.is_ok(), served, "{result:?}");
        if !served {
            assert_eq!(server.request_count(), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("behind"));
        }
    }

    /// A poll's books must hold from its start until they are served and a route priced from them
    /// lands: a poll whose read ends past its request timeout (here its wall clock reads 2 s past
    /// its start when the read is back) fails, and one ending at the timeout publishes.
    #[rstest]
    #[case::at_the_timeout(2_000, true)]
    #[case::past_it(2_001, false)]
    #[tokio::test]
    async fn a_poll_ending_too_late_for_its_books_to_be_served_fails(
        #[case] took_ms: u64,
        #[case] served: bool,
    ) {
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        let start = node.timestamp * 1000;
        let server = node.spawn().await;
        let mut source = source(&server.url(), book_config(), start);
        let reads = Arc::new(AtomicU32::new(0));
        source.clock = Arc::new(move || {
            let read = reads.fetch_add(1, Ordering::SeqCst);
            if read == 0 {
                start
            } else {
                start + took_ms
            }
        });

        let result = source.fetch().await;

        assert_eq!(result.is_ok(), served, "{result:?}");
        if !served {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("too late"));
        }
    }

    /// A load-balanced provider can answer from a block older than one already read; publishing
    /// it would roll the books back.
    #[tokio::test]
    async fn a_read_older_than_the_last_one_fails_the_poll() {
        let node = FakeNode::new(vec![FakeListing::weth_usdc(1)]);
        let now = node.timestamp * 1000;
        let mut older = node.clone();
        older.block -= 1;
        let (newer, older) = (node.spawn().await, older.spawn().await);
        let primary = source(&newer.url(), book_config(), now);
        primary.fetch().await.unwrap();
        let mut replica = source(&older.url(), book_config(), now);
        replica.last_block = Arc::clone(&primary.last_block);

        let error = replica.fetch().await.unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
    }

    /// A new feed polls every 250 ms, which the caller may change.
    #[test]
    fn the_feed_polls_every_250_ms_by_default() {
        let builder =
            EfficientPropFeedBuilder::new(book_config(), "http://node".to_string(), deployment());
        assert_eq!(builder.feed_config.poll_interval, Duration::from_millis(250));

        let feed = builder
            .feed_config(HttpFeedConfig {
                poll_interval: Duration::from_secs(1),
                ..EfficientPropFeedBuilder::default_feed_config()
            })
            .build_path()
            .unwrap();

        assert_eq!(feed.feed_config.poll_interval, Duration::from_secs(1));
    }

    /// A poll is three requests, however many doors it samples.
    #[tokio::test]
    async fn a_poll_reads_the_chain_in_three_requests() {
        let node = FakeNode::new(
            (1..=12)
                .map(FakeListing::weth_usdc)
                .collect(),
        );
        let now = node.timestamp * 1000;
        let server = node.spawn().await;

        let books = source(&server.url(), book_config(), now)
            .fetch()
            .await
            .unwrap()
            .books;

        assert_eq!(books.len(), 12);
        assert_eq!(server.request_count(), 3);
    }

    #[rstest]
    #[case::another_chain(|builder: EfficientPropFeedBuilder| {
        let mut config = book_config();
        config.chain = Chain::Ethereum;
        EfficientPropFeedBuilder::new(config, builder.rpc_url, builder.deployment)
    })]
    #[case::a_chain_with_no_id(|builder: EfficientPropFeedBuilder| {
        let mut config = book_config();
        config.chain = chain_without_id();
        EfficientPropFeedBuilder::new(config, builder.rpc_url, builder.deployment)
    })]
    #[case::not_a_url(|builder: EfficientPropFeedBuilder| {
        EfficientPropFeedBuilder::new(builder.book_config, "node".into(), builder.deployment)
    })]
    #[case::an_empty_allow_list(|mut builder: EfficientPropFeedBuilder| {
        builder.deployment.implementations.clear();
        builder
    })]
    #[case::a_front_that_is_not_an_address(|mut builder: EfficientPropFeedBuilder| {
        builder.deployment.front = Bytes::from(vec![1u8; 4]);
        builder
    })]
    #[case::a_quote_layout_on_base(|mut builder: EfficientPropFeedBuilder| {
        builder.deployment.quote_layout = Some(QUOTE_LAYOUT_BUILDER.to_string());
        builder
    })]
    #[case::no_desk_code_hash(|mut builder: EfficientPropFeedBuilder| {
        builder.deployment.cell_codehash = None;
        builder
    })]
    #[case::a_desk_code_hash_not_32_bytes(|mut builder: EfficientPropFeedBuilder| {
        builder.deployment.cell_codehash = Some(Bytes::from(vec![1u8; 20]));
        builder
    })]
    #[case::the_fallback_router(EfficientPropFeedBuilder::with_fallback_router)]
    #[case::no_sizes(|builder: EfficientPropFeedBuilder| builder.rungs(0))]
    #[case::snapshots_never_withdrawn(|builder: EfficientPropFeedBuilder| {
        builder.feed_config(HttpFeedConfig {
            max_snapshot_age: None,
            ..EfficientPropFeedBuilder::default_feed_config()
        })
    })]
    #[case::too_many_sizes(|builder: EfficientPropFeedBuilder| builder.rungs(MAX_RUNGS + 1))]
    fn the_builder_refuses_what_it_cannot_run(
        #[case] change: fn(EfficientPropFeedBuilder) -> EfficientPropFeedBuilder,
    ) {
        let builder =
            EfficientPropFeedBuilder::new(book_config(), "http://node".to_string(), deployment());

        let error = change(builder)
            .build_path()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[test]
    fn debug_output_omits_the_rpc_url() {
        let feed = EfficientPropFeedBuilder::new(
            book_config(),
            "https://base.example/secret_key".to_string(),
            deployment(),
        )
        .build_path()
        .unwrap();

        assert!(!format!("{feed:?}").contains("secret_key"));
    }

    /// The HTTP client quotes the URL a request failed on; the poll's error, which the feed logs,
    /// does not.
    #[tokio::test]
    async fn a_failed_poll_does_not_report_the_rpc_url() {
        let source = source("http://127.0.0.1:1/secret_key", book_config(), 0);

        let error = source.fetch().await.unwrap_err();

        assert!(matches!(error, FeedError::Connection(_)), "{error}");
        assert!(!error.to_string().contains("secret_key"), "{error}");
    }

    #[test]
    fn the_id_is_the_pair_id_followed_by_the_door() {
        let id = door_component_id(&bytes(weth_address()), &bytes(usdc_address()), &bytes(FRONT));

        assert_eq!(
            id.to_string(),
            format!(
                "0x{}{}{}{}",
                alloy::primitives::hex::encode("efficient_prop"),
                alloy::primitives::hex::encode(weth_address()),
                alloy::primitives::hex::encode(usdc_address()),
                alloy::primitives::hex::encode(FRONT),
            )
        );
    }

    alloy::sol! {
        function isActive(address tokenIn, address tokenOut) external view returns (bool active);
    }

    /// Polls a live front and holds every book to the doors it was read from. Each poll fits the
    /// request timeout. At one read's block:
    /// - the doors read are exactly those found by walking `doorView` and `doorOf` from listing 1
    ///   to the first unreserved id, keeping each with code that names the front and the listing,
    ///   less those with a credit mode the front does not know (`doorView`'s `creditMode` above 3;
    ///   every listing is named the test's gas, a credit listing's for both paths) or a token
    ///   outside the book configuration's, and among those reading a print (fence kind PRINT or
    ///   SESSION): those bound to a source not approved (record word 10), tied listings (settings
    ///   scopes 8), those whose live source's copy (record word 2) is unread, and SESSION listings
    ///   with a closed phase (a nonzero `closedAfter`, record word 13's bits 0-15) or an unread
    ///   session word. A listing on a guarded source is kept;
    /// - each side runs out no later than its own depth's expiry, by role;
    /// - a book quotes at every sampled size exactly the least of the door's `quote` at the read's
    ///   block and under a block override at each later block of the side's life (the book's, or
    ///   where its print lapses within it, up to its print's last block), stepped by the front's
    ///   own clock (every 200 ms where it counts blocks or reads BaseTime, with BaseTime's stored
    ///   sub-second set to each block's own, else every second), and at sizes between two samples
    ///   never more than the door at any of them (eight inside each stretch and one wei either side
    ///   of each sample);
    /// - a side with no samples is a side the door reports inactive, one whose word or depth runs
    ///   out within the book's life, one whose levels age or whose listing reads a print where the
    ///   node hid its later quotes, or one whose print lapses too soon for a route to be sent and
    ///   land;
    /// - a side the door quoted runs out after the read's block.
    ///
    /// Set `EFFICIENT_PROP_RPC` (a Base node), `EFFICIENT_PROP_FRONT` and `EFFICIENT_PROP_FACTORY`;
    /// `EFFICIENT_PROP_IMPLEMENTATIONS` (comma-separated) sets the allow-list, which is otherwise
    /// the implementation the front runs now, `EFFICIENT_PROP_PRICE_SOURCES` (comma-separated ids)
    /// the price sources approved, none otherwise, `EFFICIENT_PROP_HOLD_MS` a book's life in place
    /// of the default eight seconds, a quarter of it the request timeout, half the snapshot's age
    /// limit and a quarter the landing allowance, and `EFFICIENT_PROP_BOOKS` the number of books
    /// the read must publish. A node whose blocks trail the wall clock by more than four seconds
    /// fails the polls, as it would the feed.
    ///
    /// `EFFICIENT_PROP_RECORD` (a file path) records the read, every call it made with its answer
    /// and the doors' quotes checked, as a fixture a replay can hold wei for wei; the node must
    /// then be a plain-HTTP one, such as a local fork. No Base recording is committed: Base's
    /// deployment holds no listing yet.
    #[tokio::test]
    #[ignore = "reads Base mainnet; set EFFICIENT_PROP_RPC, EFFICIENT_PROP_FRONT and EFFICIENT_PROP_FACTORY"]
    async fn live_poll_quotes_every_door() {
        use std::collections::BTreeSet;

        use alloy::{
            eips::BlockId,
            primitives::{aliases::U24, Bytes as AlloyBytes},
            providers::{Provider, RootProvider},
            rpc::types::TransactionRequest,
            sol_types::SolCall,
        };

        use super::super::{
            reader::{doorOfCall, doorViewCall, quoteCall, state_override, FRONTCall, LISTINGCall},
            state::EfficientPropState,
            test_support::record_word_slot,
        };
        use crate::{book::sim::SwapDirection, evm::protocol::u256_num::u256_to_biguint};

        let env = |name: &str| std::env::var(name).ok();
        let (Some(rpc), Some(front), Some(factory)) =
            (env("EFFICIENT_PROP_RPC"), env("EFFICIENT_PROP_FRONT"), env("EFFICIENT_PROP_FACTORY"))
        else {
            println!(
                "skipped: EFFICIENT_PROP_RPC, EFFICIENT_PROP_FRONT or EFFICIENT_PROP_FACTORY unset"
            );
            return;
        };
        let provider: RootProvider = RootProvider::new_http(rpc.parse().unwrap());
        let front: Bytes = front.parse().unwrap();
        let front_address = Address::from_slice(&front);
        let factory: Bytes = factory.parse().unwrap();
        let factory_address = Address::from_slice(&factory);
        let implementations = match env("EFFICIENT_PROP_IMPLEMENTATIONS") {
            Some(list) => list
                .split(',')
                .map(|implementation| implementation.trim().parse().unwrap())
                .collect(),
            None => {
                let word = provider
                    .get_storage_at(
                        front_address,
                        U256::from_be_bytes(super::super::reader::IMPLEMENTATION_SLOT.0),
                    )
                    .await
                    .unwrap();
                let implementation = Address::from_word(word.into());
                println!("allowing the front's current implementation {implementation}");
                vec![bytes(implementation)]
            }
        };
        // The desk contracts' code hash the front answers, and every desk of the first 64 ids
        // approved with every listing of the first 4,096 named the test's gas: this test holds
        // the books to the doors, whoever's desk and whatever the fills cost.
        let cell_codehash = provider
            .call(
                TransactionRequest::default()
                    .to(front_address)
                    .input(
                        AlloyBytes::from(super::super::reader::CELL_CODEHASHCall {}.abi_encode())
                            .into(),
                    ),
            )
            .await
            .unwrap();
        let cell_codehash = B256::from_slice(&cell_codehash);
        let desks: Vec<Bytes> = (1..=64)
            .map(|desk| {
                bytes(super::super::slots::SLOT_RULE.account(front_address, cell_codehash, desk))
            })
            .collect();
        let deployment = EfficientPropDeployment {
            front,
            factory,
            cell_codehash: Some(Bytes::from(cell_codehash.to_vec())),
            implementations,
            quote_layout: None,
        };
        let price_sources: Vec<u32> = env("EFFICIENT_PROP_PRICE_SOURCES")
            .map(|list| {
                list.split(',')
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| id.trim().parse().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        let config = book_config();
        let record = env("EFFICIENT_PROP_RECORD");
        let proxy = match record {
            Some(_) => Some(spawn_recording_proxy(rpc.clone()).await),
            None => None,
        };
        let feed_rpc = proxy
            .as_ref()
            .map_or_else(|| rpc.clone(), |(server, _)| server.url());
        let mut builder = (1..=4096).fold(
            EfficientPropFeedBuilder::new(config.clone(), feed_rpc, deployment.clone())
                .approved_price_sources(price_sources.iter().copied())
                .with_approved_desks(desks),
            |builder, listing| builder.credit_listing(listing, named_gas()),
        );
        // A shorter life: a quarter for the poll, half served, a quarter to land.
        if let Some(hold_ms) = env("EFFICIENT_PROP_HOLD_MS") {
            let quarter = Duration::from_millis(hold_ms.parse::<u64>().unwrap() / 4);
            builder = builder
                .feed_config(HttpFeedConfig {
                    request_timeout: quarter,
                    max_snapshot_age: Some(quarter * 2),
                    ..EfficientPropFeedBuilder::default_feed_config()
                })
                .landing_allowance(quarter);
        }
        let mut source = builder.build_path().unwrap().source;
        source.clock = Arc::new(unix_now_ms);
        // Polls at the default cadence: each answers within its request timeout, and the
        // cadence they keep is reported against the interval.
        let tuning = EfficientPropFeedBuilder::default_feed_config();
        let (budget, interval) = (tuning.request_timeout, tuning.poll_interval);
        let mut slowest = Duration::ZERO;
        for _ in 0..8 {
            let started = std::time::Instant::now();
            source.fetch().await.unwrap();
            let elapsed = started.elapsed();
            slowest = slowest.max(elapsed);
            assert!(elapsed < budget, "a poll took {elapsed:?}, past its {budget:?} timeout");
            tokio::time::sleep(interval.saturating_sub(elapsed)).await;
        }
        println!(
            "8 polls, the slowest {slowest:?}, against a {interval:?} interval and a {budget:?} \
             timeout"
        );

        let scan_to = source
            .highest_listing
            .load(Ordering::Relaxed)
            .saturating_add(LISTING_LOOKAHEAD);
        if let Some((_, log)) = &proxy {
            log.lock().unwrap().clear();
        }
        let now_ms = unix_now_ms();
        let read = source
            .reader
            .read(
                scan_to,
                |token| {
                    config
                        .tokens
                        .get(&bytes(*token))
                        .map(|token| token.decimals)
                },
                Hold {
                    from_ms: now_ms,
                    for_ms: source.hold_ms,
                    max_lag_secs: MAX_READ_LAG_SECS,
                    min_side_ms: source
                        .assembler
                        .landing_ms
                        .saturating_add(MIN_SIDE_SERVED_MS),
                },
            )
            .await
            .unwrap();
        let recorded = proxy.map(|(_, log)| std::mem::take(&mut *log.lock().unwrap()));
        let mut checks = Vec::new();
        let books = source
            .assembler
            .books(&read, unix_now_ms());
        let block = BlockId::number(read.block_number);
        println!(
            "block {} at {}: {} doors read, {} books",
            read.block_number,
            read.timestamp,
            read.doors.len(),
            books.len()
        );
        if let Some(expected) = env("EFFICIENT_PROP_BOOKS") {
            assert_eq!(books.len(), expected.parse::<usize>().unwrap(), "books published");
        }
        let call = |target: Address, data: Vec<u8>| {
            let provider = provider.clone();
            async move {
                provider
                    .call(
                        TransactionRequest::default()
                            .to(target)
                            .input(AlloyBytes::from(data).into()),
                    )
                    .block(block)
                    .await
            }
        };

        // Every door, walked from listing 1 at the read's block.
        let mut doors = BTreeSet::new();
        for id in 1..=MAX_LISTING {
            let listing = U24::from(id);
            let view = call(front_address, doorViewCall { listing }.abi_encode())
                .await
                .unwrap();
            let view = doorViewCall::abi_decode_returns(&view).unwrap();
            if view.pool == Address::ZERO {
                break;
            }
            if view.creditMode > 3 {
                println!("listing {id}: left out, its credit mode {}", view.creditMode);
                continue;
            }
            let known = |token: &Address| {
                config
                    .tokens
                    .contains_key(&bytes(*token))
            };
            if !(known(&view.token0) && known(&view.token1)) {
                println!(
                    "listing {id}: left out, its tokens {} and {} not both configured",
                    view.token0, view.token1
                );
                continue;
            }
            let door = call(factory_address, doorOfCall { listing }.abi_encode())
                .await
                .unwrap();
            let door = doorOfCall::abi_decode_returns(&door).unwrap();
            let code = provider
                .get_code_at(door)
                .block_id(block)
                .await
                .unwrap();
            if code.is_empty() {
                continue;
            }
            let door_front = call(door, FRONTCall {}.abi_encode())
                .await
                .map(|answer| FRONTCall::abi_decode_returns(&answer).unwrap());
            let door_listing = call(door, LISTINGCall {}.abi_encode())
                .await
                .map(|answer| LISTINGCall::abi_decode_returns(&answer).unwrap());
            if door_front.ok() != Some(front_address) || door_listing.ok() != Some(listing) {
                continue;
            }
            // A listing reading a print (fence kind PRINT or SESSION, settings bits 228-229) is
            // served only on approved sources, its primary at word 10's bits 0-31 and its backup at
            // 32-63.
            let read_word = |index: u64| {
                let provider = provider.clone();
                async move {
                    provider
                        .get_storage_at(front_address, record_word_slot(id, index))
                        .block_id(block)
                        .await
                        .ok()
                }
            };
            let word = |index: u64| async move { read_word(index).await.unwrap() };
            let fence = (word(1).await >> 228usize) & U256::from(3u8);
            let sources = word(10).await;
            let source = |shift: usize| ((sources >> shift) & U256::from(u32::MAX)).to::<u32>();
            let (primary, backup) = (source(0), source(32));
            let prints = fence == U256::from(FENCE_PRINT) || fence == U256::from(FENCE_SESSION);
            if prints &&
                !(price_sources.contains(&primary) &&
                    (backup == 0 || price_sources.contains(&backup)))
            {
                println!(
                    "listing {id}: left out, it reads a print from sources {primary} and {backup}, \
                     not all approved"
                );
                continue;
            }
            // Nor is a tied listing (settings bits 228-231 equal to 8) or one whose live source's
            // copy of its print (record word 2) is unread; one on a guarded source (bit 247) is
            // kept.
            let tied = (word(1).await >> 228usize) & U256::from(15u8) == U256::from(8u8);
            let copy = read_word(2).await;
            if prints && (tied || copy.is_none()) {
                println!(
                    "listing {id}: left out, tied {tied}, its source's copy read {}",
                    copy.is_some()
                );
                continue;
            }
            if prints && copy.is_some_and(|copy| copy.bit(247)) {
                println!("listing {id}: on a guarded source, kept");
            }
            // Nor is a SESSION listing with a closed phase (record word 13's `closedAfter`, bits
            // 0-15) or whose session word is unread.
            let closed_after = read_word(13)
                .await
                .map(|session| session & U256::from(u16::MAX));
            if fence == U256::from(FENCE_SESSION) && closed_after != Some(U256::ZERO) {
                println!(
                    "listing {id}: left out, a SESSION listing closing after {closed_after:?} s"
                );
                continue;
            }
            doors.insert((id, door));
        }
        let read_doors: BTreeSet<_> = read
            .doors
            .iter()
            .map(|door| (door.listing, door.door))
            .collect();
        assert_eq!(read_doors, doors, "the doors read are every door of the front");

        // The door's quote at the read's block, and under a block override at each later block of
        // the book's life, as the front's clock steps, through the last before its end.
        let life = read.clock.blocks_until(read.until_ms);
        let blocks = life.len();
        println!(
            "the book's life: {blocks} blocks, {} to {} ms on the front's clock",
            life[0].ms,
            life[blocks - 1].ms
        );
        let door_quotes =
            |door: Address, token_in: Address, token_out: Address, amount: U256, until_ms: u64| {
                let provider = provider.clone();
                let data = quoteCall { tokenIn: token_in, tokenOut: token_out, amountIn: amount }
                    .abi_encode();
                let request = TransactionRequest::default()
                    .to(door)
                    .input(AlloyBytes::from(data).into());
                let life: Vec<_> = life
                    .iter()
                    .copied()
                    .filter(|at| at.ms < until_ms)
                    .collect();
                let number = read.block_number;
                async move {
                    let mut quotes = Vec::new();
                    for at in life {
                        let answer = if at.number == number {
                            provider
                                .call(request.clone())
                                .block(block)
                                .await
                                .ok()
                        } else {
                            let state = state_override(&at);
                            let at = serde_json::json!({
                                "number": format!("{:#x}", at.number),
                                "time": format!("{:#x}", at.time),
                            });
                            provider
                                .client()
                                .request::<_, AlloyBytes>(
                                    "eth_call",
                                    (request.clone(), block, state, at),
                                )
                                .await
                                .ok()
                        };
                        quotes.push(
                            answer.map(|answer| quoteCall::abi_decode_returns(&answer).unwrap()),
                        );
                    }
                    quotes
                }
            };
        for door in &read.doors {
            // Each side runs out no later than its own depth: `doorView`'s first depth word is the
            // bid's (the base in) and its third the ask's, whichever token the base is.
            let view =
                call(front_address, doorViewCall { listing: U24::from(door.listing) }.abi_encode())
                    .await
                    .unwrap();
            let view = doorViewCall::abi_decode_returns(&view).unwrap();
            let expires_ms =
                |depth: U256| ((depth >> 60usize) & U256::from(u32::MAX)).to::<u64>() * 1000;
            let (bid_expires_ms, ask_expires_ms) =
                (expires_ms(view.depth[0]), expires_ms(view.depth[2]));
            println!(
                "listing {}: bid runs out at {} ms, its depth expires at {bid_expires_ms}; ask at \
                 {} ms, {ask_expires_ms}",
                door.listing, door.bid_runs_out_ms, door.ask_runs_out_ms
            );
            assert!(door.bid_runs_out_ms <= bid_expires_ms, "listing {}: the bid", door.listing);
            assert!(door.ask_runs_out_ms <= ask_expires_ms, "listing {}: the ask", door.listing);
            let (base, quote) = (bytes(door.base), bytes(door.quote));
            let id = door_component_id(&base, &quote, &bytes(door.door));
            // The checks below run after the sides' deadlines, which they do not hold.
            let state = books.get(&id.to_string()).map(|book| {
                let state = book
                    .state
                    .as_any()
                    .downcast_ref::<EfficientPropState>()
                    .unwrap();
                for direction in [SwapDirection::BaseToQuote, SwapDirection::QuoteToBase] {
                    if let Some(until) = state.side_until(direction) {
                        println!(
                            "listing {} {direction:?}: its print lapses within the book's life, \
                             quoted until {} ms on the wall clock",
                            door.listing, until.unix_ms
                        );
                    }
                }
                state.clone().with_deadlines(None, None)
            });
            for (direction, token_in, token_out, samples, runs_out_ms, holds, side_until_ms) in [
                (
                    SwapDirection::BaseToQuote,
                    door.base,
                    door.quote,
                    &door.bid,
                    door.bid_runs_out_ms,
                    door.bid_holds,
                    door.bid_until_ms,
                ),
                (
                    SwapDirection::QuoteToBase,
                    door.quote,
                    door.base,
                    &door.ask,
                    door.ask_runs_out_ms,
                    door.ask_holds,
                    door.ask_until_ms,
                ),
            ] {
                let points: Vec<(U256, U256)> = state
                    .as_ref()
                    .map(|state| state.side(direction).clone().into())
                    .unwrap_or_default();
                let label = format!(
                    "listing {} {token_in}->{token_out} (runs out at {runs_out_ms} ms)",
                    door.listing
                );
                let quoted = samples
                    .iter()
                    .any(|(_, output)| output.is_some());
                if quoted {
                    assert!(
                        runs_out_ms > read.timestamp * 1000,
                        "{label}: the door quoted at a block past the side's run-out"
                    );
                }
                let until_ms = side_until_ms;
                if points.is_empty() && quoted && until_ms < read.until_ms {
                    println!(
                        "{label}: left out, its print lapses at {until_ms} ms, too soon for a \
                         route to be sent and land"
                    );
                    continue;
                }
                if points.is_empty() && quoted && runs_out_ms < until_ms {
                    println!("{label}: left out, it runs out before the side's end at {until_ms}");
                    continue;
                }
                if points.is_empty() && !holds {
                    println!(
                        "{label}: left out, the node hid its later quotes where its levels age or \
                         it reads a print, or its print refuses within the book's life"
                    );
                    continue;
                }
                if points.is_empty() {
                    let active = call(
                        door.door,
                        isActiveCall { tokenIn: token_in, tokenOut: token_out }.abi_encode(),
                    )
                    .await
                    .unwrap();
                    assert!(
                        !isActiveCall::abi_decode_returns(&active).unwrap(),
                        "{label}: the book quotes nothing on a side the door reports active"
                    );
                    println!("{label}: no samples, the door reports the side inactive");
                    continue;
                }
                let state = state.as_ref().unwrap();
                let (token_in_model, token_out_model) =
                    (&config.tokens[&bytes(token_in)], &config.tokens[&bytes(token_out)]);
                for &(amount, output) in &points {
                    let quotes =
                        door_quotes(door.door, token_in, token_out, amount, until_ms).await;
                    checks.push(DoorCheck {
                        door: door.door,
                        token_in,
                        token_out,
                        amount,
                        sample: true,
                        quotes: quotes.clone(),
                    });
                    let least = quotes
                        .iter()
                        .map(|quote| {
                            quote.unwrap_or_else(|| {
                                panic!("{label}: the door refuses {amount} in the book's life")
                            })
                        })
                        .min();
                    assert_eq!(
                        least,
                        Some(output),
                        "{label}: the book's sample at {amount}, against the door over {blocks} \
                         blocks: {quotes:?}"
                    );
                }
                let mut between = Vec::new();
                // A recording keeps the middle of each stretch beside the samples.
                let mut middles = Vec::new();
                for pair in points.windows(2) {
                    let ((a0, _), (a1, _)) = (pair[0], pair[1]);
                    middles.push(a0 + (a1 - a0) / U256::from(2u8));
                    between.extend([a0 + U256::from(1u8), a1 - U256::from(1u8)]);
                    between.extend(
                        (1u8..8)
                            .map(|eighth| a0 + (a1 - a0) * U256::from(eighth) / U256::from(8u8)),
                    );
                }
                let mut checked = 0;
                let mut worst_bps = 0f64;
                for amount in between {
                    let book = state
                        .get_amount_out(u256_to_biguint(amount), token_in_model, token_out_model)
                        .unwrap()
                        .amount;
                    let quotes =
                        door_quotes(door.door, token_in, token_out, amount, until_ms).await;
                    if middles.contains(&amount) {
                        checks.push(DoorCheck {
                            door: door.door,
                            token_in,
                            token_out,
                            amount,
                            sample: false,
                            quotes: quotes.clone(),
                        });
                    }
                    let fill = quotes
                        .into_iter()
                        .map(|quote| {
                            quote.unwrap_or_else(|| {
                                panic!("{label}: the book quotes {amount}, which the door refuses")
                            })
                        })
                        .min()
                        .unwrap();
                    let fill = u256_to_biguint(fill);
                    assert!(
                        book <= fill,
                        "{label}: at {amount} the book quotes {book} over the door's {fill}"
                    );
                    let gap = (&fill - &book)
                        .to_string()
                        .parse::<f64>()
                        .unwrap() /
                        fill.to_string().parse::<f64>().unwrap() *
                        1e4;
                    worst_bps = worst_bps.max(gap);
                    checked += 1;
                }
                let (first, last) = (points[0], points[points.len() - 1]);
                println!(
                    "{label}: {} samples, {} to {} in for {} to {} out, exact; {checked} sizes \
                     between, none above the door at any block of the side's life, at most \
                     {worst_bps:.3} bp under its least",
                    points.len(),
                    first.0,
                    last.0,
                    first.1,
                    last.1
                );
            }
        }
        if let (Some(path), Some(rpc)) = (record, recorded) {
            let address = |bytes: &Bytes| Address::from_slice(bytes);
            let poll = RecordedPoll {
                front: front_address,
                factory: factory_address,
                implementations: deployment
                    .implementations
                    .iter()
                    .map(address)
                    .collect(),
                price_sources,
                now_ms,
                scan_to,
                books: books.len(),
                rpc,
                checks,
            };
            std::fs::write(&path, serde_json::to_string_pretty(&poll).unwrap() + "\n").unwrap();
            println!("recorded the read in {path}");
        }
    }

    /// The listing doors on Ethereum, whose builders land each maker's update in the block that
    /// fills it: every door is read at the next block under the state map the state URLs answer.
    mod ethereum {
        use std::{
            collections::BTreeMap,
            sync::{
                atomic::{AtomicU64, AtomicUsize},
                Mutex,
            },
        };

        use tycho_common::{models::token::Token, simulation::errors::SimulationError};

        use super::*;
        use crate::{
            book::sim::SwapDirection,
            pamm::protocols::efficient_prop::{
                overrides::{BEACON_GENESIS, MERGE_WINDOW, SLOT_SECS},
                state::GasChain,
                test_support::{
                    approved_desks, credit_gas, desk_cell, ethereum_deployment, left_out_of_a_poll,
                    named_gas, price_contract, quote_word, read_state_map,
                    spawn_shared_state_server, spawn_state_server, state_map, state_map_of,
                    with_account, FakeAging, HashPins, RecordedEthereumPoll, RecordedListing,
                    RecordedSide, ETHEREUM_BLOCK, ETHEREUM_NEXT_TIME, ETHEREUM_SLOT, FENCE_NONE,
                    IMPLEMENTATION, QUOTE_BASE, VECTOR_WORDS,
                },
            },
            protocol::models::ProtocolComponent,
            snapshot_feed::http::test_support::{
                spawn_http_server, MockHttpServer, MockRequest, MockResponse,
            },
        };

        /// The wall clock of the tests: six seconds before the next block.
        const NOW_MS: u64 = (ETHEREUM_NEXT_TIME - 6) * 1000;

        /// The next block's map: for the block after the fake node's latest, its quote contracts'
        /// entries.
        fn next_map(desks: &[(u32, bool)]) -> serde_json::Value {
            state_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT, desks)
        }

        fn ethereum_config() -> BookFeedConfig {
            BookFeedConfig { chain: Chain::Ethereum, ..book_config() }
        }

        /// A source polling `node_url` on Ethereum, reading its maps from `state_urls`, its wall
        /// clock stopped at `now_ms`.
        fn ethereum_source(
            node_url: &str,
            state_urls: Vec<String>,
            now_ms: u64,
        ) -> EfficientPropBookSource {
            ethereum_source_of(node_url, state_urls, now_ms, ethereum_deployment())
        }

        /// As [`ethereum_source`], of the front `deployment` names.
        fn ethereum_source_of(
            node_url: &str,
            state_urls: Vec<String>,
            now_ms: u64,
            deployment: EfficientPropDeployment,
        ) -> EfficientPropBookSource {
            let source =
                EfficientPropFeedBuilder::new(ethereum_config(), node_url.to_string(), deployment)
                    .approved_price_sources([APPROVED_SOURCE])
                    .with_approved_desks(approved_desks())
                    .usd_quote_tokens(HashSet::from([bytes(usdc_address())]))
                    .state_urls(state_urls);
            // The fake listings trade test tokens, which no figure was measured for.
            let mut source = (1..=64)
                .fold(source, |builder, listing| builder.credit_listing(listing, named_gas()))
                .build()
                .unwrap()
                .source;
            source.clock = Arc::new(move || now_ms);
            source
        }

        /// The books one poll of `node` publishes on Ethereum, its maps from servers answering
        /// `maps` in that order.
        async fn fetch_with(
            node: FakeNode,
            maps: Vec<serde_json::Value>,
            now_ms: u64,
        ) -> Result<HashMap<String, Book>, FeedError> {
            let node = node.spawn().await;
            let mut servers = Vec::new();
            for map in maps {
                servers.push(spawn_state_server(map).await);
            }
            let urls = servers
                .iter()
                .map(|server| server.url())
                .collect();
            let snapshot = ethereum_source(&node.url(), urls, now_ms)
                .fetch()
                .await?;
            Ok(Arc::try_unwrap(snapshot.books).unwrap())
        }

        async fn ethereum_books(
            listings: Vec<FakeListing>,
            maps: Vec<serde_json::Value>,
        ) -> HashMap<String, Book> {
            fetch_with(FakeNode::ethereum(listings), maps, NOW_MS)
                .await
                .unwrap()
        }

        /// What the door quotes in block `number` for each sample of `side`, with the maker's
        /// update for that block landed.
        fn door_quotes(
            listing: &FakeListing,
            side: &[(U256, U256)],
            bid: bool,
            time: u64,
        ) -> Vec<(U256, U256)> {
            let (token_in, token_out) =
                if bid { (listing.base, listing.quote) } else { (listing.quote, listing.base) };
            side.iter()
                .map(|&(amount, _)| {
                    (
                        amount,
                        listing
                            .quote_at(token_in, token_out, amount, time * 1000)
                            .unwrap(),
                    )
                })
                .collect()
        }

        fn samples(state: &EfficientPropState, direction: SwapDirection) -> Vec<(U256, U256)> {
            state.side(direction).points().to_vec()
        }

        /// A maker's update for the next block makes its listing quotable: the book quotes the
        /// door's own answers in that block, both ways, and each side quotes until that block's
        /// time less the landing allowance, a second on Ethereum by default. The node never reads
        /// BaseTime, and asks no door without the next block's number, time and state map (the
        /// fake node asserts both).
        #[tokio::test]
        async fn the_next_blocks_state_map_makes_a_listing_quotable() {
            let listing = FakeListing::ethereum(1);

            let books = ethereum_books(vec![listing.clone()], vec![next_map(&[(1, false)])]).await;

            assert_eq!(books.len(), 1);
            let state = state_for(&books, &listing);
            let (bid, ask) = (
                samples(state, SwapDirection::BaseToQuote),
                samples(state, SwapDirection::QuoteToBase),
            );
            assert!(!bid.is_empty() && !ask.is_empty());
            assert_eq!(bid, door_quotes(&listing, &bid, true, ETHEREUM_NEXT_TIME));
            assert_eq!(ask, door_quotes(&listing, &ask, false, ETHEREUM_NEXT_TIME));
            let deadline = ETHEREUM_NEXT_TIME * 1000 + 1 - 1000;
            for direction in [SwapDirection::BaseToQuote, SwapDirection::QuoteToBase] {
                assert_eq!(
                    state
                        .side_until(direction)
                        .unwrap()
                        .unix_ms,
                    deadline
                );
            }
            // The component names the block its quotes fill in and when each side stops quoting.
            let attributes = &books
                .values()
                .next()
                .unwrap()
                .component
                .static_attributes;
            let number = |number: u64| Bytes::from(U256::from(number).to_be_bytes_trimmed_vec());
            assert_eq!(attributes["valid_from_block"], number(ETHEREUM_BLOCK + 1));
            assert_eq!(attributes["bid_valid_until_ms"], number(deadline));
            assert_eq!(attributes["ask_valid_until_ms"], number(deadline));
        }

        /// Through the fallback router every swap is charged that path's one figure, the least
        /// either of the router's branches costs less the estimator's own overhead: a listing of
        /// a pair no figure was measured for is served without its gas named.
        #[tokio::test]
        async fn a_fallback_feed_charges_the_fallback_paths_gas() {
            let listing = FakeListing::ethereum(1);
            let node = FakeNode::ethereum(vec![listing.clone()])
                .spawn()
                .await;
            let state = spawn_state_server(next_map(&[(1, false)])).await;
            let mut source =
                EfficientPropFeedBuilder::new(ethereum_config(), node.url(), ethereum_deployment())
                    .with_approved_desks(approved_desks())
                    .with_fallback_router()
                    .usd_quote_tokens(HashSet::from([bytes(usdc_address())]))
                    .state_urls([state.url()])
                    .build()
                    .unwrap()
                    .source;
            source.clock = Arc::new(|| NOW_MS);

            let books = source.fetch().await.unwrap().books;

            let book = books.values().next().unwrap();
            assert_eq!(book.component.protocol_system, "fallback:efficient_prop");
            let state = book
                .state
                .as_any()
                .downcast_ref::<EfficientPropState>()
                .unwrap();
            for (direction, token_in, token_out) in [
                (SwapDirection::BaseToQuote, weth(), usdc()),
                (SwapDirection::QuoteToBase, usdc(), weth()),
            ] {
                let (smallest, _) = state.side(direction).first().unwrap();
                let gas = state
                    .get_amount_out(
                        BigUint::from_bytes_be(&smallest.to_be_bytes::<32>()),
                        &token_in,
                        &token_out,
                    )
                    .unwrap()
                    .gas;
                assert_eq!(gas, BigUint::from(50_445u64));
            }
            // Beside that floor the book names neither what a fill at the door is expected to
            // cost nor what its call adds to a gas limit: the listing has no figure of its own to
            // state either from.
            let attributes = &book.component.static_attributes;
            assert!(!attributes.contains_key("expected_gas"));
            assert!(!attributes.contains_key("gas_limit_addition"));
        }

        /// A book sent direct names beside its swaps' charges what a fill its pool's custody
        /// pays is measured to cost, the dearer direction's figure, and what its call adds to a
        /// gas limit: that figure, the custody fill's refund of 19,900 and a quarter on top, or
        /// the dearest fill its gas names and a quarter on top where that is more. A listing
        /// with a credit mode names no expected gas, and an addition only where its gas names
        /// the dearest fill measured on the listing: that fill and a quarter on top. So does one
        /// sharing its pool, served its custody sizes alone: the front sends such a fill through
        /// the credit route where custody falls short at the fill, so the custody figures
        /// (160,877, and 225,972 for a limit) are not this book's.
        #[rstest]
        #[case::a_listing_in_no_credit_mode(
            FakeListing::ethereum(1),
            named_gas(),
            false,
            Some(96_239),
            Some(145_174)
        )]
        #[case::a_listing_in_no_credit_mode_naming_its_dearest_fill(
            FakeListing::ethereum(1),
            CreditGas { most_before_refunds: std::num::NonZeroU64::new(150_000), ..named_gas() },
            false,
            Some(96_239),
            Some(187_500)
        )]
        #[case::a_credit_listing_naming_four_figures(
            FakeListing::ethereum_credit(1),
            credit_gas(),
            false,
            None,
            None
        )]
        #[case::a_credit_listing_naming_its_dearest_fill(
            FakeListing::ethereum_credit(1),
            CreditGas { most_before_refunds: std::num::NonZeroU64::new(700_000), ..credit_gas() },
            false,
            None,
            Some(875_000)
        )]
        #[case::a_credit_listing_served_its_custody_alone_naming_four_figures(
            FakeListing::ethereum_credit(1),
            credit_gas(),
            true,
            None,
            None
        )]
        #[case::a_credit_listing_served_its_custody_alone_naming_its_dearest_fill(
            FakeListing::ethereum_credit(1),
            CreditGas { most_before_refunds: std::num::NonZeroU64::new(700_000), ..credit_gas() },
            true,
            None,
            Some(875_000)
        )]
        #[tokio::test]
        async fn a_book_names_its_expected_gas_and_its_gas_limit_addition(
            #[case] listing: FakeListing,
            #[case] gas: CreditGas,
            #[case] shares_its_pool: bool,
            #[case] expected_gas: Option<u64>,
            #[case] gas_limit_addition: Option<u64>,
        ) {
            // A second listing of the pair on the listing's pool cuts a credit listing to the
            // sizes its pool's custody fills.
            let sibling = FakeListing { pool: listing.pool, ..FakeListing::ethereum(2) };
            let (listings, desks, named) = if shares_its_pool {
                (
                    vec![listing.clone(), sibling],
                    vec![(1, false), (2, false)],
                    vec![(1, gas), (2, named_gas())],
                )
            } else {
                (vec![listing.clone()], vec![(1, false)], vec![(1, gas)])
            };
            let books = books_naming(listings, vec![next_map(&desks)], &named).await;

            let id = door_component_id(
                &bytes(listing.base),
                &bytes(listing.quote),
                &bytes(door_of(listing.id)),
            );
            let state = state_for(&books, &listing);
            let serves_a_routed_size = state.bid.routes() || state.ask.routes();
            assert_eq!(serves_a_routed_size, listing.credit_mode != 0 && !shares_its_pool);
            let attributes = &books[&id.to_string()]
                .component
                .static_attributes;
            let number = |number: u64| Bytes::from(U256::from(number).to_be_bytes_trimmed_vec());
            assert_eq!(attributes.get("expected_gas"), expected_gas.map(number).as_ref());
            assert_eq!(
                attributes.get("gas_limit_addition"),
                gas_limit_addition.map(number).as_ref()
            );
        }

        /// The books of two polls of listings 1 and 2, each with its update in the map, by a feed
        /// the builder was given `desks` and, where `list` is one, a desk list answering it.
        async fn books_approving(desks: Vec<Bytes>, list: Option<serde_json::Value>) -> Vec<Bytes> {
            let node = FakeNode::ethereum(vec![FakeListing::ethereum(1), FakeListing::ethereum(2)])
                .spawn()
                .await;
            let state = spawn_state_server(next_map(&[(1, false), (2, false)])).await;
            let mut builder =
                EfficientPropFeedBuilder::new(ethereum_config(), node.url(), ethereum_deployment())
                    .approved_price_sources([APPROVED_SOURCE])
                    .with_approved_desks(desks)
                    .credit_listing(1, named_gas())
                    .credit_listing(2, named_gas())
                    .usd_quote_tokens(HashSet::from([bytes(usdc_address())]))
                    .state_urls([state.url()]);
            let server = match list {
                Some(list) => {
                    Some(spawn_http_server(move |_| Some(("200 OK", list.to_string()))).await)
                }
                None => None,
            };
            if let Some(server) = &server {
                builder = builder.with_approved_desks_url(server.url());
            }
            let mut source = builder.build().unwrap().source;
            source.clock = Arc::new(|| NOW_MS);
            let books = source.fetch().await.unwrap().books;
            let mut doors: Vec<Bytes> = books
                .values()
                .map(|book| book.component.static_attributes["pamm_address"].clone())
                .collect();
            doors.sort();
            doors
        }

        /// A listing is served only where its maker's desk contract is approved, and a feed built
        /// with no desk serves nothing: anyone can reserve a listing and post quotes to it.
        #[tokio::test]
        async fn only_an_approved_desks_listings_are_served() {
            let door = |listing: u32| bytes(door_of(listing));

            assert_eq!(books_approving(vec![], None).await, Vec::<Bytes>::new());
            assert_eq!(books_approving(vec![bytes(desk_cell(2))], None).await, [door(2)]);
            assert_eq!(
                books_approving(vec![bytes(desk_cell(1)), bytes(desk_cell(2))], None).await,
                {
                    let mut both = vec![door(1), door(2)];
                    both.sort();
                    both
                }
            );
        }

        /// A list from the desks URL rules in place of the builder's set, however long ago it
        /// last changed, and one for another chain leaves the builder's set ruling.
        #[tokio::test]
        async fn a_desk_list_rules_and_a_refused_one_falls_back() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let list = |chain: u64, updated_at: u64| {
                Some(serde_json::json!({
                    "chainId": chain, "updatedAt": updated_at, "desks": [desk_cell(2)],
                }))
            };
            let own = vec![bytes(desk_cell(1))];

            assert_eq!(books_approving(own.clone(), list(1, now)).await, [bytes(door_of(2))]);
            assert_eq!(
                books_approving(own.clone(), list(1, now - 86_400)).await,
                [bytes(door_of(2))]
            );
            assert_eq!(books_approving(own, list(8453, now)).await, [bytes(door_of(1))]);
        }

        /// A listing a poll leaves out still counts as reserved: sixteen listings in a credit mode
        /// nobody named, then one open listing, which the second poll finds and serves. Counting
        /// served listings alone, the scan would stop sixteen past none for ever.
        #[tokio::test]
        async fn listings_left_out_do_not_hide_the_ones_reserved_after_them() {
            let mut listings: Vec<_> = (1..=LISTING_LOOKAHEAD)
                .map(FakeListing::ethereum_credit)
                .collect();
            let open = FakeListing::ethereum(LISTING_LOOKAHEAD + 1);
            listings.push(open.clone());
            let node = FakeNode::ethereum(listings)
                .spawn()
                .await;
            let state = spawn_state_server(next_map(&[(open.id, false)])).await;
            let source = ethereum_source(&node.url(), vec![state.url()], NOW_MS);

            let first = source.fetch().await.unwrap().books;
            let second = source.fetch().await.unwrap().books;

            assert!(first.is_empty());
            assert_eq!(
                source
                    .highest_listing
                    .load(Ordering::Relaxed),
                LISTING_LOOKAHEAD + 1
            );
            assert_eq!(second.len(), 1);
            state_for(&second, &open);
        }

        /// Without its update in the map a listing is not served, even where its maker's last
        /// landing, in the latest block and carrying, would still fill in the next.
        #[tokio::test]
        async fn a_listing_whose_quote_contract_has_no_entry_is_not_served() {
            let with_entry = FakeListing::ethereum(1);
            let without = FakeListing {
                carry: true,
                landed: Some((ETHEREUM_BLOCK, true)),
                ..FakeListing::ethereum(2)
            };

            let books =
                ethereum_books(vec![with_entry.clone(), without], vec![next_map(&[(1, false)])])
                    .await;

            assert_eq!(books.len(), 1);
            state_for(&books, &with_entry);
        }

        /// Two listings of one maker, on desk 1 at word indices 0 and 7 (listing ids 1 and 2), and
        /// the entries of the next block's map: `(key, pair slot, quote word)`, all on desk 1.
        fn two_on_one_desk() -> Vec<FakeListing> {
            vec![
                FakeListing { desk: 1, word_index: 0, ..FakeListing::ethereum(1) },
                FakeListing { desk: 1, word_index: 7, ..FakeListing::ethereum(2) },
            ]
        }

        fn desk_map(entries: &[(&str, u8, B256)]) -> serde_json::Value {
            let entries: Vec<_> = entries
                .iter()
                .map(|(key, pair, word)| ((*key).to_string(), 1u32, *pair, *word))
                .collect();
            state_map_of(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT, &entries)
        }

        fn fresh(epoch: u32) -> B256 {
            quote_word(ETHEREUM_BLOCK + 1, epoch, false, false)
        }

        /// The ids of the listings served, sorted.
        fn served(books: &HashMap<String, Book>, listings: &[FakeListing]) -> Vec<u32> {
            listings
                .iter()
                .filter(|listing| {
                    books.contains_key(
                        &door_component_id(
                            &bytes(listing.base),
                            &bytes(listing.quote),
                            &bytes(door_of(listing.id)),
                        )
                        .to_string(),
                    )
                })
                .map(|listing| listing.id)
                .collect()
        }

        /// Two listings on one desk contract, at word indices 0 and 7: an answer stamping only the
        /// first serves it and not the second, though the second's chain word is old.
        #[tokio::test]
        async fn an_update_of_one_pair_on_a_desk_serves_that_pair_alone() {
            let listings = two_on_one_desk();
            let map = desk_map(&[("a", 0, fresh(0))]);

            let books = ethereum_books(listings.clone(), vec![map]).await;

            assert_eq!(served(&books, &listings), [1]);
        }

        /// Both pairs stamped in the answer are both served, each from its own slot.
        #[tokio::test]
        async fn updates_of_both_pairs_on_a_desk_serve_both() {
            let listings = two_on_one_desk();
            let map = desk_map(&[("a", 0, fresh(0)), ("b", 7, fresh(0))]);

            let books = ethereum_books(listings.clone(), vec![map]).await;

            assert_eq!(served(&books, &listings), [1, 2]);
        }

        /// A stop at the head darkens both pairs of the desk, though the answer stamps both pairs'
        /// words well under the head's epoch with bit 63 clear: a pair's word never shows a stop.
        /// No door is asked for them.
        #[tokio::test]
        async fn a_stop_at_the_head_darkens_both_pairs_on_a_desk() {
            let listings = two_on_one_desk();
            let map = desk_map(&[("a", 0, fresh(5)), ("b", 7, fresh(5))]);
            let mut node = FakeNode::ethereum(listings.clone());
            node.desk_heads.insert(1, (5, true));
            let overridden = Arc::clone(&node.overridden);

            let books = fetch_with(node, vec![map], NOW_MS)
                .await
                .unwrap();

            assert!(books.is_empty());
            assert!(!overridden
                .lock()
                .unwrap()
                .contains(&desk_cell(1)));
        }

        /// After a stop and a revive in a later block the head's epoch is higher: a word written
        /// under the epoch before is empty, whatever its stamp, and a word under the new epoch is
        /// served. The fake front reads the desk word as the front does, so a book served on a
        /// stale word would also be one its door refuses.
        #[tokio::test]
        async fn a_word_written_before_a_stop_and_revive_is_not_served() {
            let listings = two_on_one_desk();
            let map = desk_map(&[("a", 0, fresh(5)), ("b", 7, fresh(6))]);
            let mut node = FakeNode::ethereum(listings.clone());
            node.desk_heads.insert(1, (6, false));

            let books = fetch_with(node, vec![map], NOW_MS)
                .await
                .unwrap();

            assert_eq!(served(&books, &listings), [2]);
        }

        /// The desk epochs 16,847,216 and 4,464 agree in their low 16 bits, not in all 31: a word
        /// of one is empty under a head at the other, in both directions.
        #[rstest]
        #[case::word_big_head_small(16_847_216, 4_464)]
        #[case::word_small_head_big(4_464, 16_847_216)]
        #[tokio::test]
        async fn epochs_agreeing_in_the_low_16_bits_serve_nothing(
            #[case] word_epoch: u32,
            #[case] head_epoch: u32,
        ) {
            let listings = vec![FakeListing::ethereum(1)];
            let map = desk_map(&[("a", 0, fresh(word_epoch))]);
            let mut node = FakeNode::ethereum(listings.clone());
            node.desk_heads
                .insert(1, (head_epoch, false));

            let books = fetch_with(node, vec![map], NOW_MS)
                .await
                .unwrap();

            assert!(books.is_empty());
        }

        /// The vectors' pair slots (0, 7, 255) at their desk epochs (0, 3, 16,847,216): a word at
        /// pair slot `i` is served for the listing at word index `i` under the head's epoch, and
        /// not for the listing at another word index of the desk.
        #[rstest]
        #[case::slot_0_epoch_0(0)]
        #[case::slot_7_epoch_3(1)]
        #[case::slot_255_epoch_16_847_216(2)]
        #[tokio::test]
        async fn a_pair_slot_serves_only_the_listing_at_its_word_index(#[case] index: usize) {
            let (pair, epoch, _, _) = VECTOR_WORDS[index];
            let listings = vec![
                FakeListing { desk: 1, word_index: pair, ..FakeListing::ethereum(1) },
                FakeListing { desk: 1, word_index: pair ^ 1, ..FakeListing::ethereum(2) },
            ];
            let map = desk_map(&[("a", pair, fresh(epoch))]);
            let mut node = FakeNode::ethereum(listings.clone());
            node.desk_heads
                .insert(1, (epoch, false));

            let books = fetch_with(node, vec![map], NOW_MS)
                .await
                .unwrap();

            assert_eq!(served(&books, &listings), [1]);
        }

        /// An answer that also writes a slot of the desk contract the front alone writes (here the
        /// desk word's) covers none of the desk's listings, though both pairs' words are stamped.
        #[rstest]
        #[case::the_desk_word(1)]
        #[case::the_word_key(3)]
        #[case::slot_zero(0)]
        #[tokio::test]
        async fn a_write_to_a_slot_of_the_front_darkens_both_pairs_on_a_desk(#[case] slot: u64) {
            let listings = two_on_one_desk();
            let mut map = desk_map(&[("a", 0, fresh(0)), ("b", 7, fresh(0))]);
            map["a"]["stateOverride"][desk_cell(1).to_string()]["stateDiff"]
                [B256::from(U256::from(slot)).to_string()] =
                B256::repeat_byte(9).to_string().into();

            let books = ethereum_books(listings.clone(), vec![map]).await;

            assert!(books.is_empty());
        }

        /// Two entries of one answer writing pair 0's word differently drop that listing, and leave
        /// the listing at word index 7 served.
        #[tokio::test]
        async fn a_conflict_on_one_pairs_word_leaves_the_other_pair_served() {
            let listings = two_on_one_desk();
            let map = desk_map(&[
                ("a-1", 0, fresh(0)),
                ("a-2", 0, quote_word(ETHEREUM_BLOCK + 1, 0, true, false)),
                ("b", 7, fresh(0)),
            ]);

            let books = ethereum_books(listings.clone(), vec![map]).await;

            assert_eq!(served(&books, &listings), [2]);
        }

        /// A registry lane's write sits in the registry's account until a fill copies it: an answer
        /// writing only the registry's account serves no listing.
        #[tokio::test]
        async fn an_answer_writing_only_the_registry_serves_nothing() {
            let registry = Address::repeat_byte(0xda);
            let listing = FakeListing::ethereum(1);
            let map = serde_json::json!({
                "blockNumber": format!("{:#x}", ETHEREUM_BLOCK + 1),
                "slot": ETHEREUM_SLOT,
                "lane": { "stateOverride": { registry.to_string(): { "stateDiff": {
                    B256::from(U256::from(QUOTE_BASE)).to_string(): fresh(0).to_string(),
                }}}},
            });

            let books = ethereum_books(vec![listing], vec![map]).await;

            assert!(books.is_empty());
        }

        /// A front whose desk contract code hash is not the configured one ends the feed with
        /// nothing listed: no maker's quote could be placed.
        #[tokio::test]
        async fn a_front_of_another_desk_code_hash_ends_the_feed() {
            let mut node = FakeNode::ethereum(vec![FakeListing::ethereum(1)]);
            node.front_cell_codehash = B256::repeat_byte(0x99);

            let error = fetch_with(node, vec![next_map(&[(1, false)])], NOW_MS)
                .await
                .unwrap_err();

            assert!(error.is_fatal(), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("desk contracts of code hash"),
                "{error}"
            );
        }

        /// A pinned door code hash holds the factory to it: a factory answering another ends the
        /// feed, the pinned one's is served, and a pin that is no hash does not build.
        #[tokio::test]
        async fn a_factory_of_another_door_code_hash_than_the_pinned_ends_the_feed() {
            let node = FakeNode::ethereum(vec![FakeListing::ethereum(1)])
                .spawn()
                .await;
            let state = spawn_state_server(next_map(&[(1, false)])).await;
            let source = |pinned: B256| {
                let mut source = ethereum_source(&node.url(), vec![state.url()], NOW_MS);
                source.reader.door_codehash = Some(pinned);
                source
            };

            let error = source(B256::repeat_byte(0x99))
                .fetch()
                .await
                .unwrap_err();
            let served =
                source(crate::pamm::protocols::efficient_prop::test_support::DOOR_CODEHASH)
                    .fetch()
                    .await
                    .unwrap();
            let unbuilt =
                EfficientPropFeedBuilder::new(ethereum_config(), node.url(), ethereum_deployment())
                    .state_urls([state.url()])
                    .with_door_code_hash(Bytes::from(vec![1u8; 31]))
                    .build()
                    .unwrap_err();

            assert!(error.is_fatal(), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("doors of code hash"),
                "{error}"
            );
            assert_eq!(served.books.len(), 1);
            assert!(
                unbuilt
                    .to_string()
                    .contains("not a hash of 32 bytes"),
                "{unbuilt}"
            );
        }

        /// A map whose entry writes a listing's quote contract stamped for another block carries
        /// no update of it for the next block, and its door is not asked: a stamp of the latest
        /// block serves only a listing that carries, through the next block alone, since the
        /// word fills no later; an older stamp serves none.
        #[rstest]
        #[case::the_latest_carried(ETHEREUM_BLOCK, true, true)]
        #[case::the_latest_not_carrying(ETHEREUM_BLOCK, false, false)]
        #[case::an_older_block(ETHEREUM_BLOCK - 1, true, false)]
        #[tokio::test]
        async fn a_map_stamping_another_block_serves_only_a_word_carried_into_the_next(
            #[case] stamp: u64,
            #[case] carry: bool,
            #[case] served: bool,
        ) {
            // The head holds the word the answer names, so a stamp of the latest block is a word
            // carried into the next.
            let listing = FakeListing {
                carry,
                landed: Some((ETHEREUM_BLOCK, true)),
                ..FakeListing::ethereum(1)
            };
            let mut map = next_map(&[(1, true)]);
            map["desk-1"]["stateOverride"][desk_cell(1).to_string()]["stateDiff"]
                [B256::from(U256::from(QUOTE_BASE)).to_string()] =
                quote_word(stamp, 0, true, false)
                    .to_string()
                    .into();
            let node = FakeNode::ethereum(vec![listing.clone()]);
            let overridden = Arc::clone(&node.overridden);

            let books = fetch_with(node, vec![map], NOW_MS)
                .await
                .unwrap();

            assert_eq!(books.len(), usize::from(served));
            assert_eq!(
                overridden
                    .lock()
                    .unwrap()
                    .contains(&desk_cell(1)),
                served
            );
            if served {
                assert_eq!(
                    state_for(&books, &listing)
                        .side_until(SwapDirection::BaseToQuote)
                        .unwrap()
                        .unix_ms,
                    ETHEREUM_NEXT_TIME * 1000 + 1 - 1000
                );
            }
        }

        /// A word stamped with the head block is carried into the next block only where the head
        /// holds it: no landing in the next block writes a stamp of the head block. Where the head
        /// holds an older word, a word that does not carry, or none, the fill in the next block
        /// reads that and refuses, so nothing is served from the answer's word.
        #[rstest]
        #[case::the_head_holds_an_older_word(Some((ETHEREUM_BLOCK - 5, false)))]
        #[case::the_head_holds_the_word_not_carrying(Some((ETHEREUM_BLOCK, false)))]
        #[case::the_pair_never_landed(None)]
        #[tokio::test]
        async fn a_carried_word_the_head_does_not_hold_is_not_served(
            #[case] landed: Option<(u64, bool)>,
        ) {
            let listing = FakeListing { carry: true, landed, ..FakeListing::ethereum(1) };
            let map = state_map_of(
                ETHEREUM_BLOCK + 1,
                ETHEREUM_SLOT,
                &[("desk-1".to_string(), 1, 0, quote_word(ETHEREUM_BLOCK, 0, true, false))],
            );

            let books = ethereum_books(vec![listing], vec![map]).await;

            assert!(books.is_empty());
        }

        /// A map for a block already made, or for one past the node's next, serves nothing.
        #[rstest]
        #[case::for_the_latest_block(ETHEREUM_BLOCK)]
        #[case::for_a_block_already_made(ETHEREUM_BLOCK - 1)]
        #[case::past_the_next_block(ETHEREUM_BLOCK + 2)]
        #[tokio::test]
        async fn a_map_not_for_the_next_block_serves_nothing(#[case] number: u64) {
            let listing = FakeListing { landed: Some((number, false)), ..FakeListing::ethereum(1) };

            let books = ethereum_books(
                vec![listing],
                vec![state_map(number, ETHEREUM_SLOT, &[(1, false)])],
            )
            .await;

            assert!(books.is_empty());
        }

        /// Of several URLs' maps, the one for the next block is read whichever URL answers it, and
        /// a map for the latest block is not.
        #[rstest]
        #[case::first(true)]
        #[case::last(false)]
        #[tokio::test]
        async fn the_map_for_the_next_block_is_read_whichever_url_answers_it(
            #[case] newest_first: bool,
        ) {
            let listing = FakeListing::ethereum(1);
            let older = state_map(ETHEREUM_BLOCK, ETHEREUM_SLOT - 1, &[]);
            let maps = if newest_first {
                vec![next_map(&[(1, false)]), older]
            } else {
                vec![older, next_map(&[(1, false)])]
            };

            let books = ethereum_books(vec![listing.clone()], maps).await;

            assert_eq!(books.len(), 1);
            state_for(&books, &listing);
        }

        /// Where the listing and the maker's word both carry, the word fills in the block after
        /// too: each side is sampled there under the same map, quotes the least of the two
        /// blocks, and quotes until the later block's time less the allowance. Where the word does
        /// not carry, or the listing's fence is NONE, the side lives through the next block only.
        #[rstest]
        #[case::both_carry(true, true, FENCE_WORD, true)]
        #[case::the_word_does_not(true, false, FENCE_WORD, false)]
        #[case::the_listing_does_not(false, true, FENCE_WORD, false)]
        #[case::no_fence(true, true, FENCE_NONE, false)]
        #[tokio::test]
        async fn a_carrying_listing_is_sampled_through_the_block_after(
            #[case] listing_carries: bool,
            #[case] word_carries: bool,
            #[case] fence: u64,
            #[case] lives_on: bool,
        ) {
            // The bid drifts 10 ppm a second: it quotes less in the block after.
            let listing = FakeListing {
                carry: listing_carries,
                fence,
                aging: FakeAging { bid_drift: 10, ..FakeAging::default() },
                ..FakeListing::ethereum(1)
            };

            let books =
                ethereum_books(vec![listing.clone()], vec![next_map(&[(1, word_carries)])]).await;

            let state = state_for(&books, &listing);
            let bid = samples(state, SwapDirection::BaseToQuote);
            let last = if lives_on { ETHEREUM_NEXT_TIME + SLOT_SECS } else { ETHEREUM_NEXT_TIME };
            assert_eq!(bid, door_quotes(&listing, &bid, true, last));
            assert_eq!(
                state
                    .side_until(SwapDirection::BaseToQuote)
                    .unwrap()
                    .unix_ms,
                last * 1000 + 1 - 1000
            );
        }

        /// A listing whose fence is NONE never lets its maker's word carry, so the poll does not
        /// ask its doors about the block after, whatever its carry bits say.
        #[tokio::test]
        async fn a_listing_without_a_fence_is_not_asked_about_the_block_after() {
            let mut calls = Vec::new();
            for carry in [false, true] {
                let listing = FakeListing { carry, fence: FENCE_NONE, ..FakeListing::ethereum(1) };
                let node = FakeNode::ethereum(vec![listing.clone()]);
                let counted = Arc::clone(&node.eth_calls);

                let books = fetch_with(node, vec![next_map(&[(1, carry)])], NOW_MS)
                    .await
                    .unwrap();

                state_for(&books, &listing);
                calls.push(counted.load(Ordering::SeqCst));
            }
            assert_eq!(calls[0], calls[1]);
        }

        /// A book on Ethereum of a pair no figure was measured for charges the gas named for its
        /// listing, not a class's.
        #[tokio::test]
        async fn a_book_on_ethereum_charges_its_gas() {
            let listing = FakeListing::ethereum(1);

            let books = ethereum_books(vec![listing.clone()], vec![next_map(&[(1, false)])]).await;

            let state = state_for(&books, &listing);
            assert_eq!(state.gas_chain, GasChain::Ethereum);
            let (smallest, _) = state
                .side(SwapDirection::BaseToQuote)
                .first()
                .unwrap();
            let gas = state
                .get_amount_out(
                    BigUint::from_bytes_be(&smallest.to_be_bytes::<32>()),
                    &weth(),
                    &usdc(),
                )
                .unwrap()
                .gas;
            assert_eq!(gas, BigUint::from(named_gas().custody.sell_base.get()));
        }

        /// A node that ignores or refuses the next block's overrides cannot show any quote on
        /// Ethereum: the poll fails.
        #[rstest]
        #[case::ignored(Overrides::Ignored)]
        #[case::refused(Overrides::Refused)]
        #[tokio::test]
        async fn a_node_without_the_next_blocks_overrides_fails_the_poll(
            #[case] overrides: Overrides,
        ) {
            let mut node = FakeNode::ethereum(vec![FakeListing::ethereum(1)]);
            node.overrides = overrides;

            let error = fetch_with(node, vec![next_map(&[(1, false)])], NOW_MS)
                .await
                .unwrap_err();

            assert!(matches!(error, FeedError::Connection(_)), "{error}");
        }

        /// A front on Ethereum reads the block's second; one on another clock ends the feed.
        #[tokio::test]
        async fn a_front_on_another_clock_ends_the_feed() {
            let mut node = FakeNode::ethereum(vec![FakeListing::ethereum(1)]);
            node.clock = FakeClock::base_time(0);

            let error = fetch_with(node, vec![next_map(&[(1, false)])], NOW_MS)
                .await
                .unwrap_err();

            assert!(matches!(error, FeedError::Fatal(_)), "{error}");
        }

        /// A map whose block's time is more than four seconds behind the wall clock fails the
        /// poll as a lagging read.
        #[tokio::test]
        async fn a_map_past_its_block_fails_the_poll() {
            let late = (ETHEREUM_NEXT_TIME + 5) * 1000;

            let error = fetch_with(
                FakeNode::ethereum(vec![FakeListing::ethereum(1)]),
                vec![next_map(&[(1, false)])],
                late,
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("behind"), "{error}");
        }

        /// A poll whose state URLs all fail fails, without the URLs, which may carry a key.
        #[tokio::test]
        async fn a_poll_whose_state_urls_fail_does_not_report_them() {
            let node = FakeNode::ethereum(vec![FakeListing::ethereum(1)])
                .spawn()
                .await;
            let source = ethereum_source(
                &node.url(),
                vec!["http://127.0.0.1:1/secret_key".to_string()],
                NOW_MS,
            );

            let error = source.fetch().await.unwrap_err();

            assert!(matches!(error, FeedError::Connection(_)), "{error}");
            assert!(!error.to_string().contains("secret_key"), "{error}");
        }

        /// Everything the feed and the HTTP client log, at every level, on this test's thread.
        #[derive(Clone, Default)]
        struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Logs {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .map_err(|e| std::io::Error::other(e.to_string()))?
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// A URL may carry a key in its query or its userinfo: neither the node's nor a state
        /// URL's reaches the poll's error, which the feed logs, or any log line at any level,
        /// whether the node fails, a state URL fails beside one that answers, or every state URL
        /// fails.
        #[rstest]
        #[case::the_node_fails(false, true)]
        #[case::one_state_url_fails(true, true)]
        #[case::every_state_url_fails(true, false)]
        #[tokio::test]
        async fn no_key_in_a_url_reaches_an_error_or_a_log(
            #[case] node_answers: bool,
            #[case] one_url_answers: bool,
        ) {
            let logs = Logs::default();
            let writer = logs.clone();
            let _logging = tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::TRACE)
                    .with_ansi(false)
                    .with_writer(move || writer.clone())
                    .finish(),
            );
            let node = FakeNode::ethereum(vec![FakeListing::ethereum(1)])
                .spawn()
                .await;
            let answering = spawn_state_server(next_map(&[(1, false)])).await;
            let keyed = |url: &str, name: &str| {
                let mut url = Url::parse(url).unwrap();
                url.set_username(&format!("{name}_user_key"))
                    .unwrap();
                url.set_password(Some(&format!("{name}_password_key")))
                    .unwrap();
                url.set_query(Some(&format!("apikey={name}_query_key")));
                url.to_string()
            };
            let node_url = keyed(
                &if node_answers { node.url() } else { "http://127.0.0.1:1/".to_string() },
                "node",
            );
            let mut state_urls = vec![keyed("http://127.0.0.1:1/v1/1/state", "state")];
            if one_url_answers {
                state_urls.push(answering.url());
            }
            let source = ethereum_source(&node_url, state_urls, NOW_MS);

            let polled = source.fetch().await;

            let error = polled
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert_eq!(polled.is_ok(), node_answers && one_url_answers, "{error}");
            let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
            assert!(logged.contains("state map not read"), "{logged}");
            for key in ["user_key", "password_key", "query_key"] {
                assert!(!error.contains(key), "{key} in the error: {error}");
                assert!(!logged.contains(key), "{key} in the logs: {logged}");
            }
        }

        /// Base builds through the public build with its desks approved and no state URL.
        #[test]
        fn the_build_serves_base() {
            assert_eq!(book_config().chain, Chain::Base);

            let built = EfficientPropFeedBuilder::new(
                book_config(),
                "http://node".to_string(),
                deployment(),
            )
            .with_approved_desks(approved_desks())
            .credit_listing(1, credit_gas())
            .build();

            assert!(built.is_ok());
        }

        /// On Ethereum the feed needs a state URL that is one; on Base none is taken; other chains
        /// are refused.
        #[rstest]
        #[case::ethereum_without_a_state_url(Chain::Ethereum, &[])]
        #[case::ethereum_with_a_state_url_that_is_not_one(Chain::Ethereum, &["state"])]
        #[case::base_with_a_state_url(Chain::Base, &["http://state"])]
        #[case::another_chain(Chain::Arbitrum, &["http://state"])]
        fn the_builder_refuses_a_chain_and_state_urls_that_do_not_fit(
            #[case] chain: Chain,
            #[case] urls: &[&str],
        ) {
            let config = BookFeedConfig { chain, ..book_config() };

            let error = EfficientPropFeedBuilder::new(
                config,
                "http://node".to_string(),
                ethereum_deployment(),
            )
            .state_urls(urls.iter().map(ToString::to_string))
            .build_path()
            .unwrap_err();

            assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
        }

        /// The desk contract code hash places each maker's desk contract on every chain, and each
        /// maker's quote on Ethereum, so it is required and must be 32 bytes. The doors' code hash
        /// is no setting at all.
        #[rstest]
        #[case::ethereum_without_it(Chain::Ethereum, None, "needs the desk contract code hash")]
        #[case::ethereum_not_32_bytes(Chain::Ethereum, Some(vec![1u8; 20]), "not 32 bytes")]
        #[case::base_without_it(Chain::Base, None, "needs the desk contract code hash")]
        #[case::base_not_32_bytes(Chain::Base, Some(vec![1u8; 20]), "not 32 bytes")]
        fn the_builder_needs_the_desk_code_hash(
            #[case] chain: Chain,
            #[case] cell_codehash: Option<Vec<u8>>,
            #[case] reason: &str,
        ) {
            let config = BookFeedConfig { chain, ..book_config() };
            let mut deployment = ethereum_deployment();
            deployment.cell_codehash = cell_codehash.map(Bytes::from);
            if chain == Chain::Base {
                // Base reads no quote contract: only the layout setting, which it refuses, differs.
                deployment.quote_layout = None;
            }

            let error =
                EfficientPropFeedBuilder::new(config, "http://node".to_string(), deployment)
                    .state_urls(
                        ["http://state".to_string()]
                            .into_iter()
                            .filter(|_| chain == Chain::Ethereum),
                    )
                    .build_path()
                    .unwrap_err();

            assert!(
                matches!(&error, FeedError::InvalidInput(message) if message.contains(reason)),
                "{error}"
            );
        }

        /// The quote contract's layout is read only on Ethereum, where the per-desk rule runs: it
        /// is required there and must be "builder"; on Base, whose path samples the doors and reads
        /// no quote contract, it is refused if set.
        #[rstest]
        #[case::ethereum_without_it(Chain::Ethereum, None, "is not one this integration reads")]
        #[case::ethereum_another_layout(
            Chain::Ethereum,
            Some("storage"),
            "is not one this integration reads"
        )]
        #[case::ethereum_empty(Chain::Ethereum, Some(""), "is not one this integration reads")]
        #[case::base_with_builder(Chain::Base, Some("builder"), "reads no quote contract")]
        #[case::base_with_another(Chain::Base, Some("storage"), "reads no quote contract")]
        fn the_builder_reads_a_quote_layout_on_ethereum_only(
            #[case] chain: Chain,
            #[case] layout: Option<&str>,
            #[case] reason: &str,
        ) {
            let config = BookFeedConfig { chain, ..book_config() };
            let mut deployment = ethereum_deployment();
            deployment.quote_layout = layout.map(str::to_string);
            if chain == Chain::Base {
                deployment.cell_codehash = None;
            }

            let error =
                EfficientPropFeedBuilder::new(config, "http://node".to_string(), deployment)
                    .state_urls(
                        ["http://state".to_string()]
                            .into_iter()
                            .filter(|_| chain == Chain::Ethereum),
                    )
                    .build_path()
                    .unwrap_err();

            assert!(
                matches!(&error, FeedError::InvalidInput(message) if message.contains(reason)),
                "{error}"
            );
        }

        /// A credit listing is named once, and only of an id the front hands out, on either
        /// chain. A zero gas figure cannot be written at all ([`DirectionGas`] holds none).
        ///
        /// [`DirectionGas`]: crate::pamm::protocols::efficient_prop::DirectionGas
        #[rstest]
        #[case::named_twice_on_base(Chain::Base, &[1, 1], "named twice")]
        #[case::past_the_last_id_on_base(Chain::Base, &[MAX_LISTING + 1], "not an id")]
        #[case::named_twice(Chain::Ethereum, &[1, 1], "named twice")]
        #[case::listing_zero(Chain::Ethereum, &[0], "not an id")]
        #[case::past_the_last_id(Chain::Ethereum, &[MAX_LISTING + 1], "not an id")]
        fn the_builder_refuses_a_credit_listing_it_cannot_serve(
            #[case] chain: Chain,
            #[case] listings: &[u32],
            #[case] reason: &str,
        ) {
            let (deployment, urls) = if chain == Chain::Base {
                (deployment(), Vec::new())
            } else {
                (ethereum_deployment(), vec!["http://state".to_string()])
            };
            let builder = listings.iter().fold(
                EfficientPropFeedBuilder::new(
                    BookFeedConfig { chain, ..book_config() },
                    "http://node".to_string(),
                    deployment,
                ),
                |builder, &listing| builder.credit_listing(listing, credit_gas()),
            );

            let error = builder
                .state_urls(urls)
                .build_path()
                .unwrap_err();

            assert!(
                matches!(&error, FeedError::InvalidInput(message) if message.contains(reason)),
                "{error}"
            );
        }

        /// The fallback router's quote runs a credit listing's route once more than the direct
        /// path its gas is named for, so the builder serves no credit listing through it.
        #[test]
        fn the_builder_refuses_a_credit_listing_through_the_fallback_router() {
            let error = EfficientPropFeedBuilder::new(
                BookFeedConfig { chain: Chain::Ethereum, ..book_config() },
                "http://node".to_string(),
                ethereum_deployment(),
            )
            .credit_listing(1, credit_gas())
            .with_fallback_router()
            .state_urls(vec!["http://state".to_string()])
            .build_path()
            .unwrap_err();

            assert!(
                matches!(&error, FeedError::InvalidInput(m) if m.contains("fallback router")),
                "{error}"
            );
        }

        /// The books one poll of an Ethereum node of `listings` publishes, its maps from
        /// `maps`, serving the credit listings `credit` at [`credit_gas`].
        /// The books one poll publishes with `credit` named at [`credit_gas`] and every listing
        /// in no credit mode at [`named_gas`]: the fake listings trade test tokens, which no
        /// figure was measured for.
        async fn credit_books(
            listings: Vec<FakeListing>,
            maps: Vec<serde_json::Value>,
            credit: &[u32],
        ) -> HashMap<String, Book> {
            let named: Vec<_> = credit
                .iter()
                .map(|&listing| (listing, credit_gas()))
                .chain(
                    listings
                        .iter()
                        .filter(|listing| listing.credit_mode == 0 && !credit.contains(&listing.id))
                        .map(|listing| (listing.id, named_gas())),
                )
                .collect();
            books_naming(listings, maps, &named).await
        }

        /// The books one poll publishes with each of `named` configured with its gas.
        async fn books_naming(
            listings: Vec<FakeListing>,
            maps: Vec<serde_json::Value>,
            named: &[(u32, CreditGas)],
        ) -> HashMap<String, Book> {
            let node = FakeNode::ethereum(listings)
                .spawn()
                .await;
            let mut servers = Vec::new();
            for map in maps {
                servers.push(spawn_state_server(map).await);
            }
            let builder = named.iter().fold(
                EfficientPropFeedBuilder::new(ethereum_config(), node.url(), ethereum_deployment()),
                |builder, &(listing, gas)| builder.credit_listing(listing, gas),
            );
            let mut source = builder
                .with_approved_desks(approved_desks())
                .usd_quote_tokens(HashSet::from([bytes(usdc_address())]))
                .state_urls(
                    servers
                        .iter()
                        .map(|server| server.url()),
                )
                .build()
                .unwrap()
                .source;
            source.clock = Arc::new(|| NOW_MS);
            let snapshot = source.fetch().await.unwrap();
            Arc::try_unwrap(snapshot.books).unwrap()
        }

        /// The least bid the credit listing's route fills: 0.1 WETH and over.
        fn credit_prefix() -> U256 {
            U256::from(100_000_000_000_000_000u64)
        }

        /// The gas a state charges a swap of `amount` in `direction`, or its refusal.
        fn charged(
            state: &EfficientPropState,
            direction: SwapDirection,
            amount: U256,
        ) -> Result<BigUint, SimulationError> {
            let (token_in, token_out) = match direction {
                SwapDirection::BaseToQuote => (weth(), usdc()),
                SwapDirection::QuoteToBase => (usdc(), weth()),
            };
            state
                .get_amount_out(
                    BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
                    &token_in,
                    &token_out,
                )
                .map(|result| result.gas)
        }

        /// A credit listing named with its gas is served on Ethereum: its bid quotes the door's
        /// own answers in the next block, the sizes inside custody and those its credit route
        /// fills alike, each charged its path's configured gas, and nothing between the largest
        /// custody size and the smallest routed one; its ask, all inside custody, the custody
        /// figure buying base. The front's plan is asked beside each quote (the fake front
        /// asserts it is asked at no tip, with the door as the taker).
        #[tokio::test]
        async fn a_named_credit_listing_is_served_with_its_route_and_its_gas() {
            let listing = FakeListing::ethereum_credit(1);

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, false)])], &[1]).await;

            let state = state_for(&books, &listing);
            assert_eq!(state.credit, Some(credit_gas()));
            let bid = samples(state, SwapDirection::BaseToQuote);
            assert_eq!(bid, door_quotes(&listing, &bid, true, ETHEREUM_NEXT_TIME));
            let (custody, routed): (Vec<_>, Vec<_>) = bid
                .iter()
                .partition(|&&(amount, _)| amount <= credit_prefix());
            let (Some(&(custody_most, _)), Some(&(route_least, _))) =
                (custody.last(), routed.first())
            else {
                panic!("expected both paths in {bid:?}");
            };
            let bid_gas = |amount| charged(state, SwapDirection::BaseToQuote, amount);
            if custody.len() > 1 {
                assert_eq!(bid_gas(custody[0].0).unwrap(), BigUint::from(160_877u64));
            }
            // The largest custody size may empty the pool: charged less what that earns back.
            assert_eq!(bid_gas(custody_most).unwrap(), BigUint::from(151_277u64));
            assert_eq!(bid_gas(route_least).unwrap(), BigUint::from(610_501u64));
            assert_eq!(bid_gas(bid.last().unwrap().0).unwrap(), BigUint::from(610_501u64));
            let across = bid_gas(custody_most + U256::from(1u8)).unwrap_err();
            let SimulationError::InvalidInput(_, Some(partial)) = across else {
                panic!("expected the custody's fill handed back, got {across}");
            };
            let custody_out = custody.last().unwrap().1;
            assert_eq!(partial.amount, BigUint::from_bytes_be(&custody_out.to_be_bytes::<32>()));
            assert_eq!(partial.gas, BigUint::from(151_277u64), "the largest custody size's gas");
            let ask = samples(state, SwapDirection::QuoteToBase);
            assert_eq!(ask, door_quotes(&listing, &ask, false, ETHEREUM_NEXT_TIME));
            assert_eq!(
                charged(state, SwapDirection::QuoteToBase, ask[0].0).unwrap(),
                BigUint::from(158_340u64)
            );
        }

        /// A credit listing's bid whose door reports nothing fillable, its pool holding none of
        /// the quote token and its route filling less than the maker's depth, is still served as
        /// far as its route reaches: the door is asked across the token's scale, the largest size
        /// it answers is 2^-42 of the probe (0.227 WETH; four times that is refused), and the
        /// steps above it are sampled up to the first the door refuses. Every sample is the
        /// door's own answer and is charged the route's gas.
        #[tokio::test]
        async fn a_credit_bid_reporting_nothing_fillable_is_served_as_far_as_its_route_reaches() {
            let most = U256::from(600_000_000_000_000_000u64);
            let bid = FakeSide {
                credit_route: Some((U256::ZERO, U256::from(2_700_000_000u64))),
                custody_most: Some(U256::ZERO),
                max_in: most,
                ..FakeSide::weth_bid()
            };
            let listing = FakeListing { bid, ..FakeListing::ethereum_credit(1) };

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, false)])], &[1]).await;

            let state = state_for(&books, &listing);
            let bid = samples(state, SwapDirection::BaseToQuote);
            assert_eq!(bid, door_quotes(&listing, &bid, true, ETHEREUM_NEXT_TIME));
            let probe = U256::from(10u8).pow(U256::from(30u8));
            let (reach, refused) = (probe >> 42usize, probe >> 40usize);
            let sampled: Vec<_> = bid
                .iter()
                .map(|&(amount, _)| amount)
                .collect();
            assert!(sampled.contains(&reach), "{sampled:?}");
            // The last of the eight steps from the reach to the size refused that the door fills.
            let last = reach + (refused - reach) * U256::from(4u8) / U256::from(8u8);
            assert_eq!(sampled.last(), Some(&last));
            assert!(last <= most && last + (refused - reach) / U256::from(8u8) > most);
            for &(amount, _) in &bid {
                assert_eq!(
                    charged(state, SwapDirection::BaseToQuote, amount).unwrap(),
                    BigUint::from(610_501u64)
                );
            }
        }

        /// A credit listing's ask whose door reports only what its pool's custody fills is also
        /// sampled above that, where its route (a repay and a withdrawal) fills: the door is asked
        /// a 64th above the custody's most, then that step doubled, and the steps from the largest
        /// answered to the first refused. Custody sizes are charged the custody's gas, routed
        /// ones the route's.
        #[tokio::test]
        async fn a_credit_ask_is_sampled_past_its_custody_where_its_route_fills() {
            let custody = U256::from(2_048_000_000u64);
            let most = U256::from(2_100_000_000u64);
            let ask = FakeSide {
                // The route's price, a little under the custody's: a routed size's output still
                // grows past the custody's most.
                credit_route: Some((custody, U256::from(999_000_000_000_000_000u64))),
                custody_most: Some(custody),
                max_in: most,
                ..FakeSide::weth_ask()
            };
            let listing = FakeListing { ask, ..FakeListing::ethereum_credit(1) };

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, false)])], &[1]).await;

            let state = state_for(&books, &listing);
            let ask = samples(state, SwapDirection::QuoteToBase);
            assert_eq!(ask, door_quotes(&listing, &ask, false, ETHEREUM_NEXT_TIME));
            let sampled: Vec<_> = ask
                .iter()
                .map(|&(amount, _)| amount)
                .collect();
            assert!(sampled.contains(&custody), "{sampled:?}");
            // 2,048 + 32 answers, 2,048 + 64 is refused: seven steps of 4 between them, of which
            // 2,084 to 2,100 fill.
            let step = custody >> 6usize;
            assert!(sampled.contains(&(custody + step)), "{sampled:?}");
            assert_eq!(sampled.last(), Some(&most));
            let ask_gas = |amount| charged(state, SwapDirection::QuoteToBase, amount).unwrap();
            // The largest custody size, charged less what emptying the pool earns back.
            assert_eq!(ask_gas(custody), BigUint::from(148_740u64));
            assert_eq!(ask_gas(custody + step), BigUint::from(187_000u64));
            assert_eq!(ask_gas(most), BigUint::from(187_000u64));
        }

        /// A credit listing's ask is served past its custody only in a state where its route
        /// fills there. The first pool's route on this side repays what the pool owes: while it
        /// owes nothing the door refuses every size above the custody's most (`NotEnoughDepth`
        /// in the fill, as measured on a fork of the deployed build), so the side is its custody
        /// sizes alone, each charged the custody's gas, and nothing is quoted above them.
        #[tokio::test]
        async fn a_credit_ask_refused_past_its_custody_is_served_its_custody_alone() {
            let custody = U256::from(2_048_000_000u64);
            let ask = FakeSide {
                credit_route: None,
                custody_most: Some(custody),
                max_in: custody,
                ..FakeSide::weth_ask()
            };
            let listing = FakeListing { ask, ..FakeListing::ethereum_credit(1) };

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, false)])], &[1]).await;

            let state = state_for(&books, &listing);
            let ask = samples(state, SwapDirection::QuoteToBase);
            assert_eq!(ask, door_quotes(&listing, &ask, false, ETHEREUM_NEXT_TIME));
            assert_eq!(ask.last().map(|&(amount, _)| amount), Some(custody));
            for &(amount, _) in &ask[..ask.len() - 1] {
                assert_eq!(
                    charged(state, SwapDirection::QuoteToBase, amount).unwrap(),
                    BigUint::from(158_340u64)
                );
            }
            assert!(charged(state, SwapDirection::QuoteToBase, custody + U256::from(1u8)).is_err());
        }

        /// A custody fill that pays into its pool a token the pool holds none of is charged the
        /// first write of the pool's balance on top of its figure. A routed fill is charged
        /// nothing more: its tokens pass through the pool and the write is mostly refunded.
        #[rstest]
        #[case::holding_both((false, false), 0, 0, 0)]
        #[case::no_base((true, false), 16_964, 0, 0)]
        #[case::no_quote((false, true), 0, 16_964, 0)]
        #[case::neither((true, true), 16_964, 16_964, 0)]
        #[tokio::test]
        async fn a_fill_into_a_pool_holding_none_of_its_token_is_charged_the_first_write(
            #[case] pool_lacks: (bool, bool),
            #[case] custody_sell: u64,
            #[case] custody_buy: u64,
            #[case] routed: u64,
        ) {
            let listing = FakeListing { pool_lacks, ..FakeListing::ethereum_credit(1) };

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, false)])], &[1]).await;

            let state = state_for(&books, &listing);
            let bid = samples(state, SwapDirection::BaseToQuote);
            let ask = samples(state, SwapDirection::QuoteToBase);
            assert_eq!(
                charged(state, SwapDirection::BaseToQuote, bid[0].0).unwrap(),
                BigUint::from(160_877 + custody_sell)
            );
            assert_eq!(
                charged(state, SwapDirection::BaseToQuote, bid.last().unwrap().0).unwrap(),
                BigUint::from(610_501 + routed)
            );
            assert_eq!(
                charged(state, SwapDirection::QuoteToBase, ask[0].0).unwrap(),
                BigUint::from(158_340 + custody_buy)
            );
        }

        /// A listing of an asset-module pair with no credit mode is served only where it is named
        /// with its gas, and its fills are then charged that figure's custody pair: its module's
        /// reads are its token's own, so its pair's class figure would charge less than it spends.
        #[rstest]
        #[case::named(&[1], true)]
        #[case::not_named(&[], false)]
        #[tokio::test]
        async fn a_listing_of_an_asset_module_pair_is_charged_its_configured_gas(
            #[case] named: &[u32],
            #[case] served: bool,
        ) {
            let listing = FakeListing { asset_module: true, ..FakeListing::ethereum(1) };
            let named: Vec<_> = named
                .iter()
                .map(|&listing| (listing, credit_gas()))
                .collect();

            let books =
                books_naming(vec![listing.clone()], vec![next_map(&[(1, false)])], &named).await;

            assert_eq!(books.len(), usize::from(served));
            if served {
                let state = state_for(&books, &listing);
                let bid = samples(state, SwapDirection::BaseToQuote);
                let ask = samples(state, SwapDirection::QuoteToBase);
                assert_eq!(
                    charged(state, SwapDirection::BaseToQuote, bid[0].0).unwrap(),
                    BigUint::from(160_877u64)
                );
                assert_eq!(
                    charged(state, SwapDirection::QuoteToBase, ask[0].0).unwrap(),
                    BigUint::from(158_340u64)
                );
            }
        }

        /// A listing quoted in a token no listing sells for a USD quote token is priced through
        /// its own base where another listing sells that for one, and served; with no listing
        /// pricing its base either, it is left out.
        #[rstest]
        #[case::its_base_priced_beside_it(true, 2)]
        #[case::alone(false, 0)]
        #[tokio::test]
        async fn a_listing_quoted_in_an_unpriced_token_is_priced_through_its_base(
            #[case] beside: bool,
            #[case] served: usize,
        ) {
            // WETH/cbBTC at 0.03 cbBTC a WETH, 8 decimals.
            let side = |price: u64, per: u64, min_in: u64, max_in: u64| FakeSide {
                price: U256::from(price),
                per: U256::from(per),
                min_in: U256::from(min_in),
                max_in: U256::from(max_in),
                ..FakeSide::weth_bid()
            };
            let unpriced = FakeListing {
                quote: cbbtc_address(),
                bid: side(
                    3_000_000,
                    1_000_000_000_000_000_000,
                    1_000_000_000_000_000,
                    1_000_000_000_000_000_000,
                ),
                ask: side(1_000_000_000_000_000_000, 3_010_000, 3_000, 3_010_000),
                min_fill: 3_000,
                ..FakeListing::ethereum(1)
            };
            let mut listings = vec![unpriced.clone()];
            let mut desks = vec![(1, false)];
            if beside {
                listings.push(FakeListing::ethereum(2));
                desks.push((2, false));
            }

            let books = credit_books(listings, vec![next_map(&desks)], &[]).await;

            assert_eq!(books.len(), served);
            if beside {
                let bid = samples(state_for(&books, &unpriced), SwapDirection::BaseToQuote);
                assert_eq!(bid, door_quotes(&unpriced, &bid, true, ETHEREUM_NEXT_TIME));
            }
        }

        /// A credit listing is left out where it is not named, or where its credit mode is one
        /// the front does not know; a listing without a credit mode beside it is served at the
        /// gas named for it.
        #[rstest]
        #[case::not_named(2, &[])]
        #[case::another_named(2, &[3])]
        #[case::a_mode_the_front_does_not_know(4, &[1])]
        #[tokio::test]
        async fn a_credit_listing_not_named_is_not_served(
            #[case] credit_mode: u8,
            #[case] named: &[u32],
        ) {
            let credit = FakeListing { credit_mode, ..FakeListing::ethereum_credit(1) };
            let plain = FakeListing::ethereum(2);

            let books = credit_books(
                vec![credit, plain.clone()],
                vec![next_map(&[(1, false), (2, false)])],
                named,
            )
            .await;

            assert_eq!(books.len(), 1);
            assert_eq!(state_for(&books, &plain).credit, Some(named_gas()));
        }

        /// A credit listing sharing its pool keeps its route beside listings its route cannot
        /// move, and is served its custody alone beside one it can: a listing trading the token
        /// its bids borrow (its quote, the pool's token 1), which the pool's cheap lane refuses to
        /// pay while owed, or one with a credit mode of its own. A listing sharing only the
        /// pool's asset, or trading two other tokens, leaves the route served.
        #[rstest]
        #[case::sharing_only_the_pools_asset(cbbtc_address(), None, Some(2), 0, true)]
        #[case::trading_the_token_its_bids_borrow(
            usdc_address(),
            Some(cbbtc_address()),
            Some(1),
            0,
            false
        )]
        #[case::a_second_credit_listing(cbbtc_address(), None, Some(2), 2, false)]
        #[tokio::test]
        async fn a_credit_listing_keeps_its_route_beside_listings_it_cannot_move(
            #[case] quote: Address,
            #[case] base: Option<Address>,
            #[case] quote_index: Option<u8>,
            #[case] credit_mode: u8,
            #[case] routed: bool,
        ) {
            let credit = FakeListing::ethereum_credit(1);
            let template = if credit_mode == 0 {
                FakeListing::ethereum(2)
            } else {
                FakeListing::ethereum_credit(2)
            };
            let sibling = FakeListing {
                pool: credit.pool,
                base: base.unwrap_or(template.base),
                base_index: if base.is_some() { 2 } else { 0 },
                quote,
                quote_index,
                ..template
            };
            let named: &[u32] = if credit_mode == 0 { &[1] } else { &[1, 2] };

            let books = credit_books(
                vec![credit.clone(), sibling],
                vec![next_map(&[(1, false), (2, false)])],
                named,
            )
            .await;

            assert_eq!(books.len(), 2);
            let bid = samples(state_for(&books, &credit), SwapDirection::BaseToQuote);
            assert!(!bid.is_empty());
            assert_eq!(
                bid.iter()
                    .any(|&(amount, _)| amount > credit_prefix()),
                routed,
                "{bid:?}"
            );
            assert_eq!(bid, door_quotes(&credit, &bid, true, ETHEREUM_NEXT_TIME));
        }

        /// A credit listing sharing its pool with another listing of the same pair is served only
        /// the sizes its pool's custody fills, at the custody's gas: its route borrows the pair's
        /// quote, which the other listing pays out, so that listing's fill reverts behind it in
        /// the same transaction. On a pool of its own it is served its route too.
        #[tokio::test]
        async fn a_credit_listing_sharing_its_pool_is_served_its_custody_alone() {
            let credit = FakeListing::ethereum_credit(1);
            let sibling = FakeListing { pool: credit.pool, ..FakeListing::ethereum(2) };

            let books = credit_books(
                vec![credit.clone(), sibling.clone()],
                vec![next_map(&[(1, false), (2, false)])],
                &[1],
            )
            .await;

            assert_eq!(books.len(), 2);
            let state = state_for(&books, &credit);
            assert_eq!(state.credit, Some(credit_gas()));
            let bid = samples(state, SwapDirection::BaseToQuote);
            assert!(!bid.is_empty());
            assert!(
                bid.iter()
                    .all(|&(amount, _)| amount <= credit_prefix()),
                "{bid:?}"
            );
            assert_eq!(bid, door_quotes(&credit, &bid, true, ETHEREUM_NEXT_TIME));
            let most = bid.last().unwrap().0;
            assert_eq!(
                charged(state, SwapDirection::BaseToQuote, most).unwrap(),
                BigUint::from(151_277u64)
            );
            let past =
                charged(state, SwapDirection::BaseToQuote, most + U256::from(1u8)).unwrap_err();
            assert!(matches!(past, SimulationError::InvalidInput(_, Some(_))), "{past}");
            assert_eq!(state_for(&books, &sibling).credit, Some(named_gas()));

            let apart = FakeListing::ethereum(2);
            let books = credit_books(
                vec![credit.clone(), apart],
                vec![next_map(&[(1, false), (2, false)])],
                &[1],
            )
            .await;

            let bid = samples(state_for(&books, &credit), SwapDirection::BaseToQuote);
            assert!(
                bid.iter()
                    .any(|&(amount, _)| amount > credit_prefix()),
                "{bid:?}"
            );
        }

        /// A size the front's plan answers otherwise than the door (untagged and another output)
        /// is no sample: a middle custody size so answered ends the bid's run below it, and where
        /// every custody size is, the bid starts at the route, every point of it routed.
        #[rstest]
        #[case::a_middle_custody_size(31_250_000_000_000_000, 31_250_000_000_000_000)]
        #[case::every_custody_size(0, 100_000_000_000_000_000)]
        #[tokio::test]
        async fn a_size_whose_plan_disagrees_with_the_door_is_not_quoted(
            #[case] least: u64,
            #[case] most: u64,
        ) {
            let (least, most) = (U256::from(least), U256::from(most));
            let listing = FakeListing {
                bid_plan_lies: Some((least, most)),
                ..FakeListing::ethereum_credit(1)
            };

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, false)])], &[1]).await;

            let state = state_for(&books, &listing);
            let bid = samples(state, SwapDirection::BaseToQuote);
            assert!(!bid.is_empty());
            if least.is_zero() {
                assert!(
                    bid.iter()
                        .all(|&(amount, _)| amount > most),
                    "{bid:?}"
                );
                assert_eq!(
                    charged(state, SwapDirection::BaseToQuote, bid[0].0).unwrap(),
                    BigUint::from(610_501u64)
                );
            } else {
                assert!(
                    bid.iter()
                        .all(|&(amount, _)| amount < least),
                    "{bid:?}"
                );
                assert_eq!(
                    charged(state, SwapDirection::BaseToQuote, bid.last().unwrap().0).unwrap(),
                    BigUint::from(151_277u64)
                );
            }
        }

        /// A carried credit listing whose route takes over at a smaller size in the block after
        /// (0.05 WETH there, 0.1 WETH in the next block) samples the sizes between inside custody
        /// in the next block and routed in the one after: those are no sample, which ends the
        /// bid's run below them; the side still lives through the block after.
        #[tokio::test]
        async fn a_size_whose_path_changes_in_the_block_after_is_not_quoted() {
            let after_ms = (ETHEREUM_NEXT_TIME + SLOT_SECS) * 1000;
            let moved = U256::from(50_000_000_000_000_000u64);
            let listing = FakeListing {
                carry: true,
                bid_prefix_from: Some((after_ms, moved)),
                ..FakeListing::ethereum_credit(1)
            };

            let books =
                credit_books(vec![listing.clone()], vec![next_map(&[(1, true)])], &[1]).await;

            let state = state_for(&books, &listing);
            let bid = samples(state, SwapDirection::BaseToQuote);
            assert!(!bid.is_empty());
            assert!(
                bid.iter()
                    .all(|&(amount, _)| amount <= moved),
                "{bid:?}"
            );
            assert_eq!(
                charged(state, SwapDirection::BaseToQuote, bid.last().unwrap().0).unwrap(),
                BigUint::from(151_277u64)
            );
            assert_eq!(
                state
                    .side_until(SwapDirection::BaseToQuote)
                    .unwrap()
                    .unix_ms,
                after_ms + 1 - 1000
            );
        }

        #[test]
        fn the_builder_takes_ethereum_with_a_state_url() {
            let feed = EfficientPropFeedBuilder::new(
                ethereum_config(),
                "http://node".to_string(),
                ethereum_deployment(),
            )
            .state_urls(["http://state/secret_key".to_string()])
            .build()
            .unwrap();

            assert!(!format!("{feed:?}").contains("secret_key"));
            assert_eq!(feed.source.assembler.landing_ms, 1_000);
        }

        /// A state URL answering once with a map far ahead of the node (a glitch, or a map of
        /// another chain) serves nothing that poll, and the next poll, answered the next block's
        /// map, serves again: the far map's block is never taken as read.
        #[tokio::test]
        async fn one_far_ahead_map_does_not_stop_later_polls() {
            let listing = FakeListing::ethereum(1);
            let node = FakeNode::ethereum(vec![listing.clone()])
                .spawn()
                .await;
            let answered = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&answered);
            let far = state_map(ETHEREUM_BLOCK + 1_000_000, ETHEREUM_SLOT, &[(1, false)]);
            let next = next_map(&[(1, false)]);
            let server = spawn_http_server(move |request: &MockRequest| {
                let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
                let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
                let map = if first { far.clone() } else { next.clone() };
                let answer =
                    serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": map });
                Some(("200 OK", answer.to_string()))
            })
            .await;
            let source = ethereum_source(&node.url(), vec![server.url()], NOW_MS);

            let first = source.fetch().await.unwrap();
            let second = source.fetch().await.unwrap();

            assert!(first.books.is_empty());
            assert_eq!(second.books.len(), 1);
        }

        /// One URL's map far ahead of the node does not hide another URL's map for the next
        /// block: only a map for the block after the node's latest is read, whichever URL answers
        /// it.
        #[rstest]
        #[case::far_ahead_first(true)]
        #[case::far_ahead_last(false)]
        #[tokio::test]
        async fn a_map_far_ahead_does_not_hide_the_next_blocks(#[case] far_first: bool) {
            let listing = FakeListing::ethereum(1);
            let far = state_map(ETHEREUM_BLOCK + 1_000_000, ETHEREUM_SLOT + 1_000, &[(1, true)]);
            let next = next_map(&[(1, false)]);
            let maps = if far_first { vec![far, next] } else { vec![next, far] };

            let books = ethereum_books(vec![listing.clone()], maps).await;

            assert_eq!(books.len(), 1);
            state_for(&books, &listing);
        }

        /// A map for the next block whose slot is at or before the node's latest block's is
        /// sampled at no block the door fills in: it serves nothing.
        #[tokio::test]
        async fn a_map_whose_slot_is_not_after_the_latest_block_serves_nothing() {
            let listing = FakeListing::ethereum(1);
            let map = state_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT - 1, &[(1, false)]);

            let books = fetch_with(FakeNode::ethereum(vec![listing]), vec![map], NOW_MS - 12_000)
                .await
                .unwrap();

            assert!(books.is_empty());
        }

        /// A map for the next block is read only at the slot right after the node's latest
        /// block's: one naming a later slot, whether a slot was missed or the map runs ahead of
        /// the wall clock, would sample the doors at a time the next block does not land at, so it
        /// serves nothing.
        #[rstest]
        #[case::one_slot_late(1)]
        #[case::two_slots_late(2)]
        #[tokio::test]
        async fn a_map_at_a_later_slot_than_the_next_serves_nothing(#[case] late: u64) {
            let listing = FakeListing::ethereum(1);
            let map = state_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT + late, &[(1, false)]);

            let books = ethereum_books(vec![listing], vec![map]).await;

            assert!(books.is_empty());
        }

        /// Of two URLs' maps for the next block, only the one at the slot right after the node's
        /// latest block's is read, whichever URL answers it: the book lives until that block.
        #[rstest]
        #[case::late_first(true)]
        #[case::late_last(false)]
        #[tokio::test]
        async fn only_the_map_at_the_next_slot_is_read(#[case] late_first: bool) {
            let listing = FakeListing::ethereum(1);
            let late = state_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT + 1, &[(1, false)]);
            let next = next_map(&[(1, false)]);
            let maps = if late_first { vec![late, next] } else { vec![next, late] };

            let books = ethereum_books(vec![listing.clone()], maps).await;

            assert_eq!(
                state_for(&books, &listing)
                    .side_until(SwapDirection::BaseToQuote)
                    .unwrap()
                    .unix_ms,
                ETHEREUM_NEXT_TIME * 1000 + 1 - 1000
            );
        }

        /// A listing whose update only a map at a later slot carries is not served, while the map
        /// at the next slot serves its own listing: one poll never mixes two maps' times.
        #[tokio::test]
        async fn a_listing_only_a_later_slots_map_writes_is_not_served() {
            let (one, two) = (FakeListing::ethereum(1), FakeListing::ethereum(2));
            let next = next_map(&[(1, false)]);
            let late = state_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT + 1, &[(2, false)]);

            let books = ethereum_books(vec![one.clone(), two], vec![next, late]).await;

            assert_eq!(books.len(), 1);
            state_for(&books, &one);
        }

        /// A state URL already on the block after the next (ahead of the node) once a poll has
        /// served the next block: that poll serves nothing and does not fail, so the feed does not
        /// back off, and the next poll, answered the next block's map, serves again.
        #[tokio::test]
        async fn a_state_url_ahead_of_the_node_serves_nothing_and_the_next_poll_serves() {
            let listing = FakeListing::ethereum(1);
            let node = FakeNode::ethereum(vec![listing.clone()])
                .spawn()
                .await;
            let answered = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&answered);
            let next = next_map(&[(1, false)]);
            let ahead = state_map(ETHEREUM_BLOCK + 2, ETHEREUM_SLOT + 1, &[(1, false)]);
            let server = spawn_http_server(move |request: &MockRequest| {
                let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
                let second = counted.fetch_add(1, Ordering::SeqCst) == 1;
                let map = if second { ahead.clone() } else { next.clone() };
                let answer =
                    serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": map });
                Some(("200 OK", answer.to_string()))
            })
            .await;
            let source = ethereum_source(&node.url(), vec![server.url()], NOW_MS);

            let first = source.fetch().await.unwrap();
            let second = source.fetch().await.unwrap();
            let third = source.fetch().await.unwrap();

            assert_eq!(first.books.len(), 1);
            assert!(second.books.is_empty());
            assert_eq!(third.books.len(), 1);
        }

        /// A load-balanced pair of nodes, one a block behind the other, after a served poll: the
        /// newer node's poll serves the map for its next block, and the older node's, answered a
        /// lagging URL's map for its own next block within the lag allowance, fails as a rollback,
        /// so a block older than one served is never published. The listing carries, so the older
        /// map's book would otherwise still be live, through the block after its own.
        #[tokio::test]
        async fn a_lagging_node_after_a_served_poll_fails_the_poll() {
            let listing = FakeListing { carry: true, ..FakeListing::ethereum(1) };
            let now = (ETHEREUM_NEXT_TIME + 1) * 1000;
            let newer = FakeNode {
                block: ETHEREUM_BLOCK + 1,
                timestamp: ETHEREUM_NEXT_TIME,
                ..FakeNode::ethereum(vec![listing.clone()])
            }
            .spawn()
            .await;
            let older = FakeNode::ethereum(vec![listing.clone()])
                .spawn()
                .await;
            let ahead =
                spawn_state_server(state_map(ETHEREUM_BLOCK + 2, ETHEREUM_SLOT + 1, &[(1, true)]))
                    .await;
            let behind = spawn_state_server(next_map(&[(1, true)])).await;
            let primary = ethereum_source(&newer.url(), vec![ahead.url()], now);
            assert_eq!(
                primary
                    .fetch()
                    .await
                    .unwrap()
                    .books
                    .len(),
                1
            );
            let mut replica = ethereum_source(&older.url(), vec![behind.url()], now);
            replica.last_block = Arc::clone(&primary.last_block);

            let error = replica.fetch().await.unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("older than block"),
                "{error}"
            );
        }

        /// A node stopped at its latest block while the state URL follows the chain: ten minutes
        /// after a served poll, the poll fails as lagging, so the feed warns and backs off, rather
        /// than publishing nothing in silence.
        #[tokio::test]
        async fn a_stalled_node_fails_the_poll_as_lagging() {
            let listing = FakeListing::ethereum(1);
            let node = FakeNode::ethereum(vec![listing.clone()])
                .spawn()
                .await;
            let answered = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&answered);
            let next = next_map(&[(1, false)]);
            // Fifty slots later, the chain's real next block.
            let real = state_map(ETHEREUM_BLOCK + 51, ETHEREUM_SLOT + 50, &[(1, false)]);
            let server = spawn_http_server(move |request: &MockRequest| {
                let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
                let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
                let map = if first { next.clone() } else { real.clone() };
                let answer =
                    serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": map });
                Some(("200 OK", answer.to_string()))
            })
            .await;
            let mut source = ethereum_source(&node.url(), vec![server.url()], NOW_MS);
            assert_eq!(
                source
                    .fetch()
                    .await
                    .unwrap()
                    .books
                    .len(),
                1
            );
            source.clock = Arc::new(|| NOW_MS + 600_000);

            let error = source.fetch().await.unwrap_err();

            assert!(matches!(error, FeedError::Connection(_)), "{error}");
            assert!(error.to_string().contains("behind"), "{error}");
        }

        /// A node's latest block may be two slots old (one slot missed) plus the lag allowance
        /// before a poll fails: up to there a poll with no map for the next slot serves nothing
        /// without failing, so a missed slot or a state URL ahead never backs the feed off. A
        /// second later it fails as lagging.
        #[rstest]
        #[case::at_two_slots_and_the_allowance(0, true)]
        #[case::a_second_past_it(1, false)]
        #[tokio::test]
        async fn a_node_fails_as_lagging_only_two_slots_and_the_allowance_behind(
            #[case] past: u64,
            #[case] serves_nothing: bool,
        ) {
            let head_time = ETHEREUM_NEXT_TIME - SLOT_SECS;
            let now = (head_time + 2 * SLOT_SECS + MAX_READ_LAG_SECS + past) * 1000 + 999;
            // The block after the latest, a slot late: the slot after the latest was missed.
            let late = state_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT + 1, &[(1, false)]);

            let books =
                fetch_with(FakeNode::ethereum(vec![FakeListing::ethereum(1)]), vec![late], now)
                    .await;

            match books {
                Ok(books) => {
                    assert!(serves_nothing, "a poll {past} s past the allowance did not fail");
                    assert!(books.is_empty());
                }
                Err(error) => {
                    assert!(!serves_nothing, "{error}");
                    assert!(error.to_string().contains("behind"), "{error}");
                }
            }
        }

        /// Two URLs carry the same quote contract's update for the next block under different
        /// keys: the first URL's is read, whatever the keys. Here the first URL's word carries and
        /// the second's, keyed to sort first, does not.
        #[tokio::test]
        async fn the_first_urls_update_is_read_whatever_its_key() {
            let listing = FakeListing { carry: true, ..FakeListing::ethereum(1) };
            let rekey = |map: serde_json::Value, key: &str| {
                let mut map = map.as_object().unwrap().clone();
                let entry = map.remove("desk-1").unwrap();
                map.insert(key.to_string(), entry);
                serde_json::Value::Object(map)
            };
            let first = rekey(next_map(&[(1, true)]), "zz-update");
            let second = rekey(next_map(&[(1, false)]), "aa-update");

            let books = ethereum_books(vec![listing.clone()], vec![first, second]).await;

            let until = state_for(&books, &listing)
                .side_until(SwapDirection::BaseToQuote)
                .unwrap()
                .unix_ms;
            assert_eq!(until, (ETHEREUM_NEXT_TIME + SLOT_SECS) * 1000 + 1 - 1000);
        }

        /// Only the accounts of the listings sampled are overridden: their quote contracts and the
        /// price contracts they are bound to. Another account a URL's map writes (here the front's
        /// storage, which holds the implementation slot and every listing's record) is never in a
        /// call's override.
        #[tokio::test]
        async fn only_the_quote_and_price_contracts_are_overridden() {
            let listing = FakeListing { fence: FENCE_PRINT, ..FakeListing::ethereum(1) };
            let map = with_account(next_map(&[(1, false)]), "desk-1", price_contract(1));
            let map = with_account(map, "desk-1", FRONT);
            let node = FakeNode::ethereum(vec![listing.clone()]);
            let overridden = Arc::clone(&node.overridden);

            let books = fetch_with(node, vec![map], NOW_MS)
                .await
                .unwrap();

            state_for(&books, &listing);
            assert_eq!(
                *overridden.lock().unwrap(),
                std::collections::BTreeSet::from([desk_cell(1), price_contract(1)])
            );
        }

        /// A listing reading a print, PRINT or SESSION (with no closed phase), is served only
        /// where the answer carrying its quote contract's update also writes the price contract it
        /// reads: without it the door would be sampled on the latest block's print, not the one
        /// the next block's fill reads.
        #[rstest]
        #[case::with_its_price_contract(true)]
        #[case::without_it(false)]
        #[tokio::test]
        async fn a_print_listing_needs_its_price_contracts_entry(
            #[case] priced: bool,
            #[values(FENCE_PRINT, FENCE_SESSION)] fence: u64,
        ) {
            let listing = FakeListing { fence, ..FakeListing::ethereum(1) };
            let mut map = next_map(&[(1, false)]);
            if priced {
                map = with_account(map, "desk-1", price_contract(APPROVED_SOURCE));
            }

            let books = ethereum_books(vec![listing.clone()], vec![map]).await;

            assert_eq!(books.len(), usize::from(priced));
        }

        /// A listing bound to a guarded source is not quoted on Ethereum, where sources are price
        /// contracts and no lane holds a print.
        #[tokio::test]
        async fn a_listing_on_a_guarded_source_is_not_quoted() {
            let listing =
                FakeListing { fence: FENCE_PRINT, guarded: true, ..FakeListing::ethereum(1) };
            let map = with_account(next_map(&[(1, false)]), "desk-1", price_contract(1));

            let books = ethereum_books(vec![listing], vec![map]).await;

            assert!(books.is_empty());
        }

        /// A URL answering at once with a map carrying no update does not cut off a slower one
        /// carrying the next block's updates: the wait for the other URLs starts only once a map
        /// with an update has answered.
        #[tokio::test]
        async fn an_empty_map_does_not_cut_off_a_slower_one_with_the_updates() {
            let listing = FakeListing::ethereum(1);
            let node = FakeNode::ethereum(vec![listing.clone()])
                .spawn()
                .await;
            let empty = spawn_state_server(next_map(&[])).await;
            let next = next_map(&[(1, false)]);
            let slow = spawn_http_server(move |request: &MockRequest| {
                let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
                let answer =
                    serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": next });
                Some(MockResponse {
                    status: "200 OK",
                    body: answer.to_string(),
                    delay: MERGE_WINDOW * 2,
                })
            })
            .await;
            let source = ethereum_source(&node.url(), vec![empty.url(), slow.url()], NOW_MS);

            let snapshot = source.fetch().await.unwrap();

            assert_eq!(snapshot.books.len(), 1);
        }

        /// A poll late in the slot, three seconds before the next block, still serves: its sides
        /// quote until a second before that block, whatever the snapshot's age limit.
        #[tokio::test]
        async fn a_poll_late_in_the_slot_still_serves() {
            let listing = FakeListing::ethereum(1);

            let books = fetch_with(
                FakeNode::ethereum(vec![listing.clone()]),
                vec![next_map(&[(1, false)])],
                (ETHEREUM_NEXT_TIME - 3) * 1000,
            )
            .await
            .unwrap();

            assert_eq!(books.len(), 1);
            state_for(&books, &listing);
        }

        /// A carrying side whose door pays more in the block after (here a premium window closes
        /// between the two blocks) quotes the next block's lesser output, and lives through both.
        #[tokio::test]
        async fn a_carried_side_quotes_the_lesser_block_where_the_later_pays_more() {
            let listing = FakeListing {
                carry: true,
                aging: FakeAging { premium: 500, window_ms: 5_000, ..FakeAging::default() },
                ..FakeListing::ethereum(1)
            };

            let books = ethereum_books(vec![listing.clone()], vec![next_map(&[(1, true)])]).await;

            let state = state_for(&books, &listing);
            for (direction, bid) in
                [(SwapDirection::BaseToQuote, true), (SwapDirection::QuoteToBase, false)]
            {
                let points = samples(state, direction);
                assert!(!points.is_empty());
                assert_eq!(points, door_quotes(&listing, &points, bid, ETHEREUM_NEXT_TIME));
                assert!(points.iter().all(|&(amount, out)| {
                    door_quotes(&listing, &[(amount, out)], bid, ETHEREUM_NEXT_TIME + SLOT_SECS)[0]
                        .1 >
                        out
                }));
                assert_eq!(
                    state
                        .side_until(direction)
                        .unwrap()
                        .unix_ms,
                    (ETHEREUM_NEXT_TIME + SLOT_SECS) * 1000 + 1 - 1000
                );
            }
        }

        /// A carrying side's size the door refuses in the next block is not quoted, though the
        /// door fills it in the block after: here the smallest bids are refused in the next block
        /// alone.
        #[tokio::test]
        async fn a_size_refused_in_the_next_block_is_not_quoted_from_the_one_after() {
            let refused_to = U256::from(10_000_000_000_000_000u64);
            let next_ms = ETHEREUM_NEXT_TIME * 1000;
            let listing = FakeListing {
                carry: true,
                bid_refused: Some((next_ms, next_ms + 1, U256::ZERO, refused_to)),
                ..FakeListing::ethereum(1)
            };

            let books = ethereum_books(vec![listing.clone()], vec![next_map(&[(1, true)])]).await;

            let bid = samples(state_for(&books, &listing), SwapDirection::BaseToQuote);
            assert!(!bid.is_empty());
            assert!(
                bid.iter()
                    .all(|&(amount, _)| amount > refused_to),
                "{bid:?}"
            );
        }

        /// The carry falls back: where the listing and its word carry but the door refuses every
        /// size in the block after (the word's life ends before it), the side keeps the next
        /// block's samples and quotes until that block alone.
        #[tokio::test]
        async fn a_carried_block_refusing_every_size_keeps_the_next_blocks_book() {
            let listing = FakeListing { carry: true, life_s: 11, ..FakeListing::ethereum(1) };

            let books = ethereum_books(vec![listing.clone()], vec![next_map(&[(1, true)])]).await;

            let state = state_for(&books, &listing);
            for (direction, bid) in
                [(SwapDirection::BaseToQuote, true), (SwapDirection::QuoteToBase, false)]
            {
                let points = samples(state, direction);
                assert!(!points.is_empty());
                assert_eq!(points, door_quotes(&listing, &points, bid, ETHEREUM_NEXT_TIME));
                assert_eq!(
                    state
                        .side_until(direction)
                        .unwrap()
                        .unix_ms,
                    ETHEREUM_NEXT_TIME * 1000 + 1 - 1000
                );
            }
        }

        /// A listing in no credit mode whose pair no figure was measured for (here test tokens) is
        /// left out on Ethereum unless its gas is named, and is then charged that: no other
        /// pair's figure, which may be over what its own tokens' transfers cost.
        #[tokio::test]
        async fn a_listing_of_a_pair_not_measured_is_served_only_with_its_gas_named() {
            let listings = vec![FakeListing::ethereum(1), FakeListing::ethereum(2)];
            let map = next_map(&[(1, false), (2, false)]);

            let books = books_naming(listings.clone(), vec![map], &[(2, named_gas())]).await;

            assert_eq!(books.len(), 1);
            let state = state_for(&books, &listings[1]);
            let (smallest, _) = state
                .side(SwapDirection::BaseToQuote)
                .first()
                .unwrap();
            let gas = state
                .get_amount_out(
                    BigUint::from_bytes_be(&smallest.to_be_bytes::<32>()),
                    &weth(),
                    &usdc(),
                )
                .unwrap()
                .gas;
            assert_eq!(gas, BigUint::from(95_825u64));
        }

        /// A recorded poll as `testdata/` holds it: one field a line, and one entry a line of
        /// its calls, maps, listings and sides.
        fn recording_text(poll: &RecordedEthereumPoll) -> String {
            let poll = serde_json::to_value(poll).unwrap();
            let fields: Vec<String> = poll
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| {
                    let entries = value.as_array().filter(|list| {
                        list.iter()
                            .any(|entry| entry.is_object() || entry.is_array())
                    });
                    let text = match entries {
                        Some(list) => {
                            let lines: Vec<String> = list
                                .iter()
                                .map(|entry| format!("  {entry}"))
                                .collect();
                            format!("[\n{}\n]", lines.join(",\n"))
                        }
                        None => value.to_string(),
                    };
                    format!("{}:{text}", serde_json::Value::from(name.as_str()))
                })
                .collect();
            format!("{{\n{}\n}}\n", fields.join(",\n"))
        }

        /// Polls an Ethereum node and its state URLs, such as a fork with the listing doors and a
        /// stub of the next block's state map, and holds the books to the doors in that block:
        /// - the map is for the block after the node's latest, and no call of the poll names
        ///   BaseTime's address, which holds no code there;
        /// - a listing is served exactly where the map writes its quote contract, both its tokens
        ///   are configured and the poll leaves it out for no reason of its own: it has no credit
        ///   mode or `EFFICIENT_PROP_ETH_CREDIT` names it, every source it reads a print from is
        ///   approved, and it is not tied ([`left_out_of_a_poll`]); a listing whose quote contract
        ///   the map does not write quotes nothing in that block (its door's `quote` reverts under
        ///   the map, both ways);
        /// - each served side quotes at each sample exactly the least of the door's `quote` under
        ///   the map at each block of its life (the next block, and the one after where its listing
        ///   and its quote carry), and between samples never more than the door at any of them
        ///   (eight sizes inside each stretch and one wei either side of each sample);
        /// - each side quotes until its life's last block's time less the landing allowance.
        ///
        /// Set `EFFICIENT_PROP_ETH_RPC` (a plain-HTTP node), `EFFICIENT_PROP_ETH_STATE_URLS`
        /// (comma-separated), `EFFICIENT_PROP_ETH_FRONT`, `EFFICIENT_PROP_ETH_FACTORY` and
        /// `EFFICIENT_PROP_ETH_TOKENS` (comma-separated `address:decimals`).
        /// `EFFICIENT_PROP_ETH_IMPLEMENTATIONS` (comma-separated) sets the allow-list, otherwise
        /// the implementation the front runs now, `EFFICIENT_PROP_ETH_CELL_CODEHASH` the desk
        /// contracts' creation code hash (required), `EFFICIENT_PROP_ETH_PRICE_SOURCES`
        /// (comma-separated ids) the price sources approved, none otherwise,
        /// `EFFICIENT_PROP_ETH_CREDIT` (comma-separated `listing=custody sell:custody buy:route
        /// sell:route buy`) the credit listings served with the gas each is charged, none
        /// otherwise,
        /// `EFFICIENT_PROP_ETH_TIMEOUT_MS` the request timeout in place of the default,
        /// `EFFICIENT_PROP_ETH_OUT` a file the books are written to (each side's samples with the
        /// gas each is charged, and its life), so that a fill sent in the next block can be held
        /// to them, and
        /// `EFFICIENT_PROP_ETH_RECORD` a file the poll is recorded in (every call it made of the
        /// node with its answer, each state URL's map, which must then be plain HTTP, what the
        /// poll made of each listing, and every sample of every side served with its gas), which
        /// `a_recorded_ethereum_poll_replays_call_for_call`
        /// replays from `testdata/`. The wall clock is taken a second after the latest block's
        /// time, as a poll early in the slot sees it.
        #[tokio::test]
        #[ignore = "reads an Ethereum node and its state map; set EFFICIENT_PROP_ETH_RPC, \
                    EFFICIENT_PROP_ETH_STATE_URLS, EFFICIENT_PROP_ETH_FRONT, \
                    EFFICIENT_PROP_ETH_FACTORY, EFFICIENT_PROP_ETH_CELL_CODEHASH and \
                    EFFICIENT_PROP_ETH_TOKENS"]
        async fn a_fork_poll_quotes_each_door_as_it_fills_in_the_next_block() {
            use alloy::{
                eips::BlockId,
                primitives::{aliases::U24, Bytes as AlloyBytes},
                providers::{Provider, RootProvider},
                rpc::types::TransactionRequest,
                sol_types::SolCall,
            };
            use tycho_common::models::token::Token;

            use super::super::super::{
                overrides::{NextBlockReader, Wanted},
                reader::{
                    doorOfCall, doorViewCall, listing_carries, price_contract, quoteCall,
                    record_word_slot, IMPLEMENTATION_SLOT, PRICE_WORD, SETTINGS_WORD, SOURCES_WORD,
                },
                slots::{HeadWords, SLOT_RULE},
            };
            use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint};

            /// BaseTime's address, unpadded and lower-case, as calldata and parameters carry it.
            const BASE_TIME_HEX: &str = "4200000000000000000000000000000000000030";

            let env = |name: &str| std::env::var(name).ok();
            let (
                Some(rpc),
                Some(state_urls),
                Some(front),
                Some(factory),
                Some(cell_codehash),
                Some(tokens),
            ) = (
                env("EFFICIENT_PROP_ETH_RPC"),
                env("EFFICIENT_PROP_ETH_STATE_URLS"),
                env("EFFICIENT_PROP_ETH_FRONT"),
                env("EFFICIENT_PROP_ETH_FACTORY"),
                env("EFFICIENT_PROP_ETH_CELL_CODEHASH"),
                env("EFFICIENT_PROP_ETH_TOKENS"),
            )
            else {
                println!(
                    "skipped: EFFICIENT_PROP_ETH_RPC, EFFICIENT_PROP_ETH_STATE_URLS, \
                     EFFICIENT_PROP_ETH_FRONT, EFFICIENT_PROP_ETH_FACTORY, \
                     EFFICIENT_PROP_ETH_CELL_CODEHASH or EFFICIENT_PROP_ETH_TOKENS unset"
                );
                return;
            };
            let cell_codehash: B256 = cell_codehash.parse().unwrap();
            let bytes_of = |hash: B256| Bytes::from(hash.to_vec());
            let list = |value: &str| -> Vec<String> {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(ToString::to_string)
                    .collect()
            };
            let state_urls = list(&state_urls);
            let tokens: HashMap<Bytes, Token> = list(&tokens)
                .iter()
                .map(|entry| {
                    let (address, decimals) = entry.split_once(':').unwrap();
                    let address = bytes(address.parse::<Address>().unwrap());
                    let decimals = decimals.parse().unwrap();
                    let token = Token::new(
                        &address,
                        "TKN",
                        decimals,
                        0,
                        &[Some(10_000)],
                        Chain::Ethereum,
                        100,
                    );
                    (address, token)
                })
                .collect();
            let config = BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd: 0.0,
            };
            let provider: RootProvider = RootProvider::new_http(rpc.parse().unwrap());
            let front_address: Address = front.parse().unwrap();
            let factory_address: Address = factory.parse().unwrap();
            let call = |target: Address, data: Vec<u8>, block: BlockId| {
                let provider = provider.clone();
                async move {
                    provider
                        .call(
                            TransactionRequest::default()
                                .to(target)
                                .input(AlloyBytes::from(data).into()),
                        )
                        .block(block)
                        .await
                }
            };
            let latest: serde_json::Value = provider
                .raw_request("eth_getBlockByNumber".into(), ("latest", false))
                .await
                .unwrap();
            let hex_u64 =
                |value: &serde_json::Value| u64::from_str_radix(&value.as_str().unwrap()[2..], 16);
            let head = hex_u64(&latest["number"]).unwrap();
            let head_time = hex_u64(&latest["timestamp"]).unwrap();
            let at_head = BlockId::number(head);

            let implementations = match env("EFFICIENT_PROP_ETH_IMPLEMENTATIONS") {
                Some(value) => list(&value)
                    .iter()
                    .map(|implementation| implementation.parse().unwrap())
                    .collect(),
                None => {
                    let word = provider
                        .get_storage_at(front_address, U256::from_be_bytes(IMPLEMENTATION_SLOT.0))
                        .block_id(at_head)
                        .await
                        .unwrap();
                    vec![bytes(Address::from_word(word.into()))]
                }
            };
            let recorded_implementations: Vec<Address> = implementations
                .iter()
                .map(|implementation: &Bytes| Address::from_slice(implementation))
                .collect();
            let price_sources: Vec<u32> = env("EFFICIENT_PROP_ETH_PRICE_SOURCES")
                .map(|value| {
                    list(&value)
                        .iter()
                        .map(|id| id.parse().unwrap())
                        .collect()
                })
                .unwrap_or_default();
            let credit: Vec<(u32, CreditGas)> = env("EFFICIENT_PROP_ETH_CREDIT")
                .map(|value| {
                    list(&value)
                        .iter()
                        .map(|entry| {
                            let (listing, gas) = entry.split_once('=').unwrap();
                            (listing.trim().parse().unwrap(), gas.parse().unwrap())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let deployment = EfficientPropDeployment {
                front: bytes(front_address),
                factory: bytes(factory_address),
                cell_codehash: Some(bytes_of(cell_codehash)),
                implementations,
                quote_layout: Some(QUOTE_LAYOUT_BUILDER.to_string()),
            };

            // The poll, through a proxy recording every call it sends the node.
            let (proxy, log) = spawn_recording_proxy(rpc.clone()).await;
            let mut builder =
                EfficientPropFeedBuilder::new(config.clone(), proxy.url(), deployment)
                    .approved_price_sources(price_sources.clone())
                    .with_approved_desks(
                        list(&env("EFFICIENT_PROP_ETH_APPROVED_DESKS").unwrap_or_default())
                            .iter()
                            .map(|desk| bytes(desk.parse().unwrap())),
                    )
                    .state_urls(state_urls.clone());
            for &(listing, gas) in &credit {
                builder = builder.credit_listing(listing, gas);
            }
            // A slower node or state URL, such as a fork fetching from its upstream.
            if let Some(timeout_ms) = env("EFFICIENT_PROP_ETH_TIMEOUT_MS") {
                builder = builder.feed_config(HttpFeedConfig {
                    request_timeout: Duration::from_millis(timeout_ms.parse().unwrap()),
                    ..EfficientPropFeedBuilder::default_feed_config()
                });
            }
            let mut source = builder.build().unwrap().source;
            let now_ms = (head_time + 1) * 1000;
            source.clock = Arc::new(move || now_ms);
            let snapshot = source.fetch().await.unwrap();
            let landing_ms = source.assembler.landing_ms;

            // The books' answers first, while every side still quotes on the process's clock.
            struct SideRead {
                direction: SwapDirection,
                token_in: Address,
                token_out: Address,
                points: Vec<(U256, U256)>,
                until_ms: Option<u64>,
                gas: Option<u64>,
                /// The gas the book charges each sample, in the samples' order.
                point_gas: Vec<Option<u64>>,
                /// Each size checked between and beside the samples, with what the book quotes.
                between: Vec<(U256, Option<U256>)>,
            }
            let mut served = std::collections::BTreeMap::new();
            for book in snapshot.books.values() {
                let state = book
                    .state
                    .as_any()
                    .downcast_ref::<EfficientPropState>()
                    .unwrap();
                let door = Address::from_slice(&state.door);
                let mut sides = Vec::new();
                for direction in [SwapDirection::BaseToQuote, SwapDirection::QuoteToBase] {
                    let (token_in, token_out) = match direction {
                        SwapDirection::BaseToQuote => (&state.base_token, &state.quote_token),
                        SwapDirection::QuoteToBase => (&state.quote_token, &state.base_token),
                    };
                    let points = state.side(direction).points().to_vec();
                    let book_out = |amount: U256| {
                        state
                            .get_amount_out(u256_to_biguint(amount), token_in, token_out)
                            .ok()
                    };
                    let point_gas: Vec<Option<u64>> = points
                        .iter()
                        .map(|&(amount, _)| {
                            book_out(amount).map(|result| u64::try_from(&result.gas).unwrap())
                        })
                        .collect();
                    let gas = point_gas.first().copied().flatten();
                    let mut sizes = Vec::new();
                    for &(amount, _) in &points {
                        sizes.extend([
                            amount.saturating_sub(U256::from(1u8)),
                            amount + U256::from(1u8),
                        ]);
                    }
                    for pair in points.windows(2) {
                        let (low, high) = (pair[0].0, pair[1].0);
                        for k in 1..=8u8 {
                            sizes.push(low + (high - low) * U256::from(k) / U256::from(9u8));
                        }
                    }
                    let between = sizes
                        .into_iter()
                        .filter(|amount| !amount.is_zero())
                        .map(|amount| {
                            (amount, book_out(amount).map(|result| biguint_to_u256(&result.amount)))
                        })
                        .collect();
                    sides.push(SideRead {
                        direction,
                        token_in: Address::from_slice(&token_in.address),
                        token_out: Address::from_slice(&token_out.address),
                        points,
                        until_ms: state
                            .side_until(direction)
                            .map(|until| until.unix_ms),
                        gas,
                        point_gas,
                        between,
                    });
                }
                served.insert(door, (state.reads, state.pair, sides));
            }

            // BaseTime is never asked: it has no code on Ethereum.
            let asked = serde_json::to_string(
                &log.lock()
                    .unwrap()
                    .iter()
                    .map(|exchange| (exchange.method.clone(), exchange.params.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
            .to_lowercase();
            assert!(!asked.contains(BASE_TIME_HEX), "the poll asked BaseTime's address");
            let base_time_code: AlloyBytes = provider
                .raw_request(
                    "eth_getCode".into(),
                    (format!("0x{BASE_TIME_HEX}"), format!("{head:#x}")),
                )
                .await
                .unwrap();
            assert!(base_time_code.is_empty(), "BaseTime's address holds code on this chain");

            // The map, read again: the stub answers the same for the same chain.
            let urls = state_urls
                .iter()
                .map(|url| url.parse().unwrap())
                .collect();
            let next = NextBlockReader::new(urls, Duration::from_secs(60))
                .unwrap()
                .read()
                .await
                .unwrap()
                .for_block_after(head, head_time)
                .expect("no map is for the block after the latest");
            let door_quote = |door: Address,
                              state: &serde_json::Value,
                              token_in: Address,
                              token_out: Address,
                              amount: U256,
                              after: u64| {
                let provider = provider.clone();
                let state = state.clone();
                let (number, time) = (next.number + after, next.time + after * SLOT_SECS);
                async move {
                    let request = TransactionRequest::default()
                        .to(door)
                        .input(
                            AlloyBytes::from(
                                quoteCall {
                                    tokenIn: token_in,
                                    tokenOut: token_out,
                                    amountIn: amount,
                                }
                                .abi_encode(),
                            )
                            .into(),
                        );
                    let block = serde_json::json!({
                        "number": format!("{number:#x}"),
                        "time": format!("{time:#x}"),
                    });
                    let answer: AlloyBytes = provider
                        .raw_request("eth_call".into(), (request, at_head, state, block))
                        .await
                        .ok()?;
                    quoteCall::abi_decode_returns(&answer).ok()
                }
            };

            // Every listing, walked from 1 to the highest the poll found reserved.
            let next_deadline = next.time * 1000 + 1 - landing_ms;
            let mut written = Vec::new();
            let mut recorded = Vec::new();
            let mut recorded_sides = Vec::new();
            let mut takeable = false;
            for id in 1..=source
                .highest_listing
                .load(Ordering::Relaxed)
            {
                let listing = U24::from(id);
                let view = call(front_address, doorViewCall { listing }.abi_encode(), at_head)
                    .await
                    .unwrap();
                let view = doorViewCall::abi_decode_returns(&view).unwrap();
                if view.pool == Address::ZERO {
                    continue;
                }
                let door = call(factory_address, doorOfCall { listing }.abi_encode(), at_head)
                    .await
                    .unwrap();
                let door = doorOfCall::abi_decode_returns(&door).unwrap();
                let place = SLOT_RULE.place_call(front_address, view.desk);
                let place = call(place.target, place.callData.to_vec(), at_head)
                    .await
                    .unwrap();
                let cell = SLOT_RULE.account(front_address, cell_codehash, view.desk);
                let head_word = provider
                    .get_storage_at(cell, SLOT_RULE.head_slot())
                    .block_id(at_head)
                    .await
                    .unwrap();
                let (quote_slot, firm_slot) = SLOT_RULE.pair_slots(view.wordIndex);
                let pair_word = |slot: U256| {
                    let provider = provider.clone();
                    async move {
                        B256::from(
                            provider
                                .get_storage_at(cell, slot)
                                .block_id(at_head)
                                .await
                                .unwrap(),
                        )
                    }
                };
                let head = HeadWords {
                    desk: head_word,
                    quote: pair_word(quote_slot).await,
                    firm: pair_word(firm_slot).await,
                };
                let quote = SLOT_RULE
                    .slots(&place, cell, view.wordIndex, head)
                    .unwrap();
                let known = |token: &Address| {
                    config
                        .tokens
                        .contains_key(&bytes(*token))
                };
                // What the poll takes for it: its quote's slots by the slot rule, and where its
                // fill reads a print, the price contract of its live source and those it is bound
                // to.
                let word = |word: u8| {
                    let provider = provider.clone();
                    async move {
                        provider
                            .get_storage_at(front_address, record_word_slot(id, word))
                            .block_id(at_head)
                            .await
                            .unwrap()
                    }
                };
                let settings = word(SETTINGS_WORD).await;
                let prints = matches!(
                    (settings >> 228usize) & U256::from(3u8),
                    fence if fence == U256::ZERO || fence == U256::from(3u8)
                );
                let contract = |id: U256| {
                    let id: u32 = (id & U256::from(u32::MAX)).to();
                    (id != 0).then(|| price_contract(front_address, id))
                };
                let mut needs = Vec::new();
                if prints {
                    needs.extend(contract(word(PRICE_WORD).await));
                }
                let taken =
                    next.take(&[Wanted { quote, carries: listing_carries(settings), needs }]);
                let written_cell = taken.listings[0].is_ok();
                // What the poll leaves out whatever the map writes: a listing with a credit mode,
                // on a source not approved, or tied.
                let named = credit
                    .iter()
                    .any(|&(listing, _)| listing == id);
                let left_out = left_out_of_a_poll(
                    if named { 0 } else { view.creditMode },
                    settings,
                    word(SOURCES_WORD).await,
                    &price_sources,
                );
                let expected = written_cell &&
                    known(&view.token0) &&
                    known(&view.token1) &&
                    left_out.is_none();
                takeable |= expected;
                recorded.push(RecordedListing {
                    listing: id,
                    door,
                    desk: view.desk,
                    credit_mode: view.creditMode,
                    quote_contract: cell,
                    written: written_cell,
                    left_out: left_out.map(str::to_string),
                    served: served.contains_key(&door),
                });
                let state = &taken.state;
                println!(
                    "listing {id}: door {door}, quote contract {cell}, {} in the map, {}",
                    if written_cell { "taken" } else { "not taken" },
                    if served.contains_key(&door) { "served" } else { "not served" }
                );
                let Some((reads, pair, sides)) = served.get(&door) else {
                    assert!(
                        !expected,
                        "listing {id}'s accounts are in one answer, its tokens configured, the \
                         poll leaves it out for no reason of its own, and it is not served"
                    );
                    if !written_cell {
                        // Nothing to fill in the next block: the door quotes nothing there.
                        let unit = |token: &Address| {
                            let decimals = config
                                .tokens
                                .get(&bytes(*token))
                                .map_or(18, |token| token.decimals);
                            U256::from(100u8) * U256::from(10u8).pow(U256::from(decimals))
                        };
                        for (token_in, token_out) in
                            [(view.token0, view.token1), (view.token1, view.token0)]
                        {
                            assert_eq!(
                                door_quote(door, state, token_in, token_out, unit(&token_in), 0)
                                    .await,
                                None,
                                "listing {id} quotes in the next block without its maker's update"
                            );
                        }
                    }
                    continue;
                };
                assert!(
                    written_cell,
                    "listing {id} is served without its accounts in one answer of the map"
                );
                assert!(
                    left_out.is_none(),
                    "listing {id} is served though {}",
                    left_out.unwrap_or_default()
                );
                let mut out_sides = Vec::new();
                for side in sides {
                    let until_ms = side
                        .until_ms
                        .expect("a side on Ethereum quotes until a set time");
                    let blocks = if until_ms == next_deadline {
                        vec![0]
                    } else {
                        assert_eq!(
                            until_ms,
                            next_deadline + SLOT_SECS * 1000,
                            "listing {id}'s side lives past the next block and the one after"
                        );
                        vec![0, 1]
                    };
                    let door_least = |amount: U256| {
                        let blocks = blocks.clone();
                        let (token_in, token_out) = (side.token_in, side.token_out);
                        async move {
                            let mut least: Option<U256> = None;
                            for after in blocks {
                                let quoted =
                                    door_quote(door, state, token_in, token_out, amount, after)
                                        .await?;
                                least = Some(least.map_or(quoted, |least| least.min(quoted)));
                            }
                            least
                        }
                    };
                    for &(amount, quoted) in &side.points {
                        assert_eq!(
                            door_least(amount).await,
                            Some(quoted),
                            "listing {id}, {:?}, the sample at {amount}",
                            side.direction
                        );
                    }
                    for &(amount, quoted) in &side.between {
                        if let Some(quoted) = quoted {
                            let door = door_least(amount).await;
                            assert!(
                                door.is_some_and(|door| quoted <= door),
                                "listing {id}, {:?}: at {amount} the book quotes {quoted}, the \
                                 door {door:?}",
                                side.direction
                            );
                        }
                    }
                    out_sides.push(serde_json::json!({
                        "direction": format!("{:?}", side.direction),
                        "tokenIn": side.token_in.to_string(),
                        "tokenOut": side.token_out.to_string(),
                        "untilMs": until_ms,
                        "blocks": blocks.iter().map(|after| next.number + after).collect::<Vec<_>>(),
                        "gas": side.gas,
                        "points": side
                            .points
                            .iter()
                            .zip(&side.point_gas)
                            .map(|((amount, out), gas)| {
                                serde_json::json!([amount.to_string(), out.to_string(), gas])
                            })
                            .collect::<Vec<_>>(),
                    }));
                    recorded_sides.push(RecordedSide {
                        door,
                        sell_base: side.direction == SwapDirection::BaseToQuote,
                        points: side
                            .points
                            .iter()
                            .zip(&side.point_gas)
                            .map(|(&(amount, out), gas)| (amount, out, gas.unwrap_or_default()))
                            .collect(),
                    });
                }
                written.push(serde_json::json!({
                    "listing": id,
                    "door": door.to_string(),
                    "quoteContract": cell.to_string(),
                    "reads": format!("{reads:?}"),
                    "pair": format!("{pair:?}"),
                    "sides": out_sides,
                }));
            }
            assert!(!written.is_empty() || !takeable, "no listing served");
            println!(
                "head {head} at {head_time}: map for block {} at {}, {} books",
                next.number,
                next.time,
                written.len()
            );
            if let Some(path) = env("EFFICIENT_PROP_ETH_OUT") {
                let out = serde_json::json!({
                    "head": head,
                    "headTime": head_time,
                    "nextBlock": next.number,
                    "nextTime": next.time,
                    "nowMs": now_ms,
                    "landingMs": landing_ms,
                    "calls": log.lock().unwrap().len(),
                    "books": written,
                });
                std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
            }
            if let Some(path) = env("EFFICIENT_PROP_ETH_RECORD") {
                let mut tokens: Vec<(Address, u32)> = config
                    .tokens
                    .values()
                    .map(|token| (Address::from_slice(&token.address), token.decimals))
                    .collect();
                tokens.sort();
                let poll = RecordedEthereumPoll {
                    front: front_address,
                    factory: factory_address,
                    cell_codehash,
                    implementations: recorded_implementations,
                    price_sources,
                    tokens,
                    now_ms,
                    head,
                    head_time,
                    rpc: log.lock().unwrap().clone(),
                    maps: state_urls
                        .iter()
                        .map(|url| read_state_map(url))
                        .collect(),
                    listings: recorded,
                    books: snapshot.books.len(),
                    credit: credit.clone(),
                    sides: recorded_sides,
                };
                std::fs::write(&path, recording_text(&poll)).unwrap();
                println!("recorded the poll in {path}");
            }
        }

        /// What a poll that names no credit listing leaves out whatever its maker's update writes:
        /// the rule the fork poll holds its served listings to.
        #[rstest]
        #[case::a_word_listing(0, FENCE_WORD, 0, (0, 0), None)]
        #[case::no_fence(0, FENCE_NONE, 0, (0, 0), None)]
        #[case::a_print_on_an_approved_source(0, FENCE_PRINT, 0, (APPROVED_SOURCE, 0), None)]
        #[case::a_print_with_an_approved_backup(0, FENCE_PRINT, 0, (APPROVED_SOURCE, 2), None)]
        #[case::a_session_on_an_approved_source(0, FENCE_SESSION, 0, (APPROVED_SOURCE, 0), None)]
        #[case::credit_mode_1(1, FENCE_WORD, 0, (0, 0), Some("credit mode"))]
        #[case::credit_mode_2_on_an_approved_source(
            2,
            FENCE_PRINT,
            0,
            (APPROVED_SOURCE, 0),
            Some("credit mode")
        )]
        #[case::credit_mode_3(3, FENCE_NONE, 0, (0, 0), Some("credit mode"))]
        #[case::a_print_on_another_source(0, FENCE_PRINT, 0, (9, 0), Some("not approved"))]
        #[case::a_print_with_another_backup(0, FENCE_PRINT, 0, (APPROVED_SOURCE, 9), Some("not approved"))]
        #[case::a_session_on_another_source(0, FENCE_SESSION, 0, (9, 0), Some("not approved"))]
        #[case::a_word_listing_naming_another_source(0, FENCE_WORD, 0, (9, 0), None)]
        #[case::a_tied_print(0, FENCE_PRINT, 2, (APPROVED_SOURCE, 0), Some("tied"))]
        fn a_poll_naming_no_credit_listing_leaves_out(
            #[case] credit_mode: u8,
            #[case] fence: u64,
            #[case] scopes_high: u64,
            #[case] sources: (u32, u32),
            #[case] why: Option<&str>,
        ) {
            // The settings word's bits 228-229 hold the fence kind and 230-231 the rest of the
            // nibble a tied listing sets to 8 with a fence kind of 0.
            let settings = (U256::from(fence) | (U256::from(scopes_high) << 2usize)) << 228usize;
            let sources = U256::from(sources.0) | (U256::from(sources.1) << 32usize);

            let left_out =
                left_out_of_a_poll(credit_mode, settings, sources, &[APPROVED_SOURCE, 2]);

            match why {
                Some(why) => assert!(
                    left_out.is_some_and(|reason| reason.contains(why)),
                    "left out for {left_out:?}, not for {why}"
                ),
                None => assert_eq!(left_out, None),
            }
        }

        /// A second implementation the reuse tests' front may run.
        const OTHER_IMPLEMENTATION: Address = Address::repeat_byte(0x1b);

        /// One source polling a node and a state URL that a test changes between its polls.
        struct Polled {
            node: Arc<Mutex<FakeNode>>,
            map: Arc<Mutex<serde_json::Value>>,
            source: EfficientPropBookSource,
            now_ms: Arc<AtomicU64>,
            urls: (String, String),
            _servers: (MockHttpServer, MockHttpServer),
        }

        impl Polled {
            async fn new(node: FakeNode, map: serde_json::Value) -> Self {
                let node = Arc::new(Mutex::new(node));
                let map = Arc::new(Mutex::new(map));
                let servers = (
                    FakeNode::spawn_shared(Arc::clone(&node)).await,
                    spawn_shared_state_server(Arc::clone(&map)).await,
                );
                let urls = (servers.0.url(), servers.1.url());
                let now_ms = Arc::new(AtomicU64::new(NOW_MS));
                let mut polled = Self {
                    node,
                    map,
                    source: ethereum_source(&urls.0, vec![urls.1.clone()], NOW_MS),
                    now_ms,
                    urls,
                    _servers: servers,
                };
                polled.source = polled.fresh();
                polled
            }

            /// A source that has sampled nothing yet, polling the same node and state URL on the
            /// same clock: its front may run either implementation.
            fn fresh(&self) -> EfficientPropBookSource {
                let deployment = EfficientPropDeployment {
                    implementations: vec![bytes(IMPLEMENTATION), bytes(OTHER_IMPLEMENTATION)],
                    ..ethereum_deployment()
                };
                let mut source =
                    ethereum_source_of(&self.urls.0, vec![self.urls.1.clone()], NOW_MS, deployment);
                let now_ms = Arc::clone(&self.now_ms);
                source.clock = Arc::new(move || now_ms.load(Ordering::SeqCst));
                source
            }

            /// How many quotes the node has answered at a door.
            fn door_quotes(&self) -> usize {
                let node = self.node.lock().unwrap();
                node.door_quotes.load(Ordering::SeqCst)
            }

            /// How many of those it was asked at a block named by its number, not its hash.
            fn door_quotes_by_number(&self) -> usize {
                let node = self.node.lock().unwrap();
                node.door_quotes_by_number
                    .load(Ordering::SeqCst)
            }

            /// How many of the source's polls asked no door.
            fn reuses(&self) -> usize {
                self.source
                    .reader
                    .reuses
                    .load(Ordering::SeqCst)
            }
        }

        /// One book as a poll publishes it: its component, its state, and when each side stops
        /// quoting on the wall clock, the bid then the ask. The state is held without those two
        /// deadlines, whose monotonic moments are each poll's own.
        type Published = (ProtocolComponent, EfficientPropState, [Option<u64>; 2]);

        /// What one poll of `source` publishes, by component id.
        async fn published(source: &EfficientPropBookSource) -> BTreeMap<String, Published> {
            let snapshot = source.fetch().await.unwrap();
            snapshot
                .books
                .iter()
                .map(|(id, book)| {
                    let mut state = book
                        .state
                        .as_any()
                        .downcast_ref::<EfficientPropState>()
                        .unwrap()
                        .clone();
                    let until = [state.bid_until.take(), state.ask_until.take()]
                        .map(|until| until.map(|until| until.unix_ms));
                    (id.clone(), (book.component.clone(), state, until))
                })
                .collect()
        }

        /// `map` with `word` written at `slot` of `account` by its entry `key`.
        fn written(
            mut map: serde_json::Value,
            key: &str,
            account: Address,
            slot: U256,
            word: B256,
        ) -> serde_json::Value {
            map[key]["stateOverride"][account.to_string()]["stateDiff"]
                [B256::from(slot).to_string()] = word.to_string().into();
            map
        }

        /// The firm word's slot of pair 0 in a desk contract.
        fn firm_slot() -> U256 {
            U256::from(0x200u64)
        }

        /// Four listings of four desks: a plain one whose word counts its firm word, one reading
        /// a print, one whose maker has sent no update, and a credit listing. The map writes the
        /// first's quote and firm words, the second's quote word and its price contract, and the
        /// fourth's quote word.
        fn reuse_fixture() -> (Vec<FakeListing>, serde_json::Value) {
            let listings = vec![
                FakeListing::ethereum(1),
                FakeListing { fence: FENCE_PRINT, ..FakeListing::ethereum(2) },
                FakeListing::ethereum(3),
                FakeListing::ethereum_credit(4),
            ];
            (listings, reuse_map(ETHEREUM_BLOCK + 1, ETHEREUM_SLOT))
        }

        /// The state answer of [`reuse_fixture`] for block `next` at slot `slot`.
        fn reuse_map(next: u64, slot: u64) -> serde_json::Value {
            let map = state_map_of(
                next,
                slot,
                &[
                    ("desk-1".to_string(), 1, 0, quote_word(next, 0, false, true)),
                    ("desk-2".to_string(), 2, 0, quote_word(next, 0, false, false)),
                    ("desk-4".to_string(), 4, 0, quote_word(next, 0, false, false)),
                ],
            );
            let map = written(map, "desk-1", desk_cell(1), firm_slot(), B256::repeat_byte(0xf1));
            with_account(map, "desk-2", price_contract(APPROVED_SOURCE))
        }

        /// A poll that finds nothing changed asks no door: the head, the next block, the state
        /// answer and every listing's words are as the last sampled poll found them, so it stops
        /// after its second request to the node and publishes that poll's books, the credit
        /// listing's among them. Before, every poll asked every door again. The poll that did ask
        /// them named their block by the head's hash, so the node answered each from that block.
        #[tokio::test]
        async fn a_poll_that_finds_nothing_changed_asks_no_door() {
            let (listings, map) = reuse_fixture();
            let polled = Polled::new(FakeNode::ethereum(listings), map).await;

            let sampled = published(&polled.source).await;
            let (asked, requests) = (polled.door_quotes(), polled._servers.0.request_count());
            let kept = published(&polled.source).await;

            assert_eq!(sampled.len(), 3);
            assert!(asked > 0);
            assert_eq!(polled.door_quotes(), asked, "an unchanged poll asked a door");
            assert_eq!(
                polled.door_quotes_by_number(),
                0,
                "a door asked at a block named by number"
            );
            assert_eq!(polled._servers.0.request_count(), requests + 2);
            assert_eq!(polled.reuses(), 1);
            assert_eq!(kept, sampled);
        }

        /// A node that does not name its blocks by hash is asked every door on every poll: no
        /// read of it is kept.
        #[tokio::test]
        async fn a_node_that_names_no_block_is_asked_every_door_on_every_poll() {
            let (listings, map) = reuse_fixture();
            let node = FakeNode { head_hash: None, ..FakeNode::ethereum(listings) };
            let polled = Polled::new(node, map).await;

            let sampled = published(&polled.source).await;
            let asked = polled.door_quotes();
            let again = published(&polled.source).await;

            assert_eq!(polled.door_quotes(), 2 * asked);
            assert_eq!(polled.door_quotes_by_number(), 2 * asked);
            assert_eq!(polled.reuses(), 0);
            assert_eq!(again, sampled);
        }

        /// The node's head `blocks` further on, with its makers' updates for the block after it
        /// and the wall clock.
        fn head_moves_on(polled: &Polled, blocks: u64) {
            let mut node = polled.node.lock().unwrap();
            node.block += blocks;
            node.timestamp += blocks * SLOT_SECS;
            for listing in &mut node.listings {
                listing.maker_ms += blocks * SLOT_SECS * 1000;
            }
            polled
                .now_ms
                .fetch_add(blocks * SLOT_SECS * 1000, Ordering::SeqCst);
            *polled.map.lock().unwrap() =
                reuse_map(node.block + 1, ETHEREUM_SLOT + node.block - ETHEREUM_BLOCK);
        }

        /// What `polls` count through the `metrics` facade: each counter, by its name and label.
        fn counted(
            polls: impl std::future::Future<Output = ()>,
        ) -> impl Fn(&str, Option<(&str, &str)>) -> u64 {
            use metrics_util::{
                debugging::{DebugValue, DebuggingRecorder},
                MetricKind,
            };

            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // `with_local_recorder` takes a sync closure, so the polls run on a runtime inside it.
            metrics::with_local_recorder(&recorder, || runtime.block_on(polls));
            let recorded: HashMap<_, _> = snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .filter(|(key, ..)| key.kind() == MetricKind::Counter)
                .map(|(key, _, _, value)| {
                    let labels: Vec<_> = key
                        .key()
                        .labels()
                        .map(|label| (label.key().to_string(), label.value().to_string()))
                        .collect();
                    ((key.key().name().to_string(), labels), value)
                })
                .collect();
            move |name, label| {
                let labels = label
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .into_iter()
                    .collect();
                match recorded.get(&(name.to_string(), labels)) {
                    Some(DebugValue::Counter(count)) => *count,
                    other => panic!("{name} {label:?} is not a counter recorded: {other:?}"),
                }
            }
        }

        /// A node that refuses a block named by its hash is asked every door on every poll, by
        /// number, as every poll was before any read was kept, and publishes what a node taking
        /// the hash does. The reader asks by hash first on three polls, one request more each,
        /// finds the form refused and the number form answered every time, and sets the hash
        /// form aside for 32 heads. Then it tries once, is refused, and sets it aside as long
        /// again.
        #[tokio::test]
        async fn a_node_refusing_a_block_named_by_its_hash_is_asked_every_door_by_number() {
            let (listings, map) = reuse_fixture();
            let taking = Polled::new(FakeNode::ethereum(listings.clone()), map.clone()).await;
            let node = FakeNode { hash_pins: HashPins::Refused, ..FakeNode::ethereum(listings) };
            let polled = Polled::new(node, map).await;

            let sampled = published(&polled.source).await;
            let asked = polled.door_quotes();
            let mut requests = vec![polled._servers.0.request_count()];
            for poll in 2..=5 {
                assert_eq!(published(&polled.source).await, sampled);
                assert_eq!(polled.door_quotes(), poll * asked);
                requests.push(polled._servers.0.request_count());
            }

            assert!(asked > 0);
            assert_eq!(polled.door_quotes_by_number(), 5 * asked);
            assert_eq!(polled.reuses(), 0);
            assert_eq!(sampled, published(&taking.source).await);
            let a_poll: Vec<_> = requests
                .windows(2)
                .map(|pair| pair[1] - pair[0])
                .collect();
            assert_eq!(a_poll, [6, 6, 5, 5]);

            let mut a_poll = Vec::new();
            for blocks in [31, 1, 0, 32] {
                head_moves_on(&polled, blocks);
                let before = polled._servers.0.request_count();
                published(&polled.source).await;
                a_poll.push(polled._servers.0.request_count() - before);
            }
            assert_eq!(a_poll, [5, 6, 5, 6]);
            assert_eq!(polled.door_quotes_by_number(), polled.door_quotes());
            assert_eq!(polled.reuses(), 0);
        }

        /// The hash form comes back. Three polls of a node that refuses a block named by its
        /// hash set the form aside; the node then takes it, and the reader goes on asking by
        /// number until 32 heads have passed, tries the hash once, is answered, keeps that read,
        /// and the poll after it asks no door. Before, the form stayed off for the reader's
        /// life, and no read was kept again. The feed counts the once it set the form aside and
        /// each read it did not keep, by reason.
        #[test]
        fn a_node_taking_the_hash_form_again_is_named_its_blocks_by_hash_again() {
            use crate::pamm::protocols::efficient_prop::telemetry::{
                HASH_FORM_SET_ASIDE, POLLS, READS_NOT_KEPT,
            };

            let counter = counted(async {
                let (listings, map) = reuse_fixture();
                let node =
                    FakeNode { hash_pins: HashPins::Refused, ..FakeNode::ethereum(listings) };
                let polled = Polled::new(node, map).await;
                for _ in 0..3 {
                    published(&polled.source).await;
                }
                polled.node.lock().unwrap().hash_pins = HashPins::Taken;
                let by_number = polled.door_quotes_by_number();
                published(&polled.source).await;
                published(&polled.source).await;
                assert!(polled.door_quotes_by_number() > by_number);
                assert_eq!(polled.reuses(), 0);

                head_moves_on(&polled, 32);
                let by_number = polled.door_quotes_by_number();
                let sampled = published(&polled.source).await;
                assert_eq!(polled.door_quotes_by_number(), by_number, "a door asked by number");
                let asked = polled.door_quotes();
                assert_eq!(published(&polled.source).await, sampled);
                assert_eq!(polled.door_quotes(), asked, "the poll after the kept read asked");
                assert_eq!(polled.reuses(), 1);
            });

            assert_eq!(counter(HASH_FORM_SET_ASIDE, None), 1);
            for (reason, reads) in [
                ("hash_read_failed", 3),
                ("hash_form_set_aside", 2),
                ("no_block_hash", 0),
                ("answered_in_part", 0),
            ] {
                assert_eq!(counter(READS_NOT_KEPT, Some(("reason", reason))), reads, "{reason}");
            }
            assert_eq!(counter(POLLS, Some(("outcome", "sampled"))), 6);
            assert_eq!(counter(POLLS, Some(("outcome", "reused"))), 1);
        }

        /// A backend that knows no block of the head's hash, one a block behind the backend that
        /// named the head, sets nothing aside, however many polls meet it: each is asked again
        /// by number and tries the hash on the next. Once a backend knows the hash, that read is
        /// kept and the poll after it asks no door. Before, three such polls in a row switched
        /// the hash form off for good.
        #[tokio::test]
        async fn a_backend_that_knows_no_block_of_the_heads_hash_sets_nothing_aside() {
            let (listings, map) = reuse_fixture();
            let node = FakeNode { hash_pins: HashPins::Unknown, ..FakeNode::ethereum(listings) };
            let polled = Polled::new(node, map).await;

            for poll in 1..=5 {
                let before = polled._servers.0.request_count();
                published(&polled.source).await;
                assert_eq!(polled._servers.0.request_count() - before, 6, "poll {poll}");
            }
            assert_eq!(polled.door_quotes_by_number(), polled.door_quotes());
            assert_eq!(polled.reuses(), 0);

            polled.node.lock().unwrap().hash_pins = HashPins::Taken;
            let by_number = polled.door_quotes_by_number();
            let sampled = published(&polled.source).await;
            assert_eq!(polled.door_quotes_by_number(), by_number, "a door asked by number");
            assert_eq!(published(&polled.source).await, sampled);
            assert_eq!(polled.reuses(), 1);
        }

        /// A read the node answers by hash starts the count of refusals again: two refused
        /// polls, one answered, and two more refused on the next head set nothing aside, so the
        /// read after them is asked by hash and kept.
        #[tokio::test]
        async fn a_read_answered_by_hash_starts_the_count_of_refusals_again() {
            let (listings, map) = reuse_fixture();
            let node = FakeNode { hash_pins: HashPins::Refused, ..FakeNode::ethereum(listings) };
            let polled = Polled::new(node, map).await;

            for blocks in [0, 1] {
                head_moves_on(&polled, blocks);
                polled.node.lock().unwrap().hash_pins = HashPins::Refused;
                published(&polled.source).await;
                published(&polled.source).await;
                polled.node.lock().unwrap().hash_pins = HashPins::Taken;
                let by_number = polled.door_quotes_by_number();
                published(&polled.source).await;
                assert_eq!(
                    polled.door_quotes_by_number(),
                    by_number,
                    "the hash form was set aside after two refusals in a row"
                );
            }
            let asked = polled.door_quotes();
            published(&polled.source).await;
            assert_eq!(polled.door_quotes(), asked);
            assert_eq!(polled.reuses(), 1);
        }

        /// A failure of the read by hash that ends the feed is not asked again by number: a
        /// front found on an implementation outside the allow-list at the block named by its
        /// hash fails the poll at that request, its third.
        #[tokio::test]
        async fn a_fatal_failure_of_the_read_by_hash_is_not_asked_again_by_number() {
            let (listings, map) = reuse_fixture();
            let node = FakeNode {
                implementation_by_hash: Some(Address::repeat_byte(0x99)),
                ..FakeNode::ethereum(listings)
            };
            let polled = Polled::new(node, map).await;

            let error = polled.source.fetch().await.unwrap_err();

            assert!(matches!(error, FeedError::Fatal(_)), "{error:?}");
            assert_eq!(polled._servers.0.request_count(), 3);
            assert_eq!(polled.door_quotes_by_number(), 0);
        }

        /// The feed counts each read it does not keep by its reason: a node that names no block
        /// by hash, and a node that never answers the block after the next for a listing that
        /// carries, are each asked every door on every poll, which the poll's outcome alone
        /// does not tell from a poll that sampled because something changed.
        #[test]
        fn the_feed_counts_a_read_it_does_not_keep_by_its_reason() {
            use crate::pamm::protocols::efficient_prop::telemetry::{
                HASH_FORM_SET_ASIDE, READS_NOT_KEPT,
            };

            let counter = counted(async {
                let (mut listings, map) = reuse_fixture();
                let node = FakeNode { head_hash: None, ..FakeNode::ethereum(listings.clone()) };
                let polled = Polled::new(node, map.clone()).await;
                published(&polled.source).await;
                published(&polled.source).await;

                listings[0].carry = true;
                let map = written(
                    map,
                    "desk-1",
                    desk_cell(1),
                    U256::from(QUOTE_BASE),
                    quote_word(ETHEREUM_BLOCK + 1, 0, true, true),
                );
                let mut node = FakeNode::ethereum(listings);
                Unanswered::BlockAfter.on(&mut node, true);
                let polled = Polled::new(node, map).await;
                for _ in 0..3 {
                    published(&polled.source).await;
                }
                assert_eq!(polled.reuses(), 0);
            });

            for (reason, reads) in [
                ("no_block_hash", 2),
                ("answered_in_part", 3),
                ("hash_read_failed", 0),
                ("hash_form_set_aside", 0),
            ] {
                assert_eq!(counter(READS_NOT_KEPT, Some(("reason", reason))), reads, "{reason}");
            }
            assert_eq!(counter(HASH_FORM_SET_ASIDE, None), 0);
        }

        /// The feed counts by its own reason a listing whose `doorView` names a credit mode above
        /// the front's three. The deployed front keeps the mode in two bits, so the reason stays
        /// at zero there; the view's field is a whole byte, and a listing left out by it is
        /// counted, not dropped from both counts.
        #[test]
        fn the_feed_counts_a_listing_of_a_credit_mode_the_front_does_not_know() {
            use crate::pamm::protocols::efficient_prop::telemetry::{
                LISTINGS_COVERED, LISTINGS_NOT_COVERED,
            };

            let counter = counted(async {
                let unknown = FakeListing { credit_mode: 4, ..FakeListing::ethereum_credit(1) };
                let books = credit_books(
                    vec![unknown, FakeListing::ethereum(2)],
                    vec![next_map(&[(1, false), (2, false)])],
                    &[1],
                )
                .await;
                assert_eq!(books.len(), 1);
            });

            assert_eq!(counter(LISTINGS_NOT_COVERED, Some(("reason", "credit_mode"))), 1);
            assert_eq!(counter(LISTINGS_NOT_COVERED, Some(("reason", "no_gas"))), 0);
            assert_eq!(counter(LISTINGS_COVERED, None), 1);
        }

        /// What a node leaves unanswered of one read of the doors, which fails no poll.
        #[derive(Debug, Clone, Copy)]
        enum Unanswered {
            /// The sources word of the listing reading a print, which is then left out.
            SourcesWord,
            /// The block after the next, which the carrying listing's sides then do not live to.
            BlockAfter,
        }

        impl Unanswered {
            fn on(self, node: &mut FakeNode, unanswered: bool) {
                match self {
                    Self::SourcesWord => {
                        node.listings[1].unread_words =
                            if unanswered { vec![10] } else { Vec::new() };
                    }
                    Self::BlockAfter => {
                        node.refused_blocks =
                            if unanswered { vec![ETHEREUM_BLOCK + 2] } else { Vec::new() };
                    }
                }
            }
        }

        /// A read of the doors the node answered in part is published by its own poll and not
        /// kept: the next poll asks every door again, where before it would have published the
        /// listing left out, or the shorter life, for as long as the head stood. Once the node
        /// answers all of it, that read is kept and the poll after it asks no door.
        #[rstest]
        #[case::a_listings_sources_word(Unanswered::SourcesWord)]
        #[case::the_block_after_the_next(Unanswered::BlockAfter)]
        #[tokio::test]
        async fn a_read_answered_in_part_is_not_kept(#[case] unanswered: Unanswered) {
            let (mut listings, map) = reuse_fixture();
            // The first listing and its word carry: its sides live to the block after the next
            // where the door fills there.
            listings[0].carry = true;
            let map = written(
                map,
                "desk-1",
                desk_cell(1),
                U256::from(QUOTE_BASE),
                quote_word(ETHEREUM_BLOCK + 1, 0, true, true),
            );
            let mut node = FakeNode::ethereum(listings);
            unanswered.on(&mut node, true);
            let polled = Polled::new(node, map).await;

            let in_part = published(&polled.source).await;
            let asked = polled.door_quotes();
            assert_eq!(published(&polled.source).await, in_part);
            assert!(polled.door_quotes() > asked, "the poll after a read in part asked no door");
            assert_eq!(polled.reuses(), 0);

            unanswered.on(&mut polled.node.lock().unwrap(), false);
            let whole = published(&polled.source).await;
            let asked = polled.door_quotes();
            assert_ne!(whole, in_part);
            assert_eq!(polled.reuses(), 0);
            assert_eq!(published(&polled.source).await, whole);
            assert_eq!(polled.door_quotes(), asked, "the poll after a whole read asked a door");
            assert_eq!(polled.reuses(), 1);
        }

        /// One thing the doors' answers are read from, which a test moves alone on the fake node
        /// or in the state answer.
        #[derive(Debug, Clone, Copy)]
        enum Moved {
            HeadHash,
            NextSecond,
            /// Its numbers, its time and the words' stamps together.
            NewBlock,
            QuoteWordInTheAnswer,
            FirmWordInTheAnswer,
            PriceContractInTheAnswer,
            DeskWord,
            DoorView,
            /// A byte of the front's `doorView` answer that no field of the listing is read from.
            DoorViewByteNoFieldReads,
            SettingsWord,
            LiveSourceWord,
            Implementation,
            ListingsServed,
        }

        impl Moved {
            fn on(self, node: &mut FakeNode, map: &mut serde_json::Value) {
                let next = ETHEREUM_BLOCK + 1;
                match self {
                    Self::HeadHash => node.head_hash = Some(B256::repeat_byte(0xb2)),
                    Self::NextSecond => {
                        node.timestamp += SLOT_SECS;
                        map["slot"] = (ETHEREUM_SLOT + 1).into();
                    }
                    Self::NewBlock => {
                        node.block += 1;
                        node.timestamp += SLOT_SECS;
                        *map = reuse_map(next + 1, ETHEREUM_SLOT + 1);
                    }
                    Self::QuoteWordInTheAnswer => {
                        let word = U256::from_be_bytes(quote_word(next, 0, false, true).0);
                        let word = B256::from(word | (U256::from(1u8) << 64usize));
                        *map = written(
                            map.take(),
                            "desk-1",
                            desk_cell(1),
                            U256::from(QUOTE_BASE),
                            word,
                        );
                    }
                    Self::FirmWordInTheAnswer => {
                        let word = B256::repeat_byte(0xf2);
                        *map = written(map.take(), "desk-1", desk_cell(1), firm_slot(), word);
                    }
                    Self::PriceContractInTheAnswer => {
                        let (account, word) =
                            (price_contract(APPROVED_SOURCE), B256::repeat_byte(0x5b));
                        *map = written(map.take(), "desk-2", account, U256::ZERO, word);
                    }
                    Self::DeskWord => node.desk_word_spare = U256::from(1u8) << 192usize,
                    // A tip cap of 5 gwei.
                    Self::DoorView => node.listings[0].tip_cap = 5 | (9 << 12),
                    Self::DoorViewByteNoFieldReads => node.listings[0].conf_mul = 7,
                    Self::SettingsWord => node.listings[0].carry = true,
                    Self::LiveSourceWord => node.listings[1].guarded = true,
                    Self::Implementation => {
                        node.implementation = OTHER_IMPLEMENTATION;
                        node.pinned_implementation = OTHER_IMPLEMENTATION;
                    }
                    Self::ListingsServed => {
                        let word = quote_word(next, 0, false, false);
                        *map = written(
                            map.take(),
                            "desk-3",
                            desk_cell(3),
                            U256::from(QUOTE_BASE),
                            word,
                        );
                    }
                }
            }
        }

        /// A change in any one thing the doors' answers are read from makes the next poll ask
        /// them again, and it publishes what a source that never sampled publishes there; the
        /// poll after it, finding that again, asks none. Each case moves one thing alone, but a
        /// new block, which moves its numbers, its time and the words' stamps together (each of
        /// those alone is another read in
        /// `a_read_differing_in_one_thing_its_doors_read_is_asked_on_another`).
        #[rstest]
        #[case::the_heads_hash(Moved::HeadHash)]
        #[case::the_next_blocks_second(Moved::NextSecond)]
        #[case::a_new_block(Moved::NewBlock)]
        #[case::its_quote_words_bytes_in_the_state_answer(Moved::QuoteWordInTheAnswer)]
        #[case::its_firm_words_bytes_in_the_state_answer(Moved::FirmWordInTheAnswer)]
        #[case::its_price_contracts_bytes_in_the_state_answer(Moved::PriceContractInTheAnswer)]
        #[case::its_desk_word(Moved::DeskWord)]
        #[case::its_door_view(Moved::DoorView)]
        #[case::a_byte_of_its_door_view_no_field_reads(Moved::DoorViewByteNoFieldReads)]
        #[case::its_settings_word(Moved::SettingsWord)]
        #[case::its_live_source_word(Moved::LiveSourceWord)]
        #[case::the_fronts_implementation(Moved::Implementation)]
        #[case::the_listings_served(Moved::ListingsServed)]
        #[tokio::test]
        async fn a_change_in_what_the_doors_read_asks_them_again(#[case] moved: Moved) {
            let (listings, map) = reuse_fixture();
            let polled = Polled::new(FakeNode::ethereum(listings), map).await;
            published(&polled.source).await;
            published(&polled.source).await;
            let asked = polled.door_quotes();
            assert_eq!(polled.reuses(), 1);

            moved.on(&mut polled.node.lock().unwrap(), &mut polled.map.lock().unwrap());
            let changed = published(&polled.source).await;

            assert!(polled.door_quotes() > asked, "the changed poll asked no door");
            assert_eq!(polled.reuses(), 1);
            let asked = polled.door_quotes();
            assert_eq!(published(&polled.source).await, changed);
            assert_eq!(polled.door_quotes(), asked, "the poll after the changed one asked a door");
            assert_eq!(polled.reuses(), 2);
            assert_eq!(changed, published(&polled.fresh()).await);
        }

        /// The feed counts what it does, through the `metrics` facade: each poll by how it ended,
        /// each listing whose door a poll's read holds, each listing it leaves out, by reason,
        /// and how old the node's answer was. Three polls here: one samples, one finds nothing
        /// changed, and one finds no state map for the next block (the node has moved on a
        /// block). Each of the first two holds three doors, finds no update for the fourth
        /// listing, no door open for the fifth, which the state answer covers, and no token of
        /// the feed's for the seventh; all three leave out the sixth, of a desk not approved,
        /// in their first request. The third poll's block is ahead of the wall clock, and its
        /// age is recorded as zero. Every outcome and reason is registered at zero from the
        /// build.
        #[test]
        fn the_feed_counts_its_polls_its_listings_and_its_answers_age() {
            use metrics_util::{
                debugging::{DebugValue, DebuggingRecorder},
                MetricKind,
            };
            use strum::IntoEnumIterator;

            use crate::pamm::protocols::efficient_prop::telemetry::{
                NotCovered, ANSWER_AGE, LISTINGS_COVERED, LISTINGS_NOT_COVERED, POLLS,
            };

            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // `with_local_recorder` takes a sync closure, so the polls run on a runtime inside it.
            metrics::with_local_recorder(&recorder, || {
                runtime.block_on(async {
                    let (mut listings, map) = reuse_fixture();
                    listings.extend([
                        FakeListing { opened: false, ..FakeListing::ethereum(5) },
                        FakeListing { desk: 99, ..FakeListing::ethereum(6) },
                        FakeListing {
                            base: Address::repeat_byte(0x77),
                            ..FakeListing::ethereum(7)
                        },
                    ]);
                    // The fifth listing's maker has its update in the state answer.
                    let mut entry = state_map_of(
                        ETHEREUM_BLOCK + 1,
                        ETHEREUM_SLOT,
                        &[(
                            "desk-5".to_string(),
                            5,
                            0,
                            quote_word(ETHEREUM_BLOCK + 1, 0, false, false),
                        )],
                    );
                    let mut map = map;
                    map["desk-5"] = entry["desk-5"].take();
                    let polled = Polled::new(FakeNode::ethereum(listings), map).await;
                    assert_eq!(published(&polled.source).await.len(), 3);
                    assert_eq!(published(&polled.source).await.len(), 3);
                    assert_eq!(polled.reuses(), 1);
                    {
                        let mut node = polled.node.lock().unwrap();
                        node.block += 1;
                        node.timestamp += SLOT_SECS;
                    }
                    assert!(published(&polled.source)
                        .await
                        .is_empty());
                });
            });

            let recorded: HashMap<_, _> = snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .map(|(key, _, _, value)| {
                    let labels: Vec<_> = key
                        .key()
                        .labels()
                        .map(|label| (label.key().to_string(), label.value().to_string()))
                        .collect();
                    ((key.kind(), key.key().name().to_string(), labels), value)
                })
                .collect();
            let counter = |name: &str, label: Option<(&str, &str)>| {
                let labels = label
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .into_iter()
                    .collect();
                match recorded.get(&(MetricKind::Counter, name.to_string(), labels)) {
                    Some(DebugValue::Counter(count)) => *count,
                    other => panic!("{name} {label:?} is not a counter recorded: {other:?}"),
                }
            };
            for (outcome, polls) in
                [("sampled", 1), ("reused", 1), ("no_state_map", 1), ("failed", 0)]
            {
                assert_eq!(counter(POLLS, Some(("outcome", outcome))), polls, "{outcome}");
            }
            assert_eq!(counter(LISTINGS_COVERED, None), 6);
            let left_out = [
                (NotCovered::DeskNotApproved, 3),
                (NotCovered::UnknownToken, 2),
                (NotCovered::NoEntry, 2),
                (NotCovered::NotOpened, 2),
            ];
            for reason in NotCovered::iter() {
                let not_covered = left_out
                    .iter()
                    .find(|(left_out, _)| *left_out == reason)
                    .map_or(0, |(_, listings)| *listings);
                let reason = <&'static str>::from(reason);
                assert_eq!(
                    counter(LISTINGS_NOT_COVERED, Some(("reason", reason))),
                    not_covered,
                    "{reason}"
                );
            }
            // The wall clock stands six seconds before the next block: six after the node's
            // latest on the first two polls, and six before it once the node has moved on, which
            // is recorded as zero.
            let ages = &recorded[&(MetricKind::Histogram, ANSWER_AGE.to_string(), Vec::new())];
            let DebugValue::Histogram(ages) = ages else {
                panic!("{ANSWER_AGE} is not a histogram: {ages:?}");
            };
            let ages: Vec<f64> = ages
                .iter()
                .map(|age| age.into_inner())
                .collect();
            assert_eq!(ages, [6.0, 6.0, 0.0]);
        }

        /// SplitMix64: the seeded chance of
        /// `no_run_of_changes_publishes_other_books_than_a_source_that_asks_every_door`.
        struct Seeded(u64);

        impl Seeded {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            }

            fn below(&mut self, bound: usize) -> usize {
                usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
            }
        }

        /// What the run's makers have sent for the next block, per desk: how many updates, each
        /// writing other bytes, or `None` where the maker sent none.
        struct Sent {
            words: BTreeMap<u32, Option<u64>>,
            prints: u64,
        }

        impl Sent {
            /// The state answer for the block after `block`: each update's quote word stamped for
            /// it under its desk's epoch at the head, its count in bits no reader reads, and the
            /// print listing's price contract with it.
            fn map(&self, node: &FakeNode) -> serde_json::Value {
                let next = node.block + 1;
                let slot = (node.timestamp + SLOT_SECS - BEACON_GENESIS) / SLOT_SECS;
                let entries: Vec<_> = self
                    .words
                    .iter()
                    .filter_map(|(&desk, &count)| {
                        let epoch = node
                            .desk_heads
                            .get(&desk)
                            .map_or(0, |head| head.0);
                        let word = U256::from_be_bytes(quote_word(next, epoch, false, false).0);
                        let word = B256::from(word | (U256::from(count?) << 64usize));
                        Some((format!("desk-{desk}"), desk, 0u8, word))
                    })
                    .collect();
                let map = state_map_of(next, slot, &entries);
                if self.words[&2].is_none() {
                    return map;
                }
                written(
                    map,
                    "desk-2",
                    price_contract(APPROVED_SOURCE),
                    U256::ZERO,
                    B256::from(U256::from(self.prints)),
                )
            }
        }

        /// Whatever happens between its polls, in whatever order, a source that keeps its doors'
        /// reads publishes what a source that never sampled publishes at that moment. Each step
        /// of a seeded run does nothing, or one thing a chain or its makers do: a maker's update
        /// (other bytes in the state answer, and another price at its door), a price update, a
        /// maker sending none, a new block, a block replaced at the same height, a desk stopped
        /// or revived. Every change of a door's answer comes as on chain, with other bytes in the
        /// state answer or under another head hash; some steps ask no door and some ask them all.
        /// The fake's doors read the block they are asked at: under another head hash each quotes
        /// other amounts, whatever else that block holds, so a block replaced by one that
        /// differs in nothing a poll reads but its hash publishes other books too.
        #[rstest]
        #[case::seed_1(1)]
        #[case::seed_2(2)]
        #[case::seed_3(3)]
        #[case::seed_4(4)]
        #[tokio::test]
        async fn no_run_of_changes_publishes_other_books_than_a_source_that_asks_every_door(
            #[case] seed: u64,
        ) {
            let (listings, _) = reuse_fixture();
            let node = FakeNode::ethereum(listings);
            let mut sent = Sent {
                words: BTreeMap::from([(1, Some(0)), (2, Some(0)), (3, None), (4, Some(0))]),
                prints: 0,
            };
            let polled = Polled::new(node.clone(), sent.map(&node)).await;
            let mut chance = Seeded(seed);
            let mut hashes = 0u8;
            let mut sampled = 0;
            let mut served_under_another_head = 0;

            for step in 0..40 {
                let mut node = polled.node.lock().unwrap().clone();
                let mut new_head = false;
                let did = match chance.below(12) {
                    0..=4 => "nothing",
                    5 | 6 => {
                        // A maker's update: other bytes, and another price at its door.
                        let at = chance.below(4);
                        let desk = 1 + u32::try_from(at).unwrap();
                        let count = sent.words[&desk].map_or(0, |count| count + 1);
                        sent.words.insert(desk, Some(count));
                        let listing = &mut node.listings[at];
                        listing.bid_out_cap = match listing.bid_out_cap {
                            None => Some(U256::from(100_000_000u64)),
                            Some(_) => None,
                        };
                        "a maker's update"
                    }
                    7 => {
                        sent.prints += 1;
                        let listing = &mut node.listings[1];
                        listing.ask_out_cap = match listing.ask_out_cap {
                            None => Some(U256::from(10u64).pow(U256::from(16u8))),
                            Some(_) => None,
                        };
                        "a price update"
                    }
                    8 => {
                        let desk = 1 + u32::try_from(chance.below(4)).unwrap();
                        sent.words.insert(desk, None);
                        "a maker sending no update"
                    }
                    9 => {
                        node.block += 1;
                        node.timestamp += SLOT_SECS;
                        polled
                            .now_ms
                            .fetch_add(SLOT_SECS * 1000, Ordering::SeqCst);
                        for listing in &mut node.listings {
                            listing.maker_ms += SLOT_SECS * 1000;
                        }
                        new_head = true;
                        "a new block"
                    }
                    10 => {
                        // Another block at the same height: on one in two its pool holds, or
                        // lacks, the base, and on the other nothing a poll reads differs but
                        // its hash.
                        if chance.below(2) == 0 {
                            let listing = &mut node.listings[chance.below(4)];
                            listing.pool_lacks.0 = !listing.pool_lacks.0;
                        }
                        new_head = true;
                        "a block replaced"
                    }
                    _ => {
                        let desk = 1 + u32::try_from(chance.below(4)).unwrap();
                        let (epoch, stopped) = node
                            .desk_heads
                            .get(&desk)
                            .copied()
                            .unwrap_or((0, false));
                        node.desk_heads
                            .insert(desk, (epoch + 1, !stopped));
                        new_head = true;
                        "a desk stopped or revived"
                    }
                };
                if new_head {
                    hashes += 1;
                    node.head_hash = Some(B256::repeat_byte(hashes));
                }
                *polled.map.lock().unwrap() = sent.map(&node);
                *polled.node.lock().unwrap() = node;
                let reuses = polled.reuses();

                let kept = published(&polled.source).await;

                sampled += usize::from(polled.reuses() == reuses);
                served_under_another_head += usize::from(new_head && !kept.is_empty());
                let fresh = published(&polled.fresh()).await;
                assert_eq!(kept, fresh, "seed {seed}, step {step}, after {did}");
            }
            assert!(polled.reuses() > 0, "seed {seed}: no poll asked no door");
            assert!(sampled > 1, "seed {seed}: no poll after the first asked its doors");
            assert!(served_under_another_head > 0, "seed {seed}: no book under another head");
            assert_eq!(polled.door_quotes_by_number(), 0, "seed {seed}: a door asked by number");
        }

        /// The door quotes a poll of each recorded Ethereum read asks, on a node naming its
        /// blocks by hash: the first poll asks every door as before, and a second one finding the
        /// same asks none and publishes the same books. The recordings hold no header, so the
        /// hash is the test's own, and the node answers a request naming its block by that hash
        /// as the recorded request naming it by its number; on a node naming no block (the
        /// recordings as they are) both polls ask every door, as every poll did before.
        #[rstest]
        #[case::no_update(include_str!("testdata/ethereum_fork_26482068.json"), 0)]
        #[case::served(include_str!("testdata/ethereum_fork_26479480_served.json"), 43)]
        #[case::served_with_the_credit_listing(
            include_str!("testdata/ethereum_fork_26479480_credit.json"),
            135
        )]
        #[tokio::test]
        async fn a_second_poll_of_a_recorded_read_asks_no_door(
            #[case] recording: &str,
            #[case] quotes_a_poll: usize,
            #[values(true, false)] names_blocks: bool,
        ) {
            let poll: RecordedEthereumPoll = serde_json::from_str(recording).unwrap();
            let node = RecordedNode::new(&poll.rpc);
            let node =
                if names_blocks { node.naming_blocks(B256::repeat_byte(0xb1)) } else { node };
            let (unrecorded, door_quotes, by_hash) =
                (node.unrecorded(), node.door_quotes(), node.by_hash());
            let node = node.spawn().await;
            let mut servers = Vec::new();
            for map in &poll.maps {
                servers.push(spawn_state_server(map.clone()).await);
            }
            let source = recorded_source(&poll, &node.url(), &servers);

            let first = published(&source).await;
            let asked = door_quotes.load(Ordering::SeqCst);
            // Where the node names its blocks, the doors' requests named theirs by its hash,
            // which the node answers as the recorded requests of that number.
            assert_eq!(by_hash.load(Ordering::SeqCst) > 0, names_blocks);
            // The recording holds one scan of the listings, the first poll's from no listing
            // known: the second poll scans as that one did.
            source
                .highest_listing
                .store(0, Ordering::Relaxed);
            let second = published(&source).await;

            assert_eq!(asked, quotes_a_poll);
            let asked_again = door_quotes.load(Ordering::SeqCst) - asked;
            assert_eq!(asked_again, if names_blocks { 0 } else { quotes_a_poll });
            assert_eq!(unrecorded.load(Ordering::SeqCst), 0, "calls the recording does not answer");
            assert_eq!(first.len(), poll.books);
            assert_eq!(second, first);
        }

        /// The Ethereum preset is the deployment the recorded polls were made against, value for
        /// value, and the price sources it approves are theirs: nothing in it is written down
        /// anywhere but there.
        #[rstest]
        #[case::no_update(include_str!("testdata/ethereum_fork_26482068.json"))]
        #[case::served(include_str!("testdata/ethereum_fork_26479480_served.json"))]
        #[case::served_with_the_credit_listing(include_str!(
            "testdata/ethereum_fork_26479480_credit.json"
        ))]
        fn the_ethereum_preset_is_the_recorded_deployment(#[case] recording: &str) {
            let poll: RecordedEthereumPoll = serde_json::from_str(recording).unwrap();

            assert_eq!(EfficientPropDeployment::ethereum(), poll.deployment());
            assert_eq!(poll.price_sources, ETHEREUM_PRICE_SOURCES);
        }

        /// A feed built from the preset needs a node, one state URL and its approved desks, and
        /// no more: it replays the recorded poll that serves four listings, every call answered
        /// from the recording, and publishes its four books.
        #[tokio::test]
        async fn a_feed_from_the_ethereum_preset_needs_a_node_a_state_url_and_its_desks() {
            let poll: RecordedEthereumPoll =
                serde_json::from_str(include_str!("testdata/ethereum_fork_26479480_served.json"))
                    .unwrap();
            let node = RecordedNode::new(&poll.rpc);
            let unrecorded = node.unrecorded();
            let node = node.spawn().await;
            let state = spawn_state_server(poll.maps[0].clone()).await;
            assert_eq!(poll.maps.len(), 1);
            let tokens: HashMap<Bytes, Token> = poll
                .tokens
                .iter()
                .map(|&(address, decimals)| {
                    let address = bytes(address);
                    let token = Token::new(
                        &address,
                        "TKN",
                        decimals,
                        0,
                        &[Some(10_000)],
                        Chain::Ethereum,
                        100,
                    );
                    (address, token)
                })
                .collect();
            let config = BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd: 0.0,
            };

            let mut source = EfficientPropFeedBuilder::ethereum(config, node.url())
                .state_urls([state.url()])
                .with_approved_desks(
                    poll.listings
                        .iter()
                        .map(|listing| bytes(listing.quote_contract)),
                )
                .build()
                .unwrap()
                .source;
            let now_ms = poll.now_ms;
            source.clock = Arc::new(move || now_ms);
            let books = source.fetch().await.unwrap().books;

            assert_eq!(unrecorded.load(Ordering::SeqCst), 0, "calls the recording does not answer");
            assert_eq!((books.len(), poll.books), (4, 4));
        }

        /// The source a recorded Ethereum read was made by, polling `node_url` and the state
        /// `servers`: the recording's tokens, front, credit listings, price sources and desks,
        /// its wall clock stopped where the recording's was.
        fn recorded_source(
            poll: &RecordedEthereumPoll,
            node_url: &str,
            servers: &[MockHttpServer],
        ) -> EfficientPropBookSource {
            let tokens: HashMap<Bytes, Token> = poll
                .tokens
                .iter()
                .map(|&(address, decimals)| {
                    let address = bytes(address);
                    let token = Token::new(
                        &address,
                        "TKN",
                        decimals,
                        0,
                        &[Some(10_000)],
                        Chain::Ethereum,
                        100,
                    );
                    (address, token)
                })
                .collect();
            let config = BookFeedConfig {
                chain: Chain::Ethereum,
                tokens: Arc::new(tokens),
                min_tvl_usd: 0.0,
            };
            let builder = poll.credit.iter().fold(
                EfficientPropFeedBuilder::new(config, node_url.to_string(), poll.deployment()),
                |builder, &(listing, gas)| builder.credit_listing(listing, gas),
            );
            let mut source = builder
                .approved_price_sources(poll.price_sources.iter().copied())
                .with_approved_desks(
                    poll.listings
                        .iter()
                        .map(|listing| bytes(listing.quote_contract)),
                )
                .state_urls(
                    servers
                        .iter()
                        .map(|server| server.url()),
                )
                .build()
                .unwrap()
                .source;
            let now_ms = poll.now_ms;
            source.clock = Arc::new(move || now_ms);
            source
        }

        /// An Ethereum poll replayed call for call from `testdata/`: every call it makes of the
        /// node answered there, its state URLs answering the maps recorded, on the path that reads
        /// the makers' desk contract by the slot rule. The poll publishes the books recorded: a
        /// book for exactly the listings recorded as served, and every side of them sample for
        /// sample with the gas it is charged. Recorded by
        /// `a_fork_poll_quotes_each_door_as_it_fills_in_the_next_block` with
        /// `EFFICIENT_PROP_ETH_RECORD`, which held each sample to the door's own answer in the
        /// block the map is for, wei for wei.
        ///
        /// - `ethereum_fork_26482068.json`: a local fork of Ethereum at its block 26,482,055 with
        ///   the deployment as it stands there (front, listing-door factory, the first maker's desk
        ///   contract and the four price contracts are Ethereum's own), the first pool's launch
        ///   replayed on the fork from the operator's previewed calls, and the four listing doors
        ///   opened on the fork, at the addresses they take on Ethereum. Its maps are empty: it
        ///   holds the poll's reads of the deployed front, the factory and the doors, and that it
        ///   serves no listing without its maker's update.
        /// - `ethereum_fork_26479480_served.json`: a local fork of Ethereum at its block 26,479,360
        ///   holding the same deployment and two pools launched through the deployed launcher: a
        ///   first pool, of wstETH, WETH, USDC and USDT with its four listings (the credit path
        ///   on), and a pool of wstETH and WETH alone with no lender, whose one listing (the fifth
        ///   the front handed out, in no credit mode) is bound to the same maker's desk at another
        ///   word index and to the wstETH/WETH price contract the first pool's listing reads. The
        ///   map writes each listing's quote word and its price contract's print as the maker's
        ///   update for the next block does (a test maker's quotes, and prints Pyth's verifier
        ///   checked against a test signer). Nothing names a listing: the scan finds the fifth, the
        ///   three listings paid from the first pool's buffers and the second pool's are served at
        ///   their classes' gas, and the first pool's wstETH/WETH listing, in credit mode 2, is
        ///   left out.
        /// - `ethereum_fork_26479480_credit.json`: the same fork and map, the first pool's
        ///   wstETH/WETH listing named with its gas: its bid is served through the credit route (a
        ///   borrow at the lender) and its ask from custody up to what the pool holds and through
        ///   the route (a repayment) past it, beside the four other listings, whose pairs hold no
        ///   token the route leaves owed or sit on another pool.
        ///
        /// The forks' blocks past their fork blocks are their own, so the pools, their listings
        /// and the doors' code are not on Ethereum yet. To be re-recorded on the chain once a
        /// block carries a maker's update.
        #[rstest]
        #[case::no_update(include_str!("testdata/ethereum_fork_26482068.json"), 0)]
        #[case::served(include_str!("testdata/ethereum_fork_26479480_served.json"), 4)]
        #[case::served_with_the_credit_listing(
            include_str!("testdata/ethereum_fork_26479480_credit.json"),
            5
        )]
        #[tokio::test]
        async fn a_recorded_ethereum_poll_replays_call_for_call(
            #[case] recording: &str,
            #[case] books_served: usize,
        ) {
            use crate::evm::protocol::u256_num::u256_to_biguint;

            let poll: RecordedEthereumPoll = serde_json::from_str(recording).unwrap();
            assert_eq!(poll.books, books_served);
            let node = RecordedNode::new(&poll.rpc);
            let unrecorded = node.unrecorded();
            let node = node.spawn().await;
            let mut servers = Vec::new();
            for map in &poll.maps {
                servers.push(spawn_state_server(map.clone()).await);
            }
            let source = recorded_source(&poll, &node.url(), &servers);

            let books = source.fetch().await.unwrap().books;

            assert_eq!(unrecorded.load(Ordering::SeqCst), 0, "calls the recording does not answer");
            assert_eq!(books.len(), poll.books);
            assert!(!poll.listings.is_empty(), "the recording holds no listing");
            for listing in &poll.listings {
                let served = books.values().any(|book| {
                    book.component.static_attributes[PAMM_ADDRESS_ATTRIBUTE] == bytes(listing.door)
                });
                assert_eq!(served, listing.served, "listing {}", listing.listing);
                assert!(
                    !(listing.served && (!listing.written || listing.left_out.is_some())),
                    "listing {} is recorded served without its maker's update or though left out",
                    listing.listing
                );
            }
            // Every side recorded, sample for sample: the output the door answered in the block
            // the map is for, and the gas charged.
            assert_eq!(poll.sides.len(), 2 * poll.books);
            for side in &poll.sides {
                let book = books
                    .values()
                    .find(|book| {
                        book.component.static_attributes[PAMM_ADDRESS_ATTRIBUTE] == bytes(side.door)
                    })
                    .expect("a recorded side's book is published");
                let state = book
                    .state
                    .as_any()
                    .downcast_ref::<EfficientPropState>()
                    .unwrap();
                let (direction, token_in, token_out) = if side.sell_base {
                    (SwapDirection::BaseToQuote, &state.base_token, &state.quote_token)
                } else {
                    (SwapDirection::QuoteToBase, &state.quote_token, &state.base_token)
                };
                let points: Vec<(U256, U256)> = side
                    .points
                    .iter()
                    .map(|&(amount, output, _)| (amount, output))
                    .collect();
                assert_eq!(state.side(direction).points(), points, "door {}", side.door);
                for &(amount, output, gas) in &side.points {
                    let quoted = state
                        .get_amount_out(u256_to_biguint(amount), token_in, token_out)
                        .unwrap();
                    assert_eq!(
                        (quoted.amount, quoted.gas),
                        (u256_to_biguint(output), BigUint::from(gas)),
                        "door {} at {amount}",
                        side.door
                    );
                }
            }
        }

        /// BNB Chain: the builder build of the front, whose makers' updates land in the block
        /// that fills them, read as on Ethereum with BNB Chain's clock.
        mod bnb {
            use super::*;
            use crate::pamm::protocols::efficient_prop::test_support::BNB_HEAD_SUB_MS;

            /// The fake node's latest block's milliseconds.
            const HEAD_MS: u64 = ETHEREUM_NEXT_TIME * 1000 + BNB_HEAD_SUB_MS;

            /// The wall clock of the tests: 50 ms after the latest block was made.
            const NOW_MS: u64 = HEAD_MS + 50;

            fn bnb_config() -> BookFeedConfig {
                BookFeedConfig { chain: Chain::Bsc, ..book_config() }
            }

            /// A builder of the fake front on BNB Chain reading its maps from `state_urls`, with
            /// every desk of the first 64 approved and the first 64 listings named the tests' gas.
            fn bnb_builder(node_url: &str, state_urls: Vec<String>) -> EfficientPropFeedBuilder {
                let builder = EfficientPropFeedBuilder::new(
                    bnb_config(),
                    node_url.to_string(),
                    ethereum_deployment(),
                )
                .approved_price_sources([APPROVED_SOURCE])
                .with_approved_desks(approved_desks())
                .usd_quote_tokens(HashSet::from([bytes(usdc_address())]))
                .state_urls(state_urls);
                (1..=64)
                    .fold(builder, |builder, listing| builder.credit_listing(listing, named_gas()))
            }

            /// The books one poll of `node` publishes on BNB Chain, its maps from servers
            /// answering `maps`, the wall clock at `now_ms`.
            async fn bnb_fetch(
                node: FakeNode,
                maps: Vec<serde_json::Value>,
                now_ms: u64,
            ) -> Result<HashMap<String, Book>, FeedError> {
                let node = node.spawn().await;
                let mut servers = Vec::new();
                for map in maps {
                    servers.push(spawn_state_server(map).await);
                }
                let urls = servers
                    .iter()
                    .map(|server| server.url())
                    .collect();
                let mut source = bnb_builder(&node.url(), urls)
                    .build()
                    .unwrap()
                    .source;
                source.clock = Arc::new(move || now_ms);
                let snapshot = source.fetch().await?;
                Ok(Arc::try_unwrap(snapshot.books).unwrap())
            }

            /// A map for the block after the node's latest, naming no slot: BNB Chain has none.
            fn bnb_map(desks: &[(u32, bool)]) -> serde_json::Value {
                let mut map = next_map(desks);
                map.as_object_mut()
                    .unwrap()
                    .remove("slot");
                map
            }

            /// The next block lands 450 ms after the latest, in its second or the one after: each
            /// side is sampled at both and quotes the least, valid for the next block, until that
            /// block's 450 ms less the 100 ms landing allowance, or where the listing and the
            /// word carry, the block after's too.
            #[rstest]
            #[case::the_next_block(false, HEAD_MS + 450 + 1 - 100)]
            #[case::through_the_block_after(true, HEAD_MS + 900 + 1 - 100)]
            #[tokio::test]
            async fn a_bnb_listing_quotes_its_next_block_at_either_second(
                #[case] carries: bool,
                #[case] until_ms: u64,
            ) {
                // The bid drifts 10 ppm a second: it quotes less in the later second.
                let listing = FakeListing {
                    carry: carries,
                    aging: FakeAging { bid_drift: 10, ..FakeAging::default() },
                    ..FakeListing::ethereum(1)
                };

                let books = bnb_fetch(
                    FakeNode::bnb(vec![listing.clone()]),
                    vec![bnb_map(&[(1, carries)])],
                    NOW_MS,
                )
                .await
                .unwrap();

                let state = state_for(&books, &listing);
                let bid = samples(state, SwapDirection::BaseToQuote);
                assert!(!bid.is_empty());
                assert_eq!(bid, door_quotes(&listing, &bid, true, ETHEREUM_NEXT_TIME + 1));
                assert_eq!(
                    state
                        .side_until(SwapDirection::BaseToQuote)
                        .unwrap()
                        .unix_ms,
                    until_ms
                );
                assert_eq!(state.gas_chain, GasChain::Bnb);
                let book = books.values().next().unwrap();
                assert_eq!(
                    book.component.static_attributes[VALID_FROM_BLOCK_ATTRIBUTE],
                    Bytes::from(U256::from(ETHEREUM_BLOCK + 1).to_be_bytes_trimmed_vec())
                );
            }

            /// A poll ending too late for a route to land before the next block is sealed serves
            /// no side of a listing that does not carry.
            #[tokio::test]
            async fn a_bnb_poll_too_late_for_the_next_block_serves_nothing() {
                let listing = FakeListing::ethereum(1);

                let books = bnb_fetch(
                    FakeNode::bnb(vec![listing]),
                    vec![bnb_map(&[(1, false)])],
                    HEAD_MS + 320,
                )
                .await
                .unwrap();

                assert!(books.is_empty());
            }

            /// A credit listing named with its gas is served on BNB Chain as on Ethereum: its
            /// bid's custody and routed sizes each charged their path's gas.
            #[tokio::test]
            async fn a_named_credit_listing_is_served_on_bnb() {
                let listing = FakeListing::ethereum_credit(1);
                let node = FakeNode::bnb(vec![listing.clone()])
                    .spawn()
                    .await;
                let server = spawn_state_server(bnb_map(&[(1, false)])).await;
                let mut source =
                    EfficientPropFeedBuilder::new(bnb_config(), node.url(), ethereum_deployment())
                        .with_approved_desks(approved_desks())
                        .usd_quote_tokens(HashSet::from([bytes(usdc_address())]))
                        .credit_listing(1, credit_gas())
                        .state_urls([server.url()])
                        .build()
                        .unwrap()
                        .source;
                source.clock = Arc::new(|| NOW_MS);

                let books = Arc::try_unwrap(source.fetch().await.unwrap().books).unwrap();

                let state = state_for(&books, &listing);
                assert_eq!(state.credit, Some(credit_gas()));
                let bid = samples(state, SwapDirection::BaseToQuote);
                let route_least = bid
                    .iter()
                    .find(|(amount, _)| *amount > credit_prefix())
                    .unwrap()
                    .0;
                assert_eq!(
                    charged(state, SwapDirection::BaseToQuote, route_least).unwrap(),
                    BigUint::from(610_501u64)
                );
            }

            /// Once BNB Chain's millisecond precompile answers, a front in clock mode 3 reads the
            /// block's milliseconds, which no block override sets: the feed ends rather than
            /// sample a block at a time it does not land at. A front in mode 0 is served
            /// whatever the precompile answers.
            #[rstest]
            #[case::mode_3_precompile_silent(3, None, true)]
            #[case::mode_3_precompile_answers(3, Some(HEAD_MS), false)]
            #[case::mode_0_precompile_answers(0, Some(HEAD_MS), true)]
            #[case::mode_2(2, None, false)]
            #[tokio::test]
            async fn a_bnb_front_is_served_while_its_clock_reads_the_second(
                #[case] mode: u64,
                #[case] precompile: Option<u64>,
                #[case] served: bool,
            ) {
                let listing = FakeListing::ethereum(1);
                let mut node = FakeNode::bnb(vec![listing]);
                node.clock.mode = mode;
                node.ms_precompile = precompile;

                let books = bnb_fetch(node, vec![bnb_map(&[(1, false)])], NOW_MS).await;

                match books {
                    Ok(books) => assert!(served && books.len() == 1, "{books:?}"),
                    Err(error) => {
                        assert!(!served, "{error}");
                        assert!(matches!(error, FeedError::Fatal(_)), "{error}");
                    }
                }
            }

            /// BNB Chain needs a state URL and the builder layout, as Ethereum does, and is not
            /// served through the fallback router.
            #[rstest]
            #[case::no_state_url(false, Some(QUOTE_LAYOUT_BUILDER), false, "needs a state URL")]
            #[case::no_layout(true, None, false, "is not one this integration reads")]
            #[case::the_fallback_router(true, Some(QUOTE_LAYOUT_BUILDER), true, "Ethereum only")]
            fn the_builder_refuses_what_bnb_cannot_run(
                #[case] state_url: bool,
                #[case] layout: Option<&str>,
                #[case] fallback: bool,
                #[case] reason: &str,
            ) {
                let mut deployment = ethereum_deployment();
                deployment.quote_layout = layout.map(str::to_string);
                let builder = EfficientPropFeedBuilder::new(
                    bnb_config(),
                    "http://node".to_string(),
                    deployment,
                )
                .state_urls(
                    ["http://state".to_string()]
                        .into_iter()
                        .filter(|_| state_url),
                );
                let builder = if fallback { builder.with_fallback_router() } else { builder };

                let error = builder.build().unwrap_err();

                assert!(
                    matches!(&error, FeedError::InvalidInput(message) if message.contains(reason)),
                    "{error}"
                );
            }
        }
    }

    /// The presets of the three chains, held to their deployments' records.
    mod presets {
        use super::*;
        use crate::pamm::protocols::efficient_prop::slots::SLOT_RULE;

        /// Each preset's desk contract code hash places desk 1's contract under the preset's front
        /// where its deployment's record has it (its `desk1`; on BNB Chain the rehearsal's): a
        /// hash other than the deployed desk code's places every desk where none is approved.
        #[rstest]
        #[case::ethereum(
            EfficientPropDeployment::ethereum(),
            address!("4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753")
        )]
        #[case::base(
            EfficientPropDeployment::base(),
            address!("2b347C8e89520aF26d30e6932f6273B699Ae746A")
        )]
        #[case::bnb(
            EfficientPropDeployment::bnb(),
            address!("4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753")
        )]
        fn each_preset_places_desk_one_where_its_deployment_opened_it(
            #[case] deployment: EfficientPropDeployment,
            #[case] desk_one: Address,
        ) {
            let front = Address::from_slice(&deployment.front);
            let cell_codehash = B256::from_slice(
                deployment
                    .cell_codehash
                    .as_ref()
                    .unwrap(),
            );

            assert_eq!(SLOT_RULE.account(front, cell_codehash, 1), desk_one);
        }

        /// Base's and BNB Chain's records name the front, its listing-door factory and its
        /// implementation at Ethereum's addresses; Base reads no quote contract and BNB Chain's
        /// is Ethereum's layout.
        #[test]
        fn the_base_and_bnb_presets_are_their_records() {
            let ethereum = EfficientPropDeployment::ethereum();
            for deployment in [EfficientPropDeployment::base(), EfficientPropDeployment::bnb()] {
                assert_eq!(
                    deployment.front,
                    bytes(address!("0000000000403aeB66691063CC2fD42E71d6AcD6"))
                );
                assert_eq!(
                    deployment.factory,
                    bytes(address!("39eD9993449C96503f6a629cCD2BB5F132D7cc93"))
                );
                assert_eq!(
                    deployment.implementations,
                    vec![bytes(address!("48C6efab497EeC8501483Ceb23B6623f626d8371"))]
                );
            }
            assert_eq!(EfficientPropDeployment::base().quote_layout, None);
            assert_eq!(EfficientPropDeployment::bnb(), ethereum);
        }

        /// A feed from the Base preset builds from a Base node alone, approving the one price
        /// source of Base's deployment, and refuses a state URL.
        #[test]
        fn a_feed_from_the_base_preset_needs_no_state_url() {
            let builder = || EfficientPropFeedBuilder::base(book_config(), "http://node".into());

            assert_eq!(builder().price_sources, BTreeSet::from(BASE_PRICE_SOURCES));
            assert_eq!(builder().price_sources, BTreeSet::from([1]));
            let feed = builder().build().unwrap();
            assert_eq!(feed.source.assembler.landing_ms, 2_000);
            let error = builder()
                .state_urls(["http://state".to_string()])
                .build()
                .unwrap_err();
            assert!(
                matches!(&error, FeedError::InvalidInput(message) if message.contains("state URL")),
                "{error}"
            );
        }

        /// A feed from the BNB Chain preset needs a state URL, approves no price source until the
        /// deployment's record names one, and values books in USD1 beside the chain's curated
        /// stablecoins.
        #[test]
        fn a_feed_from_the_bnb_preset_needs_a_state_url_and_values_usd1() {
            let config = BookFeedConfig { chain: Chain::Bsc, ..book_config() };
            let builder = || EfficientPropFeedBuilder::bnb(config.clone(), "http://node".into());

            assert!(builder().price_sources.is_empty());
            let mut usd = usd_stablecoins_for_chain(Chain::Bsc).unwrap();
            assert!(usd.insert(bytes(BNB_USD1)));
            assert_eq!(builder().usd_quote_tokens, usd);
            let error = builder().build().unwrap_err();
            assert!(
                matches!(&error, FeedError::InvalidInput(message) if message.contains("state URL")),
                "{error}"
            );
            let feed = builder()
                .state_urls(["http://state".to_string()])
                .build()
                .unwrap();
            assert_eq!(feed.source.assembler.landing_ms, 100);
        }

        /// Polls Base through the preset with desk 1 approved and prints what it serves. Each
        /// poll checks the preset against the chain (the factory's `FRONT`, the front's
        /// `CELL_CODEHASH` and the implementation it runs), and fails where any differs. Set
        /// `EFFICIENT_PROP_BASE_RPC` to a Base node.
        #[tokio::test]
        #[ignore = "reads Base mainnet; set EFFICIENT_PROP_BASE_RPC"]
        async fn live_poll_of_the_base_preset() {
            let Ok(rpc) = std::env::var("EFFICIENT_PROP_BASE_RPC") else {
                println!("skipped: EFFICIENT_PROP_BASE_RPC unset");
                return;
            };
            let deployment = EfficientPropDeployment::base();
            let desk_one = SLOT_RULE.account(
                Address::from_slice(&deployment.front),
                B256::from_slice(
                    deployment
                        .cell_codehash
                        .as_ref()
                        .unwrap(),
                ),
                1,
            );
            let source = EfficientPropFeedBuilder::base(book_config(), rpc)
                .with_approved_desks([bytes(desk_one)])
                .build()
                .unwrap()
                .source;

            for _ in 0..4 {
                let snapshot = source.fetch().await.unwrap();
                println!("{} books served", snapshot.books.len());
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }

        /// Reads BNB Chain's front through a node and holds it to the preset: the implementation
        /// the proxy runs, the front's `CELL_CODEHASH`, desk 1's contract where the preset places
        /// it, and a clock reading the block's second (mode 3, with the millisecond precompile
        /// answering nothing). Where the listing-door factory has code, its `FRONT` must be the
        /// front. A poll needs a state URL serving chain 56, which none does yet, so this reads
        /// the values directly. Set `EFFICIENT_PROP_BNB_RPC` to a BNB Chain node.
        #[tokio::test]
        #[ignore = "reads BNB Chain mainnet; set EFFICIENT_PROP_BNB_RPC"]
        async fn live_bnb_front_is_the_preset() {
            use alloy::{
                primitives::Bytes as AlloyBytes,
                providers::{Provider, RootProvider},
                rpc::types::TransactionRequest,
                sol_types::SolCall,
            };

            use crate::pamm::protocols::efficient_prop::reader::{
                clockCall, deskCellOfCall, CELL_CODEHASHCall, FRONTCall, IMPLEMENTATION_SLOT,
            };

            let Ok(rpc) = std::env::var("EFFICIENT_PROP_BNB_RPC") else {
                println!("skipped: EFFICIENT_PROP_BNB_RPC unset");
                return;
            };
            let provider: RootProvider = RootProvider::new_http(rpc.parse().unwrap());
            assert_eq!(provider.get_chain_id().await.unwrap(), 56);
            let deployment = EfficientPropDeployment::bnb();
            let front = Address::from_slice(&deployment.front);
            let factory = Address::from_slice(&deployment.factory);
            let cell_codehash = B256::from_slice(
                deployment
                    .cell_codehash
                    .as_ref()
                    .unwrap(),
            );
            let call = |to: Address, data: Vec<u8>| {
                let provider = provider.clone();
                async move {
                    provider
                        .call(
                            TransactionRequest::default()
                                .to(to)
                                .input(AlloyBytes::from(data).into()),
                        )
                        .await
                }
            };

            let slot = provider
                .get_storage_at(front, U256::from_be_bytes(IMPLEMENTATION_SLOT.0))
                .await
                .unwrap();
            let implementation = bytes(Address::from_word(slot.into()));
            assert!(deployment
                .implementations
                .contains(&implementation));
            let answer = call(front, CELL_CODEHASHCall {}.abi_encode())
                .await
                .unwrap();
            assert_eq!(CELL_CODEHASHCall::abi_decode_returns(&answer).unwrap(), cell_codehash);
            let answer = call(front, deskCellOfCall { desk: 1 }.abi_encode())
                .await
                .unwrap();
            assert_eq!(
                deskCellOfCall::abi_decode_returns(&answer).unwrap(),
                SLOT_RULE.account(front, cell_codehash, 1)
            );
            let answer = call(front, clockCall {}.abi_encode())
                .await
                .unwrap();
            assert_eq!(
                clockCall::abi_decode_returns(&answer)
                    .unwrap()
                    .clockMode,
                U256::from(3)
            );
            let precompile = call(address!("0000000000000000000000000000000000000070"), Vec::new())
                .await
                .unwrap();
            assert!(precompile.is_empty(), "the millisecond precompile answers");
            if provider
                .get_code_at(factory)
                .await
                .unwrap()
                .is_empty()
            {
                println!("the listing-door factory has no code yet");
            } else {
                let answer = call(factory, FRONTCall {}.abi_encode())
                    .await
                    .unwrap();
                assert_eq!(FRONTCall::abi_decode_returns(&answer).unwrap(), front);
            }
        }
    }
}
