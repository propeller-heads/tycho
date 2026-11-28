//! Reads every listing door of one front over JSON-RPC, all at one block.
//!
//! A read is three JSON-RPC batches, each one HTTP request, and a fourth where a print lapses
//! within the book's life (see [`DoorsReader::sample`]). The first, at the latest block, reads
//! the front proxy's EIP-1967 implementation slot (no contract can read it for Multicall3) and
//! runs `Multicall3.aggregate3` over the block's number, timestamp and chain id, the factory's
//! `FRONT` and `DOOR_CODEHASH`, the front's `clock` and BaseTime, the millisecond source its clock
//! may read, and the front's `doorView` for each listing id scanned. Each listing's door is the
//! CREATE2 address, under the factory, of the door code hash the factory itself answered, computed
//! here: no door code hash is configured. The two that
//! follow are pinned to that block's number. The second reads the implementation slot again, at
//! that block, and each listing's settings, controls and session words from the front's storage,
//! and asks each door with code for its front, listing and base and for the most each side fills
//! (`quoteFillable`). The third asks each side's `quote` at the sizes sampled, at the block and
//! again under block overrides at the end of the book's life, on a listing reading a guarded
//! source at each block of its lane's hold, and on a listing that reads a price print, whether the
//! door still fills each side's smallest and largest size at the last block of each second between
//! (see [`DoorsReader::sample`]); it asks a guarded source's lane its hold the first time. Calls to
//! the doors may fail (a door refuses a size by reverting), so each is allowed to; the calls of a
//! batch are split over several `aggregate3` calls so that none needs more gas than a node grants
//! one `eth_call`.
//!
//! On a chain whose builders land makers' updates ([`DoorsReader::read_next_block`]), a read that
//! finds everything its doors' answers are read from as the last sampled read found it asks no
//! door and is answered that read's samples ([`SampledOn`]).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{
        address, aliases::U24, b256, keccak256, Address, Bytes as AlloyBytes, B256, U256, U64,
    },
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getChainIdCall, getCurrentBlockTimestampCall,
            Call3, Result as Call3Result,
        },
        Provider, RootProvider, MULTICALL3_ADDRESS,
    },
    rpc::{
        client::{BatchRequest, RpcClient, Waiter},
        json_rpc::{RequestPacket, ResponsePacket},
        types::{TransactionInput, TransactionRequest},
    },
    sol,
    sol_types::{SolCall, SolValue},
    transports::{utils::guess_local_url, TransportError, TransportErrorKind, TransportFut},
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use tycho_common::models::Chain;

use crate::{
    pamm::protocols::efficient_prop::{
        desks::ApprovedDesks,
        ladder::FillPath,
        overrides::{Cadence, NextBlock, NextBlockReader, Wanted, SLOT_SECS},
        slots::{HeadWords, ListingSlots, SLOT_RULE},
        state::{CreditGas, FillReads, PairPlace},
        telemetry::{self, NotCovered, NotKept, PollOutcome},
    },
    snapshot_feed::errors::FeedError,
};

/// EIP-1967's implementation slot, `keccak256("eip1967.proxy.implementation") - 1`.
pub(super) const IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// The namespace of the front's store (`PropAMMStore`), whose slot 18 is the `pools` map from a
/// listing id to its record. The implementation allow-list pins the layout.
const STORE_NAMESPACE: B256 =
    b256!("ca023b73b2185d1ba92ba3c09f7870c56c9bdadd91407b68ebb408947da60b00");

/// The `pools` map's slot in the front's store, past its namespace.
const POOLS_INDEX: u8 = 18;

/// A listing record's settings word (`P1`), read for its fence kind, whether it is tied, and its
/// least fill.
pub(super) const SETTINGS_WORD: u8 = 1;

/// A listing record's copy of its live price source (`P2`), read for the source's stored price
/// lane (bits 0-159 on a storage build, as Base's front is) and its guard bit.
pub(super) const PRICE_WORD: u8 = 2;

/// The live source's guard bit in its copy (`PropAMMWords.E_GUARD`): a storage build's guarded
/// source, which a fill reads for its side's worst price over the prints its lane still holds.
const GUARD_BIT: usize = 247;

/// The longest a price lane holds a print a post replaced, ms (`PythLane.HOLD_MS`: 400 ms where
/// the fronts' clock steps 200 ms, 2 s where it reads the block's second): the hold sampled on a
/// guarded source whose lane does not answer its own.
const MAX_HOLD_MS: u64 = 2_000;

/// A tied listing's scopes, its settings word's bits 228-231 read as one number
/// (`PropAMMWords.TIED_SCOPES`): the fence kind PRINT, no carry life, and the tied bit.
const TIED_SCOPES: u64 = 8;

/// A listing record's controls word, read for how its levels age.
const CONTROLS_WORD: u8 = 7;

/// A listing record's board words, the fill memory of each side's depth post: the base in's, then
/// the quote in's (see [`RungPost`]).
const BOARD_WORDS: [u8; 2] = [8, 9];

/// The controls word's flag that lets a standing word refill a side, its walk then starting from
/// the post's first rung (`PropAMMWords.CTL_REFILL`).
const CONTROLS_REFILL_BIT: usize = 66;

/// The version of a depth post whose rungs [`RungPost`] reads: a depth word's bits 0-3.
const DEPTH_VERSION: u64 = 1;

/// The most rungs a side's depth post holds, three in its first depth word and seven in its
/// second (`PropAMMFront.MAX_RUNGS`).
const MAX_RUNGS: usize = 10;

/// The largest size exponent a depth post holds, and board exponent a pair word holds, that the
/// front writes: a post past either is not one [`RungPost`] reads.
const MAX_DEPTH_EXP: u64 = 30;
const MAX_BOARD_EXP: u64 = 16;

/// A listing record's session word, read for a SESSION listing's closed phase: its `closedAfter`,
/// bits 0-15, is zero where it has none (`PropAMMWords`, session word).
const SESSION_WORD: u8 = 13;

/// A listing record's bound price sources (`sourceIds`, `setSource`): the primary's id at bits
/// 0-31 and the backup's at 32-63, 0 for none. A fill reads the live one's copy (word 2), which
/// the front writes only from these, and a source's registration is never edited.
pub(super) const SOURCES_WORD: u8 = 10;

/// The pair word's bit set on an asset-module pair (`PropAMMWords.PAIR_ASSET`).
const PAIR_ASSET_BIT: usize = 15;

/// Milliseconds per block of the front's clock in its block-count mode (`PropAMMClock.BLOCK_MS`).
const BLOCK_MS: u64 = 200;

/// The front's clock mode that reads the block's second alone (`PropAMMClock.SEAL`).
const CLOCK_SEAL: u64 = 0;

/// The front's clock mode that counts milliseconds from the block number (`PropAMMClock.BLOCKS`).
const CLOCK_BLOCKS: u64 = 2;

/// The front's clock mode that reads the millisecond timestamp precompile, BNB Chain's BEP-706
/// (`PropAMMClock.PRECOMPILE`), and the start of the block's second where it answers nothing.
const CLOCK_PRECOMPILE: u64 = 3;

/// The BaseTime predeploy, whose `timestampMs()` answers the block's milliseconds: what the
/// front's clock reads in its modes 1 and 2 where it does not count the block
/// (`PropAMMClock.BASE_TIME`). A Base front refuses mode 3 at its construction.
const BASE_TIME: Address = address!("4200000000000000000000000000000000000030");

/// The millisecond timestamp precompile of BNB Chain's BEP-706, which a front in mode 3 reads:
/// before the precompile exists the address answers nothing, and the front reads the start of
/// the block's second, as in mode 0. A BNB front is served only while it answers nothing (see
/// [`DoorsReader::read_listings`]).
const MS_PRECOMPILE: Address = address!("0000000000000000000000000000000000000070");

/// Calls per `aggregate3`. A door's `quote` costs about 70k to 110k gas, so a full call stays
/// near 5M, well inside the gas any node grants an `eth_call`. A credit listing's routed size
/// costs more: its door's `quote` runs the pool's own preview through its venue, and its plan
/// read rides beside it. Measured on the first Ethereum pool (Morpho Blue, three positions): a
/// routed quote 563k to 684k, a refused one up to 740k, a plan 107k, so a full call of routed
/// quotes is about 36M, inside the 50M most nodes grant an `eth_call` by default. A pool on a
/// dearer lender spends its own; a chunk the node's `eth_call` gas cap starves fails the poll, as
/// any refused chunk does.
pub(super) const CALLS_PER_MULTICALL: usize = 48;

/// The tip word's bit that asks the front's tip-taking `quoteFrom` for the credit route's planned
/// output where its plan takes the route (`PropAMMFront.PLAN_ONLY`), at a tip of zero.
const PLAN_ONLY: usize = 253;

/// The bit the front sets on that answer where the plan takes the credit route
/// (`PropAMMFront.ROUTED`); its custody answer is untagged.
const ROUTED: usize = 255;

/// The highest credit mode the front knows (`PropAMMWords.CREDIT_ALWAYS`): modes 1 and 2 route
/// where custody falls short, mode 3 every fill.
const MAX_CREDIT_MODE: u8 = 3;

/// The input the most-fillable read asks for, in whole tokens: 10^12, past any listing's depth,
/// so the answer is the listing's own prefix.
const PROBE_WHOLE_TOKENS_EXP: u32 = 12;

/// How many polls in a row may find the node refusing a block named by its hash, and answering
/// the same read by number, before the reader sets the hash form aside (see [`HashForm`]).
const HASH_FORM_REFUSALS: u32 = 3;

/// For how many heads the hash form is set aside before the reader tries it once more.
const HASH_FORM_RETRY_HEADS: u64 = 32;

/// The JSON-RPC error codes by which a node says it does not take a request's form: invalid
/// params and method not found. A node that reads a block by number or tag alone answers a
/// request naming its block by hash with one of them. A block it does not know, a timeout or a
/// transport failure is none of these, and says nothing of the form.
const FORM_REFUSED_CODES: [i64; 2] = [-32602, -32601];

// The front's listing view, the factory's door address and the listing door's IPropAMM surface,
// as the PropAMM front, `PropAMMListingDoors` and `PropAMMListingDoor` declare them.
sol! {
    #[derive(Debug, Default)]
    struct DoorView {
        uint256 pairWord;
        address token0;
        address token1;
        address pool;
        uint8 baseIndex;
        uint8 quoteIndex;
        uint8 flags;
        uint32 keyEpoch;
        uint32 desk;
        uint8 wordIndex;
        uint8 scopes;
        uint32 fenceBidPpm;
        uint32 fenceAskPpm;
        uint16 tipCap;
        uint8 confMul;
        uint8 creditMode;
        uint48 bidFillMs;
        uint48 askFillMs;
        uint256 ticket;
        uint256 word;
        uint256[4] depth;
    }

    /// The front's maker-time windows and clock (`PropAMMClock.Timing`).
    #[derive(Debug, Default)]
    struct Timing {
        uint256 maxLeadMs;
        uint256 maxTtlMs;
        uint256 pullLeadMs;
        uint256 ageSlackMs;
        uint256 clockMode;
        uint256 clockBlock;
        uint256 clockMs;
    }

    function doorView(uint24 listing) external view returns (DoorView memory);
    function timestampMs() external view returns (uint256);
    function clock() external view returns (Timing memory);
    function doorOf(uint24 listing) external view returns (address);
    function deskCellOf(uint32 desk) external view returns (address);
    function FRONT() external view returns (address);
    function DOOR_CODEHASH() external view returns (bytes32);
    function CELL_CODEHASH() external view returns (bytes32);
    function LISTING() external view returns (uint24);
    function BASE() external view returns (address);
    function quote(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut);
    function quoteFillable(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 fillableAmountIn, uint256 amountOut);
    function balanceOf(address account) external view returns (uint256);
    /// The front's tip-taking quote of one listing, the one the door's `quote` asks: with bit 253
    /// of `tip` set, where the plan takes the credit route, the route's planned output tagged with
    /// bit 255, else the custody answer untagged.
    function quoteFrom(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint24 listing,
        uint256 tip,
        address taker
    ) external view returns (uint256 amountOut);
    /// How long a price lane holds a print a post replaced, on the fronts' clock, ms.
    function HOLD_MS() external view returns (uint256);
}

/// One side of a door as sampled: the sizes asked, each with the door's output or `None` where
/// it refused the size.
pub(super) type Samples = Vec<(U256, Option<U256>)>;

/// A credit listing's read: the least size of each side its credit route fills, and the gas
/// configured for its fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CreditRead {
    pub gas: CreditGas,
    /// The least bid size whose sample the credit route fills: every sample kept at or above it
    /// routes, and every one below it fills inside the pool's custody. `None` where no bid sample
    /// routes.
    pub bid_route_from: Option<U256>,
    /// The same for the ask.
    pub ask_route_from: Option<U256>,
}

/// One door's answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DoorRead {
    pub listing: u32,
    pub door: Address,
    pub base: Address,
    pub quote: Address,
    /// Selling base for quote, in increasing size: at each, the least the door quoted at the read
    /// and later in the side's life (see [`DoorsReader::sample`]).
    pub bid: Samples,
    /// Selling quote for base, in increasing size, as the bid.
    pub ask: Samples,
    /// Whether the bid's samples hold for its life (`bid_until_ms`): false where the read could
    /// not show what the door quotes later in it, on a listing whose levels may age within it or
    /// that reads a price print.
    pub bid_holds: bool,
    /// The same for the ask.
    pub ask_holds: bool,
    /// The end of the bid's own life on the front's clock, ms: the book's life's end, or sooner
    /// where the bid reads a print that lapses within it, one past the last block it lives at.
    pub bid_until_ms: u64,
    /// The same for the ask.
    pub ask_until_ms: u64,
    /// The first millisecond, on the front's clock, at which the bid fills nothing: see
    /// [`runs_out_ms`].
    pub bid_runs_out_ms: u64,
    /// The same for the ask.
    pub ask_runs_out_ms: u64,
    /// The listing's taker tip cap, wei per gas above the base fee; `None` where it has none.
    pub tip_cap_wei: Option<U256>,
    /// What a fill reads besides the word: a price print from a guarded source, a price source
    /// otherwise where the listing reads a print or its pair is an asset-module pair, whose fill
    /// asks the module too, else the word alone.
    pub reads: FillReads,
    /// Where the listing's pair sits in its pool: its hook pair (the pool's first listing, whose
    /// base is the pool's token 0 and quote its token 1), or past it with the pool's asset as its
    /// base or its quote, or without it.
    pub pair: PairPlace,
    /// On a credit listing served, which sizes its credit route fills and the gas configured
    /// for it; `None` on any other listing.
    pub credit: Option<CreditRead>,
    /// On a listing of an asset-module pair with no credit mode, the gas configured for it, whose
    /// custody pair its fills are charged; `None` on any other listing.
    pub named_gas: Option<CreditGas>,
    /// Whether the listing's pool held none of the base and none of the quote token at the block
    /// read, where the read asks (Ethereum's): a fill that pays such a token into the pool writes
    /// the pool's balance of it from zero, which costs more (see `FIRST_BALANCE_WRITE_GAS` in
    /// `state.rs`). False where the balance was not read.
    pub pool_lacks: (bool, bool),
}

/// Everything one read returned, at one block.
#[derive(Debug, Clone)]
pub(super) struct DoorsRead {
    pub block_number: u64,
    pub timestamp: u64,
    /// The front's clock at the read's block.
    #[cfg(test)]
    pub clock: FrontClock,
    /// The end of the book's life on the front's clock, ms: the samples hold at every block
    /// before it.
    pub until_ms: u64,
    /// How far the front's clock runs ahead of the wall clock, ms: from the poll's start on the
    /// wall clock to the book's life's start on the front's, zero where the front's lags.
    pub lead_ms: u64,
    /// The highest listing id the front has reserved, among those scanned, served or not.
    pub highest_listing: u32,
    pub doors: Vec<DoorRead>,
    /// Whether each side's life is the blocks it was sampled at, a builder chain's next block and,
    /// where its listing carries, the one after: every side quoting is then served only until its
    /// own life's end less the landing allowance, and `until_ms` is the latest of those lives.
    pub block_lives: bool,
    /// Whether the read was answered at `block_number`, which no later read may fall behind.
    /// False where a builder chain's poll found no state map for the next block and served
    /// nothing: its `block_number` is then the node's latest, which may be behind a block an
    /// earlier poll served, and the read neither fails against that block nor raises it.
    pub answered_at_block: bool,
    /// How the read ended, as its poll is counted where it publishes.
    pub outcome: PollOutcome,
    /// The milliseconds of the head the sides are timed from, where the chain names them and
    /// the read timed its sides by them (BNB Chain): a wall clock reading earlier at the poll's
    /// end is behind the chain's.
    pub head_ms: Option<u64>,
}

/// How long a read's books must hold: from the later of `from_ms` (Unix ms, the wall clock when
/// the poll started) and the read block's time on the front's clock, `for_ms` more. A read whose
/// block's second is more than `max_lag_secs` behind `from_ms`'s fails at its first request. A
/// side whose print lapses within that life is served for a shorter one, ending with its print,
/// where that leaves at least `min_side_ms` after the life's start (see [`DoorsReader::sample`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct Hold {
    pub from_ms: u64,
    pub for_ms: u64,
    pub max_lag_secs: u64,
    pub min_side_ms: u64,
}

/// What one side of a listing reads from its depth word: when it runs out, how fast its levels
/// drift (ppm a second) and the depth's nonce (ms), and its rungs where its words hold a post of
/// the layout [`RungPost`] reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SideView {
    runs_out_ms: u64,
    drift: u64,
    nonce_ms: u64,
    post: Option<RungPost>,
}

/// A side's depth post, as the front walks it (`PropAMMFront._walk`): at most ten rungs from the
/// side's fill memory, each filled at one price, the output floored once. Between two rung
/// boundaries the output is therefore one straight line of the input floored, and a side sampled
/// at every boundary its walk crosses is quoted within a unit of the door wherever the rungs'
/// prices worsen along the walk, as a maker's post does: the chord between two samples on one
/// line is under it by less than the floor of either end (see `ladder.rs`).
///
/// The boundaries are only sizes the door is asked at, never prices: a post read wrong moves a
/// sample, and every quote is still the door's own answer or a floored chord between two of
/// them. The depth words are `doorView`'s, the bid's `depth[0..2]` and the ask's `depth[2..4]`:
/// the version (bits 0-3), the size exponent (bits 112-117), whether the second word holds rungs
/// (bit 118), and each rung's cumulative size in its low 16 of 36 bits, three from bit 120 of the
/// first word and seven from bit 0 of the second. A size counts 10 to the size exponent board
/// units, and a board unit 10 to the side's board exponent of its input token (the pair word's
/// bits 112-119 for the base in, 120-127 for the quote in). The fill memory is the side's board
/// word ([`BOARD_WORDS`]): the board units filled (bits 52-91), which count only while its tag
/// (bits 4-51) is the depth's nonce.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RungPost {
    /// Each rung's cumulative size, in size units, zero past the last.
    sizes: [u16; MAX_RUNGS],
    /// Input atoms a size unit counts.
    scale: U256,
    /// Input atoms a board unit counts.
    unit: U256,
}

impl RungPost {
    /// The post of a side whose depth words are `first` and `second`, on a pair whose word is
    /// `pair_word`, the bid's where `bid`: `None` where the words are not of the layout read
    /// here, a depth word of another version or an exponent past the front's.
    fn of(first: U256, second: U256, pair_word: U256, bid: bool) -> Option<Self> {
        let size_exp = field(first, 112, 6);
        let unit_exp = field(pair_word, if bid { 112 } else { 120 }, 8);
        if field(first, 0, 4) != DEPTH_VERSION ||
            size_exp > MAX_DEPTH_EXP ||
            unit_exp > MAX_BOARD_EXP
        {
            return None;
        }
        let ten = U256::from(10u8);
        let unit = ten.pow(U256::from(unit_exp));
        let mut sizes = [0u16; MAX_RUNGS];
        for (j, size) in sizes.iter_mut().enumerate() {
            if j == 3 && !first.bit(118) {
                break;
            }
            let rung = if j < 3 {
                field(first, 120 + 36 * j, 36)
            } else {
                field(second, 36 * (j - 3), 36)
            };
            *size = u16::try_from(rung & 0xffff).unwrap_or_default();
            if *size == 0 {
                break;
            }
        }
        Some(Self { sizes, scale: ten.pow(U256::from(size_exp)) * unit, unit })
    }

    /// The inputs at which a walk from a fill memory of `filled` board units leaves each rung it
    /// reaches, ascending: none of a rung the memory has used up, nor of one whose size does not
    /// rise past the one before it.
    fn edges(&self, filled: U256) -> Vec<U256> {
        let cursor = filled.saturating_mul(self.unit);
        let mut edges: Vec<U256> = Vec::new();
        for size in self
            .sizes
            .iter()
            .take_while(|size| **size != 0)
        {
            let end = U256::from(*size).saturating_mul(self.scale);
            if end <= cursor {
                continue;
            }
            let edge = end - cursor;
            if edges
                .last()
                .is_some_and(|last| edge <= *last)
            {
                continue;
            }
            edges.push(edge);
        }
        edges
    }
}

/// A listing the front has reserved, and where its door would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listing {
    id: u32,
    /// The hash of the front's whole `doorView` answer for it, every word of it: the listing's
    /// pool, flags, word, depths and ticket among them, read or not (see [`SampledOn`]).
    view: B256,
    token0: Address,
    token1: Address,
    door: Address,
    /// The standing word's maker time, ms.
    word_ms: u64,
    /// Selling the base, from `doorView`'s first depth word, and selling the quote, from its
    /// third: by role, whichever of the pair's two tokens the base is and wherever it sits in its
    /// pool.
    bid: SideView,
    ask: SideView,
    tip_cap_wei: Option<U256>,
    /// The desk id bound to it, 0 for none: `doorView`'s `desk`.
    desk: u32,
    /// Its pair's word index in its desk contract: `doorView`'s `wordIndex`.
    word_index: u8,
    /// Whether its pair is an asset-module pair, `doorView`'s pair word bit 15.
    asset_module: bool,
    /// Where its pair sits in its pool: `doorView` places any listing but the hook pair's at other
    /// indices than base 0 and quote 1, and the pool's asset is its token 0.
    pair: PairPlace,
    /// Where its credit mode is set and it is served, the gas configured for its fills.
    credit: Option<CreditGas>,
    /// On a listing of an asset-module pair with no credit mode, the gas configured for it: its
    /// fills are charged that figure's custody pair.
    named_gas: Option<CreditGas>,
    /// The pool whose custody pays its fills: `doorView`'s `pool`.
    pool: Address,
}

/// How a listing's levels age, from its controls word (`PropAMMWords`, controls): the front
/// takes a discount off them for the word's age past `grace_ms` (`widen` ppm per square root
/// second, beside each side's drift), and the `premium` for `window_ms` after the latest of the
/// word's maker time, the side's depth nonce and the controls' nonce (`nonce_ms`).
#[derive(Debug, Clone, Copy, Default)]
struct Aging {
    widen: u64,
    premium: u64,
    window_ms: u64,
    grace_ms: u64,
    nonce_ms: u64,
}

impl Aging {
    fn from_controls(controls: U256) -> Self {
        Self {
            nonce_ms: field(controls, 16, 48),
            widen: field(controls, 136, 20),
            premium: field(controls, 156, 20),
            window_ms: field(controls, 176, 16),
            grace_ms: field(controls, 192, 16),
        }
    }
}

/// One side of an open door: what it fills now and how its levels age.
#[derive(Debug, Clone, Copy, Default)]
struct OpenSide {
    /// The most it fills; zero where it fills nothing.
    fillable: U256,
    /// What it pays for that.
    fillable_out: U256,
    /// The input that read asked for (see [`probe`]).
    probe: U256,
    view: SideView,
    /// Its board word at the read's block, where the node answered it ([`BOARD_WORDS`]).
    board: Option<U256>,
}

impl OpenSide {
    /// The inputs at which the front's walk leaves each rung of its depth post, from its fill
    /// memory at the read and, where its standing word may refill it (`refills`), from the
    /// post's start too, since a word in the next block's state may ask the refill: none where
    /// its post or its board word is not read (see [`RungPost`]).
    fn rung_edges(&self, refills: bool) -> Vec<U256> {
        let (Some(post), Some(board)) = (self.view.post, self.board) else {
            return Vec::new();
        };
        let filled = if field(board, 4, 48) == self.view.nonce_ms {
            U256::from(field(board, 52, 40))
        } else {
            U256::ZERO
        };
        let mut edges = post.edges(filled);
        if refills && !filled.is_zero() {
            edges.extend(post.edges(U256::ZERO));
        }
        edges
    }
}

/// The largest of a credit listing's first asks its door answered on one side, and what it pays
/// for it (see [`DoorsReader::reaches`]): zero where it answered none, or the side is not asked.
#[derive(Debug, Clone, Copy, Default)]
struct Reach {
    size: U256,
    output: U256,
    /// The next size asked above it, which the door refused; zero where it answered the largest.
    refused: U256,
}

