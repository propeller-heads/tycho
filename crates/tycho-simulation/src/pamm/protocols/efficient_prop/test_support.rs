//! Fixtures shared by the state and feed tests: a WETH/USDC state, a JSON-RPC node that serves a
//! front, its listing-door factory and the doors, with knobs for what a test wants it to get
//! wrong, and a node that replays a poll recorded from a local fork, of Base with a test listing or
//! of Ethereum with the deployed front and the first pool's launch replayed (`testdata/`).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{Read, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use alloy::{
    primitives::{address, aliases::U24, b256, keccak256, Address, B256, U256},
    providers::{
        bindings::IMulticall3::{
            aggregate3Call, getBlockNumberCall, getChainIdCall, getCurrentBlockTimestampCall,
            Result as Call3Result,
        },
        MULTICALL3_ADDRESS,
    },
    sol_types::{SolCall, SolValue},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::{
    book::BookFeedConfig,
    pamm::protocols::efficient_prop::{
        ladder::Ladder,
        overrides::{BEACON_GENESIS, SLOT_SECS, STATE_METHOD},
        reader::{
            balanceOfCall, clockCall, deskCellOfCall, doorOfCall, doorViewCall, quoteCall,
            quoteFillableCall, quoteFromCall, BASECall, CELL_CODEHASHCall, DOOR_CODEHASHCall,
            DoorView, FRONTCall, LISTINGCall, Timing, IMPLEMENTATION_SLOT,
        },
        state::{CreditGas, DirectionGas, EfficientPropState, FillReads, GasChain, PairPlace},
        EfficientPropDeployment,
    },
    snapshot_feed::http::test_support::{spawn_http_server, MockHttpServer, MockRequest},
};

pub(super) const FRONT: Address = Address::repeat_byte(0xf0);
pub(super) const FACTORY: Address = Address::repeat_byte(0xfa);
pub(super) const IMPLEMENTATION: Address = Address::repeat_byte(0x1a);
/// The fake implementation's runtime code.
pub(super) const IMPLEMENTATION_CODE: &[u8] = b"the fake front implementation";
/// The creation code hash of the fake factory's doors.
pub(super) const DOOR_CODEHASH: B256 = B256::repeat_byte(0xdc);
/// The creation code hash of the fake front's desk contracts.
pub(super) const CELL_CODEHASH: B256 = B256::repeat_byte(0xcc);

/// How far into its second the fake BNB Chain node's latest block was made, ms.
pub(super) const BNB_HEAD_SUB_MS: u64 = 300;

/// BNB Chain's millisecond precompile (BEP-706).
const MS_PRECOMPILE: Address = address!("0000000000000000000000000000000000000070");

/// Pair `i`'s quote word is at this plus `i` in a desk contract.
pub(super) const QUOTE_BASE: u64 = 0x100;
/// Pair `i`'s firm word is at this plus `i`.
pub(super) const FIRM_BASE: u64 = 0x200;

/// The desk contract's records the builder's reference encoder holds for pair slots 0, 7 and 255,
/// each `(pair slot, desk epoch, quote word, block)`: at desk epochs 0, 3 and 16,847,216. The
/// slot 7 word carries and has its firm bit set; the desk words they land under are
/// [`vector_desk_word`].
pub(super) const VECTOR_WORDS: [(u8, u32, &str, u64); 3] = [
    (0, 0, "000000000000040a01a1fa42207b012c0001012c2a003039000000000193c028", 26_460_200),
    (7, 3, "000000000201a60a01a1fa4231d7019a000900fa33abcdef000000030193c029", 26_460_201),
    (
        255,
        16_847_216,
        "000000000000000001a1fa42470f00007fff000000000000010111700193c02a",
        26_460_202,
    ),
];

/// The desk word, slot 1, of the vectors' desk at desk epoch `epoch`, not stopped: the vectors'
/// pusher in bits 0-159 and the epoch in bits 160-190.
pub(super) fn vector_desk_word(epoch: u32) -> U256 {
    (U256::from(epoch) << 160usize) |
        U256::from_str_radix("985442e6d7c3da32f2f0c8c65fb8f8c60dcabf03", 16).unwrap()
}

/// A desk word, slot 1: the vectors' pusher, the desk epoch and the stop.
pub(super) fn desk_word_of(epoch: u32, stopped: bool) -> B256 {
    B256::from(vector_desk_word(epoch) | (U256::from(u8::from(stopped)) << 224usize))
}

/// A pair's quote word as the desk contract stores it, of the fields a reader reads: the stamp
/// (the block's low 32 bits) at bits 0-31, the desk epoch it was written under at 32-62, carry at
/// 201, on at 202 and firm state at 205.
pub(super) fn quote_word(stamp: u64, epoch: u32, carry: bool, firm: bool) -> B256 {
    B256::from(
        U256::from(stamp & 0xffff_ffff) |
            (U256::from(epoch & 0x7fff_ffff) << 32usize) |
            (U256::from(u8::from(carry)) << 201usize) |
            (U256::from(1u8) << 202usize) |
            (U256::from(u8::from(firm)) << 205usize),
    )
}
const WETH: Address = address!("4200000000000000000000000000000000000006");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const CBBTC: Address = address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf");

/// The fake node's latest block timestamp, Unix seconds.
pub(super) const NODE_TIMESTAMP: u64 = 1_790_000_000;

/// The fake node's latest block number.
pub(super) const NODE_BLOCK: u64 = 38_000_000;

/// The beacon slot of the fake Ethereum node's next block.
pub(super) const ETHEREUM_SLOT: u64 = 15_264_665;

/// The fake Ethereum node's next block's time, Unix seconds: its slot's start.
pub(super) const ETHEREUM_NEXT_TIME: u64 = BEACON_GENESIS + SLOT_SECS * ETHEREUM_SLOT;

/// The fake Ethereum node's latest block number; its next is one more.
pub(super) const ETHEREUM_BLOCK: u64 = 24_000_000;

/// The hash a fake node names its latest block by, unless a test sets another.
pub(super) const HEAD_HASH: B256 = B256::repeat_byte(0xb1);

/// The BaseTime predeploy, whose `timestampMs()` answers the block's milliseconds.
const BASE_TIME: Address = address!("4200000000000000000000000000000000000030");

/// `timestampMs()`, which BaseTime answers.
const TIMESTAMP_MS_SELECTOR: [u8; 4] = [0x57, 0x45, 0xa6, 0x77];

/// The price source a fake listing is bound to unless a test says otherwise, as the front's
/// `registerSource` numbers them: the one the tests' feeds approve.
pub(super) const APPROVED_SOURCE: u32 = 1;

/// The lane a fake listing's price source stores its prints on, named in record word 2's low 160
/// bits: a guarded source's lane holds the prints a post replaced.
pub(super) const HOLD_LANE: Address = Address::repeat_byte(0x77);

alloy::sol! {
    function HOLD_MS() external view returns (uint256);
}

/// The fake front's cap on a word's life, ms, as Base's front sets it.
pub(super) const MAX_TTL_MS: u64 = 30_000;

/// The fence kinds a listing's settings word stores in its bits 228-229
/// (`PropAMMWords.FENCE_*`): a PRINT and a SESSION listing read a price print, a WORD listing
/// does not.
pub(super) const FENCE_PRINT: u64 = 0;
pub(super) const FENCE_WORD: u64 = 1;
pub(super) const FENCE_NONE: u64 = 2;
pub(super) const FENCE_SESSION: u64 = 3;

pub(super) fn bytes(address: Address) -> Bytes {
    Bytes::from(address.to_vec())
}

pub(super) fn weth_address() -> Address {
    WETH
}

pub(super) fn usdc_address() -> Address {
    USDC
}

pub(super) fn cbbtc_address() -> Address {
    CBBTC
}

pub(super) fn token(address: Address) -> Token {
    let (symbol, decimals) = match address {
        WETH => ("WETH", 18),
        USDC => ("USDC", 6),
        CBBTC => ("cbBTC", 8),
        _ => ("TKN", 18),
    };
    Token::new(&bytes(address), symbol, decimals, 0, &[Some(10_000)], Chain::Base, 100)
}

pub(super) fn weth() -> Token {
    token(WETH)
}

pub(super) fn usdc() -> Token {
    token(USDC)
}

/// WETH, USDC and cbBTC on Base, with no TVL floor.
pub(super) fn book_config() -> BookFeedConfig {
    let tokens: HashMap<Bytes, Token> = [WETH, USDC, CBBTC]
        .into_iter()
        .map(|address| (bytes(address), token(address)))
        .collect();
    BookFeedConfig { chain: Chain::Base, tokens: Arc::new(tokens), min_tvl_usd: 0.0 }
}

pub(super) fn deployment() -> EfficientPropDeployment {
    EfficientPropDeployment {
        quote_layout: None,
        front: bytes(FRONT),
        factory: bytes(FACTORY),
        cell_codehash: Some(Bytes::from(CELL_CODEHASH.to_vec())),
        implementations: vec![bytes(IMPLEMENTATION)],
    }
}

/// The fake front's deployment on Ethereum, which places makers' quotes in desk contracts.
pub(super) fn ethereum_deployment() -> EfficientPropDeployment {
    EfficientPropDeployment {
        quote_layout: Some(super::QUOTE_LAYOUT_BUILDER.to_string()),
        ..deployment()
    }
}

fn u(value: u128) -> U256 {
    U256::from(value)
}

/// A WETH/USDC door that sells 0.5 WETH for 1,499 USDC and 1 WETH for 2,990 USDC, and buys
/// 0.5 WETH for 1,505 USDC and 1 WETH for 3,010 USDC.
pub(super) fn weth_usdc_state() -> EfficientPropState {
    let ladder = |points: [(u128, u128); 2]| {
        Ladder::new(
            points
                .iter()
                .map(|&(a, o)| (u(a), u(o)))
                .collect(),
            None,
        )
        .unwrap()
    };
    EfficientPropState::new(
        weth(),
        usdc(),
        bytes(Address::repeat_byte(0xd1)),
        ladder([
            (500_000_000_000_000_000, 1_499_000_000),
            (1_000_000_000_000_000_000, 2_990_000_000),
        ]),
        ladder([
            (1_505_000_000, 500_000_000_000_000_000),
            (3_010_000_000, 1_000_000_000_000_000_000),
        ]),
        FillReads::Word,
        PairPlace::HookPair,
        GasChain::Base,
        None,
    )
}

/// One gas figure each direction, neither zero.
pub(super) fn direction_gas(sell_base: u64, buy_base: u64) -> DirectionGas {
    DirectionGas {
        sell_base: sell_base.try_into().unwrap(),
        buy_base: buy_base.try_into().unwrap(),
    }
}

/// The gas the tests name for a fake listing in no credit mode, on Base, where no figure was
/// measured, and on Ethereum, whose pair of test tokens no figure was measured for: 95,825
/// selling base and 96,239 buying it.
pub(super) fn named_gas() -> CreditGas {
    CreditGas {
        custody: direction_gas(95_825, 96_239),
        route: direction_gas(95_825, 96_239),
        most_before_refunds: None,
    }
}

