//! Reads the factories' pools and quotes their pairs over JSON-RPC.
//!
//! Anyone can create a pool, with a price feed and a hook of their own, so a factory's word that it
//! created a pool admits nothing: a pool is served where its hook and its feed are on the lists the
//! feed was built with, or where the feed was told to serve it by name. A pool a front prices is
//! never served, whatever the lists say: one whose hook its factory lists as a front, or whose
//! cheap lane is on. A listed class of hook states its shape, which decides how the pool's answers
//! are settled into ladders. A hook of no listed class, reached by name, is quoted by steps where
//! its `params()` show no leverage row, and not at all where they show one, as no listed shape
//! vouches for it.
//!
//! Discovery runs beside the polls, so its cost is never a poll's. It pages each factory's pools
//! out of the factory's storage, as `pools()` is one call whose gas grows with the array, and
//! reads a pool's views (its tokens, trade mask, hooks, lane, price feed and lenders) once per
//! discovery interval, as they only decide what to quote and at what sizes, and its hook's row
//! (`params()`) once for good, as a hook's row is fixed when it is deployed. What it cannot read it
//! keeps from the round before, and nothing it reads ends the feed.
//!
//! A pool's quotes are read on every new block, all of one poll at that block, by one `eth_call`
//! per pool of the sampler (`assets/FlammSampler.sol`), which is never deployed: the node runs it
//! by state override at Tycho's router. A hook may price by its caller, so every call the sampler
//! makes to the pool and its hook comes from the router, the caller they see when it swaps, and
//! nothing goes through Multicall3. In each direction it walks the pool's `previewSwap` at the
//! planned inputs. Where an answer carries one of the hook's leverage labels it also reads the
//! pool's request for that answer (`pairRequest`), the hook's curve fee (`quotePair`) where every
//! answer carries that one label, and on a buy the hook's top bid (`levState`), then asks once
//! more just above the highest priced answer. A read that fails leaves its words zero, which cuts
//! the ladder before its leverage answers. Each of the sampler's calls to the pool or its hook has
//! a cap of its own: a pool whose answers run short of it gets twice as much on the next poll, up
//! to [`MAX_PROBE_GAS`], and the default again once its answers fit. A node that gives the whole
//! call less gas gets a shorter walk, not a failed one. A pool whose state moved since its views
//! were read refuses the quotes it no longer fills, so a stale view costs a book, never a wrong
//! price.
//!
//! The sampler never reverts and tags every answer, so a failed call says whose fault it is:
//! - Not run: a revert, which means the router's own code ran and the node dropped the override; a
//!   method, its parameters, the override, the gas or the request's size refused; an answer that is
//!   not the sampler's; or no probe at the default cap, as the node gave the call too little gas.
//!   No pool is blamed, and a poll where most pools were not run fails: this feed needs a node that
//!   runs a call with a state override, and falls back to nothing else.
//! - Skipped: a rate limit or a null answer, an answer from a block before the poll's, a block the
//!   node does not have, or no turn before the poll's deadline. The pool leaves that snapshot.
//! - Missed: anything else, a call cut off by the poll's deadline included.
//! - The pool's own: an answer that cannot be a fill where a direction's first answer the pool did
//!   not refuse should be. The pool is set aside.
//!
//! A revert, an answer that is not the sampler's, a missing block and a miss are asked again once,
//! within the poll's deadline. A pool whose first answer in some direction ran short of its cap
//! leaves that snapshot, with no strike, until the raised cap answers; at the most cap it misses.
//!
//! A pool that reverts a view, or answers what cannot be a fill, is set aside for a while and the
//! other pools are served. So is one that misses two polls in a row while other pools answer: its
//! first miss only drops it from that snapshot. Whose fault an error that does not say the call
//! reverted is, a sampler call that runs out of gas included, shows in how the other pools fared.
//! Each strike lengthens a pool's time aside, until its quotes come back. A poll fails only where
//! most pools miss it, or where none answers and some failed at the node, which keeps the last
//! books through a moment of trouble there.
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    fmt,
    future::IntoFuture,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{keccak256, Address, Bytes as AlloyBytes, B256, U256},
    providers::{Provider, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionInput, TransactionRequest,
    },
    sol,
    sol_types::SolCall,
    transports::{RpcError, TransportErrorKind, TransportResult},
};
use futures::future::join_all;
use tokio::{
    sync::{watch, Semaphore, SemaphorePermit},
    time::{sleep, timeout, timeout_at, Instant as TokioInstant},
};
use tracing::{debug, warn};

use crate::{
    pamm::protocols::flamm::{
        feed::MAX_PROBE_GAS,
        gas::{hook_class, GasKey, GasTable, Minimum, Venue, Venues},
        guard::HeldRepay,
        ladder::{
            rung_inputs, Probe, ProbeStatus, Request, Row, Rule, Side, Terms, Trust, ANSWER_BITS,
        },
    },
    snapshot_feed::errors::FeedError,
};

sol! {
    struct HookSet { address hook; address controller; }
    struct TokenConfig {
        address token; uint8 decimals; uint64 swapPriceBandWad; uint64 feeFloorWad;
        uint256 maxSwapNotional; uint256 reserveTarget; uint256 minSupplyBatch; uint256 held;
        uint8 partner; uint8 pairId; bool debtFree;
    }
    struct VenueView {
        address account; bytes32 id; uint8 kind; uint8 pair; uint8 coll; uint8 debt;
        uint64 lltvWad; bool borrowEnabled; bool supplyEnabled; uint8 status;
        uint128 debtCap; uint128 supplyCap; uint64 maxBorrowRateWad;
    }
    // the factory
    function isPool(address pool) external view returns (bool);
    function isListedFront(address front) external view returns (bool);
    // the pool
    function tokenCount() external view returns (uint256);
    function tokenConfig(uint8 t) external view returns (TokenConfig memory);
    function tradablePairs() external view returns (
        uint256 tradable, uint256 hookPairs, uint256 operatorPairs, uint256 pendingPairs,
        uint48 executableAt
    );
    function hooks() external view returns (HookSet memory);
    function lane() external view returns (uint256 laneWord, uint256 laneConfig, uint256 pending);
    function priceFeed() external view returns (address);
    function router() external view returns (address);
    function totalAssets() external view returns (uint256);
    function position(uint8 t) external view returns (
        uint256 held, uint256 supplied, uint256 posted, uint256 owed
    );
    function previewSwap(address tokenIn, address tokenOut, uint256 amountIn)
        external view returns (uint256 amountInUsed, uint256 amountOut, uint256 feeWad);
    function pairRequest(address tokenIn, address tokenOut, uint256 amountIn)
        external view returns (uint256[26] memory);
    // the pool's hook
    function params() external view returns (uint256[35] memory);
    // the pool's lending router
    function venueCount(address pool) external view returns (uint256);
    function venue(address pool, uint16 id) external view returns (VenueView memory);
    function venueActions(address pool, uint16 id) external view returns (uint8);
    // a venue's account
    function capabilities() external view returns (uint256);
    function minimumDebt(bytes32 id) external view returns (bool ok, uint256 floor);
    // the pool's price feed
    function peekUsd(address token) external view returns (bool ok, uint256 usdWad, uint48 observedAt);
}

sol! {
    /// `assets/FlammSampler.sol`: it walks a pool's `previewSwap` in each direction asked and
    /// reads the request words the rule needs, in one call that never reverts.
    #[sol(all_derives)]
    interface FlammSampler {
        struct Ask {
            address tokenIn; address tokenOut; address hook; uint256 tierA; uint256 tierB;
            uint256 maxProbes; uint256[] seeds;
        }
        struct Limits {
            uint256 probeGas; uint256 reserveGas; uint256 flipShift; uint256 edgeProbes;
            uint256 gapPpm;
        }
        struct Probe {
            uint256 ask; uint256 used; uint256 out; uint256 feeWad; uint256 gasUsed;
            uint256 status; bytes4 reason;
        }
        struct Walk {
            Probe[] probes; uint256 curveFeeWad; uint64 bandWad; uint256 usdIn; uint256 usdOut;
            uint256[8] legs; uint256 bidWad; bool truncated;
        }
        function TAG() external view returns (bytes4);
        function sample(address pool, Ask[] calldata asks, Limits calldata limits)
            external view returns (bytes4 tag, uint256 blockNumber, Walk[] memory walks);
    }
}

/// The sampler's runtime code, raw bytes that `assets/build_sampler.py` builds. It is never
/// deployed: a node runs it by state override at the taker, so that every call it makes comes
/// from the address the pool and its hook see when the router swaps.
pub(super) const SAMPLER_RUNTIME: &[u8] = include_bytes!("assets/FlammSampler.evm.runtime");
/// The first word of every sampler answer, so a caller can tell the code it sent is what ran.
pub(super) const SAMPLER_TAG: [u8; 4] = [0xed, 0x7b, 0x4a, 0xc9];
/// The gas the sampler keeps back to encode its answer: where less is left, it stops walking.
pub(super) const SAMPLER_RESERVE_GAS: u64 = 300_000;
/// A change of label is bisected while its two asks are more than `lo >> 7` apart.
pub(super) const SAMPLER_FLIP_SHIFT: u64 = 7;
/// The sampler's bisection towards the first refusal, and its splits of a span: both off here.
pub(super) const SAMPLER_EDGE_PROBES: u64 = 0;
pub(super) const SAMPLER_GAP_PPM: u64 = 0;
/// Wordings of a rate limit, whatever the code of the error that carries them.
const RATE_WORDS: [&str; 7] = [
    "rate limit",
    "rate exceeded",
    "too many requests",
    "credits limited",
    "request limit",
    "limit reached",
    "maximum number of concurrent requests",
];
/// Codes of a rate limit.
const RATE_CODES: [i64; 6] = [429, -32005, -32007, -32012, -32016, -32055];
/// Codes of a method the node does not have, parameters it refuses, and a request too large.
const UNRUN_CODES: [i64; 3] = [-32601, -32602, -32604];
/// The code of a request the node calls invalid: one it would not run, unless it words a limit.
const INVALID_REQUEST_CODE: i64 = -32600;
/// Wordings of a block the node does not have.
const NO_BLOCK_WORDS: [&str; 4] =
    ["header not found", "unknown block", "block not found", "missing trie"];

/// The most tokens a pool's trade mask can name: 16 bits a token.
const MAX_TOKENS: usize = 16;
/// A token of the hook pair, as a venue record names it.
fn hook_pair_index(token: usize) -> u8 {
    u8::try_from(token).unwrap_or(u8::MAX)
}