/// A door that answered as one of the front's, with the most each side fills.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent read of the listing's words"
)]
struct OpenDoor {
    listing: u32,
    door: Address,
    base: Address,
    quote: Address,
    word_ms: u64,
    /// The base in.
    bid: OpenSide,
    /// The quote in.
    ask: OpenSide,
    aging: Aging,
    /// Whether the listing reads a price print: its fence kind PRINT or SESSION.
    prints: bool,
    /// Where it reads a print from a guarded source, that source's price lane, which prices a
    /// fill's side at the worst of the prints it holds.
    guarded_lane: Option<Address>,
    /// Whether its pair is an asset-module pair.
    asset_module: bool,
    /// Where its pair sits in its pool.
    pair: PairPlace,
    /// Whether its controls let a standing word refill a side ([`CONTROLS_REFILL_BIT`]).
    refills: bool,
    /// The least fill, of the quote token: the bid's output or the ask's input.
    min_fill: U256,
    tip_cap_wei: Option<U256>,
    /// Whether its word may fill in the block after its landing: the listing's carry life (its
    /// settings word's bits 230-231) is set and its fence kind is not NONE. The word's own carry
    /// is the maker's, which the door's answer in that block shows.
    carries: bool,
    /// Where its credit mode is set and it is served, the gas configured for its fills: its
    /// sizes are then classified by the path the front's plan takes.
    credit: Option<CreditGas>,
    /// On a listing with no credit mode, the gas configured for it, if any.
    named_gas: Option<CreditGas>,
    /// The pool whose custody pays its fills: a credit listing sharing it with another door
    /// sampled that its route could move is served only the sizes the custody fills (see
    /// [`DoorsReader::sample_next_block`]).
    pool: Address,
}

impl OpenDoor {
    /// What its fill reads besides the word, which the gas charged follows.
    fn fill_reads(&self) -> FillReads {
        if self.guarded_lane.is_some() {
            FillReads::GuardedSource
        } else if self.prints || self.asset_module {
            FillReads::Source
        } else {
            FillReads::Word
        }
    }
}

/// A block an `eth_call` runs at under a block override: its number and time, and the front's
/// clock there, ms, and whether that reads the block's own milliseconds (`exact`) rather than the
/// start of its second; and, where the clock there reads BaseTime, the sub-second BaseTime stores
/// at that block, ms, which the call sets with a state override (see [`state_override`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct At {
    pub number: u64,
    pub time: u64,
    pub ms: u64,
    pub exact: bool,
    pub base_time: Option<u64>,
}

/// The overriding fields of an `eth_call`'s block override, as nodes take them.
#[derive(Debug, Clone, Serialize)]
struct BlockOverride {
    number: U64,
    time: U64,
}

/// The front's clock at the read's block, as `PropAMMClock.read` reads it, and at the blocks after
/// it. In mode 2 it counts 200 ms a block from its anchor where that lands in the block's second;
/// otherwise, and in mode 1, it reads the BaseTime predeploy where that answers within the block's
/// second; otherwise, and in mode 0, the start of the block's second, to which the age clock adds
/// the front's slack.
///
/// The blocks after the read are those the chain makes: on a counted clock one every 200 ms, the
/// count staying in each block's second. Where the clock reads its millisecond source instead, one
/// every 200 ms as well, from the source's reading at the read's block: Base makes a block every
/// 200 ms whatever the front counts, and BaseTime only stores whole 200 ms of a second. BaseTime
/// answers the block's second plus the sub-second it stores, which the chain writes each block and
/// a block override does not move, so each override also sets BaseTime's stored sub-second to its
/// block's own with a state override. Where neither the count nor the source lands in the block's
/// second, one block a second, read at the start of each.
///
/// On each of those clocks a later block never reads earlier. In mode 2 with the count off the
/// source's reading by less than a second, the front reads the count in the blocks where it lands
/// in the block's second and the source in the others, so its clock can step back within a second
/// ([`Self::may_step_back`]). That clock is not modelled: a front whose clock can step back is
/// not served.
#[derive(Debug, Clone, Copy)]
pub(super) struct FrontClock {
    mode: u64,
    block0: u64,
    ms0: u64,
    number: u64,
    timestamp: u64,
    ms: u64,
    exact: bool,
    /// Whether the read's milliseconds are counted from its block number.
    counted: bool,
    /// How far into the block's second the millisecond source read at the read's block, ms;
    /// `None` where it did not answer within that second.
    source_sub_ms: Option<u64>,
    age_slack_ms: u64,
}

impl FrontClock {
    /// The clock at block `number` of time `timestamp`, where the front's millisecond source
    /// ([`Self::source`]) answered `source_ms`.
    pub(super) fn new(
        timing: &Timing,
        number: u64,
        timestamp: u64,
        source_ms: Option<u64>,
    ) -> Self {
        let mut clock = Self {
            mode: timing.clockMode.saturating_to(),
            block0: timing.clockBlock.saturating_to(),
            ms0: timing.clockMs.saturating_to(),
            number,
            timestamp,
            ms: 0,
            exact: false,
            counted: false,
            source_sub_ms: source_ms
                .and_then(|ms| ms.checked_sub(timestamp.saturating_mul(1000)))
                .filter(|sub| *sub < 1000),
            age_slack_ms: timing.ageSlackMs.saturating_to(),
        };
        (clock.ms, clock.exact) = clock.reads(number, timestamp, source_ms);
        clock.counted = clock.counted_ms(number) == Some(clock.ms) && clock.exact;
        clock
    }

    /// The clock at a later block `number` of time `timestamp` on a front reading the block's
    /// second: a builder chain's next block, which the read samples under overrides.
    pub(super) fn at_block(&self, number: u64, timestamp: u64) -> Self {
        let mut clock = Self { number, timestamp, source_sub_ms: None, counted: false, ..*self };
        (clock.ms, clock.exact) = clock.reads(number, timestamp, None);
        clock
    }

    /// The contract the front's clock reads milliseconds from, where its mode reads one.
    pub(super) fn source(&self) -> Option<Address> {
        match self.mode {
            CLOCK_SEAL => None,
            CLOCK_PRECOMPILE => Some(MS_PRECOMPILE),
            _ => Some(BASE_TIME),
        }
    }

    /// Milliseconds counted from the anchor at block `number`, in the block-count mode.
    fn counted_ms(&self, number: u64) -> Option<u64> {
        (self.mode == CLOCK_BLOCKS && number >= self.block0).then(|| {
            self.ms0
                .saturating_add((number - self.block0).saturating_mul(BLOCK_MS))
        })
    }

    /// What `PropAMMClock.read` answers at block `number` of time `time`, where the millisecond
    /// source answers `source_ms`: the clock, ms, and whether it is the block's own.
    pub(super) fn reads(&self, number: u64, time: u64, source_ms: Option<u64>) -> (u64, bool) {
        if self.mode != CLOCK_SEAL {
            if let Some(ms) = self
                .counted_ms(number)
                .filter(|ms| ms / 1000 == time)
            {
                return (ms, true);
            }
            if let Some(ms) = source_ms.filter(|ms| ms / 1000 == time) {
                return (ms, true);
            }
        }
        (time.saturating_mul(1000), false)
    }

    /// The front's clock at the read's block, ms.
    #[cfg(test)]
    pub(super) fn ms(&self) -> u64 {
        self.ms
    }

    /// The read's block.
    fn read_at(&self) -> At {
        At {
            number: self.number,
            time: self.timestamp,
            ms: self.ms,
            exact: self.exact,
            base_time: None,
        }
    }

    /// The source's reading at the read's block, ms, where the clock steps on it: where it does
    /// not count blocks and the source answered within the block's second.
    fn stepped_source_ms(&self) -> Option<u64> {
        if self.counted || self.mode == CLOCK_SEAL {
            return None;
        }
        self.source_sub_ms
            .map(|sub| self.timestamp.saturating_mul(1000) + sub)
    }

    /// Whether the front's clock may read earlier at a later block: in its block-count mode, with
    /// the count off the millisecond source's reading by less than a second, it reads the count in
    /// the blocks where that lands in the block's second and the source in the others. On Base the
    /// count sits on BaseTime's reading, and a block cadence other than 200 ms after the anchor
    /// would move it off; a front whose clock may step back is not served.
    fn may_step_back(&self) -> bool {
        let (Some(count), Some(sub)) = (self.counted_ms(self.number), self.source_sub_ms) else {
            return false;
        };
        (1..1000).contains(&count.abs_diff(self.timestamp.saturating_mul(1000) + sub))
    }

    /// The front's age clock at `at`: the slack added to a reading of the start of a second.
    fn age_ms(&self, at: &At) -> u64 {
        if at.exact {
            at.ms
        } else {
            at.ms.saturating_add(self.age_slack_ms)
        }
    }

    /// The last block at or before `target_ms` on the front's clock, and no earlier than the
    /// read's, as a block override.
    pub(super) fn at_or_before(&self, target_ms: u64) -> At {
        if self.counted {
            let blocks = target_ms.saturating_sub(self.ms) / BLOCK_MS;
            let ms = self.ms + blocks * BLOCK_MS;
            return At {
                number: self.number + blocks,
                time: ms / 1000,
                ms,
                exact: true,
                base_time: None,
            };
        }
        if let Some(source0) = self.stepped_source_ms() {
            // A block every 200 ms from the source's reading, BaseTime set to each block's own.
            let base_time = self.source() == Some(BASE_TIME);
            for blocks in (1..=target_ms.saturating_sub(source0) / BLOCK_MS).rev() {
                let source_ms = source0 + blocks * BLOCK_MS;
                let (number, time) = (self.number + blocks, source_ms / 1000);
                let (ms, exact) = self.reads(number, time, Some(source_ms));
                if ms <= target_ms {
                    let base_time = base_time.then_some(source_ms % 1000);
                    return At { number, time, ms, exact, base_time };
                }
            }
            return self.read_at();
        }
        let mut time = target_ms / 1000;
        while time > self.timestamp {
            let number = self.number + (time - self.timestamp);
            let (ms, exact) = self.reads(number, time, None);
            if ms <= target_ms {
                return At { number, time, ms, exact, base_time: None };
            }
            time -= 1;
        }
        self.read_at()
    }

    /// The last block whose clock reads before `target_ms`, where it is later than the read's.
    pub(super) fn last_before(&self, target_ms: u64) -> Option<At> {
        let at = self.at_or_before(target_ms.checked_sub(1)?);
        (at.number > self.number).then_some(at)
    }

    /// The first block whose clock reads at or after `target_ms`, where it is later than the
    /// read's: the chain makes one every [`Self::step_ms`].
    pub(super) fn first_from(&self, target_ms: u64) -> Option<At> {
        let at = self.at_or_before(target_ms.saturating_add(self.step_ms() - 1));
        (at.number > self.number && at.ms >= target_ms).then_some(at)
    }

    /// How far apart the blocks after the read are on the front's clock: 200 ms where it counts
    /// them or steps on its millisecond source, a second where it reads the start of each.
    fn step_ms(&self) -> u64 {
        if self.counted || self.stepped_source_ms().is_some() {
            BLOCK_MS
        } else {
            1000
        }
    }

    /// The last block of each second of a book's life ending at `until_ms`, as
    /// [`Self::at_or_before`] picks them: those after the read's block and before the life's
    /// last, which the read and the life's end sample already.
    pub(super) fn seconds_until(&self, until_ms: u64) -> Vec<At> {
        let last_ms = until_ms.saturating_sub(1);
        let until = self.at_or_before(last_ms);
        let mut blocks: Vec<At> = Vec::new();
        for second in self.ms / 1000..=last_ms / 1000 {
            let at = self.at_or_before(
                second
                    .saturating_mul(1000)
                    .saturating_add(999)
                    .min(last_ms),
            );
            if at.number > self.number && at != until && blocks.last() != Some(&at) {
                blocks.push(at);
            }
        }
        blocks
    }

    /// Every block after `from` and before `to`, as [`Self::at_or_before`] picks them.
    fn blocks_between(&self, from: &At, to: &At) -> Vec<At> {
        let step = self.step_ms();
        let mut blocks: Vec<At> = Vec::new();
        let mut target = from.ms.saturating_add(step);
        while target < to.ms {
            let at = self.at_or_before(target);
            if at.number > from.number && at.number < to.number && blocks.last() != Some(&at) {
                blocks.push(at);
            }
            target = target.saturating_add(step);
        }
        blocks
    }

    /// The blocks after the read's at which a print a post replaced at or before the read may still
    /// be held, `hold_ms` on the front's clock, and the first past that, at which the side prices
    /// without it, as [`Self::at_or_before`] picks them and through `until` at the latest.
    pub(super) fn blocks_held(&self, hold_ms: u64, until: &At) -> Vec<At> {
        if hold_ms == 0 || until.number <= self.number {
            return Vec::new();
        }
        let end = self
            .first_from(self.ms.saturating_add(hold_ms))
            .filter(|end| end.ms <= until.ms)
            .unwrap_or(*until);
        let mut blocks = self.blocks_between(&self.read_at(), &end);
        blocks.push(end);
        blocks
    }

    /// Every block from the read's to the last before `until_ms`, as [`Self::at_or_before`] picks
    /// them.
    #[cfg(test)]
    pub(super) fn blocks_until(&self, until_ms: u64) -> Vec<At> {
        let step = self.step_ms();
        let mut blocks: Vec<At> = Vec::new();
        let mut target = self.ms;
        while target < until_ms {
            let at = self.at_or_before(target);
            if blocks.last() != Some(&at) {
                blocks.push(at);
            }
            target += step;
        }
        blocks
    }
}

/// One `aggregate3` call of a batch: awaiting its answer, with how many calls it carries and the
/// key it is kept under where its block is named by hash, or answered already at that block.
enum SentChunk {
    Sent { waiter: Waiter<AlloyBytes>, count: usize, key: Option<B256> },
    Kept(Vec<Call3Result>),
}

/// One storage word of a batch: awaiting its answer, or answered already at its block.
enum SentWord {
    Sent(Waiter<U256>),
    Kept(U256),
}

impl SentWord {
    async fn answer(self) -> Result<U256, TransportError> {
        match self {
            Self::Sent(waiter) => waiter.await,
            Self::Kept(word) => Ok(word),
        }
    }
}

/// One `aggregate3` group of a batch: calls run at the batch's block, or under a block override.
struct Group {
    calls: Vec<Call3>,
    at: Option<At>,
}

/// What a batch reads of its block beside its calls and storage words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Also {
    Nothing,
    /// The front's implementation slot, checked against the allow-list before any answer is
    /// looked at.
    Implementation,
    /// That, and the block's header, for its hash.
    ImplementationAndHead,
    /// That, and its parent's header: on BNB Chain, for the time between the two.
    ImplementationAndHeads,
    /// The implementation, for a batch pinned to the hash of that head: and the head's parent's
    /// header by its number, for the time between the two, which the head's header names no more
    /// than the hash of the parent it is checked against.
    ImplementationAndParentOf(LatestHead),
    /// The front's implementation slot and the node's gas price: on BNB Chain, whose base fee is
    /// zero, the least tip a taker's swap pays.
    ImplementationAndGasPrice,
}

impl Also {
    fn implementation(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    fn head(self) -> bool {
        matches!(self, Self::ImplementationAndHead | Self::ImplementationAndHeads)
    }

    fn parent(self) -> bool {
        matches!(self, Self::ImplementationAndHeads)
    }

    fn parent_of(self) -> Option<LatestHead> {
        match self {
            Self::ImplementationAndParentOf(latest) => Some(latest),
            Self::Nothing |
            Self::Implementation |
            Self::ImplementationAndHead |
            Self::ImplementationAndHeads |
            Self::ImplementationAndGasPrice => None,
        }
    }

    fn gas_price(self) -> bool {
        matches!(self, Self::ImplementationAndGasPrice)
    }
}

/// A batch's answers: per group, its calls' results, or `None` where the node refused the group's
/// block override or ran its calls at another moment on the front's clock; and the storage
/// words asked for, in order, each failing where the node did not answer it.
struct BatchAnswers {
    groups: Vec<Option<Vec<Call3Result>>>,
    words: Vec<Result<U256, FeedError>>,
    /// The hash of the batch's block, where it was asked and the node answered that block.
    head: Option<B256>,
    /// That block's milliseconds, where its header names them ([`Header::ms`]).
    head_ms: Option<u64>,
    /// The milliseconds between that block and its parent, where both headers were asked and
    /// name their milliseconds, and the parent's hash is the one the block names.
    head_spacing_ms: Option<u64>,
    /// The front's implementation at the batch's block, where it was asked.
    implementation: Option<Address>,
    /// The node's gas price, wei, where it was asked.
    gas_price: Option<U256>,
}

/// A block as `eth_getBlockByNumber` answers it: the fields read.
#[derive(Debug, Deserialize)]
struct Header {
    hash: B256,
    #[serde(default, rename = "parentHash")]
    parent_hash: Option<B256>,
    number: U64,
    #[serde(default)]
    timestamp: Option<U64>,
    /// BNB Chain's milliseconds of the block (BEP-520), where the node names them.
    #[serde(default, rename = "milliTimestamp")]
    milli_timestamp: Option<U64>,
    #[serde(default, rename = "mixHash")]
    mix_hash: Option<B256>,
}

impl Header {
    /// The block's time in milliseconds on BNB Chain: its `milliTimestamp` where the node names
    /// it within the block's second, else the milliseconds BEP-520 keeps in the low eight bytes
    /// of its `mixHash` where they are under a second. `None` where neither is.
    fn ms(&self) -> Option<u64> {
        let seconds = self.timestamp?.to::<u64>();
        let base = seconds.checked_mul(1000)?;
        if let Some(ms) = self
            .milli_timestamp
            .map(|ms| ms.to::<u64>())
            .filter(|ms| ms / 1000 == seconds)
        {
            return Some(ms);
        }
        let low: [u8; 8] = self.mix_hash?.0[24..].try_into().ok()?;
        let sub = u64::from_be_bytes(low);
        (sub < 1000).then(|| base + sub)
    }
}

/// One listing a next-block read took: what the reads before its door's answered of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TakenListing {
    /// The front's `doorView` of it, whole (its `view` hash), and the door derived from it.
    listing: Listing,
    /// The inputs its most-fillable reads ask, from its tokens' configured decimals.
    probes: (U256, U256),
    /// Its record's settings word at the head.
    settings: U256,
    /// Its record's live-source word at the head, where it reads a print.
    price: Option<U256>,
    /// Its desk contract's desk word at the head, whole.
    desk: U256,
    /// Its desk contract, its two slots there, and the pair's own words at the head.
    slots: ListingSlots,
}

/// Everything a next-block read's doors were asked on. A door's answer is an `eth_call`'s: the
/// node's run of the call on one block's state under the overrides sent with it. So two reads
/// agreeing on all of the following ask every door the same calls of the same state, and the
/// second is answered as the first was:
/// - the head: its hash, which stands for every account's state there (the front and its records,
///   the pools and their lenders, the tokens, the desk contracts' own words, the price contracts'
///   code, the doors), and its number, which pins each request;
/// - the block the doors are asked at, its number and its second, the block override;
/// - the state override, whole as it is sent: each listing's own slots of its desk contract and
///   each price contract a listing reads, as the state answer wrote them;
/// - what decides which calls are made: each listing taken with its `doorView`, its record's words,
///   its desk word and its probes, the front's implementation, the tip cap floor and the sizes
///   sampled per side.
///
/// The head's hash is read with the listings' words, in the one batch pinned to the head's
/// number. Every batch that asks a door is then pinned to that hash and to no number (EIP-1898,
/// the block required canonical): the node runs it on that very block or refuses it, so an
/// answer kept is that block's by the node's own check, not by a header read beside it. The
/// words read beside that one header are members themselves and only decide which calls are
/// made. A read is kept only where its doors were so pinned and every word and block asked for
/// them was answered ([`Answered`]). A node that names no block, or refuses a block named by its
/// hash, is asked by number, and nothing of it is kept ([`HashForm`] says for how long).
#[derive(Debug, Clone, PartialEq)]
struct SampledOn {
    head: B256,
    head_number: u64,
    target_number: u64,
    target_time: u64,
    implementation: Address,
    min_tip_cap_wei: U256,
    rungs: u32,
    state: serde_json::Value,
    listings: Vec<TakenListing>,
}

/// Whether a read of the doors was answered everything it asked: one answered in part serves
/// what it has for its own poll (a shorter life, a listing left out), and is not kept for the
/// polls after it, which ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answered {
    Whole,
    InPart,
}

impl Answered {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Whole, Self::Whole) => Self::Whole,
            _ => Self::InPart,
        }
    }
}

/// Whether the reader names the block its doors are asked at by the head's hash, the form a
/// kept read needs (see [`SampledOn`]). It does until the node refuses the form itself on
/// [`HASH_FORM_REFUSALS`] polls in a row: a request naming its block by hash answered with one of
/// [`FORM_REFUSED_CODES`], on a poll whose read by number then answers. The form is then set
/// aside for [`HASH_FORM_RETRY_HEADS`] heads and tried once more: any answer by hash brings it
/// back into use, and one more refusal sets it aside as long again. A read by hash that fails
/// otherwise (a backend that does not know the head's hash yet, a block replaced under the read,
/// a timeout) is asked again by number on that poll and counts for nothing here.
#[derive(Debug, Default)]
struct HashForm {
    /// Polls in a row on which the node refused the form and answered by number.
    refusals: AtomicU32,
    /// The head from which the form is tried again; zero while it has not been set aside since
    /// the node last answered by hash.
    retry_from: AtomicU64,
    /// Whether a request of the read in flight that named its block by hash was refused its
    /// form: set by [`DoorsReader::batch`], read once that read has failed.
    refused: AtomicBool,
}

impl HashForm {
    /// Whether the form is set aside at the head `number`.
    fn set_aside(&self, number: u64) -> bool {
        number < self.retry_from.load(Ordering::Relaxed)
    }

    /// The node answered a read by hash: the form is in use, and no refusal counts any more.
    fn taken(&self) {
        self.refusals
            .store(0, Ordering::Relaxed);
        self.retry_from
            .store(0, Ordering::Relaxed);
    }

    /// The node refused the form at the head `number` and answered that read by number. Whether
    /// that sets the form aside: the last of [`HASH_FORM_REFUSALS`] in a row, or the one try
    /// after it was set aside.
    fn refused_at(&self, number: u64) -> bool {
        let tried_again = self.retry_from.load(Ordering::Relaxed) != 0;
        let refusals = self
            .refusals
            .fetch_add(1, Ordering::Relaxed) +
            1;
        if !tried_again && refusals < HASH_FORM_REFUSALS {
            return false;
        }
        self.refusals
            .store(0, Ordering::Relaxed);
        self.retry_from
            .store(number.saturating_add(HASH_FORM_RETRY_HEADS), Ordering::Relaxed);
        true
    }
}

/// What a node answered at one block named by its hash with no override: the storage words read
/// there, and each `aggregate3` run there, by the keccak of its calldata. A request so named is
/// answered from that very block or refused, and a block's state never changes, so the same
/// request at the same hash is answered from here and not sent again (see
/// [`DoorsReader::batch`]). It holds one block: a request naming another hash empties it.
#[derive(Debug, Default)]
struct HeadAnswers {
    hash: B256,
    words: HashMap<(Address, U256), U256>,
    calls: HashMap<B256, Vec<Call3Result>>,
    /// Whether the milliseconds between the block and its parent were asked, and what they came
    /// to where the headers name them: see [`Also::ImplementationAndParentOf`].
    parent_asked: bool,
    parent_ms: Option<u64>,
}

impl HeadAnswers {
    /// The answers at `hash`, emptied where they were another block's.
    fn at(&mut self, hash: B256) -> &mut Self {
        if self.hash != hash {
            *self = Self { hash, ..Self::default() };
        }
        self
    }
}

/// The node's latest block as its header names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LatestHead {
    number: u64,
    hash: B256,
    /// Its milliseconds, where the header names them ([`Header::ms`]).
    ms: Option<u64>,
    /// Its parent's hash, where the header names it.
    parent_hash: Option<B256>,
}

/// The calls and storage words of a next-block read's second request (see
/// [`DoorsReader::take_next_block`]), as the last read built them: the first request at a head
/// the reader has not read yet carries them, so that a read finding the same listings there asks
/// none of them again.
#[derive(Debug, Clone, Default)]
struct NextRequest {
    calls: Vec<Call3>,
    slots: Vec<(Address, U256)>,
}

/// One poll's reads at the node's latest block: its listings, and the block they were read at,
/// named by its hash where the reader named it so.
struct HeadRead {
    clock: FrontClock,
    listings: Vec<Listing>,
    highest_listing: u32,
    pinned: Option<LatestHead>,
}

/// Whether a node's `error` says it does not take the request's form (see
/// [`FORM_REFUSED_CODES`]).
fn refuses_the_form(error: &TransportError) -> bool {
    error
        .as_error_resp()
        .is_some_and(|payload| FORM_REFUSED_CODES.contains(&payload.code))
}

/// A next-block read's doors as sampled, and what they were asked on.
#[derive(Debug)]
struct Sampled {
    on: SampledOn,
    doors: Vec<DoorRead>,
}

/// What [`DoorsReader::take_next_block`] answers: the listings the next block's state serves, the
/// state override their doors are asked under, and the head it was all read at, where the node
/// names it.
struct NextTaken {
    served: Vec<(Listing, (U256, U256))>,
    state: serde_json::Value,
    listings: Vec<TakenListing>,
    head: Option<B256>,
    /// The head's milliseconds, where its header names them (BNB Chain).
    head_ms: Option<u64>,
    /// The milliseconds between the head and its parent, where read (BNB Chain).
    head_spacing_ms: Option<u64>,
    implementation: Option<Address>,
}

/// The blocks a next-block read samples each side at, and how long the sides it finds live.
#[derive(Debug, Clone)]
struct NextMoments {
    /// The next block at each time it may carry: its slot's on Ethereum; on BNB Chain the
    /// head's second and the one after, since a block 450 ms on may fall in either.
    next: Vec<At>,
    /// The block after it at each time it may carry, asked of the listings that carry.
    after: Vec<At>,
    /// One millisecond past the end of a side's life through the next block alone, and through
    /// the block after too: on Ethereum each block's own time, on BNB Chain the head's
    /// milliseconds and one and two blocks' spacing.
    next_until_ms: u64,
    after_until_ms: u64,
}