/// A credit listing's gas: 160,877 selling base and 158,340 buying it inside custody, 610,501
/// selling base and 187,000 buying it through the route.
pub(super) fn credit_gas() -> CreditGas {
    CreditGas {
        custody: direction_gas(160_877, 158_340),
        route: direction_gas(610_501, 187_000),
        most_before_refunds: None,
    }
}

/// A WETH/USDC credit listing on Ethereum: its bid sells 0.05 WETH for 149.5 USDC and 0.1 WETH
/// for 299 USDC inside its pool's custody, and 0.25 WETH for 675 USDC and 1 WETH for 2,700 USDC
/// through its credit route; its ask is [`weth_usdc_state`]'s, inside custody. Charged
/// [`credit_gas`].
pub(super) fn weth_usdc_credit_state() -> EfficientPropState {
    let bid = Ladder::new(
        [
            (50_000_000_000_000_000, 149_500_000),
            (100_000_000_000_000_000, 299_000_000),
            (250_000_000_000_000_000, 675_000_000),
            (1_000_000_000_000_000_000, 2_700_000_000),
        ]
        .iter()
        .map(|&(a, o)| (u(a), u(o)))
        .collect(),
        Some(2),
    )
    .unwrap();
    EfficientPropState {
        bid,
        gas_chain: GasChain::Ethereum,
        credit: Some(credit_gas()),
        ..weth_usdc_state()
    }
}

/// One side of a fake door: a price, filled between a minimum and a maximum input. The door
/// reverts for any other size, and refuses every size where `max_in` is zero.
#[derive(Clone, Debug)]
pub(super) struct FakeSide {
    /// Output atoms per `per` input atoms.
    pub price: U256,
    pub per: U256,
    pub min_in: U256,
    pub max_in: U256,
    /// Depth rungs as the front walks them: `(input size, offset ppm off the price)`, in order,
    /// each filled at the price less its offset and the last one extended to `max_in`. Empty is
    /// one flat price.
    pub rungs: Vec<(U256, u64)>,
    /// A credit listing's route: an input above the first amount fills whole at the second's
    /// price (output atoms per `per` input atoms), as the pool's own preview prices it, below the
    /// ladder's.
    pub credit_route: Option<(U256, U256)>,
    /// Where set, the most the side's `quoteFillable` reports of a probe the side does not fill
    /// whole: what its pool's custody fills, as a credit listing's door answers where its
    /// pool's preview does not fill the maker's whole depth. A probe it fills whole is answered
    /// whole.
    pub custody_most: Option<U256>,
}

impl FakeSide {
    /// Sells WETH for USDC at 2,990, from 0.001 WETH to 1 WETH.
    pub(super) fn weth_bid() -> Self {
        Self {
            price: u(2_990_000_000),
            per: u(1_000_000_000_000_000_000),
            min_in: u(1_000_000_000_000_000),
            max_in: u(1_000_000_000_000_000_000),
            rungs: Vec::new(),
            credit_route: None,
            custody_most: None,
        }
    }

    /// Buys WETH with USDC at 3,010, from 3 USDC to 3,010 USDC.
    pub(super) fn weth_ask() -> Self {
        Self {
            price: u(1_000_000_000_000_000_000),
            per: u(3_010_000_000),
            min_in: u(3_000_000),
            max_in: u(3_010_000_000),
            rungs: Vec::new(),
            credit_route: None,
            custody_most: None,
        }
    }

    pub(super) fn closed() -> Self {
        Self {
            price: U256::ZERO,
            per: u(1),
            min_in: U256::ZERO,
            max_in: U256::ZERO,
            rungs: Vec::new(),
            credit_route: None,
            custody_most: None,
        }
    }

    /// The WETH bid on stepped depth: 0.1 WETH at the price, 0.3 WETH 50 ppm under it and the
    /// rest to 1 WETH 150 ppm under it.
    pub(super) fn stepped_weth_bid() -> Self {
        Self {
            rungs: vec![
                (u(100_000_000_000_000_000), 0),
                (u(300_000_000_000_000_000), 50),
                (u(600_000_000_000_000_000), 150),
            ],
            ..Self::weth_bid()
        }
    }

    /// The WETH bid of a credit listing: the ladder's price up to 0.1 WETH, and above it the
    /// whole input at 2,700, the pool's own preview.
    pub(super) fn credit_weth_bid() -> Self {
        Self {
            credit_route: Some((u(100_000_000_000_000_000), u(2_700_000_000))),
            ..Self::weth_bid()
        }
    }

    /// The door's output for `amount_in`, or `None` where it refuses it.
    pub(super) fn quote(&self, amount_in: U256) -> Option<U256> {
        self.quote_discounted(amount_in, 0)
    }

    /// The output for `amount_in` with every level's price cut by `discount` ppm, as the front
    /// cuts an aged listing's levels: once, before the output is rounded down.
    pub(super) fn quote_discounted(&self, amount_in: U256, discount: u64) -> Option<U256> {
        if self.max_in.is_zero() || amount_in < self.min_in || amount_in > self.max_in {
            return None;
        }
        let ppm = u(1_000_000);
        let kept = ppm - U256::from(discount);
        if let Some((prefix, price)) = self.credit_route {
            if amount_in > prefix {
                return Some(amount_in * price * kept / (self.per * ppm));
            }
        }
        if self.rungs.is_empty() {
            return Some(amount_in * self.price * kept / (self.per * ppm));
        }
        let (mut left, mut out) = (amount_in, U256::ZERO);
        for (i, &(size, offset)) in self.rungs.iter().enumerate() {
            let take = if i + 1 == self.rungs.len() { left } else { left.min(size) };
            out += take * self.price * (ppm - U256::from(offset)) * kept / (ppm * ppm);
            left -= take;
        }
        Some(out / self.per)
    }

    /// The output the front's plan gives `amount_in` on the credit route, which reads no
    /// preview: the ladder's own price, as if custody held the output.
    fn planned(&self, amount_in: U256) -> Option<U256> {
        Self { credit_route: None, ..self.clone() }.quote(amount_in)
    }

    fn fillable(&self, amount_in: U256) -> Option<(U256, U256)> {
        let fill = match self.custody_most {
            Some(custody) if amount_in > self.max_in => custody,
            _ => amount_in.min(self.max_in),
        };
        if fill.is_zero() {
            return Some((U256::ZERO, U256::ZERO));
        }
        Some((fill, self.quote(fill)?))
    }
}

/// How a listing's levels age, as its depth words and controls set it: the front takes a
/// discount, ppm, off each level of `drift * aged / 1000` (each side's drift, ppm a second) and
/// `widen * sqrt(aged / 1000)`, both rounded up, `aged` being the word's age past the grace (ms),
/// and the premium while the clock is within its window of the latest of the word's maker time,
/// the side's depth nonce and the controls' nonce.
#[derive(Clone, Debug, Default)]
pub(super) struct FakeAging {
    pub bid_drift: u64,
    pub ask_drift: u64,
    pub widen: u64,
    pub grace_ms: u64,
    pub premium: u64,
    pub window_ms: u64,
    /// The controls' nonce and each side's depth nonce, ms.
    pub controls_nonce_ms: u64,
    pub bid_nonce_ms: u64,
    pub ask_nonce_ms: u64,
}

impl FakeAging {
    /// The controls word as the front stores it: version 1 and the aging fields.
    fn controls(&self) -> U256 {
        U256::from(1u8) |
            (U256::from(self.controls_nonce_ms) << 16) |
            (U256::from(self.widen) << 136) |
            (U256::from(self.premium) << 156) |
            (U256::from(self.window_ms) << 176) |
            (U256::from(self.grace_ms) << 192)
    }

    /// The discount, ppm, on side `bid` at the front's clock `now_ms` of a word made at
    /// `maker_ms`, as the front takes it.
    pub(super) fn discount(&self, bid: bool, maker_ms: u64, now_ms: u64) -> u64 {
        let aged = now_ms
            .saturating_sub(maker_ms)
            .saturating_sub(self.grace_ms);
        let drift = if bid { self.bid_drift } else { self.ask_drift };
        let mut c = (drift * aged).div_ceil(1000);
        if self.widen != 0 && aged != 0 {
            let square = u128::from(self.widen).pow(2) * u128::from(aged) * 1000;
            let mut root = square.isqrt();
            if root * root < square {
                root += 1;
            }
            c += u64::try_from(root.div_ceil(1000)).unwrap();
        }
        let nonce = if bid { self.bid_nonce_ms } else { self.ask_nonce_ms };
        let last = maker_ms
            .max(nonce)
            .max(self.controls_nonce_ms);
        if now_ms < last + self.window_ms {
            c += self.premium;
        }
        c
    }
}

/// A print a post replaced, which a guarded source's lane still holds: from `from_ms` on the
/// front's clock, for `for_ms`, each side priced at the held print where it is worse for its taker,
/// its output `bid_ppm` and `ask_ppm` lower than at the kept print alone (one of them zero: a post
/// that moves the price up holds the bid down, one that moves it down the ask).
#[derive(Clone, Copy, Debug)]
pub(super) struct FakeHold {
    pub from_ms: u64,
    pub for_ms: u64,
    pub bid_ppm: u64,
    pub ask_ppm: u64,
}

/// The slot of word `word` of listing `id`'s record in the front's storage: the store's `pools`
/// map, slot 18 of its namespace, keyed by the id.
pub(super) fn record_word_slot(id: u32, word: u64) -> U256 {
    let namespace = U256::from_be_bytes(
        b256!("ca023b73b2185d1ba92ba3c09f7870c56c9bdadd91407b68ebb408947da60b00").0,
    );
    let key = (U256::from(id), namespace + U256::from(18u8)).abi_encode();
    U256::from_be_bytes(keccak256(key).0) + U256::from(word)
}