/// The directions served of any pool: its hook pair, tokens 0 and 1. Every pair of a pool draws on
/// one inventory, so a book per pair would promise it once for each, and no book moves another.
const HOOK_PAIR_DIRECTIONS: [(usize, usize); 2] = [(0, 1), (1, 0)];
/// The most venue records read per pool: a pool with more is not served, as the charge of its
/// swaps cannot be read.
const MAX_VENUES: usize = 32;
/// `CAN_REPAY` of `MMRouter.venueActions`: the venue lets the pool repay now.
const CAN_REPAY: u8 = 2;
/// `CAP_HELD_REPAY` of an account's `capabilities()`: it holds a partial repay at a minimum
/// position, which `minimumDebt(id)` gives.
const CAP_HELD_REPAY: u64 = 512;
/// The cheap lane's version, bits 97-103 of `lane().laneConfig`: zero while the lane is off.
const LANE_VERSION_SHIFT: usize = 97;
const LANE_VERSION_MASK: u64 = 0x7f;
/// How long a pool that keeps failing is set aside at most, in discovery intervals.
const MAX_QUARANTINE_INTERVALS: u32 = 16;
/// How long a pool with nothing to serve goes unread, in discovery intervals: what keeps it out
/// (its hook, its feed, its size) rarely changes, and anyone can create as many as they like.
const UNSERVED_REST_INTERVALS: u32 = 10;
/// The slot of a factory's `_pools`: it holds the array's length, and entry `i` is at
/// `keccak256(slot) + i`. A factory is never upgraded, so its layout is fixed, and every entry is
/// checked with `isPool` before it counts.
const POOLS_SLOT: u64 = 1;
/// The code of an error answer that says the called code reverted.
const REVERT_CODE: i64 = 3;
/// The hook's `quotePair(PairRequest)` and `levState(PairRequest)`, which the sampler sends as raw
/// data, a request being the 26 static words `pairRequest` answers with.
#[cfg(test)]
pub(super) const QUOTE_PAIR_SELECTOR: [u8; 4] = [0x2f, 0x4d, 0xe4, 0x91];
#[cfg(test)]
pub(super) const LEV_STATE_SELECTOR: [u8; 4] = [0xd3, 0x8f, 0x9b, 0xd9];
/// Words of `params()`: the row's spreads, caps and band.
const PARAMS_SPREAD: usize = 27;
const PARAMS_SPREAD_DN: usize = 28;
const PARAMS_UP_CAP: usize = 29;
const PARAMS_DN_CAP: usize = 30;
const PARAMS_BAND: usize = 32;

/// How a listed class of hook prices, which decides how its pools' answers are settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookShape {
    /// An Efficient pools hook: a curve, and leverage routes where its row allows them, each
    /// answer labelled with the fee of the route that priced it.
    Everlong,
}

/// What admits a pool of a factory: a hook of a listed class over a listed price feed. A front is
/// never admitted, whatever its class.
#[derive(Clone, Debug, Default)]
pub(super) struct Admission {
    /// The classes ([`hook_class`]) of the hooks that may price a pool, each with its shape.
    pub hook_classes: HashMap<B256, HookShape>,
    pub price_feeds: HashSet<Address>,
    /// The hashes of the runtime code of feeds admitted wherever they are deployed.
    pub price_feed_codes: HashSet<B256>,
}

/// What the reader is configured with.
#[derive(Clone, Debug)]
pub(super) struct ReaderSettings {
    /// The caller every quote is asked for.
    pub taker: Address,
    pub factories: Vec<Address>,
    /// Pools served whatever their hook and feed, where the feed is limited to named pools; `None`
    /// serves the pools of the factories that [`Self::admission`] admits.
    pub allowed_pools: Option<HashSet<Address>>,
    pub admission: Admission,
    /// The gas cap of every view.
    pub call_gas: u64,
    /// How long a view may take. The client sets no deadline, and discovery has no poll's.
    pub call_timeout: Duration,
    /// The gas of each pool's sampler call.
    pub sample_gas: u64,
    /// The cap on each of the sampler's calls to a pool or its hook, until a pool's answers run
    /// short of it.
    pub probe_gas: u64,
    /// The probes a direction may take, then one more just above a tier-labelled top.
    pub max_probes_per_direction: usize,
    /// The smallest rung, in USD, of an input a price is known for.
    pub smallest_rung_usd: f64,
    pub rung_count: usize,
    pub discovery_interval: Duration,
    /// The most new entries of one factory's array a round of discovery reads.
    pub discovery_page: usize,
    /// A pool worth less than this, in USD, is not quoted: its NAV where its pool token has a
    /// price, else what its legs in the priced tokens are worth.
    pub min_nav_usd: f64,
    /// The tokens a pair may be served in; a direction outside them is not quoted.
    pub universe: HashSet<Address>,
    /// Tokens worth one USD where a pool's feed does not price them.
    pub stablecoins: HashSet<Address>,
    /// The chain's id, and the gas table its swaps are charged by: a direction it cannot charge
    /// is not quoted.
    pub chain_id: u64,
    pub gas: GasTable,
}

/// One token of a pool as discovered.
#[derive(Clone, Debug)]
pub(super) struct TokenInfo {
    pub address: Address,
    pub decimals: u8,
    /// Its USD price, from the pool's feed or as a stablecoin; `None` where neither prices it.
    pub usd: Option<f64>,
}

/// One direction a pool is quoted in.
#[derive(Clone, Debug)]
pub(super) struct DirectionPlan {
    pub token_in: usize,
    pub token_out: usize,
    pub asked: Vec<U256>,
    /// What a swap this way is charged by.
    pub gas: GasKey,
    /// The minimum position of the live venue that lends the output, where one holds a minimum.
    pub guard: Option<HeldRepay>,
}

/// One pool as discovered: what to quote it in, and how its answers are settled.
#[derive(Clone, Debug)]
pub(super) struct PoolInfo {
    pub pool: Address,
    /// The swap hook that prices the hook pair.
    pub hook: Address,
    pub rule: Rule,
    /// The hook's row, all zero where it has none.
    pub row: Row,
    /// The row's verdict at the band token 1 had when the pool was read.
    pub trust: Trust,
    pub tokens: Vec<TokenInfo>,
    pub directions: Vec<DirectionPlan>,
}

/// One direction as the sampler walked it: its probes in increasing ask, the words the rule reads,
/// zero where not read, and whether the sampler's gas gate ended the walk early.
#[derive(Clone, Debug)]
pub(super) struct DirectionRead {
    pub plan: DirectionPlan,
    pub probes: Vec<Probe>,
    pub curve_fee: U256,
    pub band_wad: U256,
    pub usd_in: U256,
    pub usd_out: U256,
    /// The input's held, supplied, posted and owed legs, then the output's.
    pub legs: [U256; 8],
    pub bid_wad: U256,
    pub truncated: bool,
}

impl DirectionRead {
    /// What the rule knows of this direction of `info`'s pool besides its probes.
    pub(super) fn terms(&self, info: &PoolInfo) -> Terms {
        Terms {
            rule: info.rule,
            tiers: info.row.tiers(),
            row_band: info.row.band_wad,
            side: if self.plan.token_in == 0 { Side::Sale } else { Side::Buy },
            decimals_in: info.tokens[self.plan.token_in].decimals,
            decimals_out: info.tokens[self.plan.token_out].decimals,
            trust: info.trust,
            curve_fee: self.curve_fee,
            request: Request {
                band_wad: self.band_wad,
                usd_in: self.usd_in,
                usd_out: self.usd_out,
                legs: self.legs,
                bid_wad: self.bid_wad,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct PoolRead {
    pub info: Arc<PoolInfo>,
    pub directions: Vec<DirectionRead>,
}

#[derive(Clone, Debug)]
pub(super) struct BookRead {
    pub block: u64,
    pub pools: Vec<PoolRead>,
    /// Whether a pool was skipped, not run or missed: the books then stand for this poll, not for
    /// the block, as the next poll at that block may read those pools.
    pub partial: bool,
}

/// Why a call failed: the code it ran refused, by reverting or by answering what does not decode,
/// or the call got no answer from that code. Running out of gas is the latter, as is every error
/// that does not say the call reverted, whatever it says instead.
enum CallFailure {
    Refused(String),
    Unanswered(FeedError),
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallFailure::Refused(reason) => f.write_str(reason),
            CallFailure::Unanswered(error) => error.fmt(f),
        }
    }
}

impl From<RpcError<TransportErrorKind>> for CallFailure {
    fn from(error: RpcError<TransportErrorKind>) -> Self {
        match error {
            // a revert has a code of its own, and a node may answer one with its data instead
            RpcError::ErrorResp(payload)
                if payload.code == REVERT_CODE || payload.as_revert_data().is_some() =>
            {
                CallFailure::Refused(payload.message.to_string())
            }
            error @ (RpcError::ErrorResp(_) |
            RpcError::NullResp |
            RpcError::UnsupportedFeature(_) |
            RpcError::LocalUsageError(_) |
            RpcError::SerError(_) |
            RpcError::DeserError { .. } |
            RpcError::Transport(_)) => {
                CallFailure::Unanswered(FeedError::Connection(error.to_string()))
            }
        }
    }
}

/// Why a pool has no read: something about the pool, which sets it aside; a call left unanswered,
/// which counts against the pool only where other pools answered; a sampler call the node did not
/// run, which counts against no pool and fails a poll where most pools have it; or no read this
/// time, for want of a turn or of the node's leave, which counts against nobody.
enum PoolFailure {
    Pool(String),
    Unanswered(FeedError),
    NotRun(String),
    Skipped(String),
}

impl From<CallFailure> for PoolFailure {
    fn from(failure: CallFailure) -> Self {
        match failure {
            CallFailure::Refused(reason) => PoolFailure::Pool(reason),
            CallFailure::Unanswered(error) => PoolFailure::Unanswered(error),
        }
    }
}

/// What a node's error to a sampler call says. The sampler never reverts, so no error is the
/// pool's: it says only whether the node ran the call, and whether to ask again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeClass {
    /// A revert: the taker's own code ran, so the node dropped the override, as one backend of
    /// several may. Not run, and asked again once.
    Reverted,
    /// A method, its parameters, the override, the gas or the request's size refused: not run.
    Refused,
    /// A rate limit, or a null answer: skipped.
    Limited,
    /// The block asked for, missing at the node: skipped, after asking again once.
    NoBlock,
    /// Anything else, a call that runs out of gas included: a miss, after asking again once.
    Missed,
}

/// The class of `error`, by the first of these its form or wording matches: a revert; a refusal;
/// a rate limit; a missing block. Some providers send a revert under a generic code, worded
/// (`crates/tycho-ethereum/src/rpc/errors.rs:104-105`), so the message counts, lower-cased.
fn node_class(error: &RpcError<TransportErrorKind>) -> NodeClass {
    let said = error.to_string().to_lowercase();
    let says = |words: &[&str]| {
        words
            .iter()
            .any(|word| said.contains(word))
    };
    let (code, revert_data, status, null) = match error {
        RpcError::ErrorResp(payload) => {
            (Some(payload.code), payload.as_revert_data().is_some(), None, false)
        }
        RpcError::Transport(kind) => (None, false, kind.as_http_error().map(|e| e.status), false),
        RpcError::NullResp => (None, false, None, true),
        RpcError::DeserError { text, .. } => (None, false, None, text.trim() == "null"),
        RpcError::UnsupportedFeature(_) | RpcError::LocalUsageError(_) | RpcError::SerError(_) => {
            (None, false, None, false)
        }
    };
    let rate_worded = says(&RATE_WORDS);
    let gas_refused = said.contains("gas") &&
        says(&["exceed", "limit", "allowance"]) &&
        !said.contains("out of gas");
    if code == Some(REVERT_CODE) || revert_data || said.contains("revert") {
        NodeClass::Reverted
    } else if code.is_some_and(|code| UNRUN_CODES.contains(&code)) ||
        (code == Some(INVALID_REQUEST_CODE) && !rate_worded) ||
        said.contains("override") ||
        gas_refused ||
        status == Some(413)
    {
        NodeClass::Refused
    } else if code.is_some_and(|code| RATE_CODES.contains(&code)) ||
        matches!(status, Some(429 | 503)) ||
        rate_worded ||
        null
    {
        NodeClass::Limited
    } else if says(&NO_BLOCK_WORDS) {
        NodeClass::NoBlock
    } else {
        NodeClass::Missed
    }
}

/// A sampler call that gave no walks: what that means for the pool, and whether one more call may
/// answer otherwise.
struct Failed {
    failure: PoolFailure,
    again: bool,
}

impl Failed {
    fn not_run(reason: impl Into<String>, again: bool) -> Self {
        Self { failure: PoolFailure::NotRun(reason.into()), again }
    }