impl NextMoments {
    /// The moments of the next block `number` after a head of milliseconds `head_ms` (on BNB
    /// Chain, from its header; unused on Ethereum), the next block's first time being `time`, on
    /// a front whose clock at the head is `head`.
    fn new(cadence: Cadence, head: &FrontClock, number: u64, time: u64, head_ms: u64) -> Self {
        let at = |number: u64, time: u64| head.at_block(number, time).read_at();
        match cadence {
            Cadence::Slots => {
                let target = at(number, time);
                let after_time = time.saturating_add(SLOT_SECS);
                let after = At {
                    number: number.saturating_add(1),
                    time: after_time,
                    ms: after_time.saturating_mul(1000),
                    exact: false,
                    base_time: None,
                };
                Self {
                    next_until_ms: target.ms.saturating_add(1),
                    after_until_ms: after.ms.saturating_add(1),
                    next: vec![target],
                    after: vec![after],
                }
            }
            Cadence::Blocks { block_ms } => {
                let seconds = [time, time.saturating_add(1)];
                Self {
                    next: seconds
                        .iter()
                        .map(|&second| at(number, second))
                        .collect(),
                    after: seconds
                        .iter()
                        .map(|&second| at(number.saturating_add(1), second))
                        .collect(),
                    next_until_ms: head_ms
                        .saturating_add(block_ms)
                        .saturating_add(1),
                    after_until_ms: head_ms
                        .saturating_add(2 * block_ms)
                        .saturating_add(1),
                }
            }
        }
    }
}

/// The node's JSON-RPC transport over HTTP: alloy's own, less the URL it records on every
/// request's span. A node's URL commonly carries a key, in its query or its userinfo, and that
/// span would print it to any log at debug level; nor does a failure here quote the URL.
#[derive(Clone)]
struct NodeTransport {
    client: reqwest::Client,
    url: Url,
}

impl tower_service::Service<RequestPacket> for NodeTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let response = this
                .client
                .post(this.url)
                .json(&req)
                .headers(req.headers())
                .send()
                .await
                .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
            let status = response.status();
            let body = response
                .bytes()
                .await
                .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
            if !status.is_success() {
                return Err(TransportErrorKind::http_error(
                    status.as_u16(),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
            }
            serde_json::from_slice(&body)
                .map_err(|e| TransportError::deser_err(e, String::from_utf8_lossy(&body)))
        })
    }
}

#[derive(Clone, derive_more::Debug)]
pub(super) struct DoorsReader {
    #[debug(skip)]
    provider: RootProvider,
    /// Removed from transport errors before they are reported: see [`Self::connection_error`].
    #[debug(skip)]
    rpc_url: String,
    chain: Chain,
    front: Address,
    factory: Address,
    /// The creation code hash of the desk contracts, on a chain whose makers have one: see
    /// [`SlotRule::account`].
    cell_codehash: Option<B256>,
    implementations: Vec<Address>,
    /// How many sizes each side is sampled at.
    rungs: u32,
    /// The lowest tip cap of a listing served: see [`EfficientPropFeedBuilder::min_tip_cap_wei`].
    ///
    /// [`EfficientPropFeedBuilder::min_tip_cap_wei`]:
    /// crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::min_tip_cap_wei
    min_tip_cap_wei: U256,
    /// The price sources a listing reading a print may be bound to and be served: see
    /// [`EfficientPropFeedBuilder::approved_price_sources`].
    ///
    /// [`EfficientPropFeedBuilder::approved_price_sources`]:
    /// crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::approved_price_sources
    price_sources: BTreeSet<u32>,
    /// The credit listings served, by id, with the gas configured for each: see
    /// [`EfficientPropFeedBuilder::credit_listing`]. On Base and BNB Chain every listing served
    /// is one of them.
    ///
    /// [`EfficientPropFeedBuilder::credit_listing`]:
    /// crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::credit_listing
    credit: BTreeMap<u32, CreditGas>,
    /// The desk contracts whose listings are served, on a chain whose makers have one: see
    /// [`ApprovedDesks`].
    desks: ApprovedDesks,
    /// The doors' creation code hash, where the caller pins it: see
    /// [`EfficientPropFeedBuilder::with_door_code_hash`].
    ///
    /// [`EfficientPropFeedBuilder::with_door_code_hash`]:
    /// crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::with_door_code_hash
    pub(super) door_codehash: Option<B256>,
    /// The runtime code hashes the front's implementation may have, where the caller pins them:
    /// see [`EfficientPropFeedBuilder::with_implementation_code_hashes`]. Empty where only the
    /// implementation's address is checked.
    ///
    /// [`EfficientPropFeedBuilder::with_implementation_code_hashes`]:
    /// crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::with_implementation_code_hashes
    pub(super) implementation_codehashes: BTreeSet<B256>,
    /// The implementations whose code a read found pinned: code at an address does not change on
    /// a chain, so each is read once.
    code_checked: Arc<Mutex<BTreeSet<Address>>>,
    /// Each guarded source's lane's `HOLD_MS` once it answered: an immutable of the lane's code,
    /// so it is asked once per lane.
    holds_ms: Arc<Mutex<BTreeMap<Address, u64>>>,
    /// On a chain whose builders land makers' updates in the block that fills them (Ethereum), the
    /// reader of the next block's state map, which every call runs under (see
    /// [`Self::read_next_block`]); `None` on Base, whose makers store their words.
    next_block: Option<NextBlockReader>,
    /// Whether the last read found the front's clock able to step back: its warning is logged
    /// when a read first finds it so, not on every poll.
    pub(super) stepping_back: Arc<AtomicBool>,
    /// How many times that warning was logged.
    #[cfg(test)]
    pub(super) step_back_warnings: Arc<std::sync::atomic::AtomicUsize>,
    /// Whether the last next-block read on BNB Chain found the chain off its block time: its
    /// warning is logged when a read first finds it so (see [`Self::on_cadence`]).
    off_cadence: Arc<AtomicBool>,
    /// How many times that warning was logged.
    #[cfg(test)]
    pub(super) off_cadence_warnings: Arc<std::sync::atomic::AtomicUsize>,
    /// The last next-block read whose doors were sampled, with what they were asked on: a read
    /// asked on the same is answered from it, and no door is asked (see [`SampledOn`]).
    sampled: Arc<Mutex<Option<Sampled>>>,
    /// Whether the doors' block is named by the head's hash, or that form is set aside.
    hash_form: Arc<HashForm>,
    /// How many reads were answered from it.
    #[cfg(test)]
    pub(super) reuses: Arc<std::sync::atomic::AtomicUsize>,
    /// What the node answered at the head named by its hash, with no override: a later poll at
    /// that head asks none of it again (see [`HeadAnswers`]).
    head_answers: Arc<Mutex<HeadAnswers>>,
    /// The head from which the next-block read reads the latest header first again, after a node
    /// answered it without a hash; zero while it does (see [`Self::read_head`]).
    head_first_from: Arc<AtomicU64>,
    /// The latest head a read found, which that is judged against.
    last_head: Arc<AtomicU64>,
    /// The last next-block read's second request (see [`NextRequest`]).
    next_request: Arc<Mutex<Option<NextRequest>>>,
}

impl DoorsReader {
    #[expect(clippy::too_many_arguments, reason = "one argument per reader setting")]
    pub(super) fn new(
        rpc_url: Url,
        chain: Chain,
        front: Address,
        factory: Address,
        cell_codehash: Option<B256>,
        implementations: Vec<Address>,
        rungs: u32,
        min_tip_cap_wei: U256,
        price_sources: BTreeSet<u32>,
        credit: BTreeMap<u32, CreditGas>,
        desks: ApprovedDesks,
        next_block: Option<NextBlockReader>,
    ) -> Self {
        let is_local = guess_local_url(rpc_url.as_str());
        Self {
            rpc_url: rpc_url.to_string(),
            provider: RootProvider::new(RpcClient::new(
                NodeTransport { client: reqwest::Client::new(), url: rpc_url },
                is_local,
            )),
            chain,
            front,
            factory,
            cell_codehash,
            implementations,
            rungs,
            min_tip_cap_wei,
            price_sources,
            credit,
            desks,
            door_codehash: None,
            implementation_codehashes: BTreeSet::new(),
            code_checked: Arc::default(),
            holds_ms: Arc::default(),
            next_block,
            stepping_back: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            step_back_warnings: Arc::default(),
            off_cadence: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            off_cadence_warnings: Arc::default(),
            sampled: Arc::default(),
            hash_form: Arc::default(),
            #[cfg(test)]
            reuses: Arc::default(),
            head_answers: Arc::default(),
            head_first_from: Arc::default(),
            last_head: Arc::default(),
            next_request: Arc::default(),
        }
    }

    /// The front's proxy.
    pub(super) fn front(&self) -> Address {
        self.front
    }

    /// Whether no price source is approved.
    #[cfg(test)]
    pub(super) fn approves_none(&self) -> bool {
        self.price_sources.is_empty()
    }

    /// Reads listings `1..=scan_to` and samples every open door whose two tokens `decimals` knows,
    /// so that its samples hold as `hold` asks. Where the front's clock may read earlier at a
    /// later block than at the read ([`FrontClock::may_step_back`]), no door is read or served: no
    /// sample at a few blocks bounds what the door quotes over the book's life there.
    ///
    /// Fails fatally when the front runs an implementation outside the allow-list, or one whose
    /// code hashes to none of the hashes pinned, when the node serves another chain, when the
    /// factory serves another front, or when the front's or the factory's views revert, since no
    /// later read will differ. A factory with no code yet fails only the read: a factory's address
    /// is known before it is deployed. So does a node lagging the poll's start by more than
    /// `hold.max_lag_secs`, at the first request, before any door is asked: the asks of a print's
    /// life run from the block to the book's end, one more for each second the node lags.
    pub(super) async fn read(
        &self,
        scan_to: u32,
        decimals: impl Fn(&Address) -> Option<u32>,
        hold: Hold,
    ) -> Result<DoorsRead, FeedError> {
        if let Some(next_block) = &self.next_block {
            return self
                .read_next_block(next_block, scan_to, decimals, hold)
                .await;
        }
        let (clock, listings, highest_listing) = self
            .read_listings(scan_to, BlockId::latest(), None)
            .await?;
        telemetry::record_answer_age(answer_age(hold.from_ms, clock.timestamp));
        let started = hold.from_ms / 1000;
        if started >
            clock
                .timestamp
                .saturating_add(hold.max_lag_secs)
        {
            return Err(lagging(started, clock.number, clock.timestamp));
        }
        let start_ms = hold.from_ms.max(clock.ms);
        let until_ms = start_ms.saturating_add(hold.for_ms);
        let lead_ms = start_ms - hold.from_ms;
        let stepping_back = clock.may_step_back();
        let changed = self
            .stepping_back
            .swap(stepping_back, Ordering::Relaxed) !=
            stepping_back;
        if changed && !stepping_back {
            info!(
                front = %self.front,
                block = clock.number,
                "serving this front's listings again: its clock no longer steps back"
            );
        }
        if stepping_back {
            if changed {
                warn!(
                    front = %self.front,
                    block = clock.number,
                    "serving no listing of this front until its clock no longer steps back: its \
                     block count is off BaseTime by less than a second, so its clock may read \
                     earlier at a later block than at the read"
                );
                #[cfg(test)]
                self.step_back_warnings
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                debug!(
                    front = %self.front,
                    block = clock.number,
                    "serving no listing of this front: its clock may step back"
                );
            }
            return Ok(DoorsRead {
                block_number: clock.number,
                timestamp: clock.timestamp,
                #[cfg(test)]
                clock,
                until_ms,
                lead_ms,
                highest_listing,
                doors: Vec::new(),
                block_lives: false,
                answered_at_block: true,
                outcome: PollOutcome::Sampled,
                head_ms: None,
            });
        }
        let block = BlockId::number(clock.number);
        let served: Vec<_> = listings
            .into_iter()
            .filter_map(|listing| {
                let probes = (probe(decimals(&listing.token0)?), probe(decimals(&listing.token1)?));
                Some((listing, probes))
            })
            .collect();
        let (open, _) = self
            .open_doors(&served, block, None)
            .await?;
        let doors = self
            .sample(&open, &clock, start_ms, until_ms, hold.min_side_ms)
            .await?;
        Ok(DoorsRead {
            block_number: clock.number,
            timestamp: clock.timestamp,
            #[cfg(test)]
            clock,
            until_ms,
            lead_ms,
            highest_listing,
            doors,
            block_lives: false,
            answered_at_block: true,
            outcome: PollOutcome::Sampled,
            head_ms: None,
        })
    }

    /// A read on a chain whose builders land makers' updates in the block that fills them: there a
    /// fill reads a maker's quote only in the block its update landed in, or the next where both
    /// the listing and the word carry, and refuses it (`NoWord`) in any other. So every door is
    /// read at the next block, the one the state maps name, under that block's number and time
    /// and the maps' storage: the node's latest block is pinned, and each call is run there under
    /// the listings' accounts of the maps as a state override and the next block as a block
    /// override.
    ///
    /// The maps are read from every state URL while the first requests read the latest header and
    /// the listings at the block it names (see [`Self::read_head`]). Only a map for the block after
    /// the node's latest, at the slot right after its (its time 12 s after the latest block's), is
    /// used: a map at or behind it is for a block already made, one further ahead was made on state
    /// the node has not seen, and one at a later slot would be sampled at a time the block does not
    /// land at. Where no map is used, the read serves nothing, and its block, the node's latest, is
    /// not counted as read: neither a map far ahead nor a state URL ahead of the node fails or
    /// holds back a later poll. A listing is served only where one answer writes its quote's stamp
    /// slot with a stamp the next block fills, and each price contract it reads (see
    /// [`Self::take_next_block`]). Each side is sampled at the next block and, where its listing
    /// carries, at the one after under the same state, and lives through the last of those its
    /// samples hold at (see [`Self::sample_next_block`]). A read whose map's block is more than
    /// `hold.max_lag_secs` behind the poll's start fails, and so does one whose node's latest block
    /// is more than two slots and `hold.max_lag_secs` behind it, map or none: that node has
    /// stalled, since with one slot missed its latest block is at most two slots old, and a poll
    /// that served nothing there would otherwise succeed in silence for as long as the node stays
    /// stuck.
    ///
    /// The doors are asked only where something their answers are read from has changed since
    /// the last read that sampled them: a read finding the same head, the same next block, the
    /// same state override and the same listings as that one ([`SampledOn`]) is answered its
    /// doors' reads. Its listings, their words at the head, the state maps, the front's
    /// implementation and the lag are checked on every poll all the same, the node asked again
    /// only what it has not answered at the head's hash ([`HeadAnswers`]). A read that does ask its
    /// doors names their block by the head's hash, and is kept for the polls after it only where
    /// the node answered all of it so. A node that does not name the head by its hash, or refuses a
    /// block named so, is asked every door on every poll, by number, and a read by hash that fails
    /// is asked again by number on that poll, unless its failure is fatal ([`HashForm`]). Each read
    /// that is not kept is counted by its reason ([`NotKept`]).
    ///
    /// On BNB Chain ([`Cadence::Blocks`]) a map names its block alone, the next block falls in the
    /// head's second or the one after, and the block after it likewise: each is sampled at both,
    /// a size served only where both fill it by one path, at the lesser output. A side lives
    /// until the head's milliseconds and one block, 450 ms, or two where it carries. The head's
    /// header must name its milliseconds, and its parent's must be 450 ms before them: otherwise
    /// the read serves nothing ([`Self::on_cadence`]). The front's clock mode 3 reads the block's
    /// second there while the millisecond precompile answers nothing, and is served as mode 0 is.
    async fn read_next_block(
        &self,
        next_block: &NextBlockReader,
        scan_to: u32,
        decimals: impl Fn(&Address) -> Option<u32>,
        hold: Hold,
    ) -> Result<DoorsRead, FeedError> {
        let (HeadRead { clock: head, listings, highest_listing, pinned }, maps) =
            tokio::try_join!(self.read_head(scan_to), next_block.read())?;
        telemetry::record_answer_age(answer_age(hold.from_ms, head.timestamp));
        let started = hold.from_ms / 1000;
        if started >
            head.timestamp
                .saturating_add(next_block.cadence.stall_secs())
                .saturating_add(hold.max_lag_secs)
        {
            return Err(lagging(started, head.number, head.timestamp));
        }
        let mut read = DoorsRead {
            block_number: head.number,
            timestamp: head.timestamp.max(started),
            #[cfg(test)]
            clock: head,
            until_ms: head.ms.saturating_add(1),
            lead_ms: 0,
            highest_listing,
            doors: Vec::new(),
            block_lives: true,
            answered_at_block: false,
            outcome: PollOutcome::NoStateMap,
            head_ms: None,
        };
        let Some(next) = maps.for_block_after_in(head.number, head.timestamp, next_block.cadence)
        else {
            debug!(
                front = %self.front,
                latest = head.number,
                "serving no listing: no state map is for the block after the node's latest at its next slot"
            );
            return Ok(read);
        };
        if started >
            next.time
                .saturating_add(hold.max_lag_secs)
        {
            return Err(lagging(started, next.number, next.time));
        }
        let clock = head.at_block(next.number, next.time);
        read.outcome = PollOutcome::Sampled;
        read.answered_at_block = true;
        read.block_number = next.number;
        read.timestamp = next.time;
        #[cfg(test)]
        {
            read.clock = clock;
        }
        read.until_ms = clock.ms.saturating_add(1);
        let block = BlockId::number(head.number);
        let served: Vec<_> = listings
            .into_iter()
            .filter_map(|listing| {
                let probes = decimals(&listing.token0)
                    .zip(decimals(&listing.token1))
                    .map(|(decimals0, decimals1)| (probe(decimals0), probe(decimals1)));
                if probes.is_none() {
                    telemetry::record_listing_not_covered(NotCovered::UnknownToken);
                }
                Some((listing, probes?))
            })
            .collect();
        let NextTaken {
            served,
            state,
            listings,
            head: taken_at,
            head_ms,
            head_spacing_ms,
            implementation,
        } = self
            .take_next_block(served, head.number, pinned, &next)
            .await?;
        // On BNB Chain a side's life is the head's milliseconds and the chain's block time, which
        // must be the chain's own: the head's header names its milliseconds, and its parent's
        // are one block time before them. Otherwise no side is served (see [`Self::on_cadence`]).
        let head_ms = match next_block.cadence {
            Cadence::Slots => None,
            Cadence::Blocks { block_ms } => {
                match self.on_cadence(head.number, head_ms, head_spacing_ms, block_ms) {
                    Some(head_ms) => Some(head_ms),
                    None => {
                        read.outcome = PollOutcome::OffCadence;
                        return Ok(read);
                    }
                }
            }
        };
        read.head_ms = head_ms;
        let moments = NextMoments::new(
            next_block.cadence,
            &head,
            next.number,
            next.time,
            head_ms.unwrap_or_default(),
        );
        let asked_on = taken_at
            .zip(implementation)
            .map(|(hash, implementation)| SampledOn {
                head: hash,
                head_number: head.number,
                target_number: next.number,
                target_time: next.time,
                implementation,
                min_tip_cap_wei: self.min_tip_cap_wei,
                rungs: self.rungs,
                state: state.clone(),
                listings,
            });
        let kept = asked_on.as_ref().and_then(|asked_on| {
            self.sampled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .filter(|sampled| sampled.on == *asked_on)
                .map(|sampled| sampled.doors.clone())
        });
        read.doors = match kept {
            Some(doors) => {
                debug!(
                    front = %self.front,
                    head = head.number,
                    listings = doors.len(),
                    "asking no door: nothing its answers are read from has changed since they \
                     were sampled"
                );
                #[cfg(test)]
                self.reuses
                    .fetch_add(1, Ordering::Relaxed);
                read.outcome = PollOutcome::Reused;
                doors
            }
            None => {
                // The doors' block by its hash, where the node named it and the form is not set
                // aside: the node then answers from that very block or refuses.
                let by_number = match &asked_on {
                    None => Some(NotKept::NoBlockHash),
                    Some(_) if self.hash_form.set_aside(head.number) => {
                        Some(NotKept::HashFormSetAside)
                    }
                    Some(_) => None,
                };
                let by_hash = asked_on
                    .as_ref()
                    .filter(|_| by_number.is_none())
                    .map(|asked_on| BlockId::hash_canonical(asked_on.head));
                let pinned = match by_hash {
                    Some(by_hash) => {
                        self.hash_form
                            .refused
                            .store(false, Ordering::Relaxed);
                        match self
                            .ask_doors(&served, by_hash, &clock, &moments, &state)
                            .await
                        {
                            Ok(asked) => Some(asked),
                            // No other form of the read passes what ends the feed: a front on
                            // an implementation outside the allow-list.
                            Err(error @ FeedError::Fatal(_)) => return Err(error),
                            Err(error) => {
                                debug!(
                                    front = %self.front,
                                    head = head.number,
                                    %error,
                                    "the doors' read by the head's hash failed: asking by number"
                                );
                                None
                            }
                        }
                    }
                    None => None,
                };
                match pinned {
                    Some((doors, answered)) => {
                        self.hash_form.taken();
                        match asked_on.filter(|_| answered == Answered::Whole) {
                            Some(asked_on) => {
                                *self
                                    .sampled
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(Sampled { on: asked_on, doors: doors.clone() });
                            }
                            None => telemetry::record_read_not_kept(NotKept::AnsweredInPart),
                        }
                        doors
                    }
                    // By number, as before any read was kept: the node may answer from another
                    // block of that number, so nothing of it is kept.
                    None => {
                        let (doors, _) = self
                            .ask_doors(&served, block, &clock, &moments, &state)
                            .await?;
                        telemetry::record_read_not_kept(
                            by_number.unwrap_or(NotKept::HashReadFailed),
                        );
                        // The node answered by number a read whose form by hash it refused.
                        if by_hash.is_some() &&
                            self.hash_form
                                .refused
                                .load(Ordering::Relaxed) &&
                            self.hash_form.refused_at(head.number)
                        {
                            telemetry::record_hash_form_set_aside();
                            warn!(
                                front = %self.front,
                                head = head.number,
                                heads = HASH_FORM_RETRY_HEADS,
                                "the node refuses a block named by its hash and answers one \
                                 named by its number: asking every door on every poll, by \
                                 number, until the hash form is tried again"
                            );
                        }
                        doors
                    }
                }
            }
        };
        read.until_ms = read
            .doors
            .iter()
            .flat_map(|door| [door.bid_until_ms, door.ask_until_ms])
            .fold(read.until_ms, u64::max);
        // Counted once the read holds its doors: each listing the state covers either has its
        // door among them or was left out on the way.
        telemetry::record_listings_covered(read.doors.len());
        telemetry::record_listings_not_covered(
            NotCovered::NotOpened,
            served
                .len()
                .saturating_sub(read.doors.len()),
        );
        Ok(read)
    }

    /// A next-block read's listings at the node's latest block, by its hash: the latest header,
    /// then the listings and the front's implementation at the block that header names, required
    /// canonical (EIP-1898), so that every request of the read after it names that very block
    /// and a request answered at it once is not sent again ([`HeadAnswers`]): a poll finding the
    /// head it last read sends the header alone. At a head not read yet, the request carries the
    /// last read's second request ([`NextRequest`]), whose answers the read finding the same
    /// listings there then takes as they are.
    ///
    /// Where the node answers the latest header without a hash, or refuses a block named by its
    /// hash, the listings are read at the latest block, as they are on any poll whose read by hash
    /// fails, and the header is not read again for [`HASH_FORM_RETRY_HEADS`] heads; nor is it
    /// while the doors' hash form is set aside ([`HashForm`]).
    async fn read_head(&self, scan_to: u32) -> Result<HeadRead, FeedError> {
        let last = self.last_head.load(Ordering::Relaxed);
        let head_first = last >=
            self.head_first_from
                .load(Ordering::Relaxed) &&
            !self.hash_form.set_aside(last);
        let mut set_aside = false;
        if head_first {
            match self.latest_head().await {
                Ok(Some(latest)) => {
                    let prefetch = self
                        .next_request
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    self.hash_form
                        .refused
                        .store(false, Ordering::Relaxed);
                    match self
                        .read_listings(scan_to, BlockId::hash_canonical(latest.hash), prefetch)
                        .await
                    {
                        Ok((clock, listings, highest_listing)) if clock.number == latest.number => {
                            self.last_head
                                .store(clock.number, Ordering::Relaxed);
                            return Ok(HeadRead {
                                clock,
                                listings,
                                highest_listing,
                                pinned: Some(latest),
                            });
                        }
                        Ok((clock, ..)) => debug!(
                            front = %self.front,
                            header = latest.number,
                            read = clock.number,
                            "the block named by the latest header's hash answered another number: \
                             reading the listings at the latest block"
                        ),
                        Err(error @ FeedError::Fatal(_)) => return Err(error),
                        Err(error) => {
                            set_aside = self
                                .hash_form
                                .refused
                                .load(Ordering::Relaxed);
                            debug!(
                                front = %self.front,
                                header = latest.number,
                                %error,
                                "the listings' read by the latest header's hash failed: reading \
                                 them at the latest block"
                            );
                        }
                    }
                }
                Ok(None) => set_aside = true,
                Err(error) => debug!(
                    front = %self.front,
                    error = %self.connection_error("latest header", error),
                    "the latest header's read failed: reading the listings at the latest block"
                ),
            }
        }
        let (clock, listings, highest_listing) = self
            .read_listings(scan_to, BlockId::latest(), None)
            .await?;
        self.last_head
            .store(clock.number, Ordering::Relaxed);
        if set_aside {
            debug!(
                front = %self.front,
                head = clock.number,
                heads = HASH_FORM_RETRY_HEADS,
                "the node names its latest block by no hash, or refuses one: reading the listings \
                 at the latest block until the header is read again"
            );
            self.head_first_from.store(
                clock
                    .number
                    .saturating_add(HASH_FORM_RETRY_HEADS),
                Ordering::Relaxed,
            );
        }
        Ok(HeadRead { clock, listings, highest_listing, pinned: None })
    }