/// A listing the fake front has reserved, and its door.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a test fixture: each flag sets one independent field of the front's record"
)]
pub(super) struct FakeListing {
    pub id: u32,
    pub base: Address,
    pub quote: Address,
    /// The pool whose custody pays its fills, `doorView`'s `pool`: one of the listing's own by
    /// default.
    pub pool: Address,
    /// Whether the pool holds none of the base and none of the quote token.
    pub pool_lacks: (bool, bool),
    /// The base's index among its pool's tokens, `doorView`'s `baseIndex`: 0 where the base is
    /// the pool's token 0, whichever of the pair's two addresses it is.
    pub base_index: u8,
    /// The quote's index among its pool's tokens, `doorView`'s `quoteIndex`: where `None`, 1 where
    /// the base is the pool's token 0, else 0.
    pub quote_index: Option<u8>,
    /// Whether its door has been opened, so has code.
    pub opened: bool,
    /// What its door answers for `FRONT` and `LISTING`.
    pub door_front: Address,
    pub door_listing: u32,
    pub bid: FakeSide,
    pub ask: FakeSide,
    /// The standing word's maker time, ms, and its life, s.
    pub maker_ms: u64,
    pub life_s: u64,
    /// When each side's depth expires, Unix seconds.
    pub bid_depth_expires: u64,
    pub ask_depth_expires: u64,
    /// The listing's credit mode, `doorView`'s `creditMode`: 0 none.
    pub credit_mode: u8,
    /// The listing's taker tip cap as the front stores it, `doorView`'s `tipCap`: a 12-bit
    /// mantissa and a 4-bit exponent of 10, wei per gas; 0 off.
    pub tip_cap: u16,
    /// `doorView`'s `confMul`, a byte of the answer no reader's field is read from.
    pub conf_mul: u8,
    /// How its levels age with the word.
    pub aging: FakeAging,
    /// Whether it is a tied listing, whose levels age with its price's print rather than the word.
    pub tied: bool,
    /// Its fence kind (`FENCE_*`), unless it is tied, which a tied listing's scopes set to PRINT.
    pub fence: u64,
    /// The least fill, of the quote token (the bid's output, the ask's input), as the front stores
    /// it: an 18-bit mantissa and a 6-bit exponent of 10.
    pub min_fill: u32,
    /// The last moment, on the front's clock, of the price print the listing reads, past which
    /// the front refuses its fills as stale; `None` for a listing that reads no print.
    pub print_ends_ms: Option<u64>,
    /// A SESSION listing's closed phase: from this moment on the front's clock, its print stale
    /// for its closed-phase delay, the door fills again where it refused past `print_ends_ms`;
    /// `None` for no closed phase.
    pub closed_from_ms: Option<u64>,
    /// The session word's `closedAfter`, s, as the front stores it in record word 13: nonzero for
    /// a SESSION listing with a closed phase. The fake prices that phase from `closed_from_ms`.
    pub closed_after_s: u64,
    /// The price sources it is bound to, record word 10 (`sourceIds`): the primary and the backup,
    /// 0 for none.
    pub sources: (u32, u32),
    /// The words of its record the node fails a read of: the session word (13), the sources'
    /// (10), the live source's copy (2).
    pub unread_words: Vec<u64>,
    /// The door refuses a bid's input from `.2` to `.3` where the front's clock reads from `.0` to
    /// before `.1`, ms, and fills it before and after: a refusal of some of the bid's sizes alone,
    /// for a while.
    pub bid_refused: Option<(u64, u64, U256, U256)>,
    /// When that print was fed, on the front's clock: the front refuses a print from its future,
    /// so the door refuses every fill where the clock reads before it; `None` for no such bound.
    pub print_fed_ms: Option<u64>,
    /// The ask's bucket, `(ms, rate)`: its room is the ask's whole depth where the front's clock
    /// reads `ms`, and `rate` input atoms a second more or less as it reads later or earlier;
    /// `None` for a bucket that never bounds the ask.
    pub ask_refill: Option<(u64, u64)>,
    /// The most the bid pays out, as a maker's bucket or the pool's room bounds it: the door
    /// refuses a bid whose output is above it; `None` for no such bound.
    pub bid_out_cap: Option<U256>,
    /// The most the ask pays out, as the pool's room bounds it: the door refuses an ask whose
    /// output is above it; `None` for no such bound.
    pub ask_out_cap: Option<U256>,
    /// Whether its pair is an asset-module pair, `doorView`'s pair word bit 15: its fills ask the
    /// module for its verdict.
    pub asset_module: bool,
    /// Whether the source it reads is guarded, the live source's `E_GUARD` (bit 247) in record
    /// word 2: a fill reading it asks its lane for the side's worst price over the prints it
    /// still holds.
    pub guarded: bool,
    /// What its source's lane answers for `HOLD_MS`; `None` where the call reverts.
    pub lane_hold_ms: Option<u64>,
    /// The print its source's lane holds, which prices a fill only where the source is guarded.
    pub held: Option<FakeHold>,
    /// The desk id bound to it, `doorView`'s `desk`: 0 for none.
    pub desk: u32,
    /// Its pair's word index in the desk contract, `doorView`'s `wordIndex`.
    pub word_index: u8,
    /// Its carry life, its settings word's bits 230-231: a word of its own may fill in the block
    /// after its landing too, where the word carries and its fence is not NONE.
    pub carry: bool,
    /// On a node of a chain whose builders land makers' updates, the block its maker's word last
    /// landed in on chain and whether that word carries; `None` for never. A state override of its
    /// quote word at its word index replaces it (see [`quote_word`]).
    pub landed: Option<(u64, bool)>,
    /// The front's plan (`quoteFrom` with `PLAN_ONLY`) answers the bid's custody sizes from `.0`
    /// to `.1` one unit over the door's quote, untagged: a plan that disagrees with the door.
    pub bid_plan_lies: Option<(U256, U256)>,
    /// From `.0` on the front's clock, ms, the bid's credit route fills every input above `.1`,
    /// in place of its own prefix.
    pub bid_prefix_from: Option<(u64, U256)>,
}

/// Listing `id`'s own pool in the fixtures.
pub(super) fn own_pool(id: u32) -> Address {
    let mut pool = [0xb0u8; 20];
    pool[16..].copy_from_slice(&id.to_be_bytes());
    Address::from(pool)
}

impl FakeListing {
    pub(super) fn weth_usdc(id: u32) -> Self {
        Self {
            id,
            base: WETH,
            quote: USDC,
            pool: own_pool(id),
            pool_lacks: (false, false),
            base_index: 0,
            quote_index: None,
            opened: true,
            door_front: FRONT,
            door_listing: id,
            bid: FakeSide::weth_bid(),
            ask: FakeSide::weth_ask(),
            maker_ms: NODE_TIMESTAMP * 1000,
            life_s: 30,
            bid_depth_expires: NODE_TIMESTAMP + 86_400,
            ask_depth_expires: NODE_TIMESTAMP + 86_400,
            credit_mode: 0,
            // 1 x 10^8 wei, 0.1 gwei.
            tip_cap: 1 | (8 << 12),
            conf_mul: 0,
            aging: FakeAging::default(),
            tied: false,
            fence: FENCE_WORD,
            // 3 x 10^6, 3 USDC.
            min_fill: 3 | (6 << 18),
            print_ends_ms: None,
            closed_from_ms: None,
            closed_after_s: 0,
            sources: (APPROVED_SOURCE, 0),
            unread_words: Vec::new(),
            bid_refused: None,
            print_fed_ms: None,
            ask_refill: None,
            bid_out_cap: None,
            ask_out_cap: None,
            asset_module: false,
            guarded: false,
            lane_hold_ms: Some(400),
            held: None,
            desk: id,
            word_index: 0,
            carry: false,
            landed: None,
            bid_plan_lies: None,
            bid_prefix_from: None,
        }
    }

    /// A WETH/USDC credit listing on Ethereum, of credit mode 2: its bid fills up to 0.1 WETH
    /// inside its pool's custody at 2,990, and a larger input whole through the credit route at
    /// 2,700 (see [`FakeSide::credit_weth_bid`]).
    pub(super) fn ethereum_credit(id: u32) -> Self {
        Self { credit_mode: 2, bid: FakeSide::credit_weth_bid(), ..Self::ethereum(id) }
    }

    /// A WETH/USDC listing on Ethereum, its maker's word made two seconds before the next block
    /// and landed on chain five blocks before the latest, so a fill finds no word for the next
    /// block without its state map. No tip cap.
    pub(super) fn ethereum(id: u32) -> Self {
        Self {
            maker_ms: (ETHEREUM_NEXT_TIME - 2) * 1000,
            bid_depth_expires: ETHEREUM_NEXT_TIME + 86_400,
            ask_depth_expires: ETHEREUM_NEXT_TIME + 86_400,
            tip_cap: 0,
            landed: Some((ETHEREUM_BLOCK - 5, false)),
            ..Self::weth_usdc(id)
        }
    }

    /// The least fill, of the quote token.
    pub(super) fn min_fill_amount(&self) -> U256 {
        U256::from(self.min_fill & 0x3ffff) * U256::from(10u8).pow(U256::from(self.min_fill >> 18))
    }

    /// The listing's `doorView`: its tokens, a pool, its standing word's maker time and life, and
    /// each side's depth expiry.
    fn door_view(&self) -> DoorView {
        let (token0, token1) = self.tokens();
        let word = (U256::from(self.maker_ms) << 72) | (U256::from(self.life_s) << 120);
        let depth = |expires: u64, nonce: u64, drift: u64| {
            (U256::from(expires) << 60) | (U256::from(nonce) << 12) | (U256::from(drift) << 92)
        };
        let aging = &self.aging;
        // The pair word's bit 8 is set where the base is the pair's lower address, its token 0,
        // and its bit 15 on an asset-module pair.
        let pair_word = if self.base == token0 { U256::from(1u16 << 8) } else { U256::ZERO } |
            if self.asset_module { U256::from(1u16 << 15) } else { U256::ZERO };
        DoorView {
            pairWord: pair_word,
            token0,
            token1,
            pool: self.pool,
            baseIndex: self.base_index,
            quoteIndex: self
                .quote_index
                .unwrap_or(if self.base_index == 0 { 1 } else { 0 }),
            word,
            depth: [
                depth(self.bid_depth_expires, aging.bid_nonce_ms, aging.bid_drift),
                U256::ZERO,
                depth(self.ask_depth_expires, aging.ask_nonce_ms, aging.ask_drift),
                U256::ZERO,
            ],
            creditMode: self.credit_mode,
            tipCap: self.tip_cap,
            confMul: self.conf_mul,
            desk: self.desk,
            wordIndex: self.word_index,
            ..DoorView::default()
        }
    }

    fn tokens(&self) -> (Address, Address) {
        if self.base < self.quote {
            (self.base, self.quote)
        } else {
            (self.quote, self.base)
        }
    }

    /// The listing's settings word as the front stores it: only its least fill and its scopes
    /// (its fence kind, and whether it is tied) are read.
    fn settings(&self) -> U256 {
        let scopes = if self.tied { 8 } else { self.fence | (u64::from(self.carry) << 2) };
        (U256::from(scopes) << 228) | (U256::from(self.min_fill) << 160)
    }

    /// The door's quote for `amount_in` of `token_in` at the front's clock `now_ms`: the side's
    /// output less the discount its age takes, or `None` where the door refuses it, the word, the
    /// depth or the print having run out included.
    pub(super) fn quote_at(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        now_ms: u64,
    ) -> Option<U256> {
        let bid = (token_in, token_out) == (self.base, self.quote);
        let side = &self.side_at(token_in, token_out, now_ms)?;
        let expires = if bid { self.bid_depth_expires } else { self.ask_depth_expires };
        if now_ms > self.maker_ms + (self.life_s * 1000).min(MAX_TTL_MS) ||
            now_ms / 1000 >= expires ||
            self.print_lapsed(now_ms) ||
            self.print_fed_ms
                .is_some_and(|fed| fed > now_ms) ||
            (bid && self
                .bid_refused
                .is_some_and(|(from, to, least, most)| {
                    (from..to).contains(&now_ms) && (least..=most).contains(&amount_in)
                }))
        {
            return None;
        }
        if let (false, Some((at_ms, rate))) = (bid, self.ask_refill) {
            let refilled = U256::from(rate * now_ms.abs_diff(at_ms) / 1000);
            let room = if now_ms >= at_ms {
                side.max_in + refilled
            } else {
                side.max_in.saturating_sub(refilled)
            };
            if amount_in > room {
                return None;
            }
        }
        let held = self
            .held
            .filter(|held| self.guarded && now_ms < held.from_ms + held.for_ms)
            .map_or(0, |held| if bid { held.bid_ppm } else { held.ask_ppm });
        let discount = self
            .aging
            .discount(bid, self.maker_ms, now_ms) +
            held;
        let out = side.quote_discounted(amount_in, discount)?;
        if bid &&
            self.bid_out_cap
                .is_some_and(|cap| out > cap)
        {
            return None;
        }
        if !bid &&
            self.ask_out_cap
                .is_some_and(|cap| out > cap)
        {
            return None;
        }
        let quoted = if bid { out } else { amount_in };
        (!out.is_zero() && quoted >= self.min_fill_amount()).then_some(out)
    }