    fn of(error: &RpcError<TransportErrorKind>) -> Self {
        let reason = error.to_string();
        match node_class(error) {
            NodeClass::Reverted => Self::not_run(reason, true),
            NodeClass::Refused => Self::not_run(reason, false),
            NodeClass::Limited => Self { failure: PoolFailure::Skipped(reason), again: false },
            NodeClass::NoBlock => Self { failure: PoolFailure::Skipped(reason), again: true },
            NodeClass::Missed => Self {
                failure: PoolFailure::Unanswered(FeedError::Connection(reason)),
                again: true,
            },
        }
    }
}

/// One sampler answer: each planned direction's walk, and the most gas any of its probes spent.
struct Walked {
    directions: Vec<DirectionRead>,
    most_spent: U256,
}

impl Walked {
    /// Whether no walk holds a probe and the gas gate ended one: the call's gas left none for a
    /// probe at the cap it was asked at.
    fn gas_short(&self) -> bool {
        self.directions
            .iter()
            .all(|d| d.probes.is_empty()) &&
            self.directions
                .iter()
                .any(|d| d.truncated)
    }
}

/// One pool's turn at a poll: whether it was given a permit before the deadline, and its read.
struct Polled {
    pool: Address,
    reached: bool,
    read: Result<PoolRead, PoolFailure>,
}

/// The rule's probe of one of the sampler's, `None` for a status the sampler never gives. A
/// refusal and a probe short of gas are as the sampler says; the rest are classed again by the
/// rule, which tells them apart by the same tests.
fn probe_of(probe: &FlammSampler::Probe) -> Option<Probe> {
    // priced, clipped, refused, short of gas, malformed
    match u64::try_from(probe.status).ok()? {
        1 | 2 | 5 => Some(Probe::classify(probe.ask, Some((probe.used, probe.out, probe.feeWad)))),
        3 => Some(Probe::classify(probe.ask, None)),
        4 => Some(Probe {
            ask: probe.ask,
            used: U256::ZERO,
            out: U256::ZERO,
            fee_wad: U256::ZERO,
            status: ProbeStatus::Starved,
        }),
        _ => None,
    }
}

/// The sampler's settings for this feed, with each of its calls capped at `probe_gas`: flips on,
/// its steps towards the first refusal and its splits of a span off.
pub(super) fn sampler_limits(probe_gas: u64) -> FlammSampler::Limits {
    FlammSampler::Limits {
        probeGas: U256::from(probe_gas),
        reserveGas: U256::from(SAMPLER_RESERVE_GAS),
        flipShift: U256::from(SAMPLER_FLIP_SHIFT),
        edgeProbes: U256::from(SAMPLER_EDGE_PROBES),
        gapPpm: U256::from(SAMPLER_GAP_PPM),
    }
}

/// The error of a poll the node did not run the sampler for.
fn not_run_error(reason: &str) -> FeedError {
    FeedError::Connection(format!("the node did not run the sampler: {reason}"))
}

/// `amount` base units of a token of `decimals` at `price` USD.
fn usd(amount: U256, decimals: u8, price: f64) -> f64 {
    u128::try_from(amount).unwrap_or(u128::MAX) as f64 / 10f64.powi(i32::from(decimals)) * price
}

/// What the feed keeps of a contract's code.
#[derive(Clone, Copy, Debug)]
struct CodeMarks {
    /// Its [`hook_class`].
    class: B256,
    hash: B256,
}

/// What discovery keeps between rounds, for the one task that runs them.
#[derive(Debug, Default)]
pub(super) struct Catalogue {
    /// Each factory's pools found so far.
    pools: HashMap<Address, Vec<Address>>,
    /// Every served pool as last read.
    served: HashMap<Address, Arc<PoolInfo>>,
    /// Pools with nothing to serve, and when they are next read.
    resting: HashMap<Address, Instant>,
}

/// What discovery has to show the polls.
#[derive(Clone, Debug)]
enum Discovered {
    /// The first round has not ended.
    Pending,
    /// No round has found a pool to serve and the last one failed, for this reason.
    Failed(String),
    Ready(Arc<Vec<Arc<PoolInfo>>>),
}

#[derive(Clone, Copy, Debug)]
struct Strikes {
    count: u32,
    until: Instant,
}

#[derive(derive_more::Debug)]
pub(super) struct FlammReader {
    #[debug(skip)]
    provider: RootProvider,
    settings: ReaderSettings,
    /// The polls' calls in flight.
    #[debug(skip)]
    permits: Semaphore,
    /// Discovery's, apart from the polls': a waiter queues behind every call asked before it
    /// (tokio 1.47.1 `src/sync/semaphore.rs:19-20`), so a shared queue would hold a round behind
    /// each poll's whole list of pools.
    #[debug(skip)]
    discovery_permits: Semaphore,
    #[debug(skip)]
    discovered: watch::Sender<Discovered>,
    #[debug(skip)]
    quarantine: Mutex<HashMap<Address, Strikes>>,
    /// The pools that missed the last poll other pools answered.
    #[debug(skip)]
    last_missed: Mutex<HashSet<Address>>,
    /// Kept for good: since EIP-6780 the code at an address does not change once it is there.
    #[debug(skip)]
    codes: Mutex<HashMap<Address, CodeMarks>>,
    /// Each hook's row as `params()` decoded, kept for good: a listed hook's row is fixed when it
    /// is deployed. Only a decoded answer is kept, so one refusal never decides a hook's rule.
    #[debug(skip)]
    rows: Mutex<HashMap<Address, Row>>,
    /// The caps raised above the default, by pool: a pool whose answers ran short of gas has its
    /// cap doubled for the next poll, up to the most, and the default again once every probe of an
    /// answer fits well under the default. Kept here, as discovery replaces a pool's read.
    #[debug(skip)]
    probe_caps: Mutex<HashMap<Address, u64>>,
    /// Where the next poll starts in the list of pools: past the last poll's answers, so that no
    /// pool is always the last asked.
    #[debug(skip)]
    poll_offset: AtomicUsize,
}

/// The bit of the swap of token `i` for token `j` in `tradablePairs().tradable`.
pub(super) fn trade_bit(i: usize, j: usize) -> U256 {
    U256::from(1) << (16 * i + j)
}

/// The slot of entry `index` of a factory's `_pools`.
fn pool_entry_slot(index: usize) -> U256 {
    let first = keccak256(U256::from(POOLS_SLOT).to_be_bytes::<32>());
    U256::from_be_bytes(first.0) + U256::from(index)
}

/// One of `semaphore`'s permits, when one is free.
async fn permit(semaphore: &Semaphore) -> Result<SemaphorePermit<'_>, CallFailure> {
    semaphore
        .acquire()
        .await
        .map_err(|closed| CallFailure::Unanswered(FeedError::Connection(closed.to_string())))
}

/// The smallest input a direction is quoted at: `usd` worth of the token where its price is
/// known, else a thousandth of a whole token.
pub(super) fn smallest_rung(decimals: u8, usd_price: Option<f64>, usd: f64) -> U256 {
    let unit = 10f64.powi(i32::from(decimals));
    let units = match usd_price {
        Some(price) if price > 0.0 && price.is_finite() => usd / price * unit,
        _ => unit / 1_000.0,
    };
    // a size the price makes absurd is clamped; the ladder still starts at one base unit
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to [1, 1e36], which a u128 holds"
    )]
    let units = units.clamp(1.0, 1e36) as u128;
    U256::from(units)
}

impl FlammReader {
    /// A reader with `max_in_flight` calls at once: a quarter of them, at least one, for
    /// discovery, and the rest, at least one, for the polls.
    pub(super) fn new(
        provider: RootProvider,
        settings: ReaderSettings,
        max_in_flight: usize,
    ) -> Self {
        let discovery = (max_in_flight / 4).max(1);
        Self {
            provider,
            settings,
            permits: Semaphore::new(
                max_in_flight
                    .saturating_sub(discovery)
                    .max(1),
            ),
            discovery_permits: Semaphore::new(discovery),
            discovered: watch::Sender::new(Discovered::Pending),
            quarantine: Mutex::new(HashMap::new()),
            last_missed: Mutex::new(HashSet::new()),
            codes: Mutex::new(HashMap::new()),
            rows: Mutex::new(HashMap::new()),
            probe_caps: Mutex::new(HashMap::new()),
            poll_offset: AtomicUsize::new(0),
        }
    }

    pub(super) async fn chain_id(&self) -> Result<u64, FeedError> {
        self.provider
            .get_chain_id()
            .await
            .map_err(|e| FeedError::Connection(format!("eth_chainId: {e}")))
    }