    /// The node's latest header: `None` where the node answers it with no block or with one
    /// that names no hash or number, an error where the request fails.
    async fn latest_head(&self) -> Result<Option<LatestHead>, TransportError> {
        let header = self
            .provider
            .client()
            .request::<_, Option<Header>>("eth_getBlockByNumber", (BlockNumberOrTag::Latest, false))
            .await;
        match header {
            Ok(header) => Ok(header.map(|header| LatestHead {
                number: header.number.to::<u64>(),
                hash: header.hash,
                ms: header.ms(),
                parent_hash: header.parent_hash,
            })),
            Err(TransportError::DeserError { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The third and fourth requests of a next-block read, and a fifth with a credit listing: the
    /// doors of `served` opened and sampled at `block` under the next block's `clock` and
    /// `state` (see [`Self::open_doors`], [`Self::sample_next_block`]), and whether the node
    /// answered all that was asked for them.
    async fn ask_doors(
        &self,
        served: &[(Listing, (U256, U256))],
        block: BlockId,
        clock: &FrontClock,
        moments: &NextMoments,
        state: &serde_json::Value,
    ) -> Result<(Vec<DoorRead>, Answered), FeedError> {
        let (open, opened) = self
            .open_doors(served, block, Some((clock, state)))
            .await?;
        let (doors, sampled) = self
            .sample_next_block(&open, clock, moments, block, state)
            .await?;
        Ok((doors, opened.and(sampled)))
    }

    /// The listings of `listings` the next block's state serves, and the state override their
    /// doors are asked under: a request at the latest block for where each listing's quote sits
    /// (by the slot rule, [`SLOT_RULE`]: its desk contract, its own two slots there) and, all at
    /// the head's block in one batch: the desk word and the pair's quote and firm words read from
    /// its desk contract, and its settings and live-source words from the front. No sources word
    /// is read: the fill reads the live source alone.
    ///
    /// A listing is served only from an answer that has no entry the parser could not read whole,
    /// and writes the listing's quote word with a stamp the next block fills: that block's, or the
    /// block before's where the listing and the word carry (the listing's carry life set and its
    /// fence not NONE, as the doors' samples read it) and the head holds that very word, under the
    /// desk epoch the head's desk word holds, on a desk the head has not stopped, and writes no
    /// slot of its desk contract but the pairs' words. A listing whose fill reads a print needs the
    /// price contract of its live source (the CREATE3 child of the front at the source's id, from
    /// the live-source word) written by that same answer, with no two entries disagreeing on one
    /// of its slots. A listing with no desk id, whose quote place or words the node does not
    /// answer, or whose slots no single answer covers, is left out (see [`NextBlock::take`]). Only
    /// these slots and accounts are overridden.
    async fn take_next_block(
        &self,
        listings: Vec<(Listing, (U256, U256))>,
        number: u64,
        pinned: Option<LatestHead>,
        next: &NextBlock,
    ) -> Result<NextTaken, FeedError> {
        let listings: Vec<_> = listings
            .into_iter()
            .filter(|(listing, _)| {
                if listing.desk == 0 {
                    debug!(listing = listing.id, "skipping listing with no quote contract");
                    telemetry::record_listing_not_covered(NotCovered::NoQuoteContract);
                }
                listing.desk != 0
            })
            .collect();
        let Some(cell_codehash) = self.cell_codehash else {
            return Err(FeedError::Fatal(
                "Efficient PropAMM: no desk contract code hash is configured".into(),
            ));
        };
        let calls: Vec<_> = listings
            .iter()
            .map(|(listing, _)| SLOT_RULE.place_call(self.front, listing.desk))
            .collect();
        // Each listing's settings and live-source words of the front, then the desk word of each
        // desk contract once, then each listing's quote and firm words of its desk contract, all
        // at the head the maps are read against.
        let mut slots: Vec<(Address, U256)> = listings
            .iter()
            .flat_map(|(listing, _)| {
                [SETTINGS_WORD, PRICE_WORD]
                    .map(|word| (self.front, record_word_slot(listing.id, word)))
            })
            .collect();
        let record_words = slots.len();
        let desks: BTreeSet<u32> = listings
            .iter()
            .map(|(listing, _)| listing.desk)
            .collect();
        let desks: BTreeMap<u32, Address> = desks
            .into_iter()
            .map(|desk| (desk, SLOT_RULE.account(self.front, cell_codehash, desk)))
            .collect();
        slots.extend(
            desks
                .values()
                .map(|account| (*account, SLOT_RULE.head_slot())),
        );
        let desk_words = slots.len();
        slots.extend(
            listings
                .iter()
                .flat_map(|(listing, _)| {
                    let (quote, firm) = SLOT_RULE.pair_slots(listing.word_index);
                    let account = desks[&listing.desk];
                    [(account, quote), (account, firm)]
                }),
        );
        *self
            .next_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(NextRequest { calls: calls.clone(), slots: slots.clone() });
        // With them the front's implementation and the head's hash: what a read that asks no
        // door is checked on and matched by (see [`SampledOn`]). On BNB Chain also the head's
        // parent, whose milliseconds the chain's block time is checked against. At the head the
        // listings were read at by its hash, where they were, else by its number with its header.
        let spaced = self
            .next_block
            .as_ref()
            .is_some_and(|next_block| matches!(next_block.cadence, Cadence::Blocks { .. }));
        let by_number = || {
            let also =
                if spaced { Also::ImplementationAndHeads } else { Also::ImplementationAndHead };
            self.batch(
                BlockId::number(number),
                vec![Group { calls: calls.clone(), at: None }],
                &slots,
                also,
                None,
                None,
            )
        };
        let answers = match pinned {
            Some(latest) => {
                let also = if spaced {
                    Also::ImplementationAndParentOf(latest)
                } else {
                    Also::Implementation
                };
                match self
                    .batch(
                        BlockId::hash_canonical(latest.hash),
                        vec![Group { calls: calls.clone(), at: None }],
                        &slots,
                        also,
                        None,
                        None,
                    )
                    .await
                {
                    Ok(answers) => {
                        BatchAnswers { head: Some(latest.hash), head_ms: latest.ms, ..answers }
                    }
                    Err(error @ FeedError::Fatal(_)) => return Err(error),
                    Err(error) => {
                        debug!(
                            front = %self.front,
                            head = number,
                            %error,
                            "the listings' words by the head's hash failed: asking by number"
                        );
                        by_number().await?
                    }
                }
            }
            None => by_number().await?,
        };
        let BatchAnswers {
            groups,
            mut words,
            head: taken_at,
            head_ms,
            head_spacing_ms,
            implementation,
            ..
        } = answers;
        let places = groups
            .into_iter()
            .next()
            .flatten()
            .unwrap_or_default();
        let pair_words = words.split_off(desk_words.min(words.len()));
        let head_words: BTreeMap<u32, Result<U256, FeedError>> = desks
            .keys()
            .copied()
            .zip(words.split_off(record_words.min(words.len())))
            .collect();
        let mut words = words.into_iter();
        let mut pair_words = pair_words.into_iter();
        let mut wanted = Vec::with_capacity(listings.len());
        for (place, (listing, probes)) in (0..listings.len())
            .map(|i| places.get(i))
            .zip(&listings)
        {
            let (Some(settings), Some(price), Some(head_quote), Some(head_firm)) =
                (words.next(), words.next(), pair_words.next(), pair_words.next())
            else {
                return Err(FeedError::Parsing("the node answered too few storage reads".into()));
            };
            // The words the head holds, none of which may be unread: a carried word is taken only
            // where the head holds it.
            let head = match (head_words.get(&listing.desk), head_quote, head_firm) {
                (Some(Ok(desk)), Ok(quote), Ok(firm)) => Some(HeadWords {
                    desk: *desk,
                    quote: B256::from(quote),
                    firm: B256::from(firm),
                }),
                _ => None,
            };
            let quote = place
                .filter(|place| place.success)
                .zip(head)
                .and_then(|(place, head)| {
                    let slots = SLOT_RULE.slots(
                        &place.returnData,
                        desks[&listing.desk],
                        listing.word_index,
                        head,
                    )?;
                    Some((slots, head.desk))
                });
            let (Some((quote, desk)), Ok(settings)) = (quote, settings) else {
                wanted.push(None);
                continue;
            };
            let prints = matches!(field(settings, 228, 2), FENCE_PRINT | FENCE_SESSION);
            let contract = |id: u64| {
                u32::try_from(id)
                    .ok()
                    .filter(|id| *id != 0)
                    .map(|id| price_contract(self.front, id))
            };
            let mut needs = Vec::new();
            let mut live_source = None;
            if prints {
                // The source the fill reads: the live source's id, the copy's low 32 bits. The
                // fill reads no other: a listing's backup source is read only once a front op
                // makes it the live one, which no state map shows.
                let Ok(price) = price else {
                    wanted.push(None);
                    continue;
                };
                needs.extend(contract(field(price, 0, 32)));
                live_source = Some(price);
            }
            let read = TakenListing {
                listing: *listing,
                probes: *probes,
                settings,
                price: live_source,
                desk,
                slots: quote.clone(),
            };
            wanted.push(Some((Wanted { quote, carries: listing_carries(settings), needs }, read)));
        }
        let taken = next.take(
            wanted
                .iter()
                .flatten()
                .map(|(wanted, _)| wanted),
        );
        let mut results = taken.listings.into_iter();
        let mut read = Vec::new();
        let served = listings
            .into_iter()
            .zip(wanted)
            .filter_map(|((listing, probes), wanted)| {
                let taken = wanted.map(|(_, read)| (read, results.next()));
                let reason = match taken {
                    Some((taken, Some(Ok(())))) => {
                        read.push(taken);
                        return Some((listing, probes));
                    }
                    None | Some((_, None)) => NotCovered::Unread,
                    Some((_, Some(Err(not_taken)))) => NotCovered::from(not_taken),
                };
                telemetry::record_listing_not_covered(reason);
                debug!(
                    listing = listing.id,
                    desk = listing.desk,
                    reason = reason.why(),
                    "skipping listing"
                );
                None
            })
            .collect();
        Ok(NextTaken {
            served,
            state: taken.state,
            listings: read,
            head: taken_at,
            head_ms,
            head_spacing_ms,
            implementation,
        })
    }

    /// The first request: the block, the front's implementation and clock, and every reserved
    /// listing up to `scan_to` with its door's address, at `block`; on BNB Chain also the node's
    /// gas price, under which no listing's tip cap is served. With them, where given, the
    /// calls and words of a next-block read's second request at that block (see
    /// [`NextRequest`]), whose answers are kept and not looked at here.
    async fn read_listings(
        &self,
        scan_to: u32,
        block: BlockId,
        prefetch: Option<NextRequest>,
    ) -> Result<(FrontClock, Vec<Listing>, u32), FeedError> {
        let mut calls = vec![
            call(MULTICALL3_ADDRESS, getBlockNumberCall {}.abi_encode()),
            call(MULTICALL3_ADDRESS, getCurrentBlockTimestampCall {}.abi_encode()),
            call(MULTICALL3_ADDRESS, getChainIdCall {}.abi_encode()),
            call(self.factory, FRONTCall {}.abi_encode()),
            call(self.factory, DOOR_CODEHASHCall {}.abi_encode()),
            call(self.front, clockCall {}.abi_encode()),
        ];
        // The front's own desk contract code hash: a configuration that differs from the front's
        // places no maker's desk contract, so nothing may be listed.
        if self.cell_codehash.is_some() {
            calls.push(call(self.front, CELL_CODEHASHCall {}.abi_encode()));
        }
        // BaseTime is Base's: a front on a chain whose builders land makers' updates reads the
        // block's second, and the predeploy's address holds no code there.
        if self.next_block.is_none() {
            calls.push(call(BASE_TIME, timestampMsCall {}.abi_encode()));
        }
        // BNB Chain's millisecond precompile, which its fronts' clock mode 3 reads: served only
        // while it answers nothing. Asked as the front asks it.
        if self.chain == Chain::Bsc {
            calls.push(call(MS_PRECOMPILE, timestampMsCall {}.abi_encode()));
        }
        let head_calls = calls.len();
        for id in 1..=scan_to {
            let listing = U24::from(id);
            calls.push(call(self.front, doorViewCall { listing }.abi_encode()));
        }
        // One JSON-RPC batch with a read of the front's implementation slot, checked before any
        // answer is looked at: after an upgrade those answers may well fail in a way that hides
        // the cause. On BNB Chain also the node's gas price: its base fee is zero, so a taker's
        // tip is its whole gas price.
        let also = if self.chain == Chain::Bsc {
            Also::ImplementationAndGasPrice
        } else {
            Also::Implementation
        };
        let mut groups = vec![Group { calls, at: None }];
        let prefetch = prefetch.unwrap_or_default();
        if !prefetch.calls.is_empty() {
            groups.push(Group { calls: prefetch.calls, at: None });
        }
        let (answers, approved_desks) = tokio::join!(
            self.batch(block, groups, &prefetch.slots, also, None, None),
            self.desks.current()
        );
        let BatchAnswers { groups, gas_price, .. } = answers?;
        let answers = groups
            .into_iter()
            .next()
            .flatten()
            .unwrap_or_default();
        // The lowest tip cap served: the caller's floor, and on BNB Chain the node's gas price,
        // under which a listing's cap refuses every swap sent at it.
        let tip_floor =
            gas_price.map_or(self.min_tip_cap_wei, |price| price.max(self.min_tip_cap_wei));
        let (head, listings) = answers.split_at(head_calls.min(answers.len()));
        let [block_number, timestamp, chain_id, factory_front, door_codehash, clock, rest @ ..] =
            head
        else {
            return Err(FeedError::Parsing("multicall answered too few calls".into()));
        };
        let (front_cell_codehash, rest) = match (self.cell_codehash, rest) {
            (Some(_), [cell, rest @ ..]) => (Some(cell), rest),
            (None, rest) => (None, rest),
            (Some(_), []) => {
                return Err(FeedError::Parsing("multicall answered too few calls".into()))
            }
        };
        let (base_time, precompile) = if self.chain == Chain::Bsc {
            let [precompile] = rest else {
                return Err(FeedError::Parsing("multicall answered too few calls".into()));
            };
            (&[][..], Some(precompile))
        } else {
            (rest, None)
        };
        let served = to_u64(setting::<getChainIdCall>(chain_id, "chain id")?, "chain id")?;
        if served != self.chain.id() {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the RPC node serves chain {served}, not {}",
                self.chain
            )));
        }
        if factory_front.success && factory_front.returnData.is_empty() {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM: the listing-door factory {} has no code",
                self.factory
            )));
        }
        let found = setting::<FRONTCall>(factory_front, "factory's FRONT")?;
        if found != self.front {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the listing-door factory {} serves front {found}, not {}",
                self.factory, self.front
            )));
        }
        // The doors' creation code hash is the factory's own answer: every door's address moves
        // with its code. Where the caller pinned one, a factory answering another is not the one
        // the caller checked.
        let door_codehash = setting::<DOOR_CODEHASHCall>(door_codehash, "factory's DOOR_CODEHASH")?;
        if let Some(pinned) = self
            .door_codehash
            .filter(|pinned| *pinned != door_codehash)
        {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the listing-door factory {} creates doors of code hash \
                 {door_codehash}, not the pinned {pinned}",
                self.factory
            )));
        }
        if let (Some(configured), Some(answer)) = (self.cell_codehash, front_cell_codehash) {
            let found = setting::<CELL_CODEHASHCall>(answer, "front's CELL_CODEHASH")?;
            if found != configured {
                return Err(FeedError::Fatal(format!(
                    "Efficient PropAMM: the front {} creates desk contracts of code hash {found}, \
                     not the configured {configured}; nothing is listed, since no desk contract \
                     could be placed",
                    self.front
                )));
            }
        }
        let timing = setting::<clockCall>(clock, "front's clock")?;
        // On BNB Chain mode 3 reads the block's second while the precompile answers nothing, as
        // mode 0 does: the next block is sampled at its second. Once it answers, the front reads
        // the block's milliseconds, which no block override sets, and nothing is served.
        let reads_seal = timing.clockMode == U256::from(CLOCK_SEAL) ||
            (timing.clockMode == U256::from(CLOCK_PRECOMPILE) &&
                precompile
                    .is_some_and(|answer| answer.success && answer.returnData.is_empty()));
        if self.next_block.is_some() && !reads_seal {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the front's clock mode {} does not read the block's second, \
                 the one a front on Ethereum runs and a front on BNB Chain reads until its \
                 millisecond precompile answers",
                timing.clockMode
            )));
        }
        if self.next_block.is_none() && timing.clockMode > U256::from(CLOCK_BLOCKS) {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the front's clock mode {} is not one a Base front runs",
                timing.clockMode
            )));
        }
        let block_number =
            to_u64(setting::<getBlockNumberCall>(block_number, "block number")?, "block number")?;
        let timestamp =
            to_u64(setting::<getCurrentBlockTimestampCall>(timestamp, "timestamp")?, "timestamp")?;
        let mut found = Vec::new();
        // Counted by reason only on a chain whose builders land makers' updates (Ethereum, BNB
        // Chain): Base's listings left out go uncounted.
        let left_out = |reason: NotCovered| {
            if self.next_block.is_some() {
                telemetry::record_listing_not_covered(reason);
            }
        };
        // The highest id the front has reserved, served or not: the next poll scans past it, so a
        // listing this read leaves out never hides the ones reserved after it.
        let mut highest = 0;
        for (id, answer) in (1..=scan_to).zip(listings) {
            let view = setting::<doorViewCall>(answer, "front's doorView")?;
            if view.pool == Address::ZERO {
                continue;
            }
            highest = id;
            // Anyone can reserve a listing and post quotes to it, so a listing is served only
            // where its maker's desk contract is approved, on every chain: the builder takes no
            // feed without the desk contracts' code hash.
            if let Some(cell_codehash) = self.cell_codehash {
                let desk = SLOT_RULE.account(self.front, cell_codehash, view.desk);
                if view.desk == 0 || !approved_desks.contains(&desk) {
                    debug!(listing = id, %desk, "skipping listing of a desk that is not approved");
                    left_out(NotCovered::DeskNotApproved);
                    continue;
                }
            }
            // A credit listing fills an input past its pool's custody whole through the pool's own
            // preview, which may pay less than a smaller input and costs other gas: its output may
            // step where the route takes over. It is served in a credit mode the front knows, and
            // only where the caller configured its gas, each size then tagged by the path the
            // front's own plan gives it at every moment sampled.
            let credit = match view.creditMode {
                0 => None,
                mode @ 1..=MAX_CREDIT_MODE => {
                    let Some(gas) = self.credit.get(&id) else {
                        debug!(
                            listing = id,
                            credit_mode = mode,
                            "skipping credit listing with no gas configured"
                        );
                        left_out(NotCovered::NoGas);
                        continue;
                    };
                    Some(*gas)
                }
                // A mode above the front's three, which the deployed front's two bits cannot
                // hold and `doorView`'s byte can: left out and counted, so a front naming one
                // shows in the count.
                mode => {
                    debug!(listing = id, credit_mode = mode, "skipping credit listing");
                    left_out(NotCovered::CreditMode);
                    continue;
                }
            };
            // A listing in no credit mode the caller configured gas for is charged it. A listing
            // of an asset-module pair (a stock token) runs its module's rules inside every fill:
            // the token's own reads, each given up to 28,000 gas (`PropAMMStockRules.READ_GAS`),
            // which differ by token and which no measured figure holds. It is served only with
            // such a figure, and left out otherwise rather than charged less than it spends.
            let asset_module = view.pairWord.bit(PAIR_ASSET_BIT);
            let named_gas = match (credit, self.credit.get(&id)) {
                (Some(_), _) => None,
                (None, Some(gas)) => Some(*gas),
                (None, None) if asset_module => {
                    debug!(
                        listing = id,
                        "skipping listing of an asset-module pair with no gas configured"
                    );
                    left_out(NotCovered::NoGas);
                    continue;
                }
                (None, None) => None,
            };
            let tip_cap_wei = tip_cap_wei(view.tipCap);
            if tip_cap_wei.is_some_and(|cap| cap < tip_floor) {
                debug!(
                    listing = id,
                    tip_cap_wei = ?tip_cap_wei,
                    %tip_floor,
                    "skipping listing capping the tip under the floor"
                );
                left_out(NotCovered::TipCapUnderFloor);
                continue;
            }
            // `doorView`'s depth words by role: the bid's (the base in) first, then the ask's. Each
            // is paired with its fills once the door names its base (`open_doors`): `baseIndex` is
            // the base's place in the pool, not in the pair.
            let side = |first: U256, second: U256, bid: bool| SideView {
                runs_out_ms: runs_out_ms(view.word, first, &timing),
                drift: field(first, 92, 20),
                nonce_ms: field(first, 12, 48),
                post: RungPost::of(first, second, view.pairWord, bid),
            };
            let (bid, ask) = (
                side(view.depth[0], view.depth[1], true),
                side(view.depth[2], view.depth[3], false),
            );
            found.push(Listing {
                id,
                view: keccak256(&answer.returnData),
                token0: view.token0,
                token1: view.token1,
                door: self
                    .factory
                    .create2(B256::from(U256::from(id)), door_codehash),
                word_ms: field(view.word, 72, 48),
                bid,
                ask,
                tip_cap_wei,
                desk: view.desk,
                word_index: view.wordIndex,
                asset_module,
                pair: PairPlace::of(view.baseIndex, view.quoteIndex),
                credit,
                named_gas,
                pool: view.pool,
            });
        }
        let clock = FrontClock::new(&timing, block_number, timestamp, None);
        let source_ms = clock
            .source()
            .and_then(|_| base_time.first())
            .and_then(ms_reading);
        Ok((FrontClock::new(&timing, block_number, timestamp, source_ms), found, highest))
    }

    /// The second request: the front's implementation and each listing's settings, controls,
    /// session, source and live-source words at the block, which of `listings` have a door of this
    /// front's, its base, and the most each side fills. A SESSION listing with a closed phase, or
    /// whose session word the node did not answer, is left out (see the module doc of
    /// `efficient_prop`); so is a listing reading a print bound to a source not approved, or whose
    /// source word is unread, a tied listing, and one reading a print whose live source's copy is
    /// unread. A listing reading a print from a guarded source keeps that source's lane.
    ///
    /// On a chain whose builders land makers' updates, `next` is the next block's clock and the
    /// state override of the listings it serves (see [`Self::take_next_block`]): the doors are
    /// asked under both, and a listing reading a print from a guarded source is left out, since
    /// that chain's sources are price contracts and no lane holds a print there. A node that
    /// refuses or ignores the next block's overrides fails the read. Beside the doors, whether
    /// the node answered every word asked for them: an unread session, sources or live-source
    /// word leaves its listing out of this read alone ([`Answered`]).
    async fn open_doors(
        &self,
        listings: &[(Listing, (U256, U256))],
        block: BlockId,
        next: Option<(&FrontClock, &serde_json::Value)>,
    ) -> Result<(Vec<OpenDoor>, Answered), FeedError> {
        let mut calls = Vec::with_capacity(listings.len() * DOOR_CALLS);
        let mut slots = Vec::with_capacity(listings.len() * RECORD_WORDS);
        for (Listing { id, token0, token1, door, .. }, (probe0, probe1)) in listings {
            calls.extend([
                call(*door, FRONTCall {}.abi_encode()),
                call(*door, LISTINGCall {}.abi_encode()),
                call(*door, BASECall {}.abi_encode()),
                call(
                    *door,
                    quoteFillableCall { tokenIn: *token0, tokenOut: *token1, amountIn: *probe0 }
                        .abi_encode(),
                ),
                call(
                    *door,
                    quoteFillableCall { tokenIn: *token1, tokenOut: *token0, amountIn: *probe1 }
                        .abi_encode(),
                ),
            ]);
            slots.extend([
                record_word_slot(*id, SETTINGS_WORD),
                record_word_slot(*id, CONTROLS_WORD),
                record_word_slot(*id, SESSION_WORD),
                record_word_slot(*id, SOURCES_WORD),
                record_word_slot(*id, PRICE_WORD),
                record_word_slot(*id, BOARD_WORDS[0]),
                record_word_slot(*id, BOARD_WORDS[1]),
            ]);
        }
        // The implementation again, at the block every door is read at: the first request's read
        // at the latest block may have come from another backend than its listings.
        let at = next.map(|(clock, _)| clock.read_at());
        let BatchAnswers { groups, words, .. } = self
            .batch(
                block,
                vec![Group { calls, at }],
                &slots
                    .iter()
                    .map(|slot| (self.front, *slot))
                    .collect::<Vec<_>>(),
                Also::Implementation,
                next.map(|(clock, _)| clock),
                next.map(|(_, state)| state),
            )
            .await?;
        let answers = groups.into_iter().next().flatten();
        if next.is_some() && answers.is_none() {
            return Err(refused_next_block());
        }
        let answers = answers.unwrap_or_default();
        let mut words = words.into_iter();
        let mut open = Vec::new();
        let mut answered = Answered::Whole;
        for ((listing, (probe0, probe1)), answers) in listings
            .iter()
            .zip(answers.chunks(DOOR_CALLS))
        {
            let (
                [front, id, base, fill0, fill1],
                Some(settings),
                Some(controls),
                Some(session),
                Some(sources),
                Some(price),
                Some(bid_board),
                Some(ask_board),
            ) = (
                answers,
                words.next(),
                words.next(),
                words.next(),
                words.next(),
                words.next(),
                words.next(),
                words.next(),
            )
            else {
                return Err(FeedError::Parsing("multicall answered too few calls".into()));
            };
            // An unread board word leaves its side sampled as it would be with no post: its
            // boundaries move sizes only, so the read is whole without them.
            let (bid_board, ask_board) = (bid_board.ok(), ask_board.ok());
            let (settings, controls) = (settings?, controls?);
            if session.is_err() || sources.is_err() || price.is_err() {
                answered = Answered::InPart;
            }
            let prints = matches!(field(settings, 228, 2), FENCE_PRINT | FENCE_SESSION);
            // A listing reading a print fills at its source's answer, and a source's code can tell
            // a fill from a quote: the listing is served only where each source it is bound to is
            // approved (see the module's Pricing), its binding read at this block.
            if prints &&
                !sources
                    .as_ref()
                    .is_ok_and(|w10| self.approves(*w10))
            {
                let bound = sources
                    .as_ref()
                    .ok()
                    .map(|w10| (field(*w10, 0, 32), field(*w10, 32, 32)));
                debug!(
                    listing = listing.id,
                    sources = ?bound,
                    "skipping listing reading a print from a source not approved, or unread"
                );
                continue;
            }
            // A tied listing is not served: it walks its maker's ladder from its print rather than
            // its word, which no few samples bound over a book's life. Nor is one reading a print
            // whose live source's copy is unread, which names the source's lane and its guard.
            let tied = field(settings, 228, 4) == TIED_SCOPES;
            let price = price.as_ref().ok().copied();
            if prints && (tied || price.is_none()) {
                debug!(
                    listing = listing.id,
                    tied,
                    "skipping tied listing, or one reading a print whose live source's copy is unread"
                );
                continue;
            }
            // A guarded source prices a fill's side at the worst of the prints its lane holds: its
            // lane, the copy's low 160 bits, is asked how long it holds one (see `sample`).
            let guarded_lane = price
                .filter(|price| prints && price.bit(GUARD_BIT))
                .map(|price| Address::from_word(B256::from(price)));
            // A SESSION listing with a closed phase, or whose session word is unread, is not
            // served: in the closed phase the band eases as the print ages while the word's levels
            // tighten, so no few samples bound what the door quotes over a book's life, and the
            // gap between the phases may be shorter than a second.
            let closed_after = session
                .as_ref()
                .ok()
                .map(|session| field(*session, 0, 16));
            if field(settings, 228, 2) == FENCE_SESSION && closed_after != Some(0) {
                debug!(
                    listing = listing.id,
                    closed_after_s = ?closed_after,
                    "skipping SESSION listing with a closed phase, or whose session word is unread"
                );
                continue;
            }
            if front.returnData.is_empty() {
                debug!(listing = listing.id, door = %listing.door, "listing has no door yet");
                continue;
            }
            let front = decode::<FRONTCall>(front, "door's FRONT");
            let id = decode::<LISTINGCall>(id, "door's LISTING");
            let base = decode::<BASECall>(base, "door's BASE");
            let (Ok(front), Ok(id), Ok(base)) = (front, id, base) else {
                warn!(
                    listing = listing.id,
                    door = %listing.door,
                    "skipping door that does not answer as one"
                );
                continue;
            };
            if front != self.front || id != U24::from(listing.id) {
                warn!(
                    listing = listing.id,
                    door = %listing.door,
                    %front,
                    door_listing = %id,
                    "skipping door that names another front or listing"
                );
                continue;
            }
            // A chain whose builders land makers' updates has no guarded source: its sources are
            // price contracts, and no lane holds a replaced print there.
            if next.is_some() && guarded_lane.is_some() {
                debug!(
                    listing = listing.id,
                    "skipping listing reading a print from a guarded source on a chain whose \
                     builders land makers' updates"
                );
                continue;
            }
            let side = |answer: &Call3Result, view: SideView, probe: U256, board: Option<U256>| {
                let (fillable, fillable_out) =
                    decode::<quoteFillableCall>(answer, "door's quoteFillable")
                        .map_or((U256::ZERO, U256::ZERO), |fill| {
                            (fill.fillableAmountIn, fill.amountOut)
                        });
                OpenSide { fillable, fillable_out, probe, view, board }
            };
            // `fill0` sells token0 and `fill1` token1: the bid's is the one selling the base.
            let (quote, bid, ask) = if base == listing.token0 {
                (
                    listing.token1,
                    side(fill0, listing.bid, *probe0, bid_board),
                    side(fill1, listing.ask, *probe1, ask_board),
                )
            } else if base == listing.token1 {
                (
                    listing.token0,
                    side(fill1, listing.bid, *probe1, bid_board),
                    side(fill0, listing.ask, *probe0, ask_board),
                )
            } else {
                warn!(
                    listing = listing.id,
                    door = %listing.door,
                    %base,
                    "skipping door whose base is not in its pair"
                );
                continue;
            };
            open.push(OpenDoor {
                listing: listing.id,
                door: listing.door,
                base,
                quote,
                word_ms: listing.word_ms,
                bid,
                ask,
                aging: Aging::from_controls(controls),
                prints,
                guarded_lane,
                asset_module: listing.asset_module,
                pair: listing.pair,
                refills: controls.bit(CONTROLS_REFILL_BIT),
                min_fill: amount24(field(settings, 160, 24)),
                tip_cap_wei: listing.tip_cap_wei,
                carries: listing_carries(settings),
                credit: listing.credit,
                named_gas: listing.named_gas,
                pool: listing.pool,
            });
        }
        Ok((open, answered))
    }

    /// The third request: every open door's quotes at each side's sampled sizes, at the read's
    /// block, and again under block overrides at the last block of the book's life, before
    /// `until_ms` on the front's clock (see [`FrontClock`]).
    ///
    /// A listing's levels may drift and widen with its word's age, and a premium may hold them back
    /// for a window after a post (`PropAMMFront._discount`), so the door may quote less at any
    /// moment of the book's life than at the read. The front's clock never reads earlier at a later
    /// block on a front served (see [`FrontClock::may_step_back`]), so as it reads later the drift
    /// and the widening only grow and the premium only ends: on every listing read (not a SESSION
    /// listing's closed phase, whose band eases as its print ages, nor a tied listing, neither of
    /// which is read, see [`Self::open_doors`]), the least the door quotes over the life is at the
    /// read, at its last block, at the last block before the premium's window closes where that
    /// falls inside it, or within a guarded source's hold (below). Each size is sampled at each of
    /// those, the window's edge on every side with a premium, and keeps the least, or no sample
    /// where the door refuses it at any. That bounds the price, not the refusals: a side bound by
    /// its output (a maker's bucket, the pool's room) pays the most at the first block at or after
    /// its premium's window closes, the bucket only refilling and the drift only lowering the
    /// output after it, so each size of a side with a premium edge in the life is also asked there
    /// and dropped where it is refused.
    ///
    /// A listing reading a print from a guarded source fills each side at the worst for its taker
    /// of the prints the source's lane holds: a print a post replaced is held for the lane's
    /// `HOLD_MS` on the fronts' clock, so where a post moved the price at or before the read, the
    /// door quotes that side less while the replaced print is held and more once its hold ends,
    /// past an output bound it then refuses. A post the read saw was made at or before it, so its
    /// hold ends at the latest `HOLD_MS` after the read: each size of both sides is also asked at
    /// every block after the read's through the first at or past that, kept at the least and
    /// dropped where it is refused at any, as at a premium's edge; from there the price moves one
    /// way again. The lane's `HOLD_MS` is asked in this request the first time its listing is read,
    /// and until it answers the longest a lane holds, 2 s, is sampled.
    ///
    /// A listing that reads a price print (its fence kind PRINT or SESSION) fills only while the
    /// front takes that print, and no later print is assumed: past the print's window or the
    /// listing's maximum age the door refuses every size. A SESSION listing with a closed phase,
    /// where the door fills again once the print is old enough and its quote is not monotone in the
    /// clock, is not read (see [`Self::open_doors`]); every listing sampled refuses its print from
    /// its lapse on. On such a listing each side is also asked, at the last block of each second of
    /// the life between the read's block and its last ([`FrontClock::seconds_until`]), whether the
    /// door still fills its smallest and its largest size; the life's end asks it too. The front
    /// refuses every size while it refuses the print, so a door that fills either size at an ask
    /// took the print there, and where it refuses both at an ask the print's life ends within the
    /// book's. A size refused at an ask within the side's life while the other fills is refused for
    /// a reason of its own (the least fill under the listing's minimum once the levels drift), and
    /// is not quoted. These asks share one group a second with every other side reading a print.
    ///
    /// A side whose print lapses within the book's life gets a life of its own: a fourth request
    /// asks every size of it at the last ask the print lived through and at each block between
    /// that ask and the next ([`FrontClock::blocks_between`]), sharing one group a block with every
    /// other such side. Its life ends one millisecond after the last of those blocks the door fills
    /// at, run without a gap from the first (`DoorRead::bid_until_ms`); its samples are the least
    /// at the read, at those blocks, and at each premium edge before that end, so the price bound
    /// above holds over its own life. Where the next ask's answer is unknown, or it comes sooner
    /// than `min_side_ms` after `start_ms` (no route priced from the book could be sent and land
    /// within the side's life), the fourth request is not made and the side lives to the read
    /// alone, which the assembler leaves out.
    ///
    /// On a credit listing each size is also asked of the front's `quoteFrom` with `PLAN_ONLY`
    /// beside the door's quote in every group that asks it, and tagged by [`classify`] at each:
    /// a size whose path differs between two moments, or whose plan disagrees with the door at
    /// any, is no sample. Its sides that may route are first asked how far they reach at the
    /// read's block ([`Self::reaches`]), and are run custody sizes then routed sizes
    /// ([`stepped`]), its custody sizes alone where another door here shares what its route
    /// owes ([`custody_run`]). Every door's pool is asked its two balances at the block.
    ///
    /// A node that refuses the override or runs it at another moment on the front's clock (caught
    /// by the reads of the clock under it, see [`DoorsReader::batch`]) leaves a side its samples
    /// at the read only where its levels cannot age within the life: no drift and no widening, or
    /// a word that stays within its grace for all of it, with no premium window closing within
    /// it, on a listing that reads no print. Any other side is marked as not holding.
    /// On a side reading a print the asks are then unknown, and it lives to the read alone.
    async fn sample(
        &self,
        doors: &[OpenDoor],
        clock: &FrontClock,
        start_ms: u64,
        until_ms: u64,
        min_side_ms: u64,
    ) -> Result<Vec<DoorRead>, FeedError> {
        if doors.is_empty() {
            return Ok(Vec::new());
        }
        // The life ends before `until_ms`: its last block is the last before it.
        let until = clock.at_or_before(until_ms.saturating_sub(1));
        // A credit listing's route fills sizes its door's most-fillable read does not report:
        // each side that may route is first asked how far it reaches, at the read's block.
        let block = BlockId::number(clock.number);
        let reaches = self
            .reaches(doors, None, clock, block, None)
            .await?;
        // The holds of the guarded sources' lanes already answered, and the lanes to ask this
        // read, whose hold is taken as the longest a lane keeps until they answer.
        let holds_ms = self
            .holds_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let lanes: Vec<Address> = doors
            .iter()
            .filter_map(|door| door.guarded_lane)
            .filter(|lane| !holds_ms.contains_key(lane))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // Per door and side: the sizes, its calls, and the block before its premium ends.
        let mut sides = Vec::with_capacity(doors.len() * 2);
        for (door, reach) in doors.iter().zip(&reaches) {
            for (bid, side) in [(true, &door.bid), (false, &door.ask)] {
                let (sizes, calls) = self.side_quotes(door, bid, reach[usize::from(!bid)]);
                // Where the side's premium window closes after the read and within the life: the
                // last block before it, where the premium last holds the levels back, and the
                // first at or after it, where they pay the most, past an output bound the door
                // then refuses.
                let aging = &door.aging;
                let ends_ms = (aging.premium != 0 && aging.window_ms != 0)
                    .then(|| {
                        let last = door
                            .word_ms
                            .max(side.view.nonce_ms)
                            .max(aging.nonce_ms);
                        last.saturating_add(aging.window_ms)
                    })
                    .filter(|&ends_ms| clock.ms < ends_ms && ends_ms <= until.ms);
                let mut edges: Vec<At> = ends_ms
                    .map(|ends_ms| {
                        [clock.last_before(ends_ms), clock.first_from(ends_ms)]
                            .into_iter()
                            .flatten()
                            .filter(|at| *at != until)
                            .collect()
                    })
                    .unwrap_or_default();
                // On a guarded source, every block of the lane's hold after the read's and the
                // first past it: a print a post replaced at or before the read prices the side
                // there, worse for its taker, and from the hold's end on it pays more, past an
                // output bound the door then refuses.
                if let Some(lane) = door.guarded_lane {
                    let hold_ms = holds_ms
                        .get(&lane)
                        .copied()
                        .unwrap_or(MAX_HOLD_MS);
                    edges.extend(
                        clock
                            .blocks_held(hold_ms, &until)
                            .into_iter()
                            .filter(|at| *at != until),
                    );
                    edges.sort();
                    edges.dedup();
                }
                sides.push((sizes, calls, edges, ends_ms.is_some()));
            }
        }
        // One group at the block, one at the life's end, and one per distinct moment a premium
        // window closes around, asking only the sides it closes on.
        let all: Vec<_> = sides
            .iter()
            .flat_map(|(_, calls, _, _)| calls.iter().cloned())
            .collect();
        // The lanes not yet answered are asked their hold at the block, after every quote, and
        // after them what each door's pool holds of its two tokens.
        let quotes = all.len();
        let at_block_calls = all
            .iter()
            .cloned()
            .chain(
                lanes
                    .iter()
                    .map(|&lane| call(lane, HOLD_MSCall {}.abi_encode())),
            )
            .chain(doors.iter().flat_map(|door| {
                [door.base, door.quote]
                    .map(|token| call(token, balanceOfCall { account: door.pool }.abi_encode()))
            }))
            .collect();
        let mut groups =
            vec![Group { calls: at_block_calls, at: None }, Group { calls: all, at: Some(until) }];
        let mut edges: BTreeMap<At, (usize, Vec<usize>)> = BTreeMap::new();
        for (i, (_, _, side_edges, _)) in sides.iter().enumerate() {
            for edge in side_edges {
                edges
                    .entry(*edge)
                    .or_default()
                    .1
                    .push(i);
            }
        }
        for (at, (group, members)) in &mut edges {
            *group = groups.len();
            let calls = members
                .iter()
                .flat_map(|&i| sides[i].1.iter().cloned())
                .collect();
            groups.push(Group { calls, at: Some(*at) });
        }
        // Then one group a second of the life between the read and its end, asking every side
        // that reads a print for its smallest and its largest size: `probed[s]` is side `s`'s
        // places in each, one where it has one size.
        let mut probed = vec![None; sides.len()];
        let mut probes = Vec::new();
        for (s, (sizes, calls, _, _)) in sides.iter().enumerate() {
            if let (true, Some(smallest), Some(largest)) = (
                doors[s / 2].prints,
                calls.first(),
                sizes
                    .len()
                    .checked_sub(1)
                    .and_then(|last| calls.get(last)),
            ) {
                let small = probes.len();
                probes.push(smallest.clone());
                if sizes.len() > 1 {
                    probes.push(largest.clone());
                }
                probed[s] = Some((small, probes.len() - 1));
            }
        }
        let seconds = if probes.is_empty() { Vec::new() } else { clock.seconds_until(until_ms) };
        for at in &seconds {
            groups.push(Group { calls: probes.clone(), at: Some(*at) });
        }
        let BatchAnswers { groups: answers, .. } = self
            .batch(block, groups, &[], Also::Nothing, Some(clock), None)
            .await?;
        let mut answers = answers.into_iter();
        let at_block = answers
            .next()
            .flatten()
            .unwrap_or_default();
        for (lane, answer) in lanes.iter().zip(
            at_block
                .get(quotes..quotes + lanes.len())
                .unwrap_or_default(),
        ) {
            match decode::<HOLD_MSCall>(answer, "price lane's HOLD_MS") {
                Ok(hold_ms) => {
                    self.holds_ms
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(*lane, hold_ms.saturating_to());
                }
                Err(error) => debug!(
                    %lane,
                    %error,
                    "guarded source's lane did not answer its hold: the longest a lane keeps is \
                     sampled"
                ),
            }
        }
        let at_end = answers.next().flatten();
        let at_edges: Vec<_> = answers
            .by_ref()
            .take(edges.len())
            .collect();
        let at_seconds: Vec<_> = answers.collect();
        if at_end.is_none() {
            debug!(
                "the node refused the block override or ran it at another moment on the front's \
                 clock; samples hold only on listings that cannot age"
            );
        }
        // Whether each door's pool holds none of its base and none of its quote, at the block.
        let mut held = at_block
            .get(quotes + lanes.len()..)
            .unwrap_or_default()
            .iter()
            .map(|answer| {
                decode::<balanceOfCall>(answer, "token's balanceOf")
                    .is_ok_and(|held| held.is_zero())
            });
        // Each side's answers in each group, by position: its quotes, then on a credit listing
        // its plans.
        let mut offsets = Vec::with_capacity(sides.len());
        let mut offset = 0;
        for (_, calls, _, _) in &sides {
            offsets.push(offset);
            offset += calls.len();
        }
        // Per side, its place in each premium edge's group it is asked in, and that edge's block.
        let mut edge_offsets = vec![Vec::new(); sides.len()];
        for (at, (group, members)) in &edges {
            let mut offset = 0;
            for &i in members {
                edge_offsets[i].push((*group - 2, offset, *at));
                offset += sides[i].1.len();
            }
        }
        // On a side that reads a print: at each second's ask and at the life's end, whether the
        // door fills its smallest and its largest size, `None` where that is unknown.
        let asks: Vec<Vec<Option<(bool, bool)>>> = (0..sides.len())
            .map(|s| {
                let size_count = sides[s].0.len();
                probed[s].map_or_else(Vec::new, |(small, large)| {
                    let end = at_end.as_deref().map(|answers| {
                        let side = &answers[offsets[s]..offsets[s] + size_count];
                        (quoted(&side[0]).is_some(), quoted(&side[size_count - 1]).is_some())
                    });
                    at_seconds
                        .iter()
                        .map(|answers| {
                            answers.as_deref().map(|answers| {
                                (
                                    quoted(&answers[small]).is_some(),
                                    quoted(&answers[large]).is_some(),
                                )
                            })
                        })
                        .chain(std::iter::once(end))
                        .collect()
                })
            })
            .collect();
        // The asks' blocks, in their order: each second's, then the life's end.
        let checkpoints: Vec<At> = seconds
            .iter()
            .copied()
            .chain(std::iter::once(until))
            .collect();
        // The front refuses every size while it refuses the print, so the print lives through
        // each ask at which the door fills either size. Where a side's print lapses within the
        // life (the door fills neither at an ask), it lived through the ask before, and maybe
        // through some of the blocks after: that ask's block and each block between it and the
        // next are asked every size of the side in a fourth request, so that the side's own life
        // ends at its print's last block, not up to a second sooner. Per side reading a print
        // whose print lapses within the life: how many asks it lived through, and the blocks
        // asked. Not asked where the next ask's answer is unknown, or comes too soon for a route
        // to be sent and land: the side then lives to the read alone, which no book serves.
        let mut refined: Vec<Option<(usize, Vec<At>)>> = vec![None; sides.len()];
        let mut refine: BTreeMap<At, (usize, Vec<usize>)> = BTreeMap::new();
        for (s, side_asks) in asks.iter().enumerate() {
            let alive = side_asks
                .iter()
                .take_while(|ask| matches!(ask, Some((small, large)) if *small || *large))
                .count();
            if probed[s].is_none() || alive == checkpoints.len() {
                continue;
            }
            let next = checkpoints[alive];
            if side_asks[alive].is_none() || next.ms < start_ms.saturating_add(min_side_ms) {
                refined[s] = Some((alive, Vec::new()));
                continue;
            }
            let last = alive
                .checked_sub(1)
                .map(|k| checkpoints[k]);
            let from = last.unwrap_or_else(|| clock.read_at());
            let blocks: Vec<At> = last
                .into_iter()
                .chain(clock.blocks_between(&from, &next))
                .collect();
            for at in &blocks {
                refine.entry(*at).or_default().1.push(s);
            }
            refined[s] = Some((alive, blocks));
        }
        let mut refine_groups = Vec::with_capacity(refine.len());
        for (at, (group, members)) in &mut refine {
            *group = refine_groups.len();
            let calls = members
                .iter()
                .flat_map(|&i| sides[i].1.iter().cloned())
                .collect();
            refine_groups.push(Group { calls, at: Some(*at) });
        }
        let at_refine = if refine_groups.is_empty() {
            Vec::new()
        } else {
            self.batch(
                BlockId::number(clock.number),
                refine_groups,
                &[],
                Also::Nothing,
                Some(clock),
                None,
            )
            .await?
            .groups
        };
        // Per side, its place in each of the fourth request's groups it is asked in, in the order
        // of their blocks.
        let mut refine_offsets = vec![Vec::new(); sides.len()];
        for (at, (group, members)) in &refine {
            let mut offset = 0;
            for &i in members {
                refine_offsets[i].push((*group, offset, *at));
                offset += sides[i].1.len();
            }
        }
        let until_age_ms = clock.age_ms(&until);
        let mut reads = Vec::with_capacity(doors.len());
        for (d, door) in doors.iter().enumerate() {
            let take = |s: usize, view: &SideView| -> (Samples, Option<U256>, bool, u64) {
                let (sizes, calls, _, premium_ends) = &sides[s];
                let size_count = sizes.len();
                let call_count = calls.len();
                // One group's answers for the side, each size with the path its fill takes.
                let classified = |answers: &[Call3Result]| {
                    let (quotes, plans) = answers.split_at(size_count.min(answers.len()));
                    classify(sizes, quotes, door.credit.map(|_| plans))
                };
                // Where its print lapses within the book's life, the side's own life ends at the
                // last block the fourth request found its print living, run without a gap from
                // the last ask it lived through: the read's block where none.
                let (lives_to, lived): (Option<At>, Vec<&[Call3Result]>) = match &refined[s] {
                    None => (None, Vec::new()),
                    Some(_) => {
                        let mut last = None;
                        let mut lived = Vec::new();
                        for &(group, offset, at) in &refine_offsets[s] {
                            let Some(answers) = at_refine[group].as_deref() else {
                                break;
                            };
                            let side = &answers[offset..offset + call_count];
                            if !side[..size_count]
                                .iter()
                                .any(|answer| quoted(answer).is_some())
                            {
                                break;
                            }
                            last = Some(at);
                            lived.push(side);
                        }
                        (Some(last.unwrap_or_else(|| clock.read_at())), lived)
                    }
                };
                let within = |at: &At| lives_to.is_none_or(|last| at.ms <= last.ms);
                // The groups whose answers the side's samples take, each `None` where unknown:
                // the life's end where the side lives to it, each premium edge within its life,
                // and the blocks its print lived through found in the fourth request.
                let later: Vec<Option<&[Call3Result]>> = {
                    let end = lives_to.is_none().then(|| {
                        at_end
                            .as_deref()
                            .map(|answers| &answers[offsets[s]..offsets[s] + call_count])
                    });
                    let edges = edge_offsets[s]
                        .iter()
                        .filter(|(_, _, at)| within(at))
                        .map(|&(group, offset, _)| {
                            at_edges[group]
                                .as_deref()
                                .map(|answers| &answers[offset..offset + call_count])
                        });
                    end.into_iter()
                        .chain(edges)
                        .chain(lived.iter().copied().map(Some))
                        .collect()
                };
                let known = later.iter().all(Option::is_some);
                let first = classified(&at_block[offsets[s]..offsets[s] + call_count]);
                // Each size's least output over the moments asked, where every one of them fills
                // it by the same path: a size whose path differs between two moments bounds
                // neither its gas nor its output over the life.
                let later: Vec<_> = later
                    .iter()
                    .flatten()
                    .map(|answers| classified(answers))
                    .collect();
                let samples: Vec<_> = first
                    .iter()
                    .enumerate()
                    .map(|(k, &(amount, here))| {
                        let least = here.and_then(|first| {
                            later
                                .iter()
                                .try_fold(first, |(least, path), there| {
                                    let (output, later_path) = there.get(k)?.1?;
                                    (later_path == path).then_some((least.min(output), path))
                                })
                        });
                        (amount, least)
                    })
                    .collect();
                // A premium window closing within the life raises what the side pays there, past
                // an output bound the door then refuses: only the samples around it show that.
                let cannot_age = ((view.drift == 0 && door.aging.widen == 0) ||
                    until_age_ms.saturating_sub(door.word_ms) <= door.aging.grace_ms) &&
                    !premium_ends;
                if !known && !cannot_age {
                    debug!(
                        listing = door.listing,
                        door = %door.door,
                        "side's quotes later in the book's life unknown, and its levels age"
                    );
                }
                // The asks within the side's life: all of them where it lives to the life's end,
                // and where its print lapses within it those it lived through.
                let side_asks = match &refined[s] {
                    None => &asks[s][..],
                    Some((alive, _)) => &asks[s][..*alive],
                };
                // Where the print lives to the life's end, every ask shows it living.
                let print_lives = probed[s].and_then(|_| {
                    side_asks
                        .iter()
                        .try_fold(true, |lives, ask| {
                            let (small, large) = (*ask)?;
                            Some(lives && (small || large))
                        })
                });
                let print_holds = !door.prints || lives_to.is_some() || print_lives == Some(true);
                if door.prints && !sizes.is_empty() && lives_to.is_some() {
                    debug!(
                        listing = door.listing,
                        door = %door.door,
                        lives_to_ms = ?lives_to.map(|at| at.ms),
                        "side's print lapses within the book's life: served only until then"
                    );
                }
                let (mut samples, route_from) = stepped(if known { samples } else { first });
                // A size the door refuses at any ask within the side's life is not quoted.
                for &(small, large) in side_asks.iter().flatten() {
                    let last = samples.len() - 1;
                    if !small {
                        samples[0].1 = None;
                    }
                    if !large {
                        samples[last].1 = None;
                    }
                }
                let holds = if door.prints { known && print_holds } else { known || cannot_age };
                let side_until_ms = lives_to.map_or(until_ms, |at| at.ms.saturating_add(1));
                (samples, route_from, holds, side_until_ms)
            };
            let pool_lacks = (held.next().unwrap_or(false), held.next().unwrap_or(false));
            let (bid, bid_route_from, bid_holds, bid_until_ms) = take(2 * d, &door.bid.view);
            let (ask, ask_route_from, ask_holds, ask_until_ms) = take(2 * d + 1, &door.ask.view);
            // A credit listing sharing its pool with a door its route could move is served its
            // custody sizes alone, as on a chain whose builders land makers' updates.
            let custody_only = door.credit.is_some() && shares_what_its_route_owes(door, doors);
            let (bid, bid_route_from) = custody_run(bid, bid_route_from, custody_only);
            let (ask, ask_route_from) = custody_run(ask, ask_route_from, custody_only);
            reads.push(DoorRead {
                listing: door.listing,
                door: door.door,
                base: door.base,
                quote: door.quote,
                bid,
                ask,
                bid_holds,
                ask_holds,
                bid_until_ms,
                ask_until_ms,
                bid_runs_out_ms: door.bid.view.runs_out_ms,
                ask_runs_out_ms: door.ask.view.runs_out_ms,
                tip_cap_wei: door.tip_cap_wei,
                reads: door.fill_reads(),
                pair: door.pair,
                credit: door
                    .credit
                    .map(|gas| CreditRead { gas, bid_route_from, ask_route_from }),
                named_gas: door.named_gas,
                pool_lacks,
            });
        }
        Ok(reads)
    }

    /// The fourth request on a chain whose builders land makers' updates: every open door's quotes
    /// at each side's sampled sizes in the next block, the one the state map names, and, on a
    /// listing that carries, in the block after it, under the same map: those are the only blocks
    /// the maker's update for the next block fills in. A side lives through the next block, and
    /// through the one after where the door fills every size there that it fills in the next block;
    /// its samples are then the least of the two. Elsewhere it lives through the next block alone,
    /// with its samples there. The samples are the door's own answers at every block of the side's
    /// life, so they hold through it, whatever its word, depth, premium, print or source. Beside
    /// the reads, whether the node answered every block asked: one refusing the block after
    /// leaves the sides that carry their next block's life alone, in this read ([`Answered`]).
    ///
    /// On a credit listing each size is also asked of the front's `quoteFrom` with `PLAN_ONLY`,
    /// beside the door's quote in the same call: an answer tagged `ROUTED` shows the size fills
    /// through the credit route, an untagged one equal to the door's quote that it fills inside
    /// the pool's custody, and any other answer leaves the size unsampled (see [`classify`]). In
    /// the block after, a size whose path differs from the next block's is no sample. The run is
    /// custody sizes, then routed sizes: a custody size above a routed one ends it, as a refusal
    /// does (see [`stepped`]). A credit listing sharing its pool with another door here that its
    /// route could move, one with a credit mode or one trading a token the route may leave owed,
    /// is served its custody sizes alone (see [`shares_what_its_route_owes`], [`custody_run`]).
    async fn sample_next_block(
        &self,
        doors: &[OpenDoor],
        clock: &FrontClock,
        moments: &NextMoments,
        block: BlockId,
        state: &serde_json::Value,
    ) -> Result<(Vec<DoorRead>, Answered), FeedError> {
        if doors.is_empty() {
            return Ok((Vec::new(), Answered::Whole));
        }
        let target = moments
            .next
            .first()
            .copied()
            .unwrap_or_else(|| clock.read_at());
        // A credit listing's route fills sizes its door's most-fillable read does not report:
        // each side that may route is first asked how far it reaches.
        let reaches = self
            .reaches(doors, Some(target), clock, block, Some(state))
            .await?;
        // Per side: its sizes, and its door's quote at each, then on a credit listing the
        // front's plan at each.
        let sides: Vec<_> = doors
            .iter()
            .zip(&reaches)
            .flat_map(|(door, reach)| {
                [true, false].map(|bid| self.side_quotes(door, bid, reach[usize::from(!bid)]))
            })
            .collect();
        // After every side's calls, what each door's pool holds of its two tokens.
        let held = doors.iter().flat_map(|door| {
            [door.base, door.quote]
                .map(|token| call(token, balanceOfCall { account: door.pool }.abi_encode()))
        });
        let all: Vec<_> = sides
            .iter()
            .flat_map(|(_, calls)| calls.iter().cloned())
            .chain(held)
            .collect();
        let carried: Vec<_> = sides
            .iter()
            .enumerate()
            .filter(|(s, _)| doors[s / 2].carries)
            .flat_map(|(_, (_, calls))| calls.iter().cloned())
            .collect();
        // Every side at each time the next block may carry, then the carrying sides at each
        // time the block after may.
        let mut groups: Vec<_> = moments
            .next
            .iter()
            .map(|&at| Group { calls: all.clone(), at: Some(at) })
            .collect();
        let carries = !carried.is_empty();
        if carries {
            groups.extend(
                moments
                    .after
                    .iter()
                    .map(|&at| Group { calls: carried.clone(), at: Some(at) }),
            );
        }
        let BatchAnswers { groups: answers, .. } = self
            .batch(block, groups, &[], Also::Nothing, Some(clock), Some(state))
            .await?;
        let mut answers = answers.into_iter();
        let at_next: Vec<Vec<Call3Result>> = answers
            .by_ref()
            .take(moments.next.len())
            .map(|answers| answers.ok_or_else(refused_next_block))
            .collect::<Result<_, _>>()?;
        let at_target = at_next
            .first()
            .cloned()
            .ok_or_else(refused_next_block)?;
        // The block after's answers at each of its times, `None` where any is unknown.
        let at_after: Option<Vec<Vec<Call3Result>>> = carries
            .then(|| answers.collect::<Option<Vec<_>>>())
            .flatten();
        let answered =
            if carries && at_after.is_none() { Answered::InPart } else { Answered::Whole };
        let (mut offset, mut after_offset) = (0, 0);
        let mut taken = Vec::with_capacity(sides.len());
        for (s, (sizes, calls)) in sides.iter().enumerate() {
            let door = &doors[s / 2];
            let count = calls.len();
            // The door's quotes, then on a credit listing the front's plans, at one block.
            let classified = |answers: &[Call3Result]| {
                let (quotes, plans) = answers.split_at(sizes.len().min(answers.len()));
                classify(sizes, quotes, door.credit.map(|_| plans))
            };
            // The next block at each of its times: a size every one fills by the same path, at
            // the least of their outputs, since the block lands at one of them.
            let mut samples = classified(&at_target[offset..offset + count]);
            for answers in at_next.iter().skip(1) {
                for ((_, here), (_, there)) in samples
                    .iter_mut()
                    .zip(classified(&answers[offset..offset + count]))
                {
                    *here = match (*here, there) {
                        (Some((here, path)), Some((there, later))) if path == later => {
                            Some((here.min(there), path))
                        }
                        _ => None,
                    };
                }
            }
            offset += count;
            let mut until_ms = moments.next_until_ms;
            if door.carries {
                let there: Option<Vec<_>> = at_after.as_deref().map(|answers| {
                    answers
                        .iter()
                        .map(|answers| classified(&answers[after_offset..after_offset + count]))
                        .collect()
                });
                after_offset += count;
                let later: Option<Vec<_>> = there.and_then(|there| {
                    samples
                        .iter()
                        .enumerate()
                        .map(|(k, (_, here))| match here {
                            None => Some(None),
                            Some((here, path)) => {
                                there
                                    .iter()
                                    .try_fold(Some(*here), |least, there| {
                                        // A size whose path differs between the two blocks
                                        // bounds neither its gas nor its output over the side's
                                        // life; one the block after refuses ends the carry.
                                        there[k].1.map(|(there, later)| {
                                            least
                                                .filter(|_| *path == later)
                                                .map(|least| least.min(there))
                                        })
                                    })
                                    .map(|least| least.map(|least| (least, *path)))
                            }
                        })
                        .collect()
                });
                if let Some(later) = later.filter(|later| later.iter().any(Option::is_some)) {
                    for ((_, sample), least) in samples.iter_mut().zip(later) {
                        *sample = least;
                    }
                    until_ms = moments.after_until_ms;
                }
            }
            let (samples, route_from) = stepped(samples);
            taken.push((samples, route_from, until_ms));
        }
        // A credit route borrows inside the swap, and the pool's cheap lane refuses to pay a token
        // it now owes: a mode-0 listing's fill paying that token reverts in the same transaction,
        // and a second credit listing's bid routes until a flush, at the route's gas and price.
        // Each listing is served as a book of its own, so a credit listing is served its routed
        // sizes only where no other door sampled here on its pool could be so moved: none of them
        // has a credit mode, and none trades a token the route may leave owed, either of the
        // credit listing's tokens but the pool's asset (its token 0, the collateral, which a
        // route supplies and withdraws and never borrows). Otherwise it is served the sizes its
        // pool's custody fills, which move no debt.
        let mut taken = taken.into_iter();
        let mut held = at_target
            .get(offset..)
            .unwrap_or_default()
            .iter()
            .map(|answer| {
                decode::<balanceOfCall>(answer, "token's balanceOf")
                    .is_ok_and(|held| held.is_zero())
            });
        let mut reads = Vec::with_capacity(doors.len());
        for door in doors {
            let pool_lacks = (held.next().unwrap_or(false), held.next().unwrap_or(false));
            let (
                Some((bid, bid_route_from, bid_until_ms)),
                Some((ask, ask_route_from, ask_until_ms)),
            ) = (taken.next(), taken.next())
            else {
                break;
            };
            let custody_only = door.credit.is_some() && shares_what_its_route_owes(door, doors);
            let (bid, bid_route_from) = custody_run(bid, bid_route_from, custody_only);
            let (ask, ask_route_from) = custody_run(ask, ask_route_from, custody_only);
            reads.push(DoorRead {
                listing: door.listing,
                door: door.door,
                base: door.base,
                quote: door.quote,
                bid,
                ask,
                bid_holds: true,
                ask_holds: true,
                bid_until_ms,
                ask_until_ms,
                // The samples are the door's answers at each block of the side's life, so a word
                // or depth that runs out within it shows there as a refusal.
                bid_runs_out_ms: u64::MAX,
                ask_runs_out_ms: u64::MAX,
                tip_cap_wei: door.tip_cap_wei,
                reads: door.fill_reads(),
                pair: door.pair,
                credit: door
                    .credit
                    .map(|gas| CreditRead { gas, bid_route_from, ask_route_from }),
                named_gas: door.named_gas,
                pool_lacks,
            });
        }
        Ok((reads, answered))
    }

    /// The front's plan for each of `sizes` on one side of a credit listing's `door`, the bid
    /// where `bid`: its tip-taking `quoteFrom` at a tip of zero with `PLAN_ONLY`, the door as the
    /// taker, as the door's own `quote` asks it without `PLAN_ONLY`.
    fn plan_quotes(&self, door: &OpenDoor, bid: bool, sizes: &[U256]) -> Vec<Call3> {
        let (token_in, token_out) =
            if bid { (door.base, door.quote) } else { (door.quote, door.base) };
        sizes
            .iter()
            .map(|&amount| {
                let plan = quoteFromCall {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    amountIn: amount,
                    listing: U24::from(door.listing),
                    tip: U256::from(1u8) << PLAN_ONLY,
                    taker: door.door,
                };
                call(self.front, plan.abi_encode())
            })
            .collect()
    }

    /// One side of `door`, the bid where `bid`, reaching `reach`: the sizes it is sampled at, in
    /// increasing size, and the door's `quote` call at each, then on a credit listing the front's
    /// plan at each ([`Self::plan_quotes`]).
    fn side_quotes(&self, door: &OpenDoor, bid: bool, reach: Reach) -> (Vec<U256>, Vec<Call3>) {
        let sizes = self.side_sizes(door, bid, reach);
        let mut calls = quote_calls(door, bid, &sizes);
        if door.credit.is_some() {
            calls.extend(self.plan_quotes(door, bid, &sizes));
        }
        (sizes, calls)
    }

    /// The sizes one side of `door` is sampled at, the bid where `bid`, in increasing size: those
    /// of the most it reports fillable ([`sizes`]) and, on a credit listing's side whose `reach`
    /// goes past that, the route's: where the side reports nothing fillable, the reach and its
    /// halvings as [`sizes`] gives them, else every first ask up to the reach; then
    /// [`REACH_STEPS`] even steps from the reach to the size refused above it. The door's answer
    /// at each decides what is quoted: a size refused ends the side's run.
    fn side_sizes(&self, door: &OpenDoor, bid: bool, reach: Reach) -> Vec<U256> {
        let side = if bid { &door.bid } else { &door.ask };
        // The fill the least size is priced off: the most the side reports, or with none, its
        // reach.
        let (most, most_out) = if side.fillable.is_zero() {
            (reach.size, reach.output)
        } else {
            (side.fillable, side.fillable_out)
        };
        // The least the side fills: the ask's input is the quote token, and the bid's output is,
        // which an input paid at the full fill's average price, the worst of any smaller fill, at
        // least reaches. A product past 256 bits puts the floor above every size the side reports
        // fillable.
        let floor = if bid && !door.min_fill.is_zero() {
            most_out
                .checked_sub(U256::from(1u8))
                .map_or(U256::ZERO, |_| {
                    door.min_fill
                        .checked_mul(most)
                        .map_or(U256::MAX, |input| input.div_ceil(most_out))
                })
        } else {
            door.min_fill
        };
        let mut all = sizes(side.fillable, self.rungs, floor);
        // Every boundary of its depth post the walk crosses below the most it fills, so that no
        // stretch between two samples spans one (see [`RungPost`]). Not on a credit listing,
        // whose route may take over past its pool's custody at any size.
        if door.credit.is_none() && !side.fillable.is_zero() {
            all.extend(
                side.rung_edges(door.refills)
                    .into_iter()
                    .filter(|edge| *edge >= floor && *edge < side.fillable),
            );
            all.sort_unstable();
            all.dedup();
        }
        if reach.size > side.fillable {
            if side.fillable.is_zero() {
                all.extend(sizes(reach.size, self.rungs, floor));
            } else {
                all.extend(
                    reach_probes(side)
                        .into_iter()
                        .filter(|size| *size <= reach.size),
                );
            }
            if let Some(gap) = reach
                .refused
                .checked_sub(reach.size)
                .filter(|_| !reach.refused.is_zero())
            {
                all.extend(
                    (1..REACH_STEPS)
                        .map(|step| reach.size + gap * U256::from(step) / U256::from(REACH_STEPS)),
                );
            }
            all.sort_unstable();
            all.dedup();
        }
        all
    }

    /// How far each side of each of `doors` reaches, the bid then the ask, at the next block
    /// `target` under `state`, or at `block` itself where neither is given (a chain whose words
    /// stand in storage): zero for every side but a credit listing's that may route (one
    /// named with its gas, on a pool none of whose other doors here its route could move, see
    /// [`shares_what_its_route_owes`]).
    ///
    /// The door's most-fillable read answers a credit listing with what its pool's custody fills
    /// unless the pool's own preview fills the whole of the maker's depth (`quoteFillable` in
    /// `PropAMMFront`), so it reports none of a route that fills less than that depth, and the
    /// route fills an input whole or not at all. Each such side is therefore asked its door's
    /// `quote` at [`reach_probes`], one more batch in a poll that serves such a listing, and its
    /// reach is the largest answered.
    async fn reaches(
        &self,
        doors: &[OpenDoor],
        target: Option<At>,
        clock: &FrontClock,
        block: BlockId,
        state: Option<&serde_json::Value>,
    ) -> Result<Vec<[Reach; 2]>, FeedError> {
        let mut reaches = vec![[Reach::default(); 2]; doors.len()];
        let asked: Vec<_> = doors
            .iter()
            .enumerate()
            .filter(|(_, door)| door.credit.is_some() && !shares_what_its_route_owes(door, doors))
            .flat_map(|(d, door)| {
                [true, false].map(|bid| {
                    let probes = reach_probes(if bid { &door.bid } else { &door.ask });
                    (d, bid, probes)
                })
            })
            .collect();
        if asked.is_empty() {
            return Ok(reaches);
        }
        let calls: Vec<_> = asked
            .iter()
            .flat_map(|(d, bid, probes)| quote_calls(&doors[*d], *bid, probes))
            .collect();
        let BatchAnswers { groups, .. } = self
            .batch(block, vec![Group { calls, at: target }], &[], Also::Nothing, Some(clock), state)
            .await?;
        let answers = groups
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(refused_next_block)?;
        let mut offset = 0;
        for (d, bid, probes) in &asked {
            let answered = answers
                .get(offset..offset + probes.len())
                .ok_or_else(|| FeedError::Parsing("multicall answered too few calls".into()))?;
            offset += probes.len();
            let reach = probes
                .iter()
                .zip(answered)
                .enumerate()
                .rev()
                .find_map(|(k, (&size, answer))| {
                    let output = quoted(answer).filter(|output| !output.is_zero())?;
                    let refused = probes
                        .get(k + 1)
                        .copied()
                        .unwrap_or_default();
                    Some(Reach { size, output, refused })
                });
            reaches[*d][usize::from(!*bid)] = reach.unwrap_or_default();
        }
        Ok(reaches)
    }

    /// Sends `groups` at `block` as one JSON-RPC batch of `aggregate3` calls, every call allowed
    /// to fail, with reads of the storage `slots` (account and slot) at the block and, when
    /// `also` asks it, of the front's implementation slot, checked before any answer is looked
    /// at: against the allow-list, and where code hashes are pinned, against them by a read of
    /// the implementation's code after the batch, the first time the implementation is found.
    ///
    /// Each `aggregate3` of a group under a block override starts with Multicall3's block number
    /// and timestamp and the front's millisecond source, read under it, from which `clock` computes
    /// the front's clock there. The group's answers are `None` where the node refused the override,
    /// or where any of its calls ran at another moment on the front's clock than the override's:
    /// a node that ignores the override, or applies only part of it. A storage word the node does
    /// not answer is its error, for the caller to judge. Any other failure fails the batch.
    ///
    /// Where `state` is given, each group under a block override runs under it as the state
    /// override: the next block's state map, on a chain whose builders land makers' updates.
    ///
    /// Where `also` asks the block's header, the batch carries an `eth_getBlockByNumber` of it,
    /// and its hash is answered only where the node answers that very block: a node that does not
    /// answer it, or answers another, fails nothing, and the batch's block is then not named. A
    /// batch with no call and no word reads no header.
    ///
    /// Where `block` names its block by hash, a call group with no override and every storage word
    /// is answered from what the node answered at that block before, where it did: an
    /// `aggregate3` by its calldata, a word by its account and slot ([`HeadAnswers`]). Only what
    /// it has not answered is sent, and a batch whose every request was answered so sends
    /// nothing. The implementation is checked all the same.
    async fn batch(
        &self,
        block: BlockId,
        groups: Vec<Group>,
        slots: &[(Address, U256)],
        also: Also,
        clock: Option<&FrontClock>,
        state: Option<&serde_json::Value>,
    ) -> Result<BatchAnswers, FeedError> {
        if groups
            .iter()
            .all(|group| group.calls.is_empty()) &&
            slots.is_empty() &&
            !also.implementation()
        {
            return Ok(BatchAnswers {
                groups: groups
                    .iter()
                    .map(|_| Some(Vec::new()))
                    .collect(),
                words: Vec::new(),
                head: None,
                head_ms: None,
                head_spacing_ms: None,
                implementation: None,
                gas_price: None,
            });
        }
        // A request naming its block by hash that the node refuses the form of, which the read's
        // caller asks about once the read has failed (see [`HashForm`]).
        let by_hash = block.is_hash();
        let note = |error: &TransportError| {
            if by_hash && refuses_the_form(error) {
                self.hash_form
                    .refused
                    .store(true, Ordering::Relaxed);
            }
        };
        // A request naming its block by hash with no override is answered once at that block:
        // the node runs it there or refuses it, and the block's state never changes.
        let kept_at = block.as_block_hash();
        let head_answers = || {
            self.head_answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let kept_chunk = |key: &B256| {
            kept_at.and_then(|hash| {
                head_answers()
                    .at(hash)
                    .calls
                    .get(key)
                    .cloned()
            })
        };
        let kept_word = |place: &(Address, U256)| {
            kept_at.and_then(|hash| {
                head_answers()
                    .at(hash)
                    .words
                    .get(place)
                    .copied()
            })
        };
        // The milliseconds between the head the batch is pinned to by its hash and its parent,
        // where an earlier batch at that hash asked for them.
        let kept_parent_ms = || {
            also.parent_of()
                .filter(|latest| kept_at == Some(latest.hash))
                .and_then(|latest| {
                    let mut kept = head_answers();
                    let kept = kept.at(latest.hash);
                    kept.parent_asked
                        .then_some(kept.parent_ms)
                })
        };
        let client = self.provider.client();
        let mut batch = BatchRequest::new(client);
        let mut asked = 0usize;
        let mut sent: Vec<(Option<At>, usize, Vec<SentChunk>)> = Vec::new();
        for group in &groups {
            let mut waiters = Vec::new();
            // Under an override, each chunk starts with its own reads of the front's clock.
            let checks = if group.at.is_some() { self.override_checks(clock) } else { Vec::new() };
            for chunk in group
                .calls
                .chunks(CALLS_PER_MULTICALL - checks.len())
            {
                let mut chunk = chunk.to_vec();
                chunk.splice(0..0, checks.iter().cloned());
                let input: AlloyBytes = aggregate3Call { calls: chunk.clone() }
                    .abi_encode()
                    .into();
                let key = kept_at
                    .filter(|_| group.at.is_none())
                    .map(|_| keccak256(&input));
                if let Some(answers) = key.as_ref().and_then(kept_chunk) {
                    waiters.push(SentChunk::Kept(answers));
                    continue;
                }
                let request = TransactionRequest::default()
                    .to(MULTICALL3_ADDRESS)
                    .input(TransactionInput::new(input));
                let waiter = match group.at {
                    None => batch.add_call::<_, AlloyBytes>("eth_call", &(request, block)),
                    Some(at) => batch.add_call::<_, AlloyBytes>(
                        "eth_call",
                        &(
                            request,
                            block,
                            state.map_or_else(|| state_override(&at), Clone::clone),
                            BlockOverride {
                                number: U64::from(at.number),
                                time: U64::from(at.time),
                            },
                        ),
                    ),
                }
                .map_err(|e| self.connection_error("multicall", e))?;
                asked += 1;
                waiters.push(SentChunk::Sent { waiter, count: chunk.len(), key });
            }
            sent.push((group.at, checks.len(), waiters));
        }
        let mut word = |account: Address, slot: U256, what: &'static str| {
            if let Some(word) = kept_word(&(account, slot)) {
                return Ok(SentWord::Kept(word));
            }
            asked += 1;
            batch
                .add_call::<_, U256>("eth_getStorageAt", &(account, slot, block))
                .map(SentWord::Sent)
                .map_err(|e| self.connection_error(what, e))
        };
        let mut words = Vec::with_capacity(slots.len());
        for (account, slot) in slots {
            words.push(((*account, *slot), word(*account, *slot, "listing record")?));
        }
        let implementation_slot = U256::from_be_bytes(IMPLEMENTATION_SLOT.0);
        let implementation = if also.implementation() {
            Some(word(self.front, implementation_slot, "implementation slot")?)
        } else {
            None
        };
        let gas_price = if also.gas_price() {
            asked += 1;
            Some(
                batch
                    .add_call::<_, U256>("eth_gasPrice", &[(); 0])
                    .map_err(|e| self.connection_error("gas price", e))?,
            )
        } else {
            None
        };
        let mut header_of = |number: u64| {
            asked += 1;
            batch
                .add_call::<_, Option<Header>>("eth_getBlockByNumber", &(U64::from(number), false))
                .map_err(|e| self.connection_error("block header", e))
        };
        let head = match block.as_u64().filter(|_| also.head()) {
            Some(number) => Some((number, header_of(number)?)),
            None => None,
        };
        // The parent's header, with the hash the head names for it: from the head's own header
        // where the batch asks it by number, else for the head the batch is pinned to by its hash.
        let parent_of = match (head.as_ref(), also.parent_of()) {
            (Some((number, _)), _) if also.parent() => number.checked_sub(1),
            (None, Some(latest)) if kept_parent_ms().is_none() => latest.number.checked_sub(1),
            _ => None,
        };
        let parent = match parent_of {
            Some(number) => Some((number, header_of(number)?)),
            None => None,
        };
        if asked > 0 {
            batch
                .send()
                .await
                .map_err(|e| self.connection_error("batch", e))?;
        }
        // What the node answered at a block named by hash, kept for the requests after it.
        let keep = |keep: &dyn Fn(&mut HeadAnswers)| {
            if let Some(hash) = kept_at {
                keep(head_answers().at(hash));
            }
        };

        let implementation = match implementation {
            Some(slot) => {
                let word = slot
                    .answer()
                    .await
                    .inspect_err(note)
                    .map_err(|e| self.connection_error("implementation slot", e))?;
                let found = Address::from_word(B256::from(word));
                self.check_implementation(found)?;
                if self.code_unchecked(found) {
                    let code = self
                        .provider
                        .get_code_at(found)
                        .block_id(block)
                        .await
                        .inspect_err(note)
                        .map_err(|e| self.connection_error("implementation code", e))?;
                    self.check_implementation_code(found, &code)?;
                }
                keep(&|kept| {
                    kept.words
                        .insert((self.front, implementation_slot), word);
                });
                Some(found)
            }
            None => None,
        };
        let header = match head {
            // The header names the batch's block only where it is that block's: no error of this
            // one read fails the batch.
            Some((number, header)) => header
                .await
                .ok()
                .flatten()
                .filter(|header| header.number == U64::from(number)),
            None => None,
        };
        let head = header
            .as_ref()
            .map(|header| header.hash);
        let head_ms = header.as_ref().and_then(Header::ms);
        // The parent's header, where the node answered that very block and it is the head's own.
        let parent = match parent {
            Some((number, parent)) => parent
                .await
                .ok()
                .flatten()
                .filter(|parent| {
                    parent.number == U64::from(number) &&
                        header
                            .as_ref()
                            .and_then(|header| header.parent_hash)
                            .or_else(|| {
                                also.parent_of()
                                    .and_then(|latest| latest.parent_hash)
                            }) ==
                            Some(parent.hash)
                }),
            None => None,
        };
        let parent_ms = match kept_parent_ms() {
            Some(kept) => kept,
            None => {
                let ms = parent.as_ref().and_then(Header::ms);
                if also.parent_of().is_some() {
                    keep(&|kept| {
                        kept.parent_asked = true;
                        kept.parent_ms = ms;
                    });
                }
                ms
            }
        };
        let head_spacing_ms = head_ms
            .or_else(|| {
                also.parent_of()
                    .and_then(|latest| latest.ms)
            })
            .zip(parent_ms)
            .and_then(|(head_ms, parent_ms)| head_ms.checked_sub(parent_ms));
        let mut answers = Vec::with_capacity(sent.len());
        for (at, checks, waiters) in sent {
            let mut group = Vec::new();
            let mut honoured = true;
            for chunk in waiters {
                let (waiter, count, key) = match chunk {
                    SentChunk::Kept(answers) => {
                        group.extend(answers);
                        continue;
                    }
                    SentChunk::Sent { waiter, count, key } => (waiter, count, key),
                };
                let raw = match waiter.await {
                    Ok(raw) => raw,
                    Err(e) if at.is_some() => {
                        note(&e);
                        debug!(error = %self.connection_error("overridden multicall", e), "block override refused");
                        honoured = false;
                        continue;
                    }
                    Err(e) => {
                        note(&e);
                        return Err(self.connection_error("multicall", e));
                    }
                };
                let chunk = aggregate3Call::abi_decode_returns(&raw)
                    .map_err(|e| FeedError::Parsing(format!("Efficient PropAMM multicall: {e}")))?;
                if chunk.len() != count {
                    return Err(FeedError::Parsing(format!(
                        "Efficient PropAMM multicall answered {} of {count} calls",
                        chunk.len()
                    )));
                }
                if let Some(key) = key {
                    keep(&|kept| {
                        kept.calls.insert(key, chunk.clone());
                    });
                }
                let mut chunk = chunk.into_iter();
                if let Some(at) = at {
                    let checks: Vec<_> = chunk.by_ref().take(checks).collect();
                    if !runs_at(&checks, &at, clock) {
                        honoured = false;
                    }
                }
                group.extend(chunk);
            }
            answers.push(honoured.then_some(group));
        }
        let mut read = Vec::with_capacity(words.len());
        for (place, word) in words {
            let word = word
                .answer()
                .await
                .inspect_err(note)
                .map_err(|e| self.connection_error("listing record", e));
            if let Ok(word) = word {
                keep(&|kept| {
                    kept.words.insert(place, word);
                });
            }
            read.push(word);
        }
        let gas_price = match gas_price {
            Some(price) => Some(
                price
                    .await
                    .map_err(|e| self.connection_error("gas price", e))?,
            ),
            None => None,
        };
        Ok(BatchAnswers {
            groups: answers,
            words: read,
            head,
            head_ms,
            head_spacing_ms,
            implementation,
            gas_price,
        })
    }

    /// The calls that open each `aggregate3` run under a block override, from which the front's
    /// clock under it is computed: the block number and the timestamp, and on Base the front's
    /// millisecond source (BaseTime where its clock reads none). A front on a chain whose builders
    /// land makers' updates reads the block's second, and BaseTime is not called there.
    fn override_checks(&self, clock: Option<&FrontClock>) -> Vec<Call3> {
        let mut checks = vec![
            call(MULTICALL3_ADDRESS, getBlockNumberCall {}.abi_encode()),
            call(MULTICALL3_ADDRESS, getCurrentBlockTimestampCall {}.abi_encode()),
        ];
        if self.next_block.is_none() {
            let source = clock
                .and_then(FrontClock::source)
                .unwrap_or(BASE_TIME);
            checks.push(call(source, timestampMsCall {}.abi_encode()));
        }
        checks
    }

    /// Whether the sources of a listing's record word 10, `w10`, are all approved: its primary, and
    /// its backup where it has one. A listing with no primary has no source to read.
    fn approves(&self, w10: U256) -> bool {
        let (primary, backup) = (field(w10, 0, 32), field(w10, 32, 32));
        let approved = |id: u64| u32::try_from(id).is_ok_and(|id| self.price_sources.contains(&id));
        approved(primary) && (backup == 0 || approved(backup))
    }

    /// Fails fatally when the front runs an implementation outside the allow-list.
    fn check_implementation(&self, found: Address) -> Result<(), FeedError> {
        if !self.implementations.contains(&found) {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the front proxy {} runs implementation {found}, which is not \
                 on the allow-list; the front was upgraded and the integration needs checking \
                 against the new implementation",
                self.front
            )));
        }
        Ok(())
    }

    /// The head's milliseconds on a chain of `block_ms` blocks, where the head's header names
    /// them and its parent's are `block_ms` before them; else `None`, and a warning the first
    /// read that finds it so, until a read finds the chain on its cadence again. A chain whose
    /// blocks come faster than `block_ms` (a fork that shortens them) seals the next block before
    /// a side timed by `block_ms` ends, and a route priced from it then lands in a later block
    /// than it quoted; a head later than its parent's time and `block_ms` (a validator that
    /// missed its turn) serves nothing for that head alone.
    fn on_cadence(
        &self,
        number: u64,
        head_ms: Option<u64>,
        spacing_ms: Option<u64>,
        block_ms: u64,
    ) -> Option<u64> {
        let on = head_ms.filter(|_| spacing_ms == Some(block_ms));
        let changed = self
            .off_cadence
            .swap(on.is_none(), Ordering::Relaxed) !=
            on.is_none();
        match on {
            Some(_) if changed => info!(
                front = %self.front,
                head = number,
                "serving this front's listings again: the chain's blocks are {block_ms} ms apart"
            ),
            Some(_) => {}
            None if changed => {
                warn!(
                    front = %self.front,
                    head = number,
                    ?head_ms,
                    ?spacing_ms,
                    "serving no listing until the chain's blocks are {block_ms} ms apart: the \
                     head's header names no milliseconds, its parent is not read, or the time \
                     between them is another, and a side's life is timed by {block_ms} ms"
                );
                #[cfg(test)]
                self.off_cadence_warnings
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => debug!(
                front = %self.front,
                head = number,
                ?spacing_ms,
                "serving no listing: the chain is off its {block_ms} ms blocks"
            ),
        }
        on
    }

    /// Whether the implementation `found` is to have its code read: the caller pinned code hashes,
    /// and no read has found its code among them yet.
    fn code_unchecked(&self, found: Address) -> bool {
        !self
            .implementation_codehashes
            .is_empty() &&
            !self
                .code_checked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&found)
    }

    /// Fails fatally where `code`, the implementation `found`'s, hashes to none of the pinned
    /// hashes: the proxy runs code this feed was not built for, as where a preset of one chain
    /// was given a node of another, at whose same address another code runs. Code that matches
    /// is not read again.
    fn check_implementation_code(&self, found: Address, code: &[u8]) -> Result<(), FeedError> {
        let hash = keccak256(code);
        if !self
            .implementation_codehashes
            .contains(&hash)
        {
            return Err(FeedError::Fatal(format!(
                "Efficient PropAMM: the front proxy {} runs implementation {found}, whose code \
                 hashes to {hash}, none of the hashes pinned; the same address holds other code \
                 on another chain, so the feed may be configured for another chain than its node's",
                self.front
            )));
        }
        self.code_checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(found);
        Ok(())
    }

    /// A transport failure, with the RPC URL removed: it commonly carries a key, and the HTTP
    /// client's errors quote the URL they failed on.
    fn connection_error(&self, what: &str, error: impl std::fmt::Display) -> FeedError {
        let error = error
            .to_string()
            .replace(&self.rpc_url, "<rpc url>");
        FeedError::Connection(format!("Efficient PropAMM {what}: {error}"))
    }
}