    /// What the front's tip-taking `quoteFrom` with `PLAN_ONLY` answers at `now_ms`: where the
    /// door fills `amount_in` past the side's custody prefix, the route's planned output tagged
    /// with bit 255; where it fills it inside custody, the door's own quote (unless the plan is
    /// set to disagree); and zero where the door refuses it.
    pub(super) fn plan_at(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        now_ms: u64,
    ) -> U256 {
        let Some(out) = self.quote_at(token_in, token_out, amount_in, now_ms) else {
            return U256::ZERO;
        };
        let bid = (token_in, token_out) == (self.base, self.quote);
        let side = self
            .side_at(token_in, token_out, now_ms)
            .unwrap();
        match side.credit_route {
            Some((prefix, _)) if amount_in > prefix => {
                side.planned(amount_in).unwrap() | (U256::from(1u8) << 255usize)
            }
            _ if bid &&
                self.bid_plan_lies
                    .is_some_and(|(least, most)| (least..=most).contains(&amount_in)) =>
            {
                out + U256::from(1u8)
            }
            _ => out,
        }
    }

    /// The side `token_in -> token_out` fills at `now_ms`, its credit route's prefix moved where
    /// [`Self::bid_prefix_from`] says; `None` for another pair.
    fn side_at(&self, token_in: Address, token_out: Address, now_ms: u64) -> Option<FakeSide> {
        let mut side = self.side(token_in, token_out)?.clone();
        let bid = (token_in, token_out) == (self.base, self.quote);
        if let (true, Some((from_ms, prefix)), Some(route)) =
            (bid, self.bid_prefix_from, side.credit_route.as_mut())
        {
            if now_ms >= from_ms {
                route.0 = prefix;
            }
        }
        Some(side)
    }

    /// Whether the front refuses the listing's print as stale at `now_ms`: past the print's end,
    /// and before a SESSION listing's closed phase.
    fn print_lapsed(&self, now_ms: u64) -> bool {
        let closed = self.fence == FENCE_SESSION &&
            !self.tied &&
            self.closed_from_ms
                .is_some_and(|closed| now_ms >= closed);
        self.print_ends_ms
            .is_some_and(|ends| now_ms > ends) &&
            !closed
    }

    /// The side `token_in -> token_out` fills, or `None` for another pair.
    fn side(&self, token_in: Address, token_out: Address) -> Option<&FakeSide> {
        if (token_in, token_out) == (self.base, self.quote) {
            Some(&self.bid)
        } else if (token_in, token_out) == (self.quote, self.base) {
            Some(&self.ask)
        } else {
            None
        }
    }
}

/// Where the factory puts listing `id`'s door: the CREATE2 address of the door code under the
/// factory, salted with the listing id.
pub(super) fn door_of(id: u32) -> Address {
    FACTORY.create2(B256::from(U256::from(id)), DOOR_CODEHASH)
}

/// A JSON-RPC node serving a front, its factory and its doors. Every call the reader makes is
/// allowed to fail, so a revert is a failed answer, and a call to an address with no code
/// succeeds with no data, as on chain.
#[derive(Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a test fixture: each flag sets one independent behaviour of the node"
)]
pub(super) struct FakeNode {
    pub listings: Vec<FakeListing>,
    pub block: u64,
    pub timestamp: u64,
    pub chain_id: u64,
    /// The implementation the front proxy's EIP-1967 slot holds at the latest block.
    pub implementation: Address,
    /// The implementation the slot holds at the block the listings were read at, which a
    /// load-balanced node may answer from another backend than the latest block's.
    pub pinned_implementation: Address,
    /// Whether the factory has code.
    pub factory_deployed: bool,
    /// What the factory answers for `FRONT`.
    pub factory_front: Address,
    /// What the front answers for `CELL_CODEHASH`.
    pub front_cell_codehash: B256,
    /// What the factory answers for `DOOR_CODEHASH`.
    pub factory_door_codehash: B256,
    /// Whether the factory's `doorOf` names an address other than the door's CREATE2 one.
    pub factory_door_of_lies: bool,
    /// Whether the front's `doorView` reverts, as after an upgrade that removed it.
    pub door_view_reverts: bool,
    /// What the node does with an `eth_call`'s block override.
    pub overrides: Overrides,
    /// The block numbers whose override the node refuses, whatever `overrides` says.
    pub refused_blocks: Vec<u64>,
    /// The front's clock, and what BaseTime answers.
    pub clock: FakeClock,
    /// How many `eth_call`s the node has answered.
    pub eth_calls: Arc<AtomicUsize>,
    /// How many times a call asked the lane for its `HOLD_MS`.
    pub hold_ms_reads: Arc<AtomicUsize>,
    /// Whether the node is one of a chain whose builders land makers' updates: see
    /// [`Self::ethereum`].
    pub lands_in_block: bool,
    /// On such a node, every account a call's state override has named.
    pub overridden: Arc<Mutex<BTreeSet<Address>>>,
    /// On such a node, each desk's desk word at the latest block, as `(epoch, stopped)`: a desk
    /// not listed is at epoch 0 and running. A stop and a revive raise the epoch.
    pub desk_heads: BTreeMap<u32, (u32, bool)>,
    /// Bits set in every desk word at the latest block beside its pusher, its epoch and its stop:
    /// bits no cover reads.
    pub desk_word_spare: U256,
    /// The hash the node names its latest block by; `None` for a node that names no block.
    pub head_hash: Option<B256>,
    /// The latest block's milliseconds its header names (`milliTimestamp`, BNB Chain's), where
    /// it names them.
    pub head_ms: Option<u64>,
    /// What BNB Chain's millisecond precompile at `0x70` answers, ms; `None` for no code.
    pub ms_precompile: Option<u64>,
    /// What the node does with a request naming its block by that hash.
    pub hash_pins: HashPins,
    /// The front's implementation as a request naming its block by that hash reads it, where
    /// that is another than the block named by its number holds.
    pub implementation_by_hash: Option<Address>,
    /// How many quotes the node has answered at a door, its most-fillable reads among them, or
    /// at the front for a door's plan.
    pub door_quotes: Arc<AtomicUsize>,
    /// How many of those it was asked at a block named by its number, not its hash.
    pub door_quotes_by_number: Arc<AtomicUsize>,
    /// The runtime code of every implementation the front's slot may name.
    pub implementation_code: Vec<u8>,
    /// How many times the node was asked an account's code.
    pub code_reads: Arc<AtomicUsize>,
}

/// The fake front's clock, as `PropAMMClock.read` reads it, and the chain's blocks: in its mode 2
/// the milliseconds counted 200 ms a block from an anchor, where they fall in the block's second,
/// else BaseTime's reading, where that does, else the start of the block's second.
///
/// BaseTime answers the block's second and the sub-second it stores, which the chain writes each
/// block: a block override moves the second but not what is stored, which a state override of
/// its slot 0 sets.
#[derive(Clone, Copy, Debug)]
pub(super) struct FakeClock {
    pub mode: u64,
    pub block0: u64,
    pub ms0: u64,
    /// The sub-second BaseTime stores at the node's block, ms; `None` where it has no code.
    pub base_time_sub_ms: Option<u64>,
    /// Whether the chain makes a block every 200 ms (else every second).
    pub fast_blocks: bool,
    /// Whether the chain writes each block's own sub-second to BaseTime, as Base does; else it
    /// stays as at the node's block, as on a local fork.
    pub base_time_per_block: bool,
}

impl FakeClock {
    /// A count that never lands in the block's second, and no BaseTime: the clock reads the start
    /// of each second, a block a second.
    pub(super) fn seal() -> Self {
        Self {
            mode: 2,
            block0: 0,
            ms0: 0,
            base_time_sub_ms: None,
            fast_blocks: false,
            base_time_per_block: false,
        }
    }

    /// Ethereum: the front reads the block's second (mode 0), a block every 12 s.
    pub(super) fn ethereum() -> Self {
        Self {
            mode: 0,
            block0: 0,
            ms0: 0,
            base_time_sub_ms: None,
            fast_blocks: false,
            base_time_per_block: false,
        }
    }

    /// Base: 200 ms a block counted from the node's block, `into_second_ms` into its second, which
    /// BaseTime reads as well.
    pub(super) fn counted(into_second_ms: u64) -> Self {
        Self {
            mode: 2,
            block0: NODE_BLOCK,
            ms0: NODE_TIMESTAMP * 1000 + into_second_ms,
            base_time_sub_ms: Some(into_second_ms),
            fast_blocks: true,
            base_time_per_block: true,
        }
    }

    /// A chain a block a second, as a local fork of Base runs: the count leaves the block's second,
    /// and the clock reads BaseTime, `sub_ms` into each second.
    pub(super) fn base_time(sub_ms: u64) -> Self {
        Self {
            mode: 2,
            block0: 0,
            ms0: 0,
            base_time_sub_ms: Some(sub_ms),
            fast_blocks: false,
            base_time_per_block: false,
        }
    }

    /// Base where the front does not count its blocks, in `mode` 1, or in mode 2 with a count that
    /// never lands in the block's second: a block every 200 ms, the node's `sub_ms` into its
    /// second, the clock reading BaseTime, which each block writes with its own sub-second.
    pub(super) fn base_time_blocks(mode: u64, sub_ms: u64) -> Self {
        Self {
            mode,
            block0: 0,
            ms0: 0,
            base_time_sub_ms: Some(sub_ms),
            fast_blocks: true,
            base_time_per_block: true,
        }
    }

    /// Base in mode 2 with the count off its anchor: a block every 200 ms, the node's `sub_ms`
    /// into its second, the count `behind_ms` behind each block's own milliseconds. The front
    /// reads the count in the blocks where it lands in the block's second, and BaseTime in the
    /// others.
    pub(super) fn behind_its_anchor(behind_ms: u64, sub_ms: u64) -> Self {
        Self {
            mode: 2,
            block0: NODE_BLOCK,
            ms0: NODE_TIMESTAMP * 1000 + sub_ms - behind_ms,
            base_time_sub_ms: Some(sub_ms),
            fast_blocks: true,
            base_time_per_block: true,
        }
    }