    /// The number and timestamp of the node's latest block.
    pub(super) async fn head(&self) -> Result<(u64, u64), FeedError> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|e| FeedError::Connection(format!("latest block: {e}")))?
            .ok_or_else(|| FeedError::Connection("the node has no latest block".into()))?;
        Ok((block.header.number, block.header.timestamp))
    }

    /// The node's answer to `request`, asked again if it comes back unanswered, and unanswered
    /// where neither attempt ends in time.
    async fn answer<T, R>(&self, request: impl Fn() -> R) -> Result<T, CallFailure>
    where
        R: IntoFuture<Output = TransportResult<T>>,
    {
        let attempts = async {
            match request()
                .await
                .map_err(CallFailure::from)
            {
                Err(CallFailure::Unanswered(_)) => request()
                    .await
                    .map_err(CallFailure::from),
                answer => answer,
            }
        };
        let deadline = self.settings.call_timeout;
        timeout(deadline, attempts)
            .await
            .unwrap_or_else(|_| {
                let overrun = format!("no answer in {deadline:?}");
                Err(CallFailure::Unanswered(FeedError::Connection(overrun)))
            })
    }

    /// A call in flight for discovery, of its own share: never one of the polls'.
    async fn discovery_permit(&self) -> Result<SemaphorePermit<'_>, CallFailure> {
        permit(&self.discovery_permits).await
    }

    async fn call(
        &self,
        to: Address,
        data: Vec<u8>,
        block: BlockId,
    ) -> Result<AlloyBytes, CallFailure> {
        let tx = TransactionRequest::default()
            .to(to)
            .gas_limit(self.settings.call_gas)
            .input(TransactionInput::new(data.into()));
        self.answer(|| {
            self.provider
                .call(tx.clone())
                .block(block)
        })
        .await
    }

    /// A view, as discovery reads them: at the node's latest block, as what it decides needs no
    /// one block, and the quotes it leads to are checked at theirs.
    async fn view<C: SolCall>(&self, to: Address, call: C) -> Result<C::Return, CallFailure> {
        let _permit = self.discovery_permit().await?;
        let raw = self
            .call(to, call.abi_encode(), BlockId::latest())
            .await?;
        C::abi_decode_returns(&raw).map_err(|e| {
            CallFailure::Refused(format!("{} at {to} decodes to nothing: {e}", C::SIGNATURE))
        })
    }

    /// The marks of the code at `address`; `None` where it holds none.
    async fn code_marks(&self, address: Address) -> Result<Option<CodeMarks>, CallFailure> {
        if let Some(marks) = self
            .codes
            .lock()
            .expect("codes lock")
            .get(&address)
        {
            return Ok(Some(*marks));
        }
        let _permit = self.discovery_permit().await?;
        let code = self
            .answer(|| self.provider.get_code_at(address))
            .await?;
        if code.is_empty() {
            return Ok(None);
        }
        let marks = CodeMarks { class: hook_class(&code), hash: keccak256(&code) };
        self.codes
            .lock()
            .expect("codes lock")
            .insert(address, marks);
        Ok(Some(marks))
    }

    /// Whether `feed` is on the lists, by its address or by the hash of its code. No address
    /// without code is, so neither is a pool with no feed.
    async fn feed_listed(&self, feed: Address) -> Result<bool, CallFailure> {
        let admission = &self.settings.admission;
        if admission.price_feeds.contains(&feed) {
            return Ok(true);
        }
        if admission.price_feed_codes.is_empty() {
            return Ok(false);
        }
        Ok(self
            .code_marks(feed)
            .await?
            .is_some_and(|marks| {
                admission
                    .price_feed_codes
                    .contains(&marks.hash)
            }))
    }

    /// Whether `factory` lists `hook` as a front. A factory that will not say lists none.
    async fn front_listed(&self, factory: Address, hook: Address) -> Result<bool, CallFailure> {
        match self
            .view(factory, isListedFrontCall { front: hook })
            .await
        {
            Ok(listed) => Ok(listed),
            Err(CallFailure::Refused(reason)) => {
                debug!(%factory, %hook, reason, "factory does not say whether it lists a front");
                Ok(false)
            }
            Err(failure @ CallFailure::Unanswered(_)) => Err(failure),
        }
    }

    /// The shape of `hook`'s class where the class is listed; `None` for a hook of no listed
    /// class or with no code.
    async fn shape_of(&self, hook: Address) -> Result<Option<HookShape>, CallFailure> {
        Ok(self
            .code_marks(hook)
            .await?
            .and_then(|marks| {
                self.settings
                    .admission
                    .hook_classes
                    .get(&marks.class)
                    .copied()
            }))
    }

    /// The row `hook`'s `params()` decodes to, asked once more where it is refused.
    async fn row(&self, hook: Address) -> Result<Row, CallFailure> {
        if let Some(row) = self
            .rows
            .lock()
            .expect("rows lock")
            .get(&hook)
        {
            return Ok(*row);
        }
        let words = match self.view(hook, paramsCall {}).await {
            Err(CallFailure::Refused(_)) => self.view(hook, paramsCall {}).await,
            answer => answer,
        }?;
        let row = Row {
            spread_wad: words[PARAMS_SPREAD],
            spread_dn_wad: words[PARAMS_SPREAD_DN],
            up_cap_wad: words[PARAMS_UP_CAP],
            dn_cap_wad: words[PARAMS_DN_CAP],
            band_wad: words[PARAMS_BAND],
        };
        self.rows
            .lock()
            .expect("rows lock")
            .insert(hook, row);
        Ok(row)
    }

    /// How a pool priced by `hook` is settled, and the hook's row. A listed shape settles by the
    /// rule with the row `params()` reads; that code answers `params()`, so a refusal is the
    /// node's and the pool is unanswered. A hook of no listed class, reached by name, is settled
    /// by steps where it shows no leverage row, this round alone where `params()` is refused, and
    /// `None`, not served, where it shows one.
    async fn pricing(
        &self,
        pool: Address,
        hook: Address,
        shape: Option<HookShape>,
    ) -> Result<Option<(Rule, Row)>, PoolFailure> {
        match (shape, self.row(hook).await) {
            (Some(HookShape::Everlong), Ok(row)) => Ok(Some((Rule::Everlong, row))),
            (Some(HookShape::Everlong), Err(CallFailure::Refused(reason))) => {
                let refused = format!("params() of the listed hook {hook} refused: {reason}");
                Err(PoolFailure::Unanswered(FeedError::Connection(refused)))
            }
            (None, Ok(row)) if row.has_leverage() => {
                debug!(%pool, %hook, "named Efficient pool's hook has a leverage row of no listed shape, not served");
                Ok(None)
            }
            (None, Ok(row)) => Ok(Some((Rule::Step, row))),
            (None, Err(CallFailure::Refused(reason))) => {
                debug!(%pool, %hook, reason, "named Efficient pool's hook refuses params(), quoted by steps this round");
                Ok(Some((Rule::Step, Row::default())))
            }
            (Some(HookShape::Everlong) | None, Err(CallFailure::Unanswered(error))) => {
                Err(PoolFailure::Unanswered(error))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn strikes(&self, pool: &Address) -> u32 {
        self.quarantine
            .lock()
            .expect("quarantine lock")
            .get(pool)
            .map_or(0, |s| s.count)
    }

    /// The pools that missed the last poll other pools answered.
    #[cfg(test)]
    pub(super) fn missed_last(&self) -> HashSet<Address> {
        self.last_missed
            .lock()
            .expect("misses lock")
            .clone()
    }

    /// The most a pool's cap is raised to: [`MAX_PROBE_GAS`], or less where the call's gas leaves
    /// less once the sampler's reserve is kept and a call is passed its 63/64.
    fn max_cap(&self) -> u64 {
        let usable = self
            .settings
            .sample_gas
            .saturating_sub(SAMPLER_RESERVE_GAS)
            .saturating_mul(63) /
            64;
        MAX_PROBE_GAS.min(usable)
    }

    /// The cap a pool's probes run at until they run short of it.
    fn default_cap(&self) -> u64 {
        self.settings
            .probe_gas
            .min(self.max_cap())
    }

    /// The cap `pool`'s probes run at next.
    pub(super) fn probe_cap(&self, pool: &Address) -> u64 {
        let raised = self
            .probe_caps
            .lock()
            .expect("caps lock")
            .get(pool)
            .copied();
        raised.unwrap_or_else(|| self.default_cap())
    }

    fn set_cap(&self, pool: Address, cap: u64) {
        let mut caps = self
            .probe_caps
            .lock()
            .expect("caps lock");
        if cap == self.default_cap() {
            caps.remove(&pool);
        } else {
            caps.insert(pool, cap);
        }
    }

    pub(super) fn quarantined(&self, pool: &Address, now: Instant) -> bool {
        self.quarantine
            .lock()
            .expect("quarantine lock")
            .get(pool)
            .is_some_and(|s| s.until > now)
    }

    fn set_aside(&self, pool: Address, reason: &str) {
        let mut quarantine = self
            .quarantine
            .lock()
            .expect("quarantine lock");
        let count = quarantine
            .get(&pool)
            .map_or(1, |s| s.count + 1);
        let intervals = 1u32 << (count - 1).min(MAX_QUARANTINE_INTERVALS.ilog2());
        let until = Instant::now() + self.settings.discovery_interval * intervals;
        warn!(%pool, strikes = count, ?until, reason, "setting an Efficient pool aside");
        quarantine.insert(pool, Strikes { count, until });
    }

    /// Forgives `pool` its strikes. Only its quotes coming back do: views that read well say
    /// nothing of the quotes that struck it.
    fn cleared(&self, pool: &Address) {
        self.quarantine
            .lock()
            .expect("quarantine lock")
            .remove(pool);
    }

    /// The pools discovery last found. The first polls wait for its first round, within their
    /// own deadline.
    async fn discovered_pools(&self) -> Result<Arc<Vec<Arc<PoolInfo>>>, FeedError> {
        let mut discovered = self.discovered.subscribe();
        let discovered = discovered
            .wait_for(|state| !matches!(state, Discovered::Pending))
            .await
            .map_err(|closed| FeedError::Connection(format!("discovery ended: {closed}")))?;
        match &*discovered {
            Discovered::Ready(pools) => Ok(Arc::clone(pools)),
            Discovered::Failed(reason) => {
                Err(FeedError::Connection(format!("discovery failed: {reason}")))
            }
            Discovered::Pending => Err(FeedError::Connection("no discovery yet".into())),
        }
    }

    /// Every served pool's walks at `block`, by one sampler call each, all by `deadline`. The
    /// pools are asked from where the last poll's answers stopped, so that no pool is always
    /// last, each once one of the polls' permits is free; a pool with no read leaves the snapshot,
    /// and one skipped, not run or missed makes the read partial. Fails where the node ran the
    /// sampler for under half the pools asked, as it then runs no state override; where no pool
    /// answered and some failed at the node, and no pool is blamed for that; and where more pools
    /// missed for the first time than answered: a moment of trouble at the node should not blank
    /// most books.
    pub(super) async fn read(
        &self,
        block: u64,
        deadline: TokioInstant,
    ) -> Result<BookRead, FeedError> {
        let pools = self.discovered_pools().await?;
        let now = Instant::now();
        let start = match pools.len() {
            0 => 0,
            n => self.poll_offset.load(Ordering::Relaxed) % n,
        };
        let asked: Vec<&Arc<PoolInfo>> = pools[start..]
            .iter()
            .chain(&pools[..start])
            .filter(|info| !self.quarantined(&info.pool, now))
            .collect();
        let polled = join_all(
            asked
                .iter()
                .map(|info| self.poll_pool(Arc::clone(info), block, deadline)),
        )
        .await;
        let mut out = Vec::with_capacity(polled.len());
        let (mut missed, mut first_error, mut not_run_reason) = (Vec::new(), None, None);
        let (mut reached, mut not_run, mut partial) = (0, 0, false);
        for Polled { pool, reached: turn, read } in polled {
            reached += usize::from(turn);
            match read {
                Ok(read) => out.push(read),
                Err(PoolFailure::Pool(reason)) => self.set_aside(pool, &reason),
                Err(PoolFailure::Unanswered(error)) => {
                    partial = true;
                    first_error.get_or_insert_with(|| error.clone());
                    missed.push((pool, error));
                }
                Err(PoolFailure::NotRun(reason)) => {
                    partial = true;
                    not_run += 1;
                    debug!(%pool, reason, "the node did not run the sampler for an Efficient pool");
                    first_error.get_or_insert_with(|| not_run_error(&reason));
                    not_run_reason.get_or_insert(reason);
                }
                Err(PoolFailure::Skipped(reason)) => {
                    partial = true;
                    debug!(%pool, reason, "Efficient pool skipped this poll");
                    first_error
                        .get_or_insert_with(|| FeedError::Connection(format!("{pool}: {reason}")));
                }
            }
        }
        // past the answers, or past every turn given where none answered, so that a run of slow
        // pools at the head of the list does not hold the rest out
        let advance = if out.is_empty() { reached } else { out.len() };
        self.poll_offset
            .fetch_add(advance, Ordering::Relaxed);
        if not_run * 2 > asked.len() {
            return Err(not_run_error(&not_run_reason.unwrap_or_default()));
        }
        if out.is_empty() {
            return match first_error {
                Some(error) => Err(error),
                None => Ok(BookRead { block, pools: out, partial: false }),
            };
        }
        let new_misses = self.note_misses(missed);
        if new_misses.len() > out.len() {
            if let Some(error) = new_misses.into_iter().next() {
                return Err(error);
            }
        }
        Ok(BookRead { block, pools: out, partial })
    }

    /// `info`'s turn at a poll: a permit, then its sampler call, neither past `deadline`. A pool
    /// given no permit in time is skipped, and so is one whose permit comes at the deadline: a
    /// timeout polls what it waits on before its clock (tokio 1.47.1
    /// `src/time/timeout.rs:201-204`), so a waiter can be handed a permit there, and its call
    /// could only overrun.
    async fn poll_pool(&self, info: Arc<PoolInfo>, block: u64, deadline: TokioInstant) -> Polled {
        let pool = info.pool;
        let skipped = |reason: String| Polled {
            pool,
            reached: false,
            read: Err(PoolFailure::Skipped(reason)),
        };
        let _permit = match timeout_at(deadline, self.permits.acquire()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(closed)) => return skipped(closed.to_string()),
            Err(_) => return skipped("no turn before the poll's deadline".into()),
        };
        if TokioInstant::now() >= deadline {
            return skipped("its turn came at the poll's deadline".into());
        }
        Polled {
            pool,
            reached: true,
            read: self
                .sample_pool(info, block, deadline)
                .await,
        }
    }

    /// Sets aside the pools among `missed` that missed the poll before too, the fault being
    /// theirs where others answered both, and returns why the rest missed this one.
    fn note_misses(&self, missed: Vec<(Address, FeedError)>) -> Vec<FeedError> {
        let mut last_missed = self
            .last_missed
            .lock()
            .expect("misses lock");
        let earlier = std::mem::take(&mut *last_missed);
        let mut new_misses = Vec::new();
        for (pool, error) in missed {
            last_missed.insert(pool);
            if earlier.contains(&pool) {
                self.set_aside(pool, &error.to_string());
            } else {
                debug!(%pool, %error, "Efficient pool missed a poll");
                new_misses.push(error);
            }
        }
        new_misses
    }

    /// Runs discovery for as long as this future lives.
    pub(super) async fn keep_discovering(&self) -> Infallible {
        let mut catalogue = Catalogue::default();
        loop {
            self.discover(&mut catalogue).await;
            sleep(self.settings.discovery_interval).await;
        }
    }

    /// One round: the factories' new pools, then the views of every pool that is due. What cannot
    /// be read stays as the last round left it, and the polls go on quoting it; a round that
    /// fails with nothing to show for any round before it fails the polls until one succeeds.
    pub(super) async fn discover(&self, catalogue: &mut Catalogue) {
        let mut failures = match &self.settings.allowed_pools {
            Some(named) => {
                self.check_named_pools(named, catalogue)
                    .await
            }
            None => self.page_factories(catalogue).await,
        };
        let now = Instant::now();
        let due: Vec<(Address, Address)> = catalogue
            .pools
            .iter()
            .flat_map(|(factory, pools)| {
                pools
                    .iter()
                    .map(|pool| (*factory, *pool))
            })
            .filter(|(_, pool)| !self.quarantined(pool, now))
            .filter(|(_, pool)| {
                catalogue
                    .resting
                    .get(pool)
                    .is_none_or(|until| *until <= now)
            })
            .collect();
        let reads = join_all(
            due.iter()
                .map(
                    |(factory, pool)| async move { (*pool, self.pool_info(*factory, *pool).await) },
                ),
        )
        .await;
        for (pool, read) in reads {
            match read {
                Ok(Some(info)) => {
                    catalogue
                        .served
                        .insert(pool, Arc::new(info));
                }
                Ok(None) => {
                    catalogue.served.remove(&pool);
                    let rest = self.settings.discovery_interval * UNSERVED_REST_INTERVALS;
                    catalogue
                        .resting
                        .insert(pool, Instant::now() + rest);
                }
                Err(PoolFailure::Pool(reason)) => {
                    catalogue.served.remove(&pool);
                    self.set_aside(pool, &reason);
                }
                Err(PoolFailure::Unanswered(error)) => {
                    warn!(%pool, %error, "could not read an Efficient pool, keeping its last read");
                    failures.push(format!("{pool}: {error}"));
                }
                Err(PoolFailure::NotRun(reason) | PoolFailure::Skipped(reason)) => {
                    warn!(%pool, reason, "could not read an Efficient pool, keeping its last read");
                    failures.push(format!("{pool}: {reason}"));
                }
            }
        }
        let discovered = match failures.first() {
            Some(failure) if catalogue.served.is_empty() => Discovered::Failed(failure.clone()),
            Some(_) | None => {
                // in a fixed order, which the polls' rotation walks
                let mut served: Vec<Arc<PoolInfo>> = catalogue
                    .served
                    .values()
                    .cloned()
                    .collect();
                served.sort_unstable_by_key(|info| info.pool);
                Discovered::Ready(Arc::new(served))
            }
        };
        self.discovered.send_replace(discovered);
    }

    /// Finds which factory created each named pool not placed yet, again each round for one no
    /// factory admits to: it may not exist yet. Returns what went wrong.
    async fn check_named_pools(
        &self,
        named: &HashSet<Address>,
        catalogue: &mut Catalogue,
    ) -> Vec<String> {
        let placed: HashSet<Address> = catalogue
            .pools
            .values()
            .flatten()
            .copied()
            .collect();
        let mut failures = Vec::new();
        for pool in named.difference(&placed) {
            let mut factory_of = None;
            let mut unread = None;
            for factory in &self.settings.factories {
                match self
                    .view(*factory, isPoolCall { pool: *pool })
                    .await
                {
                    Ok(true) => {
                        factory_of = Some(*factory);
                        break;
                    }
                    Ok(false) => {}
                    Err(failure) => unread = Some(failure),
                }
            }
            match (factory_of, unread) {
                (Some(factory), _) => catalogue
                    .pools
                    .entry(factory)
                    .or_default()
                    .push(*pool),
                (None, Some(failure)) => {
                    failures.push(format!("could not check {pool}: {failure}"));
                }
                (None, None) => {
                    warn!(%pool, "no configured factory created this pool, not served");
                    failures.push(format!("no configured factory created {pool}"));
                }
            }
        }
        failures
    }

    /// Adds the pools each factory created since the last round, a page at most, and returns
    /// what went wrong. A factory that cannot be read keeps the pools it had.
    async fn page_factories(&self, catalogue: &mut Catalogue) -> Vec<String> {
        let mut failures = Vec::new();
        for factory in &self.settings.factories {
            let known = catalogue
                .pools
                .entry(*factory)
                .or_default();
            if let Err(failure) = self.page_factory(*factory, known).await {
                warn!(%factory, failure, "could not page an Efficient pools factory");
                failures.push(failure);
            }
        }
        failures
    }

    /// Extends `known` from `factory`'s array, which only ever grows, stopping at the first entry
    /// it cannot vouch for. No gas is involved, so a long array cannot fail this, and an address
    /// with no code is told apart from a factory with no pools by asking for its code.
    async fn page_factory(&self, factory: Address, known: &mut Vec<Address>) -> Result<(), String> {
        let length = self
            .storage(factory, U256::from(POOLS_SLOT))
            .await
            .map_err(|failure| format!("{factory}'s pool count: {failure}"))?;
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        if length == 0 && known.is_empty() {
            let code = self
                .code_marks(factory)
                .await
                .map_err(|failure| format!("{factory}'s code: {failure}"))?;
            return match code {
                Some(_) => Ok(()),
                None => Err(format!("{factory} holds no code: it is no factory on this chain")),
            };
        }
        let page_end = length.min(
            known
                .len()
                .saturating_add(self.settings.discovery_page),
        );
        let entries = join_all((known.len()..page_end).map(|i| self.pool_entry(factory, i))).await;
        for entry in entries {
            known.push(entry?);
        }
        Ok(())
    }

    /// Entry `index` of `factory`'s array, which the factory must own to as a pool of its own.
    async fn pool_entry(&self, factory: Address, index: usize) -> Result<Address, String> {
        let word = self
            .storage(factory, pool_entry_slot(index))
            .await
            .map_err(|failure| format!("entry {index} of {factory}: {failure}"))?;
        let not_a_pool = || format!("entry {index} of {factory} is not one of its pools");
        if word.bit_len() > 160 {
            return Err(not_a_pool());
        }
        let pool = Address::from_word(word.into());
        match self
            .view(factory, isPoolCall { pool })
            .await
        {
            Ok(true) => Ok(pool),
            Ok(false) => Err(not_a_pool()),
            Err(failure) => Err(format!("entry {index} of {factory}: {failure}")),
        }
    }

    async fn storage(&self, address: Address, slot: U256) -> Result<U256, CallFailure> {
        let _permit = self.discovery_permit().await?;
        self.answer(|| {
            self.provider
                .get_storage_at(address, slot)
        })
        .await
    }

    /// What to quote `pool` in: `None` for a pool a front prices, one the lists do not admit, with
    /// nothing to serve or worth less than the floor. Fronts and the lists are asked first, so a
    /// pool they refuse costs little.
    async fn pool_info(
        &self,
        factory: Address,
        pool: Address,
    ) -> Result<Option<PoolInfo>, PoolFailure> {
        let (hook_set, feed, lane) = tokio::join!(
            self.view(pool, hooksCall {}),
            self.view(pool, priceFeedCall {}),
            self.view(pool, laneCall {}),
        );
        let (swap_hook, feed) = (hook_set?.hook, feed?);
        let lane_on =
            (lane?.laneConfig >> LANE_VERSION_SHIFT) & U256::from(LANE_VERSION_MASK) != U256::ZERO;
        if lane_on ||
            self.front_listed(factory, swap_hook)
                .await?
        {
            debug!(%pool, hook = %swap_hook, lane_on, "a front prices this Efficient pool, not served");
            return Ok(None);
        }
        let shape = self.shape_of(swap_hook).await?;
        if self.settings.allowed_pools.is_none() {
            if !self.feed_listed(feed).await? {
                debug!(%pool, %feed, "Efficient pool's price feed is not listed, not served");
                return Ok(None);
            }
            if shape.is_none() {
                debug!(%pool, hook = %swap_hook, "Efficient pool's hook is not listed, not served");
                return Ok(None);
            }
        }
        let Some((rule, row)) = self
            .pricing(pool, swap_hook, shape)
            .await?
        else {
            return Ok(None);
        };
        let (count, masks, router) = tokio::join!(
            self.view(pool, tokenCountCall {}),
            self.view(pool, tradablePairsCall {}),
            self.view(pool, routerCall {}),
        );
        let n = usize::try_from(count?)
            .unwrap_or(usize::MAX)
            .min(MAX_TOKENS);
        let tradable = masks?.tradable;
        let router = router?;
        if n < 2 {
            debug!(%pool, n, "Efficient pool has no pair for this feed to serve");
            return Ok(None);
        }

        let configs = join_all(
            (0..=u8::MAX)
                .take(n)
                .map(|t| self.view(pool, tokenConfigCall { t })),
        )
        .await;
        let mut configs_read = Vec::with_capacity(n);
        for config in configs {
            configs_read.push(config?);
        }
        let prices = join_all(
            configs_read
                .iter()
                .map(|c| self.usd_price(feed, c.token)),
        )
        .await;
        let mut tokens = Vec::with_capacity(n);
        for (config, price) in configs_read.iter().zip(prices) {
            tokens.push(TokenInfo {
                address: config.token,
                decimals: config.decimals,
                usd: price?,
            });
        }

        if !self
            .worth_quoting(pool, &tokens)
            .await?
        {
            return Ok(None);
        }

        let class = self
            .code_marks(swap_hook)
            .await?
            .map_or(B256::ZERO, |marks| marks.class);
        let venues = self.venues(router, pool).await?;
        let universe = &self.settings.universe;
        let directions: Vec<DirectionPlan> = HOOK_PAIR_DIRECTIONS
            .into_iter()
            .filter(|(i, j)| tradable & trade_bit(*i, *j) != U256::ZERO)
            .filter(|(i, j)| {
                universe.contains(&tokens[*i].address) && universe.contains(&tokens[*j].address)
            })
            .filter_map(|(i, j)| {
                // the gate: a direction whose venues the table cannot charge is not quoted
                let Some(gas) = self.settings.gas.key(
                    self.settings.chain_id,
                    class,
                    hook_pair_index(i),
                    hook_pair_index(j),
                    &venues,
                ) else {
                    debug!(%pool, token_in = i, token_out = j, ?venues, "no gas cell charges this Efficient pool direction, not served");
                    return None;
                };
                let cap = configs_read[i].maxSwapNotional;
                let smallest = smallest_rung(
                    tokens[i].decimals,
                    tokens[i].usd,
                    self.settings.smallest_rung_usd,
                );
                Some(DirectionPlan {
                    token_in: i,
                    token_out: j,
                    asked: rung_inputs(
                        smallest,
                        (!cap.is_zero()).then_some(cap),
                        self.settings.rung_count,
                    ),
                    gas,
                    guard: HeldRepay::lending(&venues, hook_pair_index(j)),
                })
            })
            .collect();
        if directions.is_empty() {
            debug!(%pool, "Efficient pool serves no pair in the token universe");
            return Ok(None);
        }
        // the hook pair's band is token 1's: the pool asset's is zero
        let live_band = U256::from(configs_read[1].swapPriceBandWad);
        let trust = Trust::of(&row, live_band, n);
        Ok(Some(PoolInfo { pool, hook: swap_hook, rule, row, trust, tokens, directions }))
    }

    /// A token's USD price: the pool's feed's, else one for a stablecoin, else none. A feed that
    /// refuses to price a token leaves it unpriced; it does not set the pool aside.
    async fn usd_price(&self, feed: Address, token: Address) -> Result<Option<f64>, PoolFailure> {
        let stable = self
            .settings
            .stablecoins
            .contains(&token)
            .then_some(1.0);
        if feed.is_zero() {
            return Ok(stable);
        }
        match self
            .view(feed, peekUsdCall { token })
            .await
        {
            Ok(peek) if peek.ok && !peek.usdWad.is_zero() => {
                let wad = u128::try_from(peek.usdWad).map_or(f64::MAX, |w| w as f64);
                Ok(Some(wad / 1e18)
                    .filter(|p| p.is_finite())
                    .or(stable))
            }
            Ok(_) | Err(CallFailure::Refused(_)) => Ok(stable),
            Err(CallFailure::Unanswered(error)) => Err(PoolFailure::Unanswered(error)),
        }
    }

    /// Whether the pool clears the floor before any of it is quoted: valued at its `totalAssets`
    /// where its pool token (token 0) has a price, else, or where that view refuses, at its priced
    /// tokens' legs. A pool none of whose tokens has a price is not quoted under a floor, as none
    /// of its books could clear it. The prices are the pool's feed's, so the floor saves quoting
    /// small pools; it is no defence against a hostile one, which is the lists' job.
    async fn worth_quoting(
        &self,
        pool: Address,
        tokens: &[TokenInfo],
    ) -> Result<bool, PoolFailure> {
        if self.settings.min_nav_usd <= 0.0 {
            return Ok(true);
        }
        let priced_asset = tokens
            .first()
            .and_then(|asset| Some((asset.decimals, asset.usd?)));
        let nav = match priced_asset {
            Some((decimals, price)) => match self
                .view(pool, totalAssetsCall {})
                .await
            {
                Ok(nav) => Some(usd(nav, decimals, price)),
                Err(CallFailure::Refused(reason)) => {
                    debug!(%pool, reason, "Efficient pool refuses its NAV, valued at its priced legs");
                    None
                }
                Err(CallFailure::Unanswered(error)) => return Err(PoolFailure::Unanswered(error)),
            },
            None => None,
        };
        let nav = match nav {
            Some(nav) => nav,
            None => {
                self.priced_legs_usd(pool, tokens)
                    .await?
            }
        };
        let worth = nav >= self.settings.min_nav_usd;
        if !worth {
            debug!(%pool, nav_usd = nav, "Efficient pool worth less than the floor, not quoted");
        }
        Ok(worth)
    }

    /// What the pool's legs in its priced tokens are worth in USD, at least zero: what it holds,
    /// lends and posts of them, less what it owes of them.
    async fn priced_legs_usd(
        &self,
        pool: Address,
        tokens: &[TokenInfo],
    ) -> Result<f64, PoolFailure> {
        let priced: Vec<(u8, u8, f64)> = (0..=u8::MAX)
            .zip(tokens)
            .filter_map(|(t, token)| Some((t, token.decimals, token.usd?)))
            .collect();
        let legs = join_all(
            priced
                .iter()
                .map(|(t, ..)| self.view(pool, positionCall { t: *t })),
        )
        .await;
        let mut net = 0.0;
        for ((_, decimals, price), legs) in priced.iter().zip(legs) {
            let legs = legs?;
            let assets = legs
                .held
                .saturating_add(legs.supplied)
                .saturating_add(legs.posted);
            net += usd(assets, *decimals, *price) - usd(legs.owed, *decimals, *price);
        }
        Ok(net.max(0.0))
    }

    /// The pool's venue records, as the charge and the guard read them: each one's kind, whether
    /// it is live, its credit pair's tokens, and, for a live one whose debt is a token of the hook
    /// pair, whether it lets the pool repay now and whether its account holds a partial repay at a
    /// minimum position, with that minimum. A record past [`MAX_VENUES`], one the router refuses
    /// to read, or one whose account refuses `capabilities()`, counts as unread; an actions read
    /// that fails counts as a refusal of repays, and a minimum that fails or answers not ok as
    /// unread. Each account's capabilities are read once.
    async fn venues(&self, router: Address, pool: Address) -> Result<Venues, PoolFailure> {
        if router.is_zero() {
            return Ok(Venues::default());
        }
        let count = self
            .view(router, venueCountCall { pool })
            .await?;
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let mut unread = count > MAX_VENUES;
        let records = join_all(
            (0..=u16::MAX)
                .take(count.min(MAX_VENUES))
                .map(|id| async move {
                    let record = self
                        .view(router, venueCall { pool, id })
                        .await?;
                    let live = record.status == 0;
                    let lends_the_pair = live && record.debt <= 1;
                    let repays = lends_the_pair &&
                        self.view(router, venueActionsCall { pool, id })
                            .await
                            .is_ok_and(|actions| actions & CAN_REPAY != 0);
                    let venue = Venue {
                        kind: record.kind,
                        live,
                        coll: record.coll,
                        debt: record.debt,
                        repays,
                        held_repay: None,
                    };
                    Ok::<_, CallFailure>((venue, lends_the_pair.then_some(record)))
                }),
        )
        .await;
        let mut venues = Vec::with_capacity(records.len());
        let mut accounts: Vec<Address> = Vec::new();
        let mut lenders = Vec::new();
        for record in records {
            match record {
                Ok((venue, lender)) => {
                    if let Some(lender) = lender {
                        if !accounts.contains(&lender.account) {
                            accounts.push(lender.account);
                        }
                        lenders.push((venues.len(), lender));
                    }
                    venues.push(venue);
                }
                Err(CallFailure::Refused(reason)) => {
                    debug!(%pool, reason, "an Efficient pool's venue record does not read");
                    unread = true;
                }
                Err(CallFailure::Unanswered(error)) => return Err(PoolFailure::Unanswered(error)),
            }
        }
        let capabilities = join_all(
            accounts
                .iter()
                .map(|account| self.view(*account, capabilitiesCall {})),
        )
        .await;
        let mut holds_minimum: Vec<(Address, bool)> = Vec::with_capacity(accounts.len());
        for (account, answer) in accounts.iter().zip(capabilities) {
            match answer {
                Ok(bits) => {
                    holds_minimum.push((*account, bits & U256::from(CAP_HELD_REPAY) != U256::ZERO))
                }
                Err(CallFailure::Refused(reason)) => {
                    debug!(%pool, %account, reason, "an Efficient pool's venue account does not say what it holds");
                    unread = true;
                }
                Err(CallFailure::Unanswered(error)) => return Err(PoolFailure::Unanswered(error)),
            }
        }
        let held: Vec<(usize, VenueView)> = lenders
            .into_iter()
            .filter(|(_, lender)| holds_minimum.contains(&(lender.account, true)))
            .collect();
        let minimums = join_all(
            held.iter()
                .map(|(_, lender)| self.view(lender.account, minimumDebtCall { id: lender.id })),
        )
        .await;
        for ((index, _), answer) in held.iter().zip(minimums) {
            let minimum = match answer {
                Ok(read) if read.ok => Minimum::Read(read.floor),
                Ok(_) | Err(CallFailure::Refused(_)) => Minimum::Unread,
                Err(CallFailure::Unanswered(error)) => return Err(PoolFailure::Unanswered(error)),
            };
            venues[*index].held_repay = Some(minimum);
        }
        Ok(Venues { records: venues, unread })
    }

    /// `info`'s pool read at `block` by the sampler, by `deadline`. A call whose form may be one
    /// backend's is asked again once, and a raised cap that leaves the node's gas short of a single
    /// probe goes back to the default and is asked again; a call asked again that gets no answer
    /// in time leaves the pool as the first call did.
    async fn sample_pool(
        &self,
        info: Arc<PoolInfo>,
        block: u64,
        deadline: TokioInstant,
    ) -> Result<PoolRead, PoolFailure> {
        let pool = info.pool;
        let default_cap = self.default_cap();
        let mut cap = self.probe_cap(&pool);
        let mut asked_again = false;
        let mut fallback = None;
        loop {
            if fallback.is_some() && TokioInstant::now() >= deadline {
                break;
            }
            let Ok(answer) = timeout_at(deadline, self.sample(&info, block, cap)).await else {
                break;
            };
            let failure = match answer {
                Ok(walked) if walked.gas_short() && cap == default_cap => {
                    return Err(PoolFailure::NotRun(format!(
                        "the call's gas left none for a probe at a cap of {cap}"
                    )));
                }
                // a node that gives a call less gas than a raised cap needs: the pool's event,
                // not the node's
                Ok(walked) if walked.gas_short() => {
                    let raised = cap;
                    cap = default_cap;
                    self.set_cap(pool, cap);
                    PoolFailure::Skipped(format!(
                        "the call's gas left none for a probe at a cap of {raised}"
                    ))
                }
                Ok(walked) => return self.pool_read(info, walked, cap),
                Err(Failed { failure, again: true }) if !asked_again => {
                    asked_again = true;
                    failure
                }
                Err(Failed { failure, .. }) => return Err(failure),
            };
            fallback = Some(failure);
        }
        Err(fallback.unwrap_or_else(|| {
            PoolFailure::Unanswered(FeedError::Connection(format!(
                "no answer from {pool}'s sampler call by the poll's deadline"
            )))
        }))
    }

    /// One sampler call for `info`'s pool at `block`, each of its calls to the pool or its hook
    /// capped at `cap`: the walks, or why there are none. An answer that is not the sampler's is
    /// the node's, as is one from a block before `block`.
    async fn sample(&self, info: &PoolInfo, block: u64, cap: u64) -> Result<Walked, Failed> {
        let call = self.sample_call(info, cap);
        let asks = call.asks.len();
        let raw = self
            .run_sampler(call.abi_encode(), block)
            .await
            .map_err(|error| Failed::of(&error))?;
        if raw.is_empty() {
            return Err(Failed::not_run("the taker answered with no data", true));
        }
        let answer = FlammSampler::sampleCall::abi_decode_returns(&raw).map_err(|e| {
            Failed::not_run(format!("an answer that is not the sampler's: {e}"), true)
        })?;
        if answer.tag.0 != SAMPLER_TAG {
            return Err(Failed::not_run(format!("an answer tagged {}", answer.tag), true));
        }
        if answer.walks.len() != asks {
            let walks = answer.walks.len();
            return Err(Failed::not_run(format!("{walks} walks for {asks} directions"), true));
        }
        if answer.blockNumber < U256::from(block) {
            let skipped = format!("an answer from block {}, before {block}", answer.blockNumber);
            return Err(Failed { failure: PoolFailure::Skipped(skipped), again: false });
        }
        let mut most_spent = U256::ZERO;
        let mut directions = Vec::with_capacity(asks);
        for (plan, walk) in info.directions.iter().zip(answer.walks) {
            let mut probes: Vec<Probe> = Vec::with_capacity(walk.probes.len());
            for sampled in &walk.probes {
                let Some(probe) = probe_of(sampled) else {
                    let status = sampled.status;
                    return Err(Failed::not_run(format!("a probe of status {status}"), true));
                };
                if probes
                    .last()
                    .is_some_and(|last| last.ask >= probe.ask)
                {
                    return Err(Failed::not_run("a walk out of order", true));
                }
                most_spent = most_spent.max(sampled.gasUsed);
                probes.push(probe);
            }
            directions.push(DirectionRead {
                plan: plan.clone(),
                probes,
                curve_fee: walk.curveFeeWad,
                band_wad: U256::from(walk.bandWad),
                usd_in: walk.usdIn,
                usd_out: walk.usdOut,
                legs: walk.legs,
                bid_wad: walk.bidWad,
                truncated: walk.truncated,
            });
        }
        Ok(Walked { directions, most_spent })
    }

    /// The sampler call for `info`'s pool, each of its calls capped at `cap`: a walk per planned
    /// direction, seeded with its rungs under `2^120`. Only where the rule reads leverage answers
    /// does it name the hook and the row's labels, so that the sampler reads their request and
    /// probes above a top they priced.
    fn sample_call(&self, info: &PoolInfo, cap: u64) -> FlammSampler::sampleCall {
        let reads_leverage =
            info.rule == Rule::Everlong && !info.hook.is_zero() && !info.row.tiers().is_empty();
        let (hook, tier_a, tier_b) = if reads_leverage {
            (info.hook, info.row.spread_wad, info.row.spread_dn_wad)
        } else {
            (Address::ZERO, U256::ZERO, U256::ZERO)
        };
        let asks = info
            .directions
            .iter()
            .map(|plan| FlammSampler::Ask {
                tokenIn: info.tokens[plan.token_in].address,
                tokenOut: info.tokens[plan.token_out].address,
                hook,
                tierA: tier_a,
                tierB: tier_b,
                maxProbes: U256::from(self.settings.max_probes_per_direction),
                seeds: plan
                    .asked
                    .iter()
                    .copied()
                    .filter(|seed| !seed.is_zero() && seed.bit_len() <= ANSWER_BITS)
                    .collect(),
            })
            .collect();
        FlammSampler::sampleCall { pool: info.pool, asks, limits: sampler_limits(cap) }
    }

    /// `data` sent to the sampler, which the node runs at the taker by state override, at
    /// `block`. No sender is set: the pool sees the taker by where the code runs, and nothing on a
    /// hook pair reads the origin.
    async fn run_sampler(&self, data: Vec<u8>, block: u64) -> TransportResult<AlloyBytes> {
        let taker = self.settings.taker;
        let tx = TransactionRequest::default()
            .to(taker)
            .gas_limit(self.settings.sample_gas)
            .input(TransactionInput::new(data.into()));
        let mut overrides = StateOverride::default();
        overrides.insert(
            taker,
            AccountOverride {
                code: Some(AlloyBytes::from_static(SAMPLER_RUNTIME)),
                ..Default::default()
            },
        );
        self.provider
            .call(tx)
            .overrides(overrides)
            .block(BlockId::number(block))
            .await
    }

    /// What `walked`, answered at `cap`, says of `info`'s pool: its read, or why it has none. A
    /// direction whose first answer the pool did not refuse cannot be a fill is the pool's; one
    /// that ran short of gas says nothing of the pool, and below the most cap only raises it. The
    /// pool's strikes go only with a priced answer.
    fn pool_read(
        &self,
        info: Arc<PoolInfo>,
        walked: Walked,
        cap: u64,
    ) -> Result<PoolRead, PoolFailure> {
        let pool = info.pool;
        let default_cap = self.default_cap();
        let probes = || {
            walked
                .directions
                .iter()
                .flat_map(|d| &d.probes)
        };
        let next_cap = if probes().any(|p| p.status == ProbeStatus::Starved) {
            cap.saturating_mul(2)
                .min(self.max_cap())
        } else if walked.most_spent * U256::from(8) < U256::from(default_cap) * U256::from(7) {
            default_cap
        } else {
            cap
        };
        self.set_cap(pool, next_cap);
        let firsts: Vec<(&DirectionRead, &Probe)> = walked
            .directions
            .iter()
            .filter_map(|d| {
                d.probes
                    .iter()
                    .find(|p| p.status != ProbeStatus::Refused)
                    .map(|p| (d, p))
            })
            .collect();
        if let Some((direction, first)) = firsts
            .iter()
            .find(|(_, p)| p.status == ProbeStatus::Malformed)
        {
            let token_in = info.tokens[direction.plan.token_in].address;
            return Err(PoolFailure::Pool(format!(
                "answers {} of {token_in} with what cannot be a fill",
                first.ask
            )));
        }
        if let Some((_, first)) = firsts
            .iter()
            .find(|(_, p)| p.status == ProbeStatus::Starved)
        {
            let ask = first.ask;
            if cap < self.max_cap() {
                return Err(PoolFailure::Skipped(format!(
                    "its answer at {ask} ran short of a cap of {cap}, raised to {next_cap}"
                )));
            }
            return Err(PoolFailure::Unanswered(FeedError::Connection(format!(
                "{pool}'s answer at {ask} ran short of the most cap, {cap}"
            ))));
        }
        for direction in &walked.directions {
            if direction.truncated {
                let probes = direction.probes.len();
                debug!(%pool, token_in = direction.plan.token_in, probes, "the sampler's gas gate ended an Efficient pool's walk early");
            }
        }
        if probes().any(|p| p.status == ProbeStatus::Priced) {
            self.cleared(&pool);
        }
        Ok(PoolRead { info, directions: walked.directions })
    }
}

#[cfg(test)]
impl FlammReader {
    /// One sampler call for `info`'s pool at `block`, at the default cap, as a poll makes it: the
    /// walks of `info`'s directions at their own asks, or why there are none.
    pub(super) async fn walks(
        &self,
        info: &PoolInfo,
        block: u64,
    ) -> Result<Vec<DirectionRead>, String> {
        match self
            .sample(info, block, self.default_cap())
            .await
        {
            Ok(walked) => Ok(walked.directions),
            Err(Failed { failure, .. }) => Err(match failure {
                PoolFailure::Pool(reason) |
                PoolFailure::NotRun(reason) |
                PoolFailure::Skipped(reason) => reason,
                PoolFailure::Unanswered(error) => error.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// An error's form decides its class, so no wording on either side of the line can move it.
    #[rstest]
    #[case::a_revert(r#"{"code":3,"message":"execution reverted: PairNotTradable"}"#, true)]
    #[case::revert_data_under_another_code(
        r#"{"code":-32000,"message":"execution reverted","data":"0x1e4f7d8c"}"#,
        true
    )]
    #[case::revert_data_inside_an_object(
        r#"{"code":-32603,"message":"reverted","data":{"data":"0x1e4f7d8c"}}"#,
        true
    )]
    #[case::the_word_alone_is_no_revert(r#"{"code":-32603,"message":"reverted"}"#, false)]
    #[case::out_of_gas(r#"{"code":-32000,"message":"out of gas"}"#, false)]
    #[case::out_of_gas_with_another_code(
        r#"{"code":-32003,"message":"EVM error OutOfGas"}"#,
        false
    )]
    #[case::an_unfunded_sender(
        r#"{"code":-32003,"message":"Insufficient funds for gas * price + value"}"#,
        false
    )]
    #[case::a_limit_with_no_known_wording(r#"{"code":-32000,"message":"quota spent"}"#, false)]
    #[case::a_timeout(r#"{"code":-32000,"message":"execution aborted (timeout = 5s)"}"#, false)]
    #[case::missing_state(r#"{"code":-32000,"message":"missing trie node 5b1c (path )"}"#, false)]
    #[case::an_internal_error(r#"{"code":-32603,"message":"internal error"}"#, false)]
    fn only_an_error_in_a_reverts_form_is_the_pools_answer(
        #[case] answer: &str,
        #[case] refused: bool,
    ) {
        let error = RpcError::ErrorResp(serde_json::from_str(answer).unwrap());
        assert_eq!(matches!(CallFailure::from(error), CallFailure::Refused(_)), refused);
    }

    fn answered(answer: &str) -> RpcError<TransportErrorKind> {
        RpcError::ErrorResp(serde_json::from_str(answer).unwrap())
    }

    /// The sampler never reverts, so a node's error to its call is the node's, and its form or
    /// wording says which way: the first match wins, so a limit worded with gas is a refusal.
    #[rstest]
    #[case::the_routers_own_revert(
        r#"{"code":3,"message":"execution reverted","data":"0xa6cdd2a5"}"#,
        NodeClass::Reverted
    )]
    #[case::revert_data_under_another_code(
        r#"{"code":-32000,"message":"execution reverted","data":"0xf902f96b"}"#,
        NodeClass::Reverted
    )]
    #[case::a_bare_revert_in_words(
        r#"{"code":-32015,"message":"VM Exception: revert"}"#,
        NodeClass::Reverted
    )]
    #[case::no_such_method(
        r#"{"code":-32601,"message":"the method does not exist"}"#,
        NodeClass::Refused
    )]
    #[case::bad_parameters(r#"{"code":-32602,"message":"invalid argument 2"}"#, NodeClass::Refused)]
    #[case::too_large(
        r#"{"code":-32604,"message":"request entity too large"}"#,
        NodeClass::Refused
    )]
    #[case::an_invalid_request(
        r#"{"code":-32600,"message":"invalid request"}"#,
        NodeClass::Refused
    )]
    #[case::an_invalid_request_worded_as_a_limit(
        r#"{"code":-32600,"message":"too many requests"}"#,
        NodeClass::Limited
    )]
    #[case::overrides_refused(
        r#"{"code":-32000,"message":"State Override is not supported"}"#,
        NodeClass::Refused
    )]
    #[case::gas_over_the_cap(
        r#"{"code":-32000,"message":"gas required exceeds allowance (50000000)"}"#,
        NodeClass::Refused
    )]
    #[case::a_gas_limit_reached(
        r#"{"code":-32000,"message":"gas limit reached"}"#,
        NodeClass::Refused
    )]
    #[case::out_of_gas(r#"{"code":-32000,"message":"out of gas"}"#, NodeClass::Missed)]
    #[case::a_limit_code(r#"{"code":429,"message":"slow down"}"#, NodeClass::Limited)]
    #[case::a_resource_code(r#"{"code":-32005,"message":"slow down"}"#, NodeClass::Limited)]
    #[case::another_limit_code(r#"{"code":-32007,"message":"slow down"}"#, NodeClass::Limited)]
    #[case::a_quota_code(r#"{"code":-32012,"message":"slow down"}"#, NodeClass::Limited)]
    #[case::a_capacity_code(r#"{"code":-32016,"message":"slow down"}"#, NodeClass::Limited)]
    #[case::a_batch_code(r#"{"code":-32055,"message":"slow down"}"#, NodeClass::Limited)]
    #[case::a_rate_limit(r#"{"code":-32000,"message":"Rate limit exceeded"}"#, NodeClass::Limited)]
    #[case::rate_exceeded(
        r#"{"code":-32000,"message":"request rate exceeded"}"#,
        NodeClass::Limited
    )]
    #[case::credits(r#"{"code":-32000,"message":"Your credits limited"}"#, NodeClass::Limited)]
    #[case::a_request_limit(
        r#"{"code":-32000,"message":"daily request limit"}"#,
        NodeClass::Limited
    )]
    #[case::a_limit_reached(r#"{"code":-32000,"message":"limit reached"}"#, NodeClass::Limited)]
    #[case::concurrency(
        r#"{"code":-32000,"message":"maximum number of concurrent requests"}"#,
        NodeClass::Limited
    )]
    #[case::a_missing_header(r#"{"code":-32000,"message":"header not found"}"#, NodeClass::NoBlock)]
    #[case::an_unknown_block(r#"{"code":-32000,"message":"unknown block"}"#, NodeClass::NoBlock)]
    #[case::a_block_not_found(r#"{"code":-32001,"message":"block not found"}"#, NodeClass::NoBlock)]
    #[case::missing_state(
        r#"{"code":-32000,"message":"missing trie node 5b1c (path )"}"#,
        NodeClass::NoBlock
    )]
    #[case::an_internal_error(r#"{"code":-32603,"message":"internal error"}"#, NodeClass::Missed)]
    #[case::a_timeout(
        r#"{"code":-32000,"message":"execution aborted (timeout = 5s)"}"#,
        NodeClass::Missed
    )]
    fn a_nodes_error_to_the_sampler_is_classed_by_its_form_then_its_words(
        #[case] answer: &str,
        #[case] class: NodeClass,
    ) {
        assert_eq!(node_class(&answered(answer)), class);
    }

    #[rstest]
    #[case::too_many_requests(429, NodeClass::Limited)]
    #[case::unavailable(503, NodeClass::Limited)]
    #[case::too_large(413, NodeClass::Refused)]
    #[case::a_bad_gateway(502, NodeClass::Missed)]
    fn an_http_status_is_classed_by_its_code(#[case] status: u16, #[case] class: NodeClass) {
        let error = TransportErrorKind::http_error(status, "<html></html>".into());
        assert_eq!(node_class(&error), class);
    }

    #[test]
    fn a_null_answer_is_a_limit() {
        assert_eq!(node_class(&RpcError::NullResp), NodeClass::Limited);
        let err = serde_json::from_str::<AlloyBytes>("null").unwrap_err();
        let null = RpcError::DeserError { err, text: "null".into() };
        assert_eq!(node_class(&null), NodeClass::Limited);
        let err = serde_json::from_str::<AlloyBytes>("1").unwrap_err();
        let other = RpcError::DeserError { err, text: "1".into() };
        assert_eq!(node_class(&other), NodeClass::Missed);
    }

    /// The sampler's own statuses map onto the rule's, and one it never gives is no answer.
    #[test]
    fn the_samplers_statuses_are_the_rules() {
        let probe = |status: u64, used: u64, out: u64| FlammSampler::Probe {
            ask: U256::from(100u64),
            used: U256::from(used),
            out: U256::from(out),
            feeWad: U256::from(7u64),
            gasUsed: U256::from(1u64),
            status: U256::from(status),
            reason: Default::default(),
        };
        let status = |p: FlammSampler::Probe| probe_of(&p).map(|p| p.status);
        assert_eq!(status(probe(1, 100, 5)), Some(ProbeStatus::Priced));
        assert_eq!(status(probe(2, 50, 5)), Some(ProbeStatus::Clipped));
        assert_eq!(status(probe(3, 0, 0)), Some(ProbeStatus::Refused));
        assert_eq!(status(probe(4, 100, 5)), Some(ProbeStatus::Starved));
        assert_eq!(status(probe(5, 101, 5)), Some(ProbeStatus::Malformed));
        assert_eq!(status(probe(0, 100, 5)), None);
        assert_eq!(status(probe(6, 100, 5)), None);
    }

    #[test]
    fn bits_follow_the_pool() {
        assert_eq!(trade_bit(1, 0), U256::from(1) << 16);
        assert_eq!(trade_bit(0, 1), U256::from(1) << 1);
    }

    #[test]
    fn a_factorys_entries_follow_soliditys_array_layout() {
        // keccak256 of the slot, written out so that the two cannot drift together
        let first = U256::from_be_slice(&alloy::hex!(
            "b10e2d527612073b26eecdfd717e6a320cf44b4afac2b0732d9fcbe2b7fa0cf6"
        ));
        assert_eq!(pool_entry_slot(0), first);
        assert_eq!(pool_entry_slot(7), first + U256::from(7));
    }

    #[test]
    fn rungs_start_at_the_usd_size_or_a_thousandth_of_a_token() {
        // 10 USD of an 8-decimal token at 80,000 USD
        assert_eq!(smallest_rung(8, Some(80_000.0), 10.0), U256::from(12_500u64));
        // an unpriced 18-decimal token: 0.001 of it
        assert_eq!(smallest_rung(18, None, 10.0), U256::from(10u64).pow(U256::from(15)));
        // an unpriced token of no decimals still starts at one unit
        assert_eq!(smallest_rung(0, None, 10.0), U256::from(1u64));
    }
}