/// The fence kinds, bits 228-229 of a listing's settings word, that read a price print
/// (`PropAMMWords.FENCE_PRINT` and `FENCE_SESSION`).
const FENCE_PRINT: u64 = 0;
const FENCE_SESSION: u64 = 3;

/// The fence kind of a listing that reads no fence, whose word never carries
/// (`PropAMMWords.FENCE_NONE`).
const FENCE_NONE: u64 = 2;

/// Whether a listing's word may fill in the block after its landing, from its settings word: its
/// carry life (bits 230-231) is set and its fence kind (bits 228-229) is not NONE. The word's own
/// carry bits decide the rest, and the door reads them.
pub(super) fn listing_carries(settings: U256) -> bool {
    field(settings, 230, 2) != 0 && field(settings, 228, 2) != FENCE_NONE
}

/// How old the node's answer is at a poll that started at `from_ms` on the wall clock, its latest
/// block's time `timestamp`: seconds, negative where the block's time is ahead of the clock (the
/// metric records that as zero, see `telemetry::ANSWER_AGE`).
fn answer_age(from_ms: u64, timestamp: u64) -> f64 {
    Duration::from_millis(from_ms).as_secs_f64() - Duration::from_secs(timestamp).as_secs_f64()
}

/// The failure of a read whose node refused or ignored the next block's overrides: on a chain whose
/// builders land makers' updates, nothing can be quoted without them.
fn refused_next_block() -> FeedError {
    FeedError::Connection(
        "Efficient PropAMM node refused or ignored the next block's overrides, without which no \
         listing on this chain can be quoted"
            .into(),
    )
}