    /// The same with the count `ahead_ms` ahead of each block's own milliseconds, as where Base
    /// made blocks faster than 200 ms for a while after the anchor.
    pub(super) fn ahead_of_its_anchor(ahead_ms: u64, sub_ms: u64) -> Self {
        Self {
            mode: 2,
            block0: NODE_BLOCK,
            ms0: NODE_TIMESTAMP * 1000 + sub_ms + ahead_ms,
            base_time_sub_ms: Some(sub_ms),
            fast_blocks: true,
            base_time_per_block: true,
        }
    }

    /// The front's clock at block `number` of time `time`, ms, BaseTime storing the node's
    /// block's sub-second.
    pub(super) fn ms(&self, number: u64, time: u64) -> u64 {
        self.ms_with(number, time, self.base_time_sub_ms)
    }

    /// The front's clock at block `number` of time `time`, ms, BaseTime storing `base_time_sub`.
    fn ms_with(&self, number: u64, time: u64, base_time_sub: Option<u64>) -> u64 {
        if self.mode == 2 && number >= self.block0 {
            let ms = self.ms0 + (number - self.block0) * 200;
            if ms / 1000 == time {
                return ms;
            }
        }
        if let Some(ms) = base_time_sub
            .map(|sub| time * 1000 + sub)
            .filter(|ms| ms / 1000 == time)
        {
            return ms;
        }
        time * 1000
    }

    /// The front's clock at each block the chain makes from `block` at `timestamp` on, while it
    /// reads before `end_ms`.
    pub(super) fn blocks(&self, block: u64, timestamp: u64, end_ms: u64) -> Vec<u64> {
        let mut blocks = Vec::new();
        for k in 0.. {
            let mut base_time_sub = self.base_time_sub_ms;
            let (number, time) = if self.fast_blocks && self.base_time_per_block {
                // Each block 200 ms after the last, as BaseTime reads at the node's block.
                let ms = timestamp * 1000 + base_time_sub.unwrap_or_default() + k * 200;
                base_time_sub = base_time_sub.map(|_| ms % 1000);
                (block + k, ms / 1000)
            } else if self.fast_blocks {
                let ms = self.ms(block, timestamp) + k * 200;
                (block + k, ms / 1000)
            } else {
                (block + k, timestamp + k)
            };
            let ms = self.ms_with(number, time, base_time_sub);
            if ms >= end_ms {
                break;
            }
            blocks.push(ms);
        }
        blocks
    }
}

/// What a node does with a request naming its block by hash (EIP-1898).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HashPins {
    /// Answers it at the block of that hash, its latest.
    Taken,
    /// Answers it with an invalid-params error, as a node that reads a block by number or tag
    /// alone.
    Refused,
    /// Answers it that it knows no block of that hash, as a backend a block behind the one that
    /// named the head.
    Unknown,
}

/// What a node does with an `eth_call`'s block override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Overrides {
    /// Runs the call at the overriding block's number and time.
    Honoured,
    /// Answers the call with an error.
    Refused,
    /// Runs the call at the block it names, as if there were no override.
    Ignored,
    /// Runs the call at the overriding time, but at the block number it names.
    TimeOnly,
    /// Honours an override of an odd block number and refuses one of an even number, as a node
    /// that answers some overrides of a batch and not others.
    OddBlocksOnly,
}

impl FakeNode {
    pub(super) fn new(listings: Vec<FakeListing>) -> Self {
        Self {
            listings,
            block: NODE_BLOCK,
            timestamp: NODE_TIMESTAMP,
            chain_id: Chain::Base.id(),
            implementation: IMPLEMENTATION,
            pinned_implementation: IMPLEMENTATION,
            factory_deployed: true,
            factory_front: FRONT,
            front_cell_codehash: CELL_CODEHASH,
            factory_door_codehash: DOOR_CODEHASH,
            factory_door_of_lies: false,
            door_view_reverts: false,
            overrides: Overrides::Honoured,
            refused_blocks: Vec::new(),
            clock: FakeClock::seal(),
            eth_calls: Arc::new(AtomicUsize::new(0)),
            hold_ms_reads: Arc::new(AtomicUsize::new(0)),
            lands_in_block: false,
            overridden: Arc::default(),
            desk_heads: BTreeMap::new(),
            desk_word_spare: U256::ZERO,
            head_hash: Some(HEAD_HASH),
            head_ms: None,
            ms_precompile: None,
            hash_pins: HashPins::Taken,
            implementation_by_hash: None,
            door_quotes: Arc::default(),
            door_quotes_by_number: Arc::default(),
            implementation_code: IMPLEMENTATION_CODE.to_vec(),
            code_reads: Arc::default(),
        }
    }

    /// A node of Ethereum, whose builders land makers' updates in the block that fills them: its
    /// latest block is [`ETHEREUM_BLOCK`], a slot before [`ETHEREUM_NEXT_TIME`], and a door fills
    /// only where its maker's word landed in the call's block, or in the one before where the
    /// listing and the word carry (see [`FakeListing::landed`]). BaseTime has no code there, and
    /// reading it panics.
    pub(super) fn ethereum(listings: Vec<FakeListing>) -> Self {
        Self {
            block: ETHEREUM_BLOCK,
            timestamp: ETHEREUM_NEXT_TIME - SLOT_SECS,
            chain_id: Chain::Ethereum.id(),
            clock: FakeClock::ethereum(),
            lands_in_block: true,
            ..Self::new(listings)
        }
    }

    /// A node of BNB Chain: as [`Self::ethereum`], its latest block [`ETHEREUM_BLOCK`] made at
    /// [`ETHEREUM_NEXT_TIME`] and [`BNB_HEAD_SUB_MS`] (named in its header), a block every
    /// 450 ms, and the front in clock mode 3 with no code at the millisecond precompile, so it
    /// reads each block's second.
    pub(super) fn bnb(listings: Vec<FakeListing>) -> Self {
        Self {
            timestamp: ETHEREUM_NEXT_TIME,
            chain_id: Chain::Bsc.id(),
            clock: FakeClock { mode: 3, ..FakeClock::ethereum() },
            head_ms: Some(ETHEREUM_NEXT_TIME * 1000 + BNB_HEAD_SUB_MS),
            ..Self::ethereum(listings)
        }
    }

    /// The front's clock at each block the chain makes in a book's life: from the later of
    /// `wall_ms` and the read's block, `life_ms` long.
    pub(super) fn life(&self, wall_ms: u64, life_ms: u64) -> Vec<u64> {
        let from = wall_ms.max(
            self.clock
                .ms(self.block, self.timestamp),
        );
        self.clock
            .blocks(self.block, self.timestamp, from + life_ms)
    }

    pub(super) async fn spawn(self) -> MockHttpServer {
        spawn_http_server(move |request| Some(("200 OK", self.answer(request)))).await
    }

    /// A server answering as `node` is when each request arrives: a test changes the node
    /// between two polls of one source.
    pub(super) async fn spawn_shared(node: Arc<Mutex<Self>>) -> MockHttpServer {
        spawn_http_server(move |request| {
            let node = node.lock().unwrap().clone();
            Some(("200 OK", node.answer(request)))
        })
        .await
    }

    /// Answers one request or a batch of them, each by its method.
    fn answer(&self, request: &MockRequest) -> String {
        let request: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        let answer = match request.as_array() {
            Some(batch) => batch
                .iter()
                .map(|request| self.answer_one(request))
                .collect(),
            None => self.answer_one(&request),
        };
        answer.to_string()
    }