/// The init code hash of the proxy a CREATE3 child is deployed through (Solady's `CREATE3`).
const CREATE3_PROXY_CODEHASH: B256 =
    b256!("21c35dbe1b344a2488cf3321d6ce542f8e9f305544ff09e4993a62319a497c1f");

/// The price contract of source `id` on a front whose builders land makers' updates: the CREATE3
/// child of `front` at salt `bytes32(id)`, the address the front's fill calls for that source.
pub(super) fn price_contract(front: Address, id: u32) -> Address {
    front
        .create2(B256::from(U256::from(id)), CREATE3_PROXY_CODEHASH)
        .create(1)
}

/// The slot of word `word` of listing `id`'s record in the front's storage.
pub(super) fn record_word_slot(id: u32, word: u8) -> U256 {
    let pools = U256::from_be_bytes(STORE_NAMESPACE.0) + U256::from(POOLS_INDEX);
    U256::from_be_bytes(keccak256((U256::from(id), pools).abi_encode()).0) + U256::from(word)
}

/// `bits` bits of `value` from bit `shift`.
fn field(value: U256, shift: usize, bits: usize) -> u64 {
    ((value >> shift) & ((U256::from(1u8) << bits) - U256::from(1u8))).saturating_to()
}

/// The state override of an `eth_call` under the block override `at`: BaseTime's slot 0, which
/// stores the sub-second it adds to the block's second, set to `at`'s, where `at` reads BaseTime;
/// none otherwise.
pub(super) fn state_override(at: &At) -> serde_json::Value {
    let Some(sub) = at.base_time else {
        return serde_json::Value::Null;
    };
    let diff = serde_json::Map::from_iter([(
        B256::ZERO.to_string(),
        serde_json::Value::String(B256::from(U256::from(sub)).to_string()),
    )]);
    serde_json::Value::Object(serde_json::Map::from_iter([(
        BASE_TIME.to_string(),
        serde_json::json!({ "stateDiff": diff }),
    )]))
}

/// Whether an `aggregate3` run under the override `at` ran there: its opening reads of the block
/// number, the timestamp and the front's millisecond source show the override's number and time,
/// and the front's clock reading as `at` does.
fn runs_at(checks: &[Call3Result], at: &At, clock: Option<&FrontClock>) -> bool {
    let [number, time, source @ ..] = checks else {
        return false;
    };
    let number = decode::<getBlockNumberCall>(number, "block number").ok();
    let time = decode::<getCurrentBlockTimestampCall>(time, "timestamp").ok();
    let (Some(number), Some(time), Some(clock)) = (number, time, clock) else {
        return false;
    };
    let (number, time) = (number.saturating_to::<u64>(), time.saturating_to::<u64>());
    number == at.number &&
        time == at.time &&
        clock.reads(number, time, source.first().and_then(ms_reading)) == (at.ms, at.exact)
}

/// A millisecond source's answer, as the front takes it: a word, where the call succeeded and
/// returned at least one.
fn ms_reading(answer: &Call3Result) -> Option<u64> {
    (answer.success && answer.returnData.len() >= 32)
        .then(|| U256::from_be_slice(&answer.returnData[..32]).saturating_to())
}

/// A door's quote, or `None` where it refused.
fn quoted(answer: &Call3Result) -> Option<U256> {
    decode::<quoteCall>(answer, "door's quote").ok()
}

/// One side's samples at one block: the door's `quotes` at each of `sizes` and, on a credit
/// listing, the front's `plans` at each (see [`DoorsReader::plan_quotes`]). Each answered size is
/// paired with the path its fill takes: inside custody on a listing without plans, and on a
/// credit listing the route where the plan's answer is tagged `ROUTED`, custody where it is
/// untagged and equal to the door's quote. A size whose plan answers anything else (another
/// output, a refusal) is no sample: the door and the front's plan do not agree on how it fills.
fn classify(
    sizes: &[U256],
    quotes: &[Call3Result],
    plans: Option<&[Call3Result]>,
) -> Vec<(U256, Option<(U256, FillPath)>)> {
    sizes
        .iter()
        .zip(quotes)
        .enumerate()
        .map(|(k, (&size, quote))| {
            let output = quoted(quote);
            let sample = match plans {
                None => output.map(|output| (output, FillPath::Custody)),
                Some(plans) => output
                    .zip(plans.get(k))
                    .and_then(|(output, plan)| {
                        let plan = decode::<quoteFromCall>(plan, "front's quoteFrom").ok()?;
                        if plan.bit(ROUTED) {
                            Some((output, FillPath::Route))
                        } else {
                            (plan == output).then_some((output, FillPath::Custody))
                        }
                    }),
            };
            (size, sample)
        })
        .collect()
}

/// A side's classified samples as its ladder takes them, and the least size that routes: a
/// custody sample above a routed one is no sample, which ends the ladder's run there, as a
/// refusal does. A side of any other listing, every sample inside custody, is unchanged.
fn stepped(samples: Vec<(U256, Option<(U256, FillPath)>)>) -> (Samples, Option<U256>) {
    let mut route_from = None;
    let samples = samples
        .into_iter()
        .map(|(size, sample)| {
            let output = match sample {
                Some((output, FillPath::Route)) => {
                    route_from.get_or_insert(size);
                    Some(output)
                }
                Some((output, FillPath::Custody)) if route_from.is_none() => Some(output),
                Some((_, FillPath::Custody)) | None => None,
            };
            (size, output)
        })
        .collect();
    (samples, route_from)
}

/// The tokens a credit listing's route may leave its pool owing: the listing's own but the pool's
/// asset (its token 0), which is the collateral a route supplies and withdraws.
fn may_owe(door: &OpenDoor) -> Vec<Address> {
    match door.pair {
        PairPlace::HookPair | PairPlace::PastWithAsset => vec![door.quote],
        PairPlace::PastWithAssetAsQuote => vec![door.base],
        PairPlace::PastWithoutAsset => vec![door.base, door.quote],
    }
}

/// Whether another of `doors` on the credit listing `door`'s pool could be moved by its route: one
/// with a credit mode of its own, or one trading a token the route may leave owed
/// ([`may_owe`]).
fn shares_what_its_route_owes(door: &OpenDoor, doors: &[OpenDoor]) -> bool {
    let owed = may_owe(door);
    doors.iter().any(|other| {
        other.listing != door.listing &&
            other.pool == door.pool &&
            (other.credit.is_some() || owed.contains(&other.base) || owed.contains(&other.quote))
    })
}

/// A side's samples and its least routed size, or where `custody_only`, its custody run alone:
/// every sample at or above the least routed size is dropped.
fn custody_run(
    samples: Samples,
    route_from: Option<U256>,
    custody_only: bool,
) -> (Samples, Option<U256>) {
    match route_from {
        Some(from) if custody_only => (
            samples
                .into_iter()
                .map(|(size, output)| (size, output.filter(|_| size < from)))
                .collect(),
            None,
        ),
        _ => (samples, route_from),
    }
}

/// The failure of a read whose block `number`, of time `timestamp`, trails the wall clock's second
/// `now` by more than a poll allows.
pub(super) fn lagging(now: u64, number: u64, timestamp: u64) -> FeedError {
    FeedError::Connection(format!(
        "Efficient PropAMM node is {} s behind: its latest block {number} is from {timestamp}",
        now.saturating_sub(timestamp)
    ))
}

/// Calls per listing in the second request: see [`DoorsReader::open_doors`].
const DOOR_CALLS: usize = 5;

/// Words of each listing's record read in the second request: its settings, controls, session,
/// source and live-source words, and its two board words.
const RECORD_WORDS: usize = 7;

/// The first millisecond at which a listing's side fills nothing, as the front reads its standing
/// word and the side's depth word (`doorView`'s `word`, and `depth[0]` for the bid, where the
/// taker sells the base, or `depth[2]` for the ask): the word stands for its life from its maker
/// time, capped at the front's `maxTtlMs`, until the front's age clock passes its end, and the
/// depth until the block's timestamp reaches its expiry, in seconds. On Base the front's clock is
/// the block's own milliseconds; where it falls back to the block's second, the age clock adds
/// `ageSlackMs`, which this takes off the word's end in either case. The result bounds the side's
/// life from above only: the side stops sooner when the operator replaces the word, or when the
/// door refuses the price print the listing reads, which the asks at each second of the book's
/// life show (see [`DoorsReader::sample`]).
pub(super) fn runs_out_ms(word: U256, depth: U256, clock: &Timing) -> u64 {
    let maker_ms = field(word, 72, 48);
    let life_ms = (field(word, 120, 16) * 1000).min(clock.maxTtlMs.saturating_to());
    let word_out = (maker_ms + life_ms + 1).saturating_sub(clock.ageSlackMs.saturating_to());
    let depth_out = field(depth, 60, 32) * 1000;
    word_out.min(depth_out)
}

/// A listing's taker tip cap as the front stores it (`doorView`'s `tipCap`: a 12-bit mantissa
/// and a 4-bit exponent of 10), in wei per gas above the base fee; `None` for 0, no cap.
pub(super) fn tip_cap_wei(stored: u16) -> Option<U256> {
    let mantissa = U256::from(stored & 0xfff);
    let exponent = U256::from(stored >> 12);
    (stored != 0).then(|| mantissa * U256::from(10u8).pow(exponent))
}

/// The input the most-fillable read asks for, of a token with `decimals`.
fn probe(decimals: u32) -> U256 {
    U256::from(10u64).pow(U256::from(decimals + PROBE_WHOLE_TOKENS_EXP))
}

/// The first asks of a credit listing's side that may route, in increasing size, none of them a
/// size its most-fillable read already covers.
///
/// A side reporting nothing fillable is asked across the scale of its token: [`REACH_PROBES`]
/// sizes a factor of four apart, the largest about 950,000 whole tokens (its probe over 2^20) and
/// the smallest about 0.0002. A side reporting some is asked above it: the most fillable plus a
/// 64th of it, then that step doubled, [`REACH_PROBES`] times in all, up to 1,025 times it.
fn reach_probes(side: &OpenSide) -> Vec<U256> {
    let mut probes: Vec<_> = if side.fillable.is_zero() {
        (0..REACH_PROBES)
            .map(|step| side.probe >> (20 + 2 * step))
            .collect()
    } else {
        (0..REACH_PROBES)
            .rev()
            .map(|step| {
                side.fillable.saturating_add(
                    (side.fillable >> 6usize).saturating_mul(U256::from(1u64 << step)),
                )
            })
            .collect()
    };
    probes.reverse();
    probes.retain(|size| *size > side.fillable);
    probes.dedup();
    probes
}

/// The first asks of one side of a credit listing that may route: see [`reach_probes`].
const REACH_PROBES: usize = 17;

/// The even steps a side's reach is refined in, up to the first size its door refused: see
/// [`DoorsReader::side_sizes`].
const REACH_STEPS: u64 = 8;

/// The door's `quote` call at each of `sizes` on one side of `door`, the bid where `bid`.
fn quote_calls(door: &OpenDoor, bid: bool, sizes: &[U256]) -> Vec<Call3> {
    let (token_in, token_out) = if bid { (door.base, door.quote) } else { (door.quote, door.base) };
    sizes
        .iter()
        .map(|&amount| {
            let quote = quoteCall { tokenIn: token_in, tokenOut: token_out, amountIn: amount };
            call(door.door, quote.abi_encode())
        })
        .collect()
}

/// The most sizes one side is sampled at below its halvings: see [`sizes`].
const MAX_LOW_SIZES: usize = 32;

/// The sizes one side is sampled at, in increasing size: `fillable` and its halvings, `rungs` in
/// all, then quarterings of the smallest of those down to `floor`, the least the listing fills,
/// and `floor` itself, at most [`MAX_LOW_SIZES`] more, so a small swap is quoted however deep the
/// listing. None under `floor` or rounding to zero, and none where the side fills nothing.
pub(super) fn sizes(fillable: U256, rungs: u32, floor: U256) -> Vec<U256> {
    let mut sizes: Vec<_> = (0..rungs)
        .map(|halvings| fillable >> halvings)
        .filter(|size| !size.is_zero() && *size >= floor)
        .collect();
    if !floor.is_zero() && floor <= fillable {
        let mut size = sizes
            .last()
            .copied()
            .unwrap_or(fillable) >>
            2;
        while size > floor && sizes.len() < rungs as usize + MAX_LOW_SIZES - 1 {
            sizes.push(size);
            size >>= 2;
        }
        if sizes.last() != Some(&floor) {
            sizes.push(floor);
        }
    }
    sizes.reverse();
    sizes
}

/// A 24-bit amount as the front stores one (`PropAMMLaneLib.amount24`): an 18-bit mantissa times
/// 10 to a 6-bit exponent.
fn amount24(stored: u64) -> U256 {
    U256::from(stored & 0x3ffff)
        .saturating_mul(U256::from(10u8).saturating_pow(U256::from(stored >> 18)))
}

pub(super) fn call(target: Address, data: Vec<u8>) -> Call3 {
    Call3 { target, allowFailure: true, callData: AlloyBytes::from(data) }
}

/// One answer of the front or the factory, decoded; fails fatally when it reverted, which their
/// views only do once the deployment no longer answers the way this integration was built for.
fn setting<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, FeedError> {
    if !answer.success {
        return Err(FeedError::Fatal(format!(
            "Efficient PropAMM: the {what} view reverted; the deployment changed and the \
             integration needs updating"
        )));
    }
    decode::<C>(answer, what)
}

fn decode<C: SolCall>(answer: &Call3Result, what: &str) -> Result<C::Return, FeedError> {
    if !answer.success {
        return Err(FeedError::Parsing(format!("Efficient PropAMM {what} reverted")));
    }
    C::abi_decode_returns(&answer.returnData)
        .map_err(|e| FeedError::Parsing(format!("Efficient PropAMM {what}: {e}")))
}