    fn answer_one(&self, request: &serde_json::Value) -> serde_json::Value {
        let by_hash = request["params"]
            .as_array()
            .is_some_and(|params| {
                params
                    .iter()
                    .any(|param| param.get("blockHash").is_some())
            });
        let refusal = match self.hash_pins {
            HashPins::Taken => None,
            HashPins::Refused => Some((-32602, "invalid argument: a block tag or number")),
            HashPins::Unknown => Some((-32000, "header for hash not found")),
        };
        if let Some((code, message)) = refusal.filter(|_| by_hash) {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": { "code": code, "message": message },
            });
        }
        match request["method"].as_str() {
            Some("eth_call") => {
                self.eth_calls
                    .fetch_add(1, Ordering::SeqCst);
                let before = self.door_quotes.load(Ordering::SeqCst);
                let answer = self.answer_call(request);
                if !by_hash {
                    self.door_quotes_by_number.fetch_add(
                        self.door_quotes.load(Ordering::SeqCst) - before,
                        Ordering::SeqCst,
                    );
                }
                answer
            }
            Some("eth_getStorageAt") => self.answer_storage(request),
            Some("eth_getBlockByNumber") => {
                // The block the listings were read at, named by its hash where the node names it.
                assert_eq!(request["params"][0], self.pinned(), "the header of the pinned block");
                assert_eq!(request["params"][1], false, "a header without its transactions");
                let header = self.head_hash.map(|hash| {
                    let mut header = serde_json::json!({
                        "hash": hash,
                        "number": self.pinned(),
                        "timestamp": format!("{:#x}", self.timestamp),
                    });
                    if let Some(ms) = self.head_ms {
                        header["milliTimestamp"] = serde_json::json!(format!("{ms:#x}"));
                    }
                    header
                });
                serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": header })
            }
            Some("eth_getCode") => {
                self.code_reads
                    .fetch_add(1, Ordering::SeqCst);
                let account: Address =
                    serde_json::from_value(request["params"][0].clone()).unwrap();
                assert!(
                    [self.implementation, self.pinned_implementation]
                        .into_iter()
                        .chain(self.implementation_by_hash)
                        .any(|implementation| implementation == account),
                    "only an implementation's code is read"
                );
                let block = &request["params"][1];
                assert!(
                    *block == "latest" || self.names(block),
                    "the code is read at a read's block"
                );
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": alloy::primitives::Bytes::from(self.implementation_code.clone()),
                })
            }
            method => panic!("the fake node does not serve {method:?}"),
        }
    }

    /// The front's implementation slot, read with the listings at the latest block and again at
    /// the block they were read at, and the listings' settings, controls, session and source
    /// words, read at that block.
    fn answer_storage(&self, request: &serde_json::Value) -> serde_json::Value {
        let params = &request["params"];
        let proxy: Address = serde_json::from_value(params[0].clone()).unwrap();
        let slot: U256 = serde_json::from_value(params[1].clone()).unwrap();
        if let Some(desk) = self
            .listings
            .iter()
            .map(|listing| listing.desk)
            .chain(self.desk_heads.keys().copied())
            .find(|desk| desk_cell(*desk) == proxy)
        {
            // A desk contract's desk word and its listings' pair words, read at the listings'
            // block: the word a listing landed on chain (see `FakeListing::landed`), and no firm
            // word.
            assert_eq!(params[2], self.pinned(), "a desk contract is read at the listings' block");
            let (epoch, stopped) = self.head(desk);
            let word = if slot == U256::from(1u8) {
                B256::from(
                    U256::from_be_bytes(desk_word_of(epoch, stopped).0) | self.desk_word_spare,
                )
            } else {
                self.listings
                    .iter()
                    .filter(|listing| listing.desk == desk)
                    .find(|listing| slot == U256::from(QUOTE_BASE + u64::from(listing.word_index)))
                    .and_then(|listing| listing.landed)
                    .map_or(B256::ZERO, |(stamp, carry)| quote_word(stamp, epoch, carry, false))
            };
            return serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": word });
        }
        assert_eq!(proxy, FRONT, "only the front's and the desk contracts' slots are read");
        let word = if slot == U256::from_be_bytes(IMPLEMENTATION_SLOT.0) {
            let implementation = if params[2] == "latest" {
                self.implementation
            } else {
                assert!(self.names(&params[2]), "the slot is read at the listings' block");
                self.implementation_by_hash
                    .filter(|_| params[2].is_object())
                    .unwrap_or(self.pinned_implementation)
            };
            implementation.into_word()
        } else {
            assert!(self.names(&params[2]), "a listing's record is read at its block");
            if self.listings.iter().any(|listing| {
                listing
                    .unread_words
                    .iter()
                    .any(|&word| slot == record_word_slot(listing.id, word))
            }) {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": { "code": -32000, "message": "missing trie node" },
                });
            }
            let word = self
                .listings
                .iter()
                .find_map(|listing| {
                    if slot == record_word_slot(listing.id, 1) {
                        Some(listing.settings())
                    } else if slot == record_word_slot(listing.id, 7) {
                        Some(listing.aging.controls())
                    } else if slot == record_word_slot(listing.id, 13) {
                        Some(U256::from(listing.closed_after_s))
                    } else if slot == record_word_slot(listing.id, 2) {
                        // The live source's copy: its lane (on a chain whose builders land
                        // makers' updates, the primary's id, whose price contract the front
                        // derives), its window (4 s, eight 500 ms steps) and its guard bit.
                        let guard =
                            if listing.guarded { U256::from(1u8) << 247 } else { U256::ZERO };
                        let source = if self.lands_in_block {
                            U256::from(listing.sources.0)
                        } else {
                            U256::from_be_slice(HOLD_LANE.as_slice())
                        };
                        Some(source | (U256::from(8u8) << 240) | guard)
                    } else if slot == record_word_slot(listing.id, 10) {
                        let (primary, backup) = listing.sources;
                        Some(U256::from(primary) | (U256::from(backup) << 32))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("the fake node holds no slot {slot:#x}"));
            B256::from(word)
        };
        serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": word })
    }

    /// The block the listings were read at, as a request names it by its number.
    fn pinned(&self) -> String {
        format!("0x{:x}", self.block)
    }

    /// Whether `block` names the block the listings were read at: by its number, or by the hash
    /// the node names it by, required canonical (EIP-1898).
    fn names(&self, block: &serde_json::Value) -> bool {
        *block == self.pinned().as_str() ||
            self.head_hash.is_some_and(|hash| {
                *block == serde_json::json!({ "blockHash": hash, "requireCanonical": true })
            })
    }

    /// What the node's latest block takes off every quote and plan of a door, in atoms: its
    /// hash's last byte against [`HEAD_HASH`]'s. A door's answer is read from the block it is
    /// asked at, so another block of the same number answers other amounts, whatever else it
    /// holds.
    fn head_dust(&self) -> U256 {
        self.head_hash
            .map_or(U256::ZERO, |hash| U256::from(hash[31] ^ HEAD_HASH[31]))
    }

    /// One `aggregate3`: the listings' read at the latest block, and every other pinned to the
    /// block the listings were read at. A door is never read at the latest block: the node's
    /// latest may have moved on since the listings were read.
    ///
    /// A call under a block override runs at the override's number and time, as the node's
    /// [`Overrides`] says.
    fn answer_call(&self, request: &serde_json::Value) -> serde_json::Value {
        let block = &request["params"][1];
        let latest = *block == "latest";
        assert!(latest || self.names(block), "read at block {block}");
        let mut at = (self.block, self.timestamp, self.clock.base_time_sub_ms);
        let overridden = request["params"]
            .get(2)
            .filter(|state| !state.is_null());
        let landings = if self.lands_in_block {
            if let Some(state) = overridden {
                self.overridden.lock().unwrap().extend(
                    state
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|account| account.parse::<Address>().unwrap()),
                );
            }
            overridden
                .map(desk_landings)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let state = overridden
            .filter(|_| !self.lands_in_block)
            .map(base_time_override);
        let block_overridden = request["params"]
            .get(3)
            .is_some_and(|block| !block.is_null());
        if let Some(block) = request["params"]
            .get(3)
            .filter(|block| !block.is_null())
        {
            assert!(!latest, "an override of the latest block");
            let field = |name: &str| {
                let hex = block[name].as_str().unwrap();
                u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap()
            };
            if self
                .refused_blocks
                .contains(&field("number"))
            {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": { "code": -32000, "message": "header not found" },
                });
            }
            match self.overrides {
                Overrides::Honoured => {
                    at = (field("number"), field("time"), state.or(at.2));
                    assert!(
                        at.0 >= self.block && at.1 >= self.timestamp,
                        "an override back in time"
                    );
                }
                Overrides::TimeOnly => {
                    at.1 = field("time");
                    at.2 = state.or(at.2);
                }
                Overrides::OddBlocksOnly if field("number") % 2 == 1 => {
                    at = (field("number"), field("time"), state.or(at.2));
                }
                Overrides::Refused | Overrides::OddBlocksOnly => {
                    return serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": { "code": -32602, "message": "too many arguments" },
                    });
                }
                Overrides::Ignored => {}
            }
        }
        let input = request["params"][0]["input"]
            .as_str()
            .or_else(|| request["params"][0]["data"].as_str())
            .unwrap();
        let input = alloy::primitives::hex::decode(input).unwrap();
        let calls = aggregate3Call::abi_decode(&input)
            .unwrap()
            .calls;
        assert!(calls.len() <= super::reader::CALLS_PER_MULTICALL);
        let answers: Vec<_> = calls
            .iter()
            .map(|call| {
                assert!(call.allowFailure, "every call may fail");
                assert!(
                    !latest ||
                        [MULTICALL3_ADDRESS, FRONT, FACTORY, BASE_TIME, MS_PRECOMPILE]
                            .contains(&call.target),
                    "{} read at the latest block",
                    call.target
                );
                assert!(
                    !self.lands_in_block || call.target != BASE_TIME,
                    "BaseTime read on Ethereum"
                );
                assert!(
                    !self.lands_in_block ||
                        !self.is_door(call.target) ||
                        (block_overridden && overridden.is_some()),
                    "a door asked on Ethereum without the next block's overrides"
                );
                let answer = self.call(call.target, &call.callData, at, &landings);
                Call3Result {
                    success: answer.is_some(),
                    returnData: answer.unwrap_or_default().into(),
                }
            })
            .collect();
        let encoded = aggregate3Call::abi_encode_returns(&answers);
        let result = format!("0x{}", alloy::primitives::hex::encode(encoded));
        serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result })
    }

    /// Where the fake factory puts listing `id`'s door: the CREATE2 address of the door code its
    /// `DOOR_CODEHASH` names.
    pub(super) fn door_address(&self, id: u32) -> Address {
        FACTORY.create2(B256::from(U256::from(id)), self.factory_door_codehash)
    }

    /// Whether `target` is one of the listings' doors.
    fn is_door(&self, target: Address) -> bool {
        self.listings
            .iter()
            .any(|listing| self.door_address(listing.id) == target)
    }

    /// On Ethereum, whether listing's word fills at block `number`, as the front reads it: the
    /// desk is not stopped at its head, the word is of the head's desk epoch (a word of another
    /// epoch is empty), and it landed at `number`, or in the block before where the listing and
    /// the word carry and its fence is not NONE. `landings` are the words the call's state
    /// override sets at the pair's quote slot, which replace the chain's.
    fn word_fresh(&self, listing: &FakeListing, number: u64, landings: &Landings) -> bool {
        let (epoch, stopped) = self.head(listing.desk);
        let slot = U256::from(QUOTE_BASE + u64::from(listing.word_index));
        let word = landings
            .get(&(desk_cell(listing.desk), slot))
            .copied()
            .or_else(|| {
                listing
                    .landed
                    .map(|(stamp, carry)| quote_word(stamp, epoch, carry, false))
            });
        let Some(word) = word else {
            return false;
        };
        let word = U256::from_be_bytes(word.0);
        let stamp: u64 = (word & U256::from(u32::MAX)).to();
        let word_epoch: u32 = ((word >> 32usize) & U256::from(0x7fff_ffffu32)).to();
        !stopped &&
            word_epoch == epoch &&
            (number == stamp ||
                (number == stamp + 1 &&
                    listing.carry &&
                    word.bit(201) &&
                    listing.fence != FENCE_NONE))
    }

    /// A desk's desk word at the latest block: its epoch and whether it is stopped.
    fn head(&self, desk: u32) -> (u32, bool) {
        self.desk_heads
            .get(&desk)
            .copied()
            .unwrap_or((0, false))
    }

    fn listing(&self, id: U24) -> Option<&FakeListing> {
        self.listings
            .iter()
            .find(|listing| U24::from(listing.id) == id)
    }

    /// The answer to one call at block `at` (number, time, and the sub-second BaseTime stores), or
    /// `None` where the chain reverts.
    fn call(
        &self,
        target: Address,
        data: &[u8],
        at: (u64, u64, Option<u64>),
        landings: &Landings,
    ) -> Option<Vec<u8>> {
        if target == MS_PRECOMPILE {
            return Some(
                self.ms_precompile
                    .map(|ms| U256::from(ms).abi_encode())
                    .unwrap_or_default(),
            );
        }
        let selector = <[u8; 4]>::try_from(&data[..4]).unwrap();
        if target == MULTICALL3_ADDRESS {
            return Some(match selector {
                getBlockNumberCall::SELECTOR => U256::from(at.0).abi_encode(),
                getCurrentBlockTimestampCall::SELECTOR => U256::from(at.1).abi_encode(),
                getChainIdCall::SELECTOR => U256::from(self.chain_id).abi_encode(),
                _ => panic!("the fake node does not answer {selector:?} on Multicall3"),
            });
        }
        if target == BASE_TIME {
            assert_eq!(selector, TIMESTAMP_MS_SELECTOR, "only timestampMs is read on BaseTime");
            return Some(
                at.2.map(|sub| U256::from(at.1 * 1000 + sub).abi_encode())
                    .unwrap_or_default(),
            );
        }
        if target == HOLD_LANE {
            assert_eq!(selector, HOLD_MSCall::SELECTOR, "only HOLD_MS is read on the lane");
            self.hold_ms_reads
                .fetch_add(1, Ordering::SeqCst);
            let hold_ms = self
                .listings
                .iter()
                .find_map(|listing| listing.lane_hold_ms)?;
            return Some(U256::from(hold_ms).abi_encode());
        }
        if target == FACTORY {
            if !self.factory_deployed {
                return Some(Vec::new());
            }
            return Some(match selector {
                FRONTCall::SELECTOR => self.factory_front.abi_encode(),
                DOOR_CODEHASHCall::SELECTOR => self.factory_door_codehash.abi_encode(),
                doorOfCall::SELECTOR => {
                    let id = doorOfCall::abi_decode(data)
                        .unwrap()
                        .listing;
                    if self.factory_door_of_lies {
                        return Some(Address::repeat_byte(0xee).abi_encode());
                    }
                    self.door_address(id.to()).abi_encode()
                }
                _ => panic!("the fake node does not answer {selector:?} on the factory"),
            });
        }
        if target == FRONT {
            if selector == clockCall::SELECTOR {
                let clock = Timing {
                    maxLeadMs: U256::from(1_000u64),
                    maxTtlMs: U256::from(MAX_TTL_MS),
                    clockMode: U256::from(self.clock.mode),
                    clockBlock: U256::from(self.clock.block0),
                    clockMs: U256::from(self.clock.ms0),
                    ..Timing::default()
                };
                return Some(clockCall::abi_encode_returns(&clock));
            }
            if selector == CELL_CODEHASHCall::SELECTOR {
                return Some(self.front_cell_codehash.abi_encode());
            }
            if selector == quoteFromCall::SELECTOR {
                self.door_quotes
                    .fetch_add(1, Ordering::SeqCst);
                let call = quoteFromCall::abi_decode(data).unwrap();
                assert_eq!(call.tip, U256::from(1u8) << 253usize, "the plan is asked at no tip");
                // The front refuses the plan of a listing it does not hold.
                let listing = self.listing(call.listing)?;
                assert_eq!(call.taker, self.door_address(listing.id), "the door is the taker");
                assert_ne!(listing.credit_mode, 0, "the plan of a credit listing alone is asked");
                if self.lands_in_block && !self.word_fresh(listing, at.0, landings) {
                    return Some(U256::ZERO.abi_encode());
                }
                let ms = self.clock.ms_with(at.0, at.1, at.2);
                return Some(
                    listing
                        .plan_at(call.tokenIn, call.tokenOut, call.amountIn, ms)
                        .saturating_sub(self.head_dust())
                        .abi_encode(),
                );
            }
            if selector == deskCellOfCall::SELECTOR {
                let desk = deskCellOfCall::abi_decode(data)
                    .unwrap()
                    .desk;
                return Some(desk_cell(desk).abi_encode());
            }
            assert_eq!(
                selector,
                doorViewCall::SELECTOR,
                "only doorView, clock, deskCellOf, CELL_CODEHASH and quoteFrom are read"
            );
            if self.door_view_reverts {
                return None;
            }
            let id = doorViewCall::abi_decode(data)
                .unwrap()
                .listing;
            let view = self
                .listing(id)
                .map_or_else(DoorView::default, FakeListing::door_view);
            return Some(doorViewCall::abi_encode_returns(&view));
        }
        if selector == balanceOfCall::SELECTOR {
            // A token asked what a listing's pool holds of it: some, unless the listing's pool
            // is set to hold none.
            let pool = balanceOfCall::abi_decode(data)
                .unwrap()
                .account;
            let lacks = self.listings.iter().any(|listing| {
                listing.pool == pool &&
                    ((listing.base == target && listing.pool_lacks.0) ||
                        (listing.quote == target && listing.pool_lacks.1))
            });
            return Some(U256::from(u8::from(!lacks)).abi_encode());
        }
        let Some(listing) = self
            .listings
            .iter()
            .find(|listing| self.door_address(listing.id) == target)
        else {
            panic!("the fake node does not answer {selector:?} on {target}");
        };
        if !listing.opened {
            return Some(Vec::new());
        }
        Some(match selector {
            FRONTCall::SELECTOR => listing.door_front.abi_encode(),
            LISTINGCall::SELECTOR => U256::from(listing.door_listing).abi_encode(),
            BASECall::SELECTOR => listing.base.abi_encode(),
            quoteCall::SELECTOR => {
                self.door_quotes
                    .fetch_add(1, Ordering::SeqCst);
                let call = quoteCall::abi_decode(data).unwrap();
                if self.lands_in_block && !self.word_fresh(listing, at.0, landings) {
                    return None;
                }
                listing
                    .quote_at(
                        call.tokenIn,
                        call.tokenOut,
                        call.amountIn,
                        self.clock.ms_with(at.0, at.1, at.2),
                    )?
                    .saturating_sub(self.head_dust())
                    .abi_encode()
            }
            quoteFillableCall::SELECTOR => {
                self.door_quotes
                    .fetch_add(1, Ordering::SeqCst);
                let call = quoteFillableCall::abi_decode(data).unwrap();
                listing
                    .side(call.tokenIn, call.tokenOut)?
                    .fillable(call.amountIn)?
                    .abi_encode_params()
            }
            _ => panic!("the fake node does not answer {selector:?} on a door"),
        })
    }
}

/// The desk contract of desk id `desk`, as the fake front's `deskCellOf` answers it: the front's
/// CREATE2 child at salt `(1 << 252) | desk` under the desk contracts' creation code hash.
pub(super) fn desk_cell(desk: u32) -> Address {
    FRONT.create2(B256::from((U256::from(1u8) << 252usize) | U256::from(desk)), CELL_CODEHASH)
}

/// The desk contracts of the fake front's first 64 desk ids, which the Ethereum tests approve.
pub(super) fn approved_desks() -> Vec<Bytes> {
    (1..=64)
        .map(|desk| bytes(desk_cell(desk)))
        .collect()
}

/// The words an Ethereum `eth_call`'s state override sets, per desk contract and slot.
type Landings = HashMap<(Address, U256), B256>;

fn desk_landings(state: &Value) -> Landings {
    state
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(account, account_state)| {
            let account: Address = account.parse().unwrap();
            account_state["stateDiff"]
                .as_object()
                .into_iter()
                .flatten()
                .map(move |(slot, word)| {
                    ((account, slot.parse().unwrap()), word.as_str().unwrap().parse().unwrap())
                })
        })
        .collect()
}

/// A `/state` answer's result for block `number` (hex) and slot `slot`, with one entry per
/// `(desk, carry)` of `desks`: pair 0's quote word of the desk contract written for a landing in
/// `number` under desk epoch 0, the word carrying where its flag says. Nothing else is written, as
/// a relay entry writes nothing else.
pub(super) fn state_map(number: u64, slot: u64, desks: &[(u32, bool)]) -> Value {
    let entries: Vec<_> = desks
        .iter()
        .map(|&(desk, carry)| {
            (format!("desk-{desk}"), desk, 0u8, quote_word(number, 0, carry, false))
        })
        .collect();
    state_map_of(number, slot, &entries)
}

/// A `/state` answer's result for block `number` and slot `slot`, an entry per `(key, desk, pair
/// slot, quote word)`, each writing that word at `0x100 + pair slot` of the desk's contract.
pub(super) fn state_map_of(number: u64, slot: u64, entries: &[(String, u32, u8, B256)]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("blockNumber".into(), Value::from(format!("{number:#x}")));
    map.insert("slot".into(), Value::from(slot));
    for (key, desk, pair, word) in entries {
        let slot = B256::from(U256::from(QUOTE_BASE + u64::from(*pair)));
        map.insert(
            key.clone(),
            serde_json::json!({ "stateOverride": { desk_cell(*desk).to_string(): {
                "stateDiff": { slot.to_string(): word.to_string() },
            }}}),
        );
    }
    Value::Object(map)
}

/// The price contract of source `id` on the fake front: the CREATE3 child of the front at salt
/// `bytes32(id)`, as a front on a chain whose builders land makers' updates derives it.
pub(super) fn price_contract(id: u32) -> Address {
    super::reader::price_contract(FRONT, id)
}

/// `map` with `account`'s slot 0 written by its entry `key` too, as an update that posts to a
/// price contract writes it.
pub(super) fn with_account(mut map: Value, key: &str, account: Address) -> Value {
    map[key]["stateOverride"][account.to_string()] = serde_json::json!({
        "stateDiff": { B256::ZERO.to_string(): B256::repeat_byte(0x5a).to_string() },
    });
    map
}

/// A `/state` endpoint answering `titan_getPammStateOverrides` with `map`, and nothing else.
pub(super) async fn spawn_state_server(map: Value) -> MockHttpServer {
    spawn_http_server(move |request| {
        let request: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(request["method"], STATE_METHOD);
        let answer = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": map });
        Some(("200 OK", answer.to_string()))
    })
    .await
}

/// A `/state` endpoint answering with `map` as it is when each request arrives: a test changes
/// the map between two polls of one source.
pub(super) async fn spawn_shared_state_server(map: Arc<Mutex<Value>>) -> MockHttpServer {
    spawn_http_server(move |request| {
        let request: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(request["method"], STATE_METHOD);
        let map = map.lock().unwrap().clone();
        let answer = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": map });
        Some(("200 OK", answer.to_string()))
    })
    .await
}

/// The sub-second an `eth_call`'s state override stores in BaseTime: the only state the reader
/// overrides, BaseTime's slot 0.
fn base_time_override(state: &Value) -> u64 {
    let state = state.as_object().unwrap();
    assert_eq!(state.len(), 1, "only BaseTime's state is overridden");
    let (account, account_state) = state.iter().next().unwrap();
    assert_eq!(account.parse::<Address>().unwrap(), BASE_TIME, "only BaseTime is overridden");
    let diff = account_state["stateDiff"]
        .as_object()
        .unwrap();
    assert_eq!(diff.len(), 1, "only BaseTime's slot 0 is overridden");
    let (slot, value) = diff.iter().next().unwrap();
    assert_eq!(slot.parse::<U256>().unwrap(), U256::ZERO, "only BaseTime's slot 0 is overridden");
    let sub: u64 = value
        .as_str()
        .unwrap()
        .parse::<U256>()
        .unwrap()
        .to();
    assert!(
        sub < 1000 && sub.is_multiple_of(200),
        "BaseTime stores whole 200 ms of a second, not {sub}"
    );
    sub
}

/// One JSON-RPC call a node answered: its method and parameters, and its result or its error.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Exchange {
    pub method: String,
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// A door's own `quote` at one size, read at each block of a book's life.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct DoorCheck {
    pub door: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount: U256,
    /// Whether the size is one of the book's samples, where the book quotes the least of `quotes`
    /// exactly; between two samples it quotes no more than it.
    pub sample: bool,
    /// At the read's block and under an override at each later block of the life; `None` where
    /// the door refused.
    pub quotes: Vec<Option<U256>>,
}

/// One poll recorded from a node: the deployment read, the wall clock and the listing ids
/// scanned, every call the poll made with its answer, how many books it published, and the
/// doors' own quotes over the books' life at each size checked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RecordedPoll {
    pub front: Address,
    pub factory: Address,
    pub implementations: Vec<Address>,
    /// The price sources the poll approved.
    pub price_sources: Vec<u32>,
    pub now_ms: u64,
    pub scan_to: u32,
    pub books: usize,
    pub rpc: Vec<Exchange>,
    pub checks: Vec<DoorCheck>,
}