fn to_u64(value: U256, what: &str) -> Result<u64, FeedError> {
    value
        .try_into()
        .map_err(|e| FeedError::Parsing(format!("Efficient PropAMM {what} {value}: {e}")))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// A node refuses a request's form by the two JSON-RPC codes that say so, and by no other:
    /// a block it does not know, under either code nodes answer that with, says nothing of the
    /// form.
    #[rstest]
    #[case::invalid_params(-32602, true)]
    #[case::method_not_found(-32601, true)]
    #[case::a_header_not_found(-32000, false)]
    #[case::a_resource_not_found(-32001, false)]
    #[case::an_internal_error(-32603, false)]
    fn only_an_error_of_its_form_refuses_a_request(#[case] code: i64, #[case] refuses: bool) {
        let payload = serde_json::from_str(&format!(r#"{{"code":{code},"message":"no"}}"#));

        assert_eq!(refuses_the_form(&TransportError::ErrorResp(payload.unwrap())), refuses);
    }

    /// A BNB Chain header's milliseconds: its `milliTimestamp` where that falls in the header's
    /// second, else the low eight bytes of its `mixHash` where they are under a second. A header
    /// naming neither names none, nor does one whose `mixHash` holds no milliseconds there, as
    /// another chain's randomness does: its life would be timed from a moment that is not its own.
    #[rstest]
    #[case::its_milli_timestamp(Some(1_000_450), None, Some(1_000_450))]
    #[case::a_milli_timestamp_of_another_second(Some(1_001_450), None, None)]
    #[case::then_its_mix_hash(Some(1_001_450), Some(450), Some(1_000_450))]
    #[case::its_mix_hash(None, Some(999), Some(1_000_999))]
    #[case::a_mix_hash_of_a_second(None, Some(1_000), None)]
    #[case::a_mix_hash_of_randomness(None, Some(0xabab_abab_abab_abab), None)]
    #[case::neither(None, None, None)]
    fn a_header_names_its_milliseconds_within_its_second(
        #[case] milli_timestamp: Option<u64>,
        #[case] mix_low: Option<u64>,
        #[case] ms: Option<u64>,
    ) {
        let header = Header {
            hash: B256::ZERO,
            parent_hash: None,
            number: U64::from(1u8),
            timestamp: Some(U64::from(1_000u16)),
            milli_timestamp: milli_timestamp.map(U64::from),
            mix_hash: mix_low.map(|low| {
                let mut hash = B256::repeat_byte(0xab);
                hash.0[24..].copy_from_slice(&low.to_be_bytes());
                hash
            }),
        };

        assert_eq!(header.ms(), ms);
    }

    /// Nor does a failure that is no answer of the node's at all, as a timeout.
    #[test]
    fn a_transport_failure_refuses_no_form() {
        assert!(!refuses_the_form(&TransportErrorKind::custom_str("timed out")));
    }
    use crate::pamm::protocols::efficient_prop::slots::DeskHead;

    /// What a read of one listing reading a print was asked on.
    fn sampled_on() -> SampledOn {
        let desk = Address::repeat_byte(0xde);
        let listing = Listing {
            id: 7,
            view: B256::repeat_byte(0x71),
            token0: Address::repeat_byte(0xa0),
            token1: Address::repeat_byte(0xa1),
            door: Address::repeat_byte(0xd0),
            word_ms: 1,
            bid: SideView::default(),
            ask: SideView::default(),
            tip_cap_wei: None,
            desk: 3,
            word_index: 0,
            asset_module: false,
            pair: PairPlace::HookPair,
            credit: None,
            named_gas: None,
            pool: Address::repeat_byte(0xb0),
        };
        SampledOn {
            head: B256::repeat_byte(0xb1),
            head_number: 100,
            target_number: 101,
            target_time: 1_790_000_012,
            implementation: Address::repeat_byte(0x1a),
            min_tip_cap_wei: U256::ZERO,
            rungs: 8,
            state: serde_json::json!({
                desk.to_string(): { "stateDiff": { "0x100": "0x01", "0x200": "0x02" } },
                Address::repeat_byte(0x9c).to_string(): { "stateDiff": { "0x0": "0x03" } },
            }),
            listings: vec![TakenListing {
                listing,
                probes: (U256::from(1u8), U256::from(2u8)),
                settings: U256::from(3u8),
                price: Some(U256::from(4u8)),
                desk: U256::from(5u8),
                slots: ListingSlots {
                    account: desk,
                    quote_slot: B256::from(U256::from(0x100u64)),
                    firm_slot: B256::from(U256::from(0x200u64)),
                    head: DeskHead { epoch: 0, stopped: false },
                    head_quote: B256::ZERO,
                    head_firm: B256::ZERO,
                },
            }],
        }
    }

    fn desk_diff(on: &mut SampledOn) -> &mut serde_json::Value {
        &mut on.state[Address::repeat_byte(0xde).to_string()]["stateDiff"]
    }

    /// A read is answered from the last one sampled only where it is asked on the same: one
    /// differing in any one thing the doors' answers are read from is asked on another, and its
    /// doors are asked again.
    #[rstest]
    #[case::the_heads_hash(|on: &mut SampledOn| on.head = B256::repeat_byte(0xb2))]
    #[case::the_heads_number(|on: &mut SampledOn| on.head_number += 1)]
    #[case::the_next_blocks_number(|on: &mut SampledOn| on.target_number += 1)]
    #[case::the_next_blocks_second(|on: &mut SampledOn| on.target_time += 12)]
    #[case::the_fronts_implementation(
        |on: &mut SampledOn| on.implementation = Address::repeat_byte(0x1b)
    )]
    #[case::the_tip_cap_floor(|on: &mut SampledOn| on.min_tip_cap_wei = U256::from(1u8))]
    #[case::the_sizes_sampled(|on: &mut SampledOn| on.rungs += 1)]
    #[case::a_quote_word_of_the_state_answer(
        |on: &mut SampledOn| desk_diff(on)["0x100"] = "0x11".into()
    )]
    #[case::a_firm_word_of_the_state_answer(
        |on: &mut SampledOn| desk_diff(on)["0x200"] = "0x12".into()
    )]
    #[case::a_price_contracts_slot_of_the_state_answer(|on: &mut SampledOn| {
        on.state[Address::repeat_byte(0x9c).to_string()]["stateDiff"]["0x0"] = "0x13".into();
    })]
    #[case::an_account_more_in_the_state_answer(|on: &mut SampledOn| {
        on.state[Address::repeat_byte(0x9d).to_string()] = serde_json::json!({ "stateDiff": {} });
    })]
    #[case::a_listing_less(|on: &mut SampledOn| on.listings.clear())]
    #[case::a_listings_door_view(
        |on: &mut SampledOn| on.listings[0].listing.view = B256::repeat_byte(0x72)
    )]
    #[case::a_listings_tip_cap(
        |on: &mut SampledOn| on.listings[0].listing.tip_cap_wei = Some(U256::from(1u8))
    )]
    #[case::a_listings_pool(
        |on: &mut SampledOn| on.listings[0].listing.pool = Address::repeat_byte(0xb9)
    )]
    #[case::a_listings_gas(|on: &mut SampledOn| {
        on.listings[0].listing.pair = PairPlace::PastWithAsset;
    })]
    #[case::a_listings_probes(|on: &mut SampledOn| on.listings[0].probes.0 = U256::from(9u8))]
    #[case::a_listings_settings_word(|on: &mut SampledOn| on.listings[0].settings = U256::ZERO)]
    #[case::a_listings_live_source_word(|on: &mut SampledOn| on.listings[0].price = None)]
    #[case::a_listings_desk_word(|on: &mut SampledOn| on.listings[0].desk = U256::from(6u8))]
    #[case::a_listings_desk_epoch(|on: &mut SampledOn| on.listings[0].slots.head.epoch = 1)]
    #[case::a_listings_quote_word_at_the_head(
        |on: &mut SampledOn| on.listings[0].slots.head_quote = B256::repeat_byte(1)
    )]
    #[case::a_listings_firm_word_at_the_head(
        |on: &mut SampledOn| on.listings[0].slots.head_firm = B256::repeat_byte(1)
    )]
    fn a_read_differing_in_one_thing_its_doors_read_is_asked_on_another(
        #[case] change: fn(&mut SampledOn),
    ) {
        let sampled = sampled_on();
        let mut asked = sampled.clone();

        change(&mut asked);

        assert_ne!(asked, sampled);
        assert_eq!(sampled, sampled_on());
    }

    /// A source's price contract is the CREATE3 child of the front at its id, as the front derives
    /// it: the price contracts of sources 1 and 2 of the front on Ethereum, which hold code there.
    #[rstest]
    #[case::the_first(1, address!("F166e802fb1994929b522f1ca5410a5f5BD1A366"))]
    #[case::the_second(2, address!("Ca143657998DbeFb3AC6bb78342B8Cf3Db04dee8"))]
    fn a_sources_price_contract_is_the_fronts_create3_child(#[case] id: u32, #[case] at: Address) {
        let front = address!("0000000000403aeB66691063CC2fD42E71d6AcD6");

        assert_eq!(price_contract(front, id), at);
    }

    #[rstest]
    #[case::the_fill_and_its_halvings(
        4096,
        12,
        0,
        &[2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096]
    )]
    #[case::halvings_that_round_to_zero_are_dropped(5, 4, 0, &[1, 2, 5])]
    #[case::nothing_fillable(0, 4, 0, &[])]
    #[case::none_under_the_floor(4096, 12, 300, &[300, 512, 1024, 2048, 4096])]
    #[case::quarterings_down_to_the_floor(
        1 << 20,
        4,
        1_000,
        &[1_000, 2_048, 8_192, 32_768, 131_072, 262_144, 524_288, 1_048_576]
    )]
    #[case::the_floor_a_quartering(1 << 20, 4, 2_048, &[2_048, 8_192, 32_768, 131_072, 262_144, 524_288, 1_048_576])]
    #[case::a_floor_past_the_fill(4096, 4, 5_000, &[])]
    fn a_side_is_sampled_at_its_fill_and_down_to_its_floor(
        #[case] fillable: u64,
        #[case] rungs: u32,
        #[case] floor: u64,
        #[case] expected: &[u64],
    ) {
        let expected: Vec<_> = expected
            .iter()
            .map(|&size| U256::from(size))
            .collect();

        assert_eq!(sizes(U256::from(fillable), rungs, U256::from(floor)), expected);
    }

    /// A bid's depth post of six rungs, at 0.07, 0.19, 0.33, 0.52, 0.71 and 1 WETH from its
    /// start, three in each depth word: version 1, size units of 1e4 board units of 1e12 wei.
    fn six_rungs() -> (U256, U256, U256) {
        let rung = |size: u64, offset: u64, at: usize| {
            (U256::from(size) | (U256::from(offset) << 16)) << at
        };
        let first = U256::from(DEPTH_VERSION) |
            (U256::from(4u8) << 112) |
            (U256::from(1u8) << 118) |
            rung(7, 0, 120) |
            rung(19, 40, 156) |
            rung(33, 90, 192);
        let second = rung(52, 160, 0) | rung(71, 250, 36) | rung(100, 400, 72);
        (first, second, U256::from(12u8) << 112)
    }

    /// `hundredths` of a WETH each, in wei.
    fn weth_hundredths(hundredths: &[u64]) -> Vec<U256> {
        hundredths
            .iter()
            .map(|h| U256::from(*h) * U256::from(10_000_000_000_000_000u64))
            .collect()
    }

    /// A depth post is read as the front walks it: each rung's end from the post's start, or
    /// from the fill memory's place (0.25 WETH here, 250,000 board units), skipping the rungs it
    /// used up; the second word's rungs only where the first says it holds them; nothing of a
    /// post of another version or past the front's exponents. The ask reads its board exponent
    /// from the pair word's next byte.
    #[test]
    fn a_depth_post_is_read_as_the_front_walks_it() {
        let (first, second, pair) = six_rungs();
        let post = RungPost::of(first, second, pair, true).unwrap();

        assert_eq!(post.edges(U256::ZERO), weth_hundredths(&[7, 19, 33, 52, 71, 100]));
        assert_eq!(post.edges(U256::from(250_000u64)), weth_hundredths(&[8, 27, 46, 75]));
        let one_word = RungPost::of(first & !(U256::from(1u8) << 118usize), second, pair, true);
        assert_eq!(one_word.unwrap().edges(U256::ZERO), weth_hundredths(&[7, 19, 33]));
        assert!(RungPost::of(first + U256::from(1u8), second, pair, true).is_none());
        assert!(RungPost::of(first | (U256::from(31u8) << 112usize), second, pair, true).is_none());
        assert!(RungPost::of(first, second, U256::from(17u8) << 112usize, true).is_none());
        assert_eq!(
            RungPost::of(first, second, pair, false)
                .unwrap()
                .unit,
            U256::from(1u8)
        );
    }

    /// A side's fill memory counts only while its board word is tagged with the post's nonce;
    /// where its controls let a standing word refill it, the boundaries from the post's start
    /// are asked too; an unread board word, or no post, asks none.
    #[test]
    fn a_sides_boundaries_count_its_fill_memory_only_on_its_own_post() {
        let (first, second, pair) = six_rungs();
        let board = |tag: u64| (U256::from(tag) << 4) | (U256::from(250_000u64) << 52);
        let side = |board: Option<U256>| OpenSide {
            view: SideView {
                nonce_ms: 77,
                post: RungPost::of(first, second, pair, true),
                ..SideView::default()
            },
            board,
            ..OpenSide::default()
        };
        let (filled, whole) =
            (weth_hundredths(&[8, 27, 46, 75]), weth_hundredths(&[7, 19, 33, 52, 71, 100]));

        assert_eq!(side(Some(board(77))).rung_edges(false), filled);
        assert_eq!(side(Some(board(77))).rung_edges(true), [filled, whole.clone()].concat());
        assert_eq!(side(Some(board(78))).rung_edges(false), whole);
        assert!(side(None).rung_edges(true).is_empty());
        assert!(OpenSide { board: Some(board(77)), ..OpenSide::default() }
            .rung_edges(true)
            .is_empty());
    }

    /// At most [`MAX_LOW_SIZES`] sizes below the halvings, the floor the last of them.
    #[test]
    fn a_deep_side_samples_a_bounded_number_of_sizes() {
        let sizes = sizes(U256::MAX, 12, U256::from(1u8));

        assert_eq!(sizes.len(), 12 + MAX_LOW_SIZES);
        assert_eq!(sizes[0], U256::from(1u8));
    }

    /// Base's listings store a 1 USDC least fill as mantissa 1, exponent 6.
    #[test]
    fn a_least_fill_is_its_mantissa_times_ten_to_its_exponent() {
        assert_eq!(amount24(1 | (6 << 18)), U256::from(1_000_000u64));
        assert_eq!(amount24(0x3ffff), U256::from(0x3ffffu64));
    }

    /// A word made at 1,000,000 ms with a 60 s life, under a 30 s cap: out at 1,030,001 unless
    /// its depth expires first (in seconds) or the age clock's slack brings its end forward.
    #[rstest]
    #[case::the_capped_word(2_000, 0, 1_030_001)]
    #[case::the_depth_first(1_020, 0, 1_020_000)]
    #[case::with_age_slack(2_000, 500, 1_029_501)]
    fn a_side_runs_out_at_its_word_or_depth_end(
        #[case] depth_expires: u64,
        #[case] age_slack_ms: u64,
        #[case] expected: u64,
    ) {
        let word = (U256::from(1_000_000u64) << 72) | (U256::from(60u8) << 120) | U256::from(1u8);
        let depth = (U256::from(depth_expires) << 60) | U256::from(0x1234u16);
        let clock = Timing {
            maxTtlMs: U256::from(30_000u64),
            ageSlackMs: U256::from(age_slack_ms),
            ..Timing::default()
        };

        assert_eq!(runs_out_ms(word, depth, &clock), expected);
    }

    /// Base's listings store 0.1 gwei as mantissa 1, exponent 8.
    #[rstest]
    #[case::off(0, None)]
    #[case::a_tenth_of_a_gwei(1 | (8 << 12), Some(100_000_000))]
    #[case::the_largest(0xffff, Some(4_095_000_000_000_000_000))]
    fn a_tip_cap_is_its_mantissa_times_ten_to_its_exponent(
        #[case] stored: u16,
        #[case] expected: Option<u128>,
    ) {
        assert_eq!(tip_cap_wei(stored), expected.map(U256::from));
    }

    #[test]
    fn the_probe_is_a_trillion_whole_tokens() {
        assert_eq!(probe(6), U256::from(10u64).pow(U256::from(18)));
    }

    #[test]
    fn the_implementation_slot_is_eip_1967s() {
        use alloy::primitives::keccak256;

        let hash = keccak256("eip1967.proxy.implementation");
        let slot = U256::from_be_bytes(hash.0) - U256::from(1);

        assert_eq!(IMPLEMENTATION_SLOT, B256::from(slot));
    }

    /// The front's store keeps listing 1's record here on Base, as read from its storage.
    #[test]
    fn a_listings_record_is_in_the_fronts_pools_map() {
        let slot = b256!("c8e1c483f59f8925a663796ac1a1d7a52011d2a3656a2af9a7b6948cf2d8cd5b");

        assert_eq!(record_word_slot(1, 0), U256::from_be_bytes(slot.0));
        assert_eq!(record_word_slot(1, CONTROLS_WORD), U256::from_be_bytes(slot.0) + U256::from(7));
    }

    fn timing(clock_block: u64, clock_ms: u64) -> Timing {
        Timing {
            ageSlackMs: U256::from(1_000u64),
            clockMode: U256::from(CLOCK_BLOCKS),
            clockBlock: U256::from(clock_block),
            clockMs: U256::from(clock_ms),
            ..Timing::default()
        }
    }

    fn at(number: u64, time: u64, ms: u64, exact: bool) -> At {
        At { number, time, ms, exact, base_time: None }
    }

    /// A block at which the front reads BaseTime, its stored sub-second set to the block's own.
    fn at_base_time(number: u64, ms: u64) -> At {
        At { number, time: ms / 1000, ms, exact: true, base_time: Some(ms % 1000) }
    }

    /// Counting 200 ms a block from block 100 at 1,000,000 ms, block 112 is at 1,002,400 ms, in
    /// its own second 1,002: the clock is exact, and an override lands on the last block at or
    /// before a time, or the last before one.
    #[rstest]
    #[case::at_a_block(1_003_000, at(115, 1_003, 1_003_000, true))]
    #[case::between_blocks(1_003_199, at(115, 1_003, 1_003_000, true))]
    #[case::before_the_read(1_000_000, at(112, 1_002, 1_002_400, true))]
    fn an_exact_clock_overrides_the_last_block_at_or_before_a_time(
        #[case] target_ms: u64,
        #[case] expected: At,
    ) {
        let clock = FrontClock::new(&timing(100, 1_000_000), 112, 1_002, None);

        assert_eq!(clock.ms(), 1_002_400);
        assert_eq!(clock.at_or_before(target_ms), expected);
    }

    #[rstest]
    #[case::past_a_block(1_003_001, Some(at(115, 1_003, 1_003_000, true)))]
    #[case::at_a_block(1_003_000, Some(at(114, 1_002, 1_002_800, true)))]
    #[case::the_next_block(1_002_601, Some(at(113, 1_002, 1_002_600, true)))]
    #[case::none_after_the_read(1_002_600, None)]
    fn an_exact_clock_overrides_the_last_block_before_a_time(
        #[case] target_ms: u64,
        #[case] expected: Option<At>,
    ) {
        let clock = FrontClock::new(&timing(100, 1_000_000), 112, 1_002, None);

        assert_eq!(clock.last_before(target_ms), expected);
    }

    /// A block whose counted milliseconds disagree with its second, where no millisecond source
    /// answers, reads the start of the second, and the age clock adds the slack; overrides then
    /// step whole seconds, a block each.
    #[test]
    fn a_seal_clock_overrides_whole_seconds() {
        let clock = FrontClock::new(&timing(100, 1_000_000), 112, 1_005, None);

        assert_eq!(clock.ms(), 1_005_000);
        assert_eq!(clock.age_ms(&clock.read_at()), 1_006_000);
        assert_eq!(clock.at_or_before(1_008_999), at(115, 1_008, 1_008_000, false));
        assert_eq!(clock.last_before(1_008_001), Some(at(115, 1_008, 1_008_000, false)));
        assert_eq!(clock.last_before(1_006_000), None);
    }

    /// Where the count leaves the block's second, the front reads BaseTime, here 600 ms into the
    /// read's second. Base makes a block every 200 ms, each storing its own sub-second in
    /// BaseTime, so an override lands on the last 200 ms at or before a time, BaseTime set there:
    /// never a block younger than the chain's last before it.
    #[rstest]
    #[case::into_a_second_past_the_reading(1_008_700, at_base_time(127, 1_008_600))]
    #[case::into_a_second_before_the_reading(1_008_300, at_base_time(125, 1_008_200))]
    #[case::at_a_block(1_007_000, at_base_time(119, 1_007_000))]
    #[case::the_next_block(1_005_999, at_base_time(113, 1_005_800))]
    #[case::before_the_read(1_005_000, at(112, 1_005, 1_005_600, true))]
    fn a_base_time_clock_overrides_every_200_ms_setting_base_time(
        #[case] target_ms: u64,
        #[case] expected: At,
        #[values(1, 2)] mode: u64,
    ) {
        let timing = Timing { clockMode: U256::from(mode), ..timing(100, 1_000_000) };
        let clock = FrontClock::new(&timing, 112, 1_005, Some(1_005_600));

        assert_eq!(clock.ms(), 1_005_600);
        assert_eq!(clock.age_ms(&clock.read_at()), 1_005_600);
        assert_eq!(clock.at_or_before(target_ms), expected);
        assert_eq!(clock.last_before(1_005_800), None);
        assert_eq!(clock.last_before(1_005_801), Some(at_base_time(113, 1_005_800)));
        assert_eq!(clock.last_before(1_006_100), Some(at_base_time(114, 1_006_000)));
    }

    /// A print's life is asked at the last block of each second of a book's life, after the read's
    /// block and before the life's last: counted from 1,002,400 ms to a life ending at
    /// 1,010,000 ms, at 800 ms into seconds 1,002 to 1,008, 1,009's being the life's last; on the
    /// seal clock, at the start of seconds 1,006 to 1,012, the read's own and 1,013's left out; and
    /// where the front reads BaseTime 600 ms into the read's second, at 800 ms into each.
    #[rstest]
    #[case::counted(
        FrontClock::new(&timing(100, 1_000_000), 112, 1_002, None),
        1_010_000,
        (1_002..=1_008).map(|second| at(114 + (second - 1_002) * 5, second, second * 1_000 + 800, true)).collect()
    )]
    #[case::seal(
        FrontClock::new(&timing(100, 1_000_000), 112, 1_005, None),
        1_013_500,
        (1_006..=1_012).map(|second| at(112 + second - 1_005, second, second * 1_000, false)).collect()
    )]
    #[case::from_base_time(
        FrontClock::new(&timing(100, 1_000_000), 112, 1_005, Some(1_005_600)),
        1_008_000,
        (1_005..=1_006).map(|second| at_base_time(113 + (second - 1_005) * 5, second * 1_000 + 800)).collect()
    )]
    fn a_print_is_asked_at_the_last_block_of_each_second_of_the_life(
        #[case] clock: FrontClock,
        #[case] until_ms: u64,
        #[case] expected: Vec<At>,
    ) {
        assert_eq!(clock.seconds_until(until_ms), expected);
    }

    /// The front's clock may read earlier at a later block only in its block-count mode, with the
    /// count off BaseTime by less than a second: block 112 counts `count_ms`, where BaseTime reads
    /// `source_ms` in second 1,005. On its anchor, or a second or more off it, the clock reads the
    /// count at every block, or BaseTime at every block.
    #[rstest]
    #[case::on_its_anchor(2, 1_005_600, Some(1_005_600), false)]
    #[case::a_millisecond_behind(2, 1_005_599, Some(1_005_600), true)]
    #[case::a_block_behind(2, 1_005_400, Some(1_005_600), true)]
    #[case::under_a_second_behind(2, 1_004_601, Some(1_005_600), true)]
    #[case::a_second_behind(2, 1_004_600, Some(1_005_600), false)]
    #[case::three_blocks_ahead(2, 1_006_200, Some(1_005_600), true)]
    #[case::under_a_second_ahead(2, 1_006_599, Some(1_005_600), true)]
    #[case::a_second_ahead(2, 1_006_600, Some(1_005_600), false)]
    #[case::far_off(2, 1_002_400, Some(1_005_600), false)]
    #[case::not_counting(1, 1_005_400, Some(1_005_600), false)]
    #[case::without_base_time(2, 1_005_400, None, false)]
    #[case::base_time_outside_the_second(2, 1_005_400, Some(1_006_000), false)]
    fn the_clock_may_step_back_only_with_its_count_off_base_time_by_under_a_second(
        #[case] mode: u64,
        #[case] count_ms: u64,
        #[case] source_ms: Option<u64>,
        #[case] expected: bool,
    ) {
        let timing = Timing { clockMode: U256::from(mode), ..timing(100, count_ms - 2_400) };
        let clock = FrontClock::new(&timing, 112, 1_005, source_ms);

        assert_eq!(clock.may_step_back(), expected);
    }

    /// The seal clock and a counted one set no state.
    #[test]
    fn only_a_base_time_reading_sets_base_time() {
        let counted = FrontClock::new(&timing(100, 1_000_000), 112, 1_002, Some(1_002_400));
        let seal = FrontClock::new(&timing(100, 1_000_000), 112, 1_005, None);
        let base_time = FrontClock::new(&timing(100, 1_000_000), 112, 1_005, Some(1_005_600));

        assert_eq!(state_override(&counted.at_or_before(1_004_000)), serde_json::Value::Null);
        assert_eq!(state_override(&seal.at_or_before(1_008_000)), serde_json::Value::Null);
        assert_eq!(
            state_override(&base_time.at_or_before(1_008_300)),
            serde_json::json!({
                "0x4200000000000000000000000000000000000030": {
                    "stateDiff": {
                        "0x0000000000000000000000000000000000000000000000000000000000000000":
                            "0x00000000000000000000000000000000000000000000000000000000000000c8"
                    }
                }
            })
        );
    }

    /// The front reads BaseTime only where it answers within the block's second, and not at all
    /// in its seal mode.
    #[rstest]
    #[case::outside_the_second(2, Some(1_006_000), 1_005_000, false)]
    #[case::none(2, None, 1_005_000, false)]
    #[case::in_the_seal_mode(0, Some(1_005_600), 1_005_000, false)]
    #[case::in_the_base_time_mode(1, Some(1_005_600), 1_005_600, true)]
    fn the_clock_reads_its_source_only_within_the_blocks_second(
        #[case] mode: u64,
        #[case] source_ms: Option<u64>,
        #[case] ms: u64,
        #[case] exact: bool,
    ) {
        let timing = Timing { clockMode: U256::from(mode), ..timing(100, 1_000_000) };
        let clock = FrontClock::new(&timing, 112, 1_005, source_ms);

        assert_eq!((clock.ms(), clock.exact), (ms, exact));
    }

    /// An override's opening reads must show the front's clock where the override meant it: a
    /// node that moves the time but not the number leaves a counted clock elsewhere.
    #[test]
    fn an_override_holds_only_where_the_fronts_clock_reads_as_asked() {
        let clock = FrontClock::new(&timing(100, 1_000_000), 112, 1_002, Some(1_002_400));
        let target = clock.at_or_before(1_004_000);
        let answer = |value: u64| Call3Result {
            success: true,
            returnData: U256::from(value).abi_encode().into(),
        };
        let checks =
            |number: u64, time: u64, source: u64| [answer(number), answer(time), answer(source)];

        assert!(runs_at(&checks(120, 1_004, 1_004_000), &target, Some(&clock)));
        assert!(!runs_at(&checks(112, 1_004, 1_004_400), &target, Some(&clock)));
        assert!(!runs_at(&checks(120, 1_002, 1_002_400), &target, Some(&clock)));
    }

    /// The selectors of the door surface, as the door's interface lists them.
    #[test]
    fn the_door_selectors_are_ipropamms() {
        assert_eq!(quoteCall::SELECTOR, [0xb6, 0x46, 0x63, 0x84]);
        assert_eq!(quoteFillableCall::SELECTOR, [0x7a, 0xe8, 0x48, 0x25]);
    }
}