/// One listing as an Ethereum poll's recording holds it: where the front put it, and what the poll
/// made of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RecordedListing {
    pub listing: u32,
    pub door: Address,
    pub desk: u32,
    pub credit_mode: u8,
    pub quote_contract: Address,
    /// Whether one answer of the maps writes everything its fill reads in the next block.
    pub written: bool,
    /// Why the poll serves no such listing whatever the maps write, where it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<String>,
    pub served: bool,
}

/// One Ethereum poll recorded from a node and its state URLs: the deployment and the book
/// configuration read with, the wall clock, every call the poll made of the node with its answer,
/// the map each state URL answered, in the configured order, and what the poll published.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RecordedEthereumPoll {
    pub front: Address,
    pub factory: Address,
    pub cell_codehash: B256,
    pub implementations: Vec<Address>,
    /// The price sources the poll approved.
    pub price_sources: Vec<u32>,
    /// The tokens the book configuration named, each with its decimals.
    pub tokens: Vec<(Address, u32)>,
    pub now_ms: u64,
    pub head: u64,
    pub head_time: u64,
    pub rpc: Vec<Exchange>,
    pub maps: Vec<Value>,
    pub listings: Vec<RecordedListing>,
    pub books: usize,
    /// The credit listings the poll named, each with the gas it is charged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credit: Vec<(u32, CreditGas)>,
    /// Every side the poll served, sample for sample.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sides: Vec<RecordedSide>,
}

/// One side of a book a recorded poll served: its door, its direction, and each sample as the
/// book quotes it, `(input, output, gas charged)`, each output the door's own answer in the block
/// the map is for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RecordedSide {
    pub door: Address,
    pub sell_base: bool,
    pub points: Vec<(U256, U256, u64)>,
}

impl RecordedEthereumPoll {
    pub(super) fn deployment(&self) -> EfficientPropDeployment {
        EfficientPropDeployment {
            quote_layout: Some(super::QUOTE_LAYOUT_BUILDER.to_string()),
            front: bytes(self.front),
            factory: bytes(self.factory),
            cell_codehash: Some(Bytes::from(self.cell_codehash.to_vec())),
            implementations: self
                .implementations
                .iter()
                .map(|&implementation| bytes(implementation))
                .collect(),
        }
    }
}

/// The result a plain-HTTP `/state` endpoint at `url` answers `titan_getPammStateOverrides` with,
/// as it stands, for a recording.
pub(super) fn read_state_map(url: &str) -> Value {
    let body =
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": STATE_METHOD, "params": [] });
    let answer: Value = serde_json::from_str(&forward(url, &body.to_string())).unwrap();
    answer["result"].clone()
}

/// Why a poll that names no credit listing serves no such listing, whatever its maker's update
/// writes, if it does not: one with a credit mode, one reading a print (fence kind PRINT or
/// SESSION, the settings word's bits 228-229) from a primary or a backup source (the sources
/// word's bits 0-31 and 32-63) not among `approved`, or a tied listing (the settings word's bits
/// 228-231 equal to 8). `None` where the poll must serve it once one answer writes what its fill
/// reads and both its tokens are configured.
pub(super) fn left_out_of_a_poll(
    credit_mode: u8,
    settings: U256,
    sources: U256,
    approved: &[u32],
) -> Option<&'static str> {
    let fence = ((settings >> 228usize) & U256::from(3u8)).to::<u64>();
    let source = |shift: usize| ((sources >> shift) & U256::from(u32::MAX)).to::<u32>();
    let (primary, backup) = (source(0), source(32));
    let prints = fence == FENCE_PRINT || fence == FENCE_SESSION;
    if credit_mode != 0 {
        Some("it has a credit mode and the poll names no credit listing")
    } else if prints &&
        !(approved.contains(&primary) && (backup == 0 || approved.contains(&backup)))
    {
        Some("it reads a print from a source not approved")
    } else if prints && (settings >> 228usize) & U256::from(15u8) == U256::from(8u8) {
        Some("it is tied")
    } else {
        None
    }
}

/// The key a call is recorded under: its method and its parameters.
fn call_key(method: &Value, params: &Value) -> String {
    serde_json::json!([method, params]).to_string()
}

/// A node that answers each call with the answer recorded for its method and parameters, and an
/// error for a call never recorded, which it counts.
///
/// The recordings were made before a poll read its pinned block's header, so none holds an
/// `eth_getBlockByNumber`: the node answers it as a node that names no block, with no header,
/// unless a test gives it a hash to name every block by (see [`Self::naming_blocks`]). Either way
/// that read is not counted as unrecorded. A request naming its block by that hash (EIP-1898) is
/// answered as the recorded request naming it by the number the header was last asked of: the
/// same request of the same block, as a node holding one block of that number answers it.
pub(super) struct RecordedNode {
    answers: HashMap<String, Exchange>,
    unrecorded: Arc<AtomicUsize>,
    head_hash: Option<B256>,
    /// The number the header was last asked of, which the hash then names.
    named: Mutex<Option<Value>>,
    door_quotes: Arc<AtomicUsize>,
    by_hash: Arc<AtomicUsize>,
    /// The code the node answers for any account, where a test gives one: no recording holds a
    /// read of code.
    code: Option<alloy::primitives::Bytes>,
}

impl RecordedNode {
    pub(super) fn new(exchanges: &[Exchange]) -> Self {
        let answers = exchanges
            .iter()
            .map(|exchange| {
                (
                    call_key(&Value::from(exchange.method.clone()), &exchange.params),
                    exchange.clone(),
                )
            })
            .collect();
        Self {
            answers,
            unrecorded: Arc::default(),
            head_hash: None,
            named: Mutex::default(),
            door_quotes: Arc::default(),
            by_hash: Arc::default(),
            code: None,
        }
    }

    /// The node answering every read of an account's code with `code`.
    pub(super) fn with_code(mut self, code: &[u8]) -> Self {
        self.code = Some(alloy::primitives::Bytes::copy_from_slice(code));
        self
    }

    /// How many requests named their block by the node's hash.
    pub(super) fn by_hash(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.by_hash)
    }

    /// `params` with a block named by the node's hash named by its number instead.
    fn by_number(&self, params: &Value) -> Value {
        let (Some(hash), Some(number), Some(params)) =
            (self.head_hash, self.named.lock().unwrap().clone(), params.as_array())
        else {
            return params.clone();
        };
        let named = serde_json::json!({ "blockHash": hash, "requireCanonical": true });
        params
            .iter()
            .map(|param| {
                if *param == named {
                    self.by_hash
                        .fetch_add(1, Ordering::SeqCst);
                    number.clone()
                } else {
                    param.clone()
                }
            })
            .collect()
    }

    /// The node naming every block it is asked the header of by `hash`: a hash made by the test,
    /// since no recording holds one.
    pub(super) fn naming_blocks(mut self, hash: B256) -> Self {
        self.head_hash = Some(hash);
        self
    }

    /// How many calls the node was asked that were never recorded.
    pub(super) fn unrecorded(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.unrecorded)
    }

    /// How many quotes the node was asked at a door, its most-fillable reads among them, or at
    /// the front for a door's plan: the calls of each `aggregate3` it was sent, by selector.
    pub(super) fn door_quotes(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.door_quotes)
    }

    fn count_door_quotes(&self, request: &Value) {
        if request["method"] != "eth_call" {
            return;
        }
        let Some(calls) = request["params"][0]["input"]
            .as_str()
            .and_then(|input| alloy::primitives::hex::decode(input).ok())
            .and_then(|input| aggregate3Call::abi_decode(&input).ok())
        else {
            return;
        };
        let quotes = calls
            .calls
            .iter()
            .filter(|call| {
                call.callData
                    .get(..4)
                    .is_some_and(|selector| {
                        selector == quoteCall::SELECTOR ||
                            selector == quoteFillableCall::SELECTOR ||
                            selector == quoteFromCall::SELECTOR
                    })
            })
            .count();
        self.door_quotes
            .fetch_add(quotes, Ordering::SeqCst);
    }

    pub(super) async fn spawn(self) -> MockHttpServer {
        spawn_http_server(move |request| Some(("200 OK", self.answer(&request.body)))).await
    }

    fn answer(&self, body: &str) -> String {
        let request: Value = serde_json::from_str(body).unwrap();
        let answer = |request: &Value| {
            let id = request["id"].clone();
            self.count_door_quotes(request);
            if let Some(code) = self
                .code
                .as_ref()
                .filter(|_| request["method"] == "eth_getCode")
            {
                return serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": code });
            }
            if request["method"] == "eth_getBlockByNumber" {
                let header = self.head_hash.map(
                    |hash| serde_json::json!({ "hash": hash, "number": request["params"][0] }),
                );
                *self.named.lock().unwrap() = Some(request["params"][0].clone());
                return serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": header });
            }
            match self
                .answers
                .get(&call_key(&request["method"], &self.by_number(&request["params"])))
            {
                Some(Exchange { error: Some(error), .. }) => {
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": error })
                }
                Some(Exchange { result, .. }) => {
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
                }
                None => {
                    self.unrecorded
                        .fetch_add(1, Ordering::SeqCst);
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32000, "message": format!("not recorded: {}", request["method"]) },
                    })
                }
            }
        };
        match request.as_array() {
            Some(batch) => Value::from(
                batch
                    .iter()
                    .map(answer)
                    .collect::<Vec<_>>(),
            ),
            None => answer(&request),
        }
        .to_string()
    }
}

/// A proxy to the plain-HTTP node at `upstream` that records every call it forwards.
pub(super) async fn spawn_recording_proxy(
    upstream: String,
) -> (MockHttpServer, Arc<Mutex<Vec<Exchange>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&log);
    let server = spawn_http_server(move |request| {
        let answer = forward(&upstream, &request.body);
        let asked: Value = serde_json::from_str(&request.body).unwrap();
        let answered: Value = serde_json::from_str(&answer).unwrap();
        let (asked, answered) = match (asked, answered) {
            (Value::Array(asked), Value::Array(answered)) => (asked, answered),
            (asked, answered) => (vec![asked], vec![answered]),
        };
        let mut log = kept.lock().unwrap();
        for call in &asked {
            let answer = answered
                .iter()
                .find(|answer| answer["id"] == call["id"])
                .expect("the node answers every call");
            log.push(Exchange {
                method: call["method"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                params: call["params"].clone(),
                result: answer.get("result").cloned(),
                error: answer.get("error").cloned(),
            });
        }
        Some(("200 OK", answer))
    })
    .await;
    (server, log)
}

/// POSTs `body` to the plain-HTTP JSON-RPC node at `url`, at its path where it has one (a `/state`
/// endpoint's route), and returns its answer's body.
fn forward(url: &str, body: &str) -> String {
    let rest = url.trim_start_matches("http://");
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let mut stream = TcpStream::connect(host).unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: \
         {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    let answer = String::from_utf8(answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    if !head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return body.to_string();
    }
    let (mut rest, mut whole) = (body, String::new());
    loop {
        let (size, after) = rest.split_once("\r\n").unwrap();
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        if size == 0 {
            return whole;
        }
        whole.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
}
