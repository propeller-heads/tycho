//! The sampler's own bytes on revm, against `testdata/MaxOfTwoPool.evm.runtime`: one runtime that
//! serves an Efficient pool's `previewSwap` and `pairRequest` and its hook's `quotePair`,
//! `levState` and `params()`, over two concave routes, with each fault and route switch in a
//! storage slot (`testdata/MaxOfTwoPool.sol` lists them). As a node runs it, the sampler's code
//! sits at the taker's address and is called from no sender, and every call it makes is traced.
//!
//! The ladder tests walk a pool state the rule guards against at the feed's settings, settle the
//! walk, and compare each quote on a dense grid with the pool's own `previewSwap` at that size: the
//! rule never quotes more than the pool pays, and each control, the rule with one of its steps left
//! out or a word it reads forced, goes over where that step is what keeps the state safe.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use alloy::{
    primitives::{
        address, b256, keccak256, ruint::UintTryFrom, Address, Bytes, FixedBytes, B256, U256, U512,
    },
    sol_types::SolCall,
};
use revm::{
    context::{
        result::{ExecutionResult, Output},
        BlockEnv, CfgEnv, Context, TxEnv,
    },
    database::{CacheDB, EmptyDB},
    primitives::{hardfork::SpecId, TxKind},
    state::{AccountInfo, Bytecode},
    ExecuteEvm, InspectEvm, MainBuilder, MainContext,
};
use revm_inspectors::tracing::{types::CallKind, TracingInspector, TracingInspectorConfig};
use rstest::rstest;
use tycho_common::models::Chain;

use super::{
    feed::{
        DEFAULT_PROBE_GAS, DEFAULT_RUNG_COUNT, DEFAULT_SAMPLE_GAS, DEFAULT_SMALLEST_RUNG_USD,
        MAX_PROBES_PER_DIRECTION, MAX_PROBE_GAS, SAMPLE_GAS_FLOOR,
    },
    ladder::{
        rung_inputs, settle, settle_with, AboveTest, Ladder, Probe, ProbeStatus, Request, RoomTest,
        Row, Rule, Segment, Side, Terms, Trust, ABOVE_SHIFT, ANSWER_BITS, DUST_OUT,
    },
    reader::{
        pairRequestCall, previewSwapCall, sampler_limits, smallest_rung, FlammSampler,
        LEV_STATE_SELECTOR, QUOTE_PAIR_SELECTOR, SAMPLER_RESERVE_GAS, SAMPLER_RUNTIME, SAMPLER_TAG,
    },
    router,
};

const POOL_RUNTIME: &[u8] = include_bytes!("testdata/MaxOfTwoPool.evm.runtime");
const SAMPLER_SOURCE: &[u8] = include_bytes!("assets/FlammSampler.sol");
const POOL_SOURCE: &[u8] = include_bytes!("testdata/MaxOfTwoPool.sol");
/// Pinned in every client of the sampler, so that all of them run the same bytes, built from the
/// same source, and test them against the same pool.
const SAMPLER_RUNTIME_KECCAK: B256 =
    b256!("def8a5c6daa5ac664aff33e93625b8ef24d0b23fd041eb57193d3da960212ac5");
const SAMPLER_SOURCE_KECCAK: B256 =
    b256!("cf941e0e26687c978ca84ff807257393e5008bb1ce0e08bf9f0a5b6543e6e5fd");
const POOL_RUNTIME_KECCAK: B256 =
    b256!("e9b47028abbbbde6b37871b0bc7383dcd45447c40f072dbd271019abbb678941");
const POOL_SOURCE_KECCAK: B256 =
    b256!("ed86138a409c1fa6cd6ddadc3f6e526b3709b563ffb8fdcffcd99c1657175d3c");
/// Opcodes a build for paris never holds.
const PUSH0: u8 = 0x5f;
const MCOPY: u8 = 0x5e;

/// The fake pool, which is also its own hook.
const POOL: Address = address!("00000000000000000000000000000000000f0010");
const TOKEN_0: Address = address!("00000000000000000000000000000000000000c0");
const TOKEN_1: Address = address!("00000000000000000000000000000000000000c1");
/// An address that is not the taker, to run the sampler's code at.
const ELSEWHERE: Address = address!("000000000000000000000000000000000000dead");
const BLOCK: u64 = 100;
const SIDES: [Side; 2] = [Side::Sale, Side::Buy];

// The fake pool's slots.
const SLOT_TOKEN_0: u64 = 0;
const SLOT_TOKEN_1: u64 = 1;
const FAULTS: u64 = 2;
const SLOT_TAKER: u64 = 3;
/// The gas a burner leaves before it answers; zero: it runs out.
const BURN_LEAVES: u64 = 4;
const BOMB_BYTES: u64 = 5;
const REVERT_REASON: u64 = 6;
/// The live band, request word 10.
const LIVE_BAND: u64 = 7;
const USD_0: u64 = 8;
const USD_1: u64 = 9;
/// Token 1 units per token 0 unit, scaled by `1e36`, for the band's lines.
const P36: u64 = 10;
const CURVE_FEE: u64 = 11;
/// `levState` word 6, the lever-up's top bid.
const BID: u64 = 12;
/// `levState` word 0.
const CV: u64 = 13;
/// Token 0's legs, held, supplied, posted and owed, from here; token 1's from [`LEGS_1`].
const LEGS_0: u64 = 16;
const LEGS_1: u64 = 20;
/// A direction's routes and switches, at its base plus the offsets below.
const SALE: u64 = 0x40;
const BUY: u64 = 0x50;
const CURVE_R: u64 = 0;
const CURVE_X: u64 = 1;
const LEVER_R: u64 = 2;
const LEVER_X: u64 = 3;
/// Leverage refuses above this size.
const EDGE: u64 = 4;
/// Leverage refuses where `size % HOLE_EVERY == HOLE_AT`.
const HOLE_EVERY: u64 = 5;
const HOLE_AT: u64 = 6;
/// Leverage refuses where its output is at or under the band's far line, or above it by less than
/// a jitter drawn in `[0, JITTER)`.
const JITTER: u64 = 7;
/// Leverage refuses on `[FLICKER_AT, FLICKER_AT + FLICKER_LEN)`.
const FLICKER_AT: u64 = 8;
const FLICKER_LEN: u64 = 9;
/// Leverage pays `FALL_BY` less above size `FALL_AT`.
const FALL_AT: u64 = 10;
const FALL_BY: u64 = 11;
/// Leverage refuses a net over this, which is also request word 14.
const CEILING: u64 = 12;
/// The curve uses at most this input.
const CLIP: u64 = 13;
/// `params()` word `k` is slot `PARAMS + k`.
const PARAMS: u64 = 0x100;
const P_SPREAD: u64 = PARAMS + 27;
const P_SPREAD_DN: u64 = PARAMS + 28;
const P_UP_CAP: u64 = PARAMS + 29;
const P_DN_CAP: u64 = PARAMS + 30;
const P_BAND: u64 = PARAMS + 32;

const WAD: u128 = 1_000_000_000_000_000_000;
/// Where the floor flicker and the token ceiling sit on a sale.
const SWITCH_AT: u64 = 10_000_000;
/// Where the lever-down's output falls on a buy.
const JOIN_AT: u64 = 50_000_000_000;
/// The hole rows' lattice: leverage refuses every size 3 mod 7.
const LATTICE: u64 = 7;
const LATTICE_AT: u64 = 3;
/// A bomb's size.
const MIB: u64 = 1 << 20;
/// What a burner leaves in the case that prices.
const BURNER_LEAVES: u64 = 680_000;
/// The call gas the gas-gate tests cut the walks at: a few probes each past the gate.
const PART_GAS: u64 = 5_600_000;
/// What timing a call adds to its callee's gas: a cold account's access, 2,600, and the 11 gas of
/// the opcodes the sampler times around the call.
const CALL_OVERHEAD: u64 = 2_611;
/// The same on a warm account, whose access costs 100.
const WARM_CALL_OVERHEAD: u64 = 111;
/// How many sizes of a ladder's span the audit sweeps, evenly in log.
const SWEEP: usize = 2_000;

/// A fault of the pool's, one bit of [`FAULTS`].
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// `previewSwap` and `pairRequest` revert `NotTaker()` unless the taker calls.
    Caller,
    /// `previewSwap` burns gas down to [`BURN_LEAVES`], then answers.
    Burn,
    /// `previewSwap` answers [`BOMB_BYTES`] bytes.
    ReturnBomb,
    /// `previewSwap` reverts with [`BOMB_BYTES`] bytes.
    RevertBomb,
    /// `previewSwap` answers 64 bytes.
    Short,
    /// `previewSwap` reverts with [`REVERT_REASON`]'s four bytes.
    Reason,
    /// `previewSwap` reverts with no data.
    Bare,
    /// The leverage route carries the curve's fee.
    EqualFee,
    /// A fill under the live band's far line reverts.
    CoreBand,
    /// `used` is the ask less a ten-thousandth of it.
    UsedOneBp,
    RequestRevert,
    LevStateRevert,
    /// `levState` answers 7 words.
    LevStateShort,
}

impl Fault {
    const ALL: [Self; 13] = [
        Self::Caller,
        Self::Burn,
        Self::ReturnBomb,
        Self::RevertBomb,
        Self::Short,
        Self::Reason,
        Self::Bare,
        Self::EqualFee,
        Self::CoreBand,
        Self::UsedOneBp,
        Self::RequestRevert,
        Self::LevStateRevert,
        Self::LevStateShort,
    ];

    fn bit(self) -> u64 {
        let shift = match self {
            Self::Caller => 0,
            Self::Burn => 1,
            Self::ReturnBomb => 2,
            Self::RevertBomb => 3,
            Self::Short => 4,
            Self::Reason => 5,
            Self::Bare => 6,
            Self::EqualFee => 7,
            Self::CoreBand => 8,
            Self::UsedOneBp => 9,
            Self::RequestRevert => 10,
            Self::LevStateRevert => 11,
            Self::LevStateShort => 12,
        };
        1 << shift
    }
}

/// The selector of a custom error, as a revert reason.
fn reason_of(signature: &str) -> u32 {
    let hash = keccak256(signature);
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

fn tag_reason() -> u32 {
    u32::from_be_bytes(SAMPLER_TAG)
}

fn taker() -> Address {
    router(Chain::Base).expect("Base has a router")
}

fn pow10(exponent: u8) -> U256 {
    U256::from(10).pow(U256::from(exponent))
}

fn wide_pow10(exponent: u8) -> U512 {
    U512::from(10).pow(U512::from(exponent))
}

fn tokens(side: Side) -> (Address, Address) {
    match side {
        Side::Sale => (TOKEN_0, TOKEN_1),
        Side::Buy => (TOKEN_1, TOKEN_0),
    }
}

/// Whether `code`, read as instructions, holds `op`.
fn uses_opcode(code: &[u8], op: u8) -> bool {
    let mut pc = 0;
    while let Some(&byte) = code.get(pc) {
        if byte == op {
            return true;
        }
        // PUSH1 to PUSH32 carry 1 to 32 bytes of data
        let data = if (0x60..=0x7f).contains(&byte) { usize::from(byte - 0x5f) } else { 0 };
        pc += 1 + data;
    }
    false
}

/// The sizes a walk starts from.
#[derive(Clone, Copy, Debug)]
enum Seeds {
    /// The feed's own: rungs doubling from 10 USD of the input.
    Rungs,
    /// [`sweep`]'s: from dust to far past any book, more than a walk's budget.
    Sweep,
}

/// 32 sizes evenly spaced in log from a thousandth of a token to a billion tokens, rounded, the
/// last exact.
pub(super) fn sweep(decimals: u8) -> Vec<U256> {
    let (low, high) = (pow10(decimals - 3), pow10(decimals + 9));
    let (lo, hi) = (f64::from(low), f64::from(high));
    let step = (hi / lo).ln() / 31.0;
    let mut sizes = vec![low];
    for k in 1..32u32 {
        let size = if k == 31 {
            high
        } else {
            U256::try_from((lo * (step * f64::from(k)).exp()).round()).expect("a size under 2^256")
        };
        if sizes.last() < Some(&size) && size <= high {
            sizes.push(size);
        }
    }
    sizes
}

/// The largest `room` of the sweep's sizes under `top`.
fn sweep_under(decimals: u8, top: U256, room: usize) -> BTreeSet<U256> {
    let under: Vec<U256> = sweep(decimals)
        .into_iter()
        .filter(|size| *size < top)
        .collect();
    under[under.len().saturating_sub(room)..]
        .iter()
        .copied()
        .collect()
}

/// `count` sizes evenly spaced in log over `[lo, hi]`, rounded down.
fn log_sweep(lo: U256, hi: U256, count: usize) -> BTreeSet<U256> {
    let (lo, hi) = (f64::from(lo), f64::from(hi));
    let steps = (count - 1) as f64;
    (0..count)
        .map(|k| U256::try_from(lo * (hi / lo).powf(k as f64 / steps)).expect("a size under 2^256"))
        .collect()
}

/// The fake pool's slots, and its tokens' decimals.
#[derive(Clone, Debug)]
struct Fake {
    slots: BTreeMap<u64, U256>,
    decimals: [u8; 2],
}

impl Fake {
    /// A leverage row with spreads of 1.75 % and 1.3 %, caps of 0.5 % and 2 % inside a band of 5 %,
    /// a live band of 8 %, and an 8/6 pair at 75,600 USD: leverage pays the small fills of both
    /// directions and the curve the large ones.
    fn base() -> Self {
        Self::priced([8, 6], 75_600)
    }

    fn priced(decimals: [u8; 2], usd: u64) -> Self {
        let p36 = U256::from(usd) * pow10(decimals[1]) * pow10(36) / pow10(decimals[0]);
        let slots = BTreeMap::from([
            (SLOT_TOKEN_0, U256::from_be_slice(TOKEN_0.as_slice())),
            (SLOT_TOKEN_1, U256::from_be_slice(TOKEN_1.as_slice())),
            (SLOT_TAKER, U256::from_be_slice(taker().as_slice())),
            (LIVE_BAND, U256::from(80_000_000_000_000_000u128)),
            (USD_0, U256::from(usd) * U256::from(WAD)),
            (USD_1, U256::from(WAD)),
            (P36, p36),
            (CURVE_FEE, U256::from(12_644_526_805_792_593u128)),
            (BID, U256::from(736_000_000_000_000_000u128)),
            (CV, U256::from(230_000_000_000_000_000_000_000u128)),
            (LEGS_0, U256::from(117_098_252)),
            (LEGS_0 + 2, U256::from(192_927_801)),
            (LEGS_1 + 3, U256::from(80_231_847_915u128)),
            (SALE + CURVE_R, U256::from(7_450_000_000_000u128)),
            (SALE + CURVE_X, U256::from(10_000_000_000u128)),
            (SALE + LEVER_R, U256::from(1_717_500_000_000u128)),
            (SALE + LEVER_X, U256::from(2_290_000_000u128)),
            (BUY + CURVE_R, U256::from(13_156_000_000u128)),
            (BUY + CURVE_X, U256::from(10_000_000_000_000u128)),
            (BUY + LEVER_R, U256::from(6_700_000_000u128)),
            (BUY + LEVER_X, U256::from(5_000_000_000_000u128)),
            (PARAMS, U256::from(2 * WAD)),
            (PARAMS + 1, U256::from(1_020_000_000_000_000_000u128)),
            (PARAMS + 2, U256::from(6 * WAD)),
            (P_SPREAD, U256::from(17_500_000_000_000_000u128)),
            (P_SPREAD_DN, U256::from(13_000_000_000_000_000u128)),
            (P_UP_CAP, U256::from(5_000_000_000_000_000u128)),
            (P_DN_CAP, U256::from(20_000_000_000_000_000u128)),
            (PARAMS + 31, U256::from(2_200_000_000_000_000_000u128)),
            (P_BAND, U256::from(50_000_000_000_000_000u128)),
            (PARAMS + 34, U256::from(1_700_000_000_000_000_000u128)),
        ]);
        Self { slots, decimals }
    }

    fn with<T>(mut self, slot: u64, value: T) -> Self
    where
        U256: UintTryFrom<T>,
    {
        self.slots
            .insert(slot, U256::from(value));
        self
    }

    fn fault(self, fault: Fault) -> Self {
        let faults = self.get(FAULTS) | U256::from(fault.bit());
        self.with(FAULTS, faults)
    }

    fn get(&self, slot: u64) -> U256 {
        self.slots
            .get(&slot)
            .copied()
            .unwrap_or_default()
    }

    fn row(&self) -> Row {
        Row {
            spread_wad: self.get(P_SPREAD),
            spread_dn_wad: self.get(P_SPREAD_DN),
            up_cap_wad: self.get(P_UP_CAP),
            dn_cap_wad: self.get(P_DN_CAP),
            band_wad: self.get(P_BAND),
        }
    }

    /// The input's decimals and the output's.
    fn decimals(&self, side: Side) -> (u8, u8) {
        let [decimals_0, decimals_1] = self.decimals;
        match side {
            Side::Sale => (decimals_0, decimals_1),
            Side::Buy => (decimals_1, decimals_0),
        }
    }

    fn seeds(&self, side: Side, seeds: Seeds) -> Vec<U256> {
        let decimals_in = self.decimals(side).0;
        match seeds {
            Seeds::Rungs => {
                let usd = match side {
                    Side::Sale => self.get(USD_0),
                    Side::Buy => self.get(USD_1),
                };
                let price = f64::from(usd) / f64::from(U256::from(WAD));
                let smallest = smallest_rung(decimals_in, Some(price), DEFAULT_SMALLEST_RUNG_USD);
                rung_inputs(smallest, None, DEFAULT_RUNG_COUNT)
            }
            Seeds::Sweep => sweep(decimals_in),
        }
    }

    fn ask(&self, side: Side, seeds: Vec<U256>) -> FlammSampler::Ask {
        let (token_in, token_out) = tokens(side);
        FlammSampler::Ask {
            tokenIn: token_in,
            tokenOut: token_out,
            hook: POOL,
            tierA: self.get(P_SPREAD),
            tierB: self.get(P_SPREAD_DN),
            maxProbes: U256::from(MAX_PROBES_PER_DIRECTION),
            seeds,
        }
    }

    /// Both directions from `seeds`, the sale first.
    fn asks(&self, seeds: Seeds) -> Vec<FlammSampler::Ask> {
        SIDES
            .into_iter()
            .map(|side| self.ask(side, self.seeds(side, seeds)))
            .collect()
    }

    /// The words of `side`'s request the pool answers, and on a buy its hook's top bid.
    fn request(&self, side: Side) -> Request {
        let (usd_in, usd_out, legs_in, legs_out) = match side {
            Side::Sale => (USD_0, USD_1, LEGS_0, LEGS_1),
            Side::Buy => (USD_1, USD_0, LEGS_1, LEGS_0),
        };
        let mut legs = [U256::ZERO; 8];
        for (leg, slot) in legs
            .iter_mut()
            .zip((legs_in..legs_in + 4).chain(legs_out..legs_out + 4))
        {
            *leg = self.get(slot);
        }
        let bid_wad = match side {
            Side::Sale => U256::ZERO,
            Side::Buy => self.get(BID),
        };
        Request {
            band_wad: self.get(LIVE_BAND),
            usd_in: self.get(usd_in),
            usd_out: self.get(usd_out),
            legs,
            bid_wad,
        }
    }

    /// What the rule knows of `side` besides its probes, from the walk's reads. The pair's band is
    /// the live band, as the feed reads it from the token's config.
    fn terms(&self, side: Side, walk: &FlammSampler::Walk) -> Terms {
        let row = self.row();
        let (decimals_in, decimals_out) = self.decimals(side);
        Terms {
            rule: Rule::Everlong,
            tiers: row.tiers(),
            row_band: row.band_wad,
            side,
            decimals_in,
            decimals_out,
            trust: Trust::of(&row, self.get(LIVE_BAND), 2),
            curve_fee: walk.curveFeeWad,
            request: read(walk),
        }
    }
}

/// Every fault at once, a burner first.
fn every_fault() -> Fake {
    let mut fake = Fake::base()
        .with(BOMB_BYTES, MIB)
        .with(REVERT_REASON, reason_of("ReadStarved()"));
    for fault in Fault::ALL {
        fake = fake.fault(fault);
    }
    fake
}

/// Leverage refuses the one size [`SWITCH_AT`] and every size past three above it.
fn floor_flicker() -> Fake {
    Fake::base()
        .with(SALE + FLICKER_AT, SWITCH_AT)
        .with(SALE + FLICKER_LEN, 1u64)
        .with(SALE + EDGE, SWITCH_AT + 3)
}

/// Leverage's net falls 1,100 units past [`SWITCH_AT`], between one and two units of its slope,
/// and a ceiling one unit under its net there refuses that size and those past one above it: the
/// sizes on either side stay on leverage's label, the one above paying more.
fn token_ceiling() -> Fake {
    let base = Fake::base();
    let at = U256::from(SWITCH_AT);
    let net = base.get(SALE + LEVER_R) * at / (base.get(SALE + LEVER_X) + at);
    base.with(SALE + FALL_AT, SWITCH_AT)
        .with(SALE + FALL_BY, 1_100u64)
        .with(SALE + CEILING, net - U256::from(1))
}

/// Leverage refuses every size on the lattice, both ways, and the curve fills there.
fn holes() -> Fake {
    Fake::base()
        .with(SALE + HOLE_EVERY, LATTICE)
        .with(SALE + HOLE_AT, LATTICE_AT)
        .with(BUY + HOLE_EVERY, LATTICE)
        .with(BUY + HOLE_AT, LATTICE_AT)
}

/// A down cap as wide as its band, 0.1 %, and leverage pricing 0.05 % under the feed.
fn hole_row_a() -> Fake {
    holes()
        .with(P_DN_CAP, 1_000_000_000_000_000u64)
        .with(P_BAND, 1_000_000_000_000_000u64)
        .with(SALE + LEVER_R, 7_556_000_000_000_000u64)
        .with(SALE + LEVER_X, 10_000_000_000_000u64)
        .with(BUY + LEVER_R, 1_322_100_000_000u64)
        .with(BUY + LEVER_X, 1_000_000_000_000_000u64)
}

/// No down spread.
fn hole_row_b() -> Fake {
    holes().with(P_SPREAD_DN, 0u64)
}

/// The feed's settings for the sampler, as the reader sends them at the default cap.
fn limits() -> FlammSampler::Limits {
    sampler_limits(DEFAULT_PROBE_GAS)
}

/// The same with the sampler's splits of a span on, at 1 bp, and three steps towards the first
/// refusal: steps this feed leaves off, in the same bytes.
fn refining() -> FlammSampler::Limits {
    FlammSampler::Limits { edgeProbes: U256::from(3), gapPpm: U256::from(100), ..limits() }
}

fn capped(probe_gas: u64) -> FlammSampler::Limits {
    FlammSampler::Limits { probeGas: U256::from(probe_gas), ..limits() }
}

/// The sampler's probe statuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Priced,
    Clipped,
    Refused,
    Starved,
    Malformed,
}

fn status(probe: &FlammSampler::Probe) -> Status {
    match probe.status.to::<u64>() {
        1 => Status::Priced,
        2 => Status::Clipped,
        3 => Status::Refused,
        4 => Status::Starved,
        5 => Status::Malformed,
        other => panic!("the sampler has no status {other}"),
    }
}

/// Priced and paying at least [`DUST_OUT`]: what the sampler's flips, edges and probe above count.
fn good(probe: &FlammSampler::Probe) -> bool {
    status(probe) == Status::Priced && probe.out >= U256::from(DUST_OUT)
}

/// Where the probe above a top at `probe` asks.
fn above(probe: &FlammSampler::Probe) -> U256 {
    probe.used + (probe.used >> ABOVE_SHIFT) + U256::from(1)
}

/// The rule's probe of what the sampler answered, whose status must be the sampler's.
fn rule_probe(probe: &FlammSampler::Probe) -> Probe {
    let answer = Some((probe.used, probe.out, probe.feeWad));
    let (answer, want) = match status(probe) {
        Status::Priced => (answer, ProbeStatus::Priced),
        Status::Clipped => (answer, ProbeStatus::Clipped),
        Status::Malformed => (answer, ProbeStatus::Malformed),
        Status::Refused => (None, ProbeStatus::Refused),
        Status::Starved => panic!("no ladder case runs a probe short of gas"),
    };
    let classified = Probe::classify(probe.ask, answer);
    assert_eq!(classified.status, want, "the sampler and the rule class {probe:?} apart");
    classified
}

fn probes_of(walk: &FlammSampler::Walk) -> Vec<Probe> {
    walk.probes
        .iter()
        .map(rule_probe)
        .collect()
}

/// The request words a walk read; all zero where it read none.
fn read(walk: &FlammSampler::Walk) -> Request {
    Request {
        band_wad: U256::from(walk.bandWad),
        usd_in: walk.usdIn,
        usd_out: walk.usdOut,
        legs: walk.legs,
        bid_wad: walk.bidWad,
    }
}

/// Word `k` of an answer.
fn word(bytes: &[u8], k: usize) -> U256 {
    U256::from_be_slice(&bytes[32 * k..32 * (k + 1)])
}

/// A call the sampler made.
#[derive(Clone, Debug)]
struct Inner {
    caller: Address,
    to: Address,
    kind: CallKind,
    input: Bytes,
    output: Bytes,
    /// The gas its callee was given, and what it spent.
    gas_limit: u64,
    gas_used: u64,
}

impl Inner {
    fn selector(&self) -> [u8; 4] {
        self.input[..4]
            .try_into()
            .expect("every call the sampler makes has a selector")
    }

    /// Argument word `k`.
    fn arg(&self, k: usize) -> U256 {
        word(&self.input[4..], k)
    }

    /// The token the call is about: a preview's or a request's input, and word 1 of the request a
    /// hook's read takes.
    fn token_in(&self) -> Address {
        let k = if self.selector() == QUOTE_PAIR_SELECTOR || self.selector() == LEV_STATE_SELECTOR {
            1
        } else {
            0
        };
        Address::from_word(self.arg(k).into())
    }
}

/// One sampler call: its answer and the answer's size, the gas the call used, and every call it
/// made, in order.
struct Sampled {
    answer: FlammSampler::sampleReturn,
    size: usize,
    gas_used: u64,
    calls: Vec<Inner>,
}

impl Sampled {
    fn walks(&self) -> &[FlammSampler::Walk] {
        &self.answer.walks
    }

    /// The calls of `selector` the sampler made for `side`'s walk, in order.
    fn calls_of(&self, selector: [u8; 4], side: Side) -> Vec<&Inner> {
        let (token_in, _) = tokens(side);
        self.calls
            .iter()
            .filter(|call| call.selector() == selector && call.token_in() == token_in)
            .collect()
    }

    /// The asks of the walk's `previewSwap` calls, in the order made.
    fn preview_asks(&self, side: Side) -> Vec<U256> {
        self.calls_of(previewSwapCall::SELECTOR, side)
            .into_iter()
            .map(|call| call.arg(2))
            .collect()
    }

    /// What every answer keeps, whatever the pool does: the tag and the block first, a walk per
    /// ask, each walk ascending by ask, under `2^120` and at most one past its budget, each probe
    /// one `previewSwap` whose callee was held to the cap, an answer whose size follows the probes
    /// alone, and every call a read from the sampler's own address.
    fn check(&self, at: Address, asks: &[FlammSampler::Ask], limits: &FlammSampler::Limits) {
        assert_eq!(self.answer.tag, FixedBytes::from(SAMPLER_TAG));
        assert_eq!(self.answer.blockNumber, U256::from(BLOCK));
        assert_eq!(self.walks().len(), asks.len());
        let cap = limits.probeGas.to::<u64>();
        let limit = U256::from(1) << ANSWER_BITS;
        let mut probes = 0;
        for (walk, ask) in self.walks().iter().zip(asks) {
            assert!(walk.probes.len() <= ask.maxProbes.to::<usize>() + 1, "{walk:?}");
            assert!(
                walk.probes
                    .is_sorted_by(|a, b| a.ask < b.ask),
                "{walk:?}"
            );
            for probe in &walk.probes {
                assert!(probe.ask < limit, "{probe:?}");
                assert!(probe.gasUsed <= U256::from(cap + CALL_OVERHEAD), "{probe:?}");
            }
            let previews = self
                .calls
                .iter()
                .filter(|call| {
                    call.selector() == previewSwapCall::SELECTOR && call.token_in() == ask.tokenIn
                })
                .count();
            assert_eq!(previews, walk.probes.len(), "a probe is one previewSwap");
            probes += walk.probes.len();
        }
        // three head words, the walks' length and offsets, then a walk's 15 words, its probes'
        // length and 7 words a probe
        assert_eq!(self.size, 128 + 544 * asks.len() + 224 * probes, "the answer's size");
        for call in &self.calls {
            assert_eq!((call.caller, call.to, call.kind), (at, POOL, CallKind::StaticCall));
            assert!(call.gas_limit <= cap, "{call:?}");
        }
    }
}

/// An in-memory chain with the fake pool at [`POOL`] and the sampler's code at the taker and at
/// [`ELSEWHERE`].
struct Evm {
    db: CacheDB<EmptyDB>,
}

fn tx(caller: Address, to: Address, data: Bytes, gas: u64) -> TxEnv {
    TxEnv { caller, gas_limit: gas, kind: TxKind::Call(to), data, ..Default::default() }
}

fn cfg() -> CfgEnv {
    let mut cfg = CfgEnv::new_with_spec(SpecId::PRAGUE);
    cfg.disable_nonce_check = true;
    // the previews come from the taker, which holds the sampler's code here
    cfg.disable_eip3607 = true;
    cfg
}

fn block() -> BlockEnv {
    BlockEnv { number: U256::from(BLOCK), ..Default::default() }
}

impl Evm {
    fn new(fake: &Fake) -> Self {
        let mut db = CacheDB::new(EmptyDB::default());
        let code = |runtime: &'static [u8]| {
            AccountInfo::from_bytecode(Bytecode::new_raw(Bytes::from_static(runtime)))
        };
        db.insert_account_info(POOL, code(POOL_RUNTIME));
        db.insert_account_info(taker(), code(SAMPLER_RUNTIME));
        db.insert_account_info(ELSEWHERE, code(SAMPLER_RUNTIME));
        for (slot, value) in &fake.slots {
            db.insert_account_storage(POOL, U256::from(*slot), *value)
                .expect("an in-memory database takes every slot");
        }
        Self { db }
    }

    fn call(&self, caller: Address, to: Address, data: Bytes, gas: u64) -> ExecutionResult {
        Context::mainnet()
            .with_cfg(cfg())
            .with_block(block())
            .with_ref_db(&self.db)
            .build_mainnet()
            .transact_one(tx(caller, to, data, gas))
            .expect("the call is a valid transaction")
    }

    /// The sampler at `at`, from no sender, as the feed calls it.
    fn sample(
        &self,
        at: Address,
        asks: &[FlammSampler::Ask],
        limits: &FlammSampler::Limits,
        gas: u64,
    ) -> Sampled {
        let data =
            FlammSampler::sampleCall { pool: POOL, asks: asks.to_vec(), limits: limits.clone() }
                .abi_encode();
        let mut tracer = TracingInspector::new(TracingInspectorConfig::none());
        let result = Context::mainnet()
            .with_cfg(cfg())
            .with_block(block())
            .with_ref_db(&self.db)
            .build_mainnet_with_inspector(&mut tracer)
            .inspect_one_tx(tx(Address::ZERO, at, data.into(), gas))
            .expect("the call is a valid transaction");
        let calls = tracer
            .into_traces()
            .into_nodes()
            .into_iter()
            .filter(|node| node.trace.depth > 0)
            .map(|node| Inner {
                caller: node.trace.caller,
                to: node.trace.address,
                kind: node.trace.kind,
                input: node.trace.data,
                output: node.trace.output,
                gas_limit: node.trace.gas_limit,
                gas_used: node.trace.gas_used,
            })
            .collect();
        let ExecutionResult::Success { output: Output::Call(output), gas_used, .. } = result else {
            panic!("the sampler did not return: {result:?}");
        };
        let answer = FlammSampler::sampleCall::abi_decode_returns(&output)
            .expect("the sampler's answer decodes");
        let sampled = Sampled { answer, size: output.len(), gas_used, calls };
        sampled.check(at, asks, limits);
        sampled
    }

    /// The pool's own preview of `amount` on `side`, from the taker; `None` where it reverts.
    fn preview(&self, side: Side, amount: U256) -> Option<(U256, U256, U256)> {
        let (token_in, token_out) = tokens(side);
        let data = previewSwapCall { tokenIn: token_in, tokenOut: token_out, amountIn: amount }
            .abi_encode();
        match self.call(taker(), POOL, data.into(), DEFAULT_PROBE_GAS) {
            ExecutionResult::Success { output: Output::Call(output), .. } => {
                let answer = previewSwapCall::abi_decode_returns(&output)
                    .expect("a preview answers three words");
                Some((answer.amountInUsed, answer.amountOut, answer.feeWad))
            }
            ExecutionResult::Success { output: Output::Create(..), .. } |
            ExecutionResult::Revert { .. } |
            ExecutionResult::Halt { .. } => None,
        }
    }
}

/// The pool's own previews of one side, kept by size.
struct Previews<'a> {
    evm: &'a Evm,
    side: Side,
    seen: BTreeMap<U256, Option<(U256, U256, U256)>>,
}

impl<'a> Previews<'a> {
    fn new(evm: &'a Evm, side: Side) -> Self {
        Self { evm, side, seen: BTreeMap::new() }
    }

    fn at(&mut self, amount: U256) -> Option<(U256, U256, U256)> {
        *self
            .seen
            .entry(amount)
            .or_insert_with(|| self.evm.preview(self.side, amount))
    }

    fn labelled(&mut self, amount: U256, label: U256) -> bool {
        self.at(amount)
            .is_some_and(|(_, _, fee)| fee == label)
    }
}

/// A ladder's quotes against the pool's own previews.
#[derive(Default)]
struct Audit {
    rungs: usize,
    /// The sizes the ladder quotes, and those the pool refuses, which cost a swap nothing.
    sizes: usize,
    refused: usize,
    /// Those quoted above what the pool pays, the largest gap, in units and in bp of the pool's
    /// output, and where.
    over: usize,
    worst: U256,
    worst_bp: f64,
    at: U256,
}

impl fmt::Display for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} rungs, {} sizes, {} where the pool refuses, {} over",
            self.rungs, self.sizes, self.refused, self.over
        )?;
        if self.over > 0 {
            write!(f, ", worst {} units at {}, {:.2} bp", self.worst, self.at, self.worst_bp)?;
        }
        Ok(())
    }
}

/// Each quote of `ladder` on a grid inside its span, against the pool's own preview of that size:
/// 2,000 sizes evenly in log, each rung and two units either side of it, 300 sizes across each
/// flat span, and `extra`.
fn audit(previews: &mut Previews<'_>, ladder: &Ladder, extra: &[U256]) -> Audit {
    let rungs = ladder.rungs();
    let mut res = Audit { rungs: rungs.len(), ..Audit::default() };
    let (Some(first), Some(last)) = (ladder.first(), ladder.last()) else {
        return res;
    };
    let mut grid = log_sweep(first.input, last.input, SWEEP);
    let two = U256::from(2);
    for rung in rungs {
        grid.extend((0..5u64).map(|d| (rung.input + U256::from(d)).saturating_sub(two)));
    }
    for pair in rungs.windows(2) {
        if pair[1].segment == Segment::Flat {
            let (below, span) = (pair[0].input, pair[1].input - pair[0].input);
            grid.extend((0..300u64).map(|k| below + span * U256::from(k) / U256::from(299)));
        }
    }
    grid.extend(extra.iter().copied());
    for &size in grid.range(first.input..=last.input) {
        let quote = ladder
            .output(size)
            .expect("a size inside the ladder quotes");
        res.sizes += 1;
        let Some((_, out, _)) = previews.at(size) else {
            res.refused += 1;
            continue;
        };
        if quote > out {
            res.over += 1;
            let gap = quote - out;
            if gap > res.worst {
                res.worst = gap;
                res.worst_bp = f64::from(gap) / f64::from(out) * 1e4;
                res.at = size;
            }
        }
    }
    res
}

/// The sizes where a walk's route changes: between two adjacent priced probes on different labels,
/// the least size the pool fills on the upper one's label, bisected on its own preview.
fn switches_of(previews: &mut Previews<'_>, walk: &FlammSampler::Walk) -> Vec<U256> {
    let mut switches = Vec::new();
    for pair in walk.probes.windows(2) {
        let (low, high) = (&pair[0], &pair[1]);
        if status(low) != Status::Priced ||
            status(high) != Status::Priced ||
            low.feeWad == high.feeWad
        {
            continue;
        }
        let (mut lo, mut hi) = (low.ask, high.ask);
        while hi - lo > U256::from(1) {
            let mid = (lo + hi) >> 1;
            if previews.labelled(mid, high.feeWad) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        switches.push(hi);
    }
    switches
}

/// One direction of a ladder case, walked by the sampler at the feed's settings.
struct Walked<'a> {
    fake: &'a Fake,
    side: Side,
    sampled: Sampled,
}

impl<'a> Walked<'a> {
    fn new(evm: &Evm, fake: &'a Fake, side: Side, seeds: BTreeSet<U256>) -> Self {
        let ask = fake.ask(side, seeds.into_iter().collect());
        let sampled = evm.sample(taker(), &[ask], &limits(), DEFAULT_SAMPLE_GAS);
        Self { fake, side, sampled }
    }

    fn walk(&self) -> &FlammSampler::Walk {
        &self.sampled.walks()[0]
    }

    fn terms(&self) -> Terms {
        self.fake.terms(self.side, self.walk())
    }

    fn rule(&self) -> Ladder {
        settle(&probes_of(self.walk()), &self.terms()).ladder
    }

    /// The walk's probes less its probe above, which must be its last `previewSwap` call, asked
    /// above its top priced probe paying at least `DUST_OUT`, a tier-labelled one.
    fn before_the_probe_above(&self) -> Vec<Probe> {
        let last = *self
            .sampled
            .preview_asks(self.side)
            .last()
            .expect("the walk made a probe");
        let rest: Vec<&FlammSampler::Probe> = self
            .walk()
            .probes
            .iter()
            .filter(|probe| probe.ask != last)
            .collect();
        let top = rest
            .iter()
            .rev()
            .find(|probe| good(probe))
            .expect("a priced probe under the last");
        assert!(
            self.fake
                .row()
                .tiers()
                .contains(top.feeWad),
            "the top is not tier-labelled"
        );
        assert_eq!(last, above(top), "the walk made no probe above");
        rest.into_iter()
            .map(rule_probe)
            .collect()
    }
}

/// The rule with one of its steps left out, or a word it reads forced.
#[derive(Clone, Copy, Debug)]
enum Control {
    /// The room test keeps every answer; the probe above stays.
    RoomOff,
    /// The walk without its probe above, and the rule without the test that asks for one.
    AboveOff,
    /// Both.
    BothOff,
    /// The probe above off, and the room test asking only the margin's fixed part.
    NeedOnly,
    /// The hook's bid word read as positive.
    BidForced,
    /// The pool trusted whatever its row.
    TrustedForced,
}

impl Control {
    fn ladder(self, walked: &Walked<'_>) -> Ladder {
        let mut terms = walked.terms();
        let (probes, room, above) = match self {
            Self::RoomOff => (probes_of(walked.walk()), RoomTest::Off, AboveTest::Priced),
            Self::AboveOff => (walked.before_the_probe_above(), RoomTest::Margin, AboveTest::Off),
            Self::BothOff => (walked.before_the_probe_above(), RoomTest::Off, AboveTest::Off),
            Self::NeedOnly => (walked.before_the_probe_above(), RoomTest::Need, AboveTest::Off),
            Self::BidForced => {
                terms.request.bid_wad = Fake::base().get(BID);
                (probes_of(walked.walk()), RoomTest::Margin, AboveTest::Priced)
            }
            Self::TrustedForced => {
                terms.trust = Trust::Trusted;
                (probes_of(walked.walk()), RoomTest::Margin, AboveTest::Priced)
            }
        };
        settle_with(&probes, &terms, room, above).ladder
    }
}

/// A ladder case: one direction of a pool state, the sizes it starts from, and what it asks.
struct Case {
    fake: Fake,
    side: Side,
    seeds: BTreeSet<U256>,
    /// Sizes the audits add to their grids, around the state's switch.
    extra: Vec<U256>,
    /// More of them, from the walk.
    extra_of: Option<fn(&FlammSampler::Walk) -> Vec<U256>>,
    /// Sizes the rule must quote.
    covers: Vec<U256>,
    /// The row fails its test at the live band.
    gated: bool,
    /// The rule quotes nothing.
    dark: bool,
    /// Each control, and whether its quotes must pass the pool's somewhere.
    controls: Vec<(Control, bool)>,
}

impl Case {
    fn new(fake: Fake, side: Side, seeds: impl IntoIterator<Item = U256>) -> Self {
        Self {
            fake,
            side,
            seeds: seeds.into_iter().collect(),
            extra: Vec::new(),
            extra_of: None,
            covers: Vec::new(),
            gated: false,
            dark: false,
            controls: Vec::new(),
        }
    }
}

/// The rule's ladder never quotes more than the pool, on a grid that also holds every route
/// switch of the walk and 150 sizes either side of it; a control that must go over goes over by
/// more than a rounding, and one that must not stays under the pool.
fn assert_case(case: Case) {
    let Case { fake, side, seeds, mut extra, extra_of, covers, gated, dark, controls } = case;
    assert_eq!(!fake.row().passes(fake.get(LIVE_BAND)), gated, "the row's test");
    let evm = Evm::new(&fake);
    let walked = Walked::new(&evm, &fake, side, seeds);
    let mut previews = Previews::new(&evm, side);
    if let Some(extra_of) = extra_of {
        extra.extend(extra_of(walked.walk()));
    }
    let switches = switches_of(&mut previews, walked.walk());
    for &switch in &switches {
        extra.extend((0..300u64).map(|d| (switch + U256::from(d)).saturating_sub(U256::from(150))));
    }
    let ladder = walked.rule();
    let rule = audit(&mut previews, &ladder, &extra);
    println!(
        "the rule: {} probes, read {}, {} route switches, {rule}",
        walked.walk().probes.len(),
        walked.walk().bandWad != 0,
        switches.len()
    );
    assert_eq!(rule.over, 0, "the rule's ladder quotes more than the pool: {rule}");
    if dark {
        assert_eq!(rule.rungs, 0, "{rule}");
    } else {
        assert!(rule.sizes > SWEEP, "{rule}");
    }
    for size in covers {
        assert!(ladder.output(size).is_some(), "the rule does not quote {size}");
    }
    for (control, over) in controls {
        let res = audit(&mut previews, &control.ladder(&walked), &extra);
        println!("{control:?}: {res}");
        if over {
            assert!(res.over > 0, "{control:?}: the state is not armed: {res}");
            assert!(res.worst_bp > 1.0, "{control:?}: over by no more than a rounding: {res}");
        } else {
            assert_eq!(res.over, 0, "{control:?}: the rule's other steps leave it over: {res}");
        }
    }
}

/// The refused sizes just above each priced probe of a walk over a hole row.
fn lattice_above(walk: &FlammSampler::Walk) -> Vec<U256> {
    let every = U256::from(LATTICE);
    walk.probes
        .iter()
        .filter(|probe| status(probe) == Status::Priced)
        .flat_map(|probe| {
            let base = probe.ask - probe.ask % every + U256::from(LATTICE_AT);
            (0..4u64).map(move |k| base + every * U256::from(k))
        })
        .collect()
}

/// The top of the far-edge lottery on a sale.
struct Zone {
    /// The tier-labelled sizes near the last one of the grid.
    tiers: Vec<U256>,
    /// The sizes between the first and the last of them that the curve fills.
    holes: Vec<U256>,
}

/// The last tier-labelled size of a grid of `steps` over `[lo, hi]`, bisected to 256 units, then
/// every size within 400 of that bracket.
fn lottery_zone(previews: &mut Previews<'_>, lo: U256, hi: U256, steps: u64, tier: U256) -> Zone {
    let grid: Vec<U256> = (0..=steps)
        .map(|k| lo + (hi - lo) * U256::from(k) / U256::from(steps))
        .collect();
    let last = grid
        .iter()
        .rposition(|size| previews.labelled(*size, tier))
        .expect("leverage prices on the grid");
    assert!(last + 1 < grid.len(), "leverage prices the whole grid");
    let (mut a, mut b) = (grid[last], grid[last + 1]);
    while b - a > U256::from(256) {
        let mid = (a + b) >> 1;
        if previews.labelled(mid, tier) {
            a = mid;
        } else {
            b = mid;
        }
    }
    let (mut tiers, mut others) = (Vec::new(), Vec::new());
    let mut size = a - U256::from(400);
    while size < b + U256::from(400) {
        match previews.at(size) {
            Some((_, _, fee)) if fee == tier => tiers.push(size),
            Some(_) => others.push(size),
            None => {}
        }
        size += U256::from(1);
    }
    let (Some(&first), Some(&top)) = (tiers.first(), tiers.last()) else {
        panic!("no tier-labelled size in the zone");
    };
    let holes = others
        .into_iter()
        .filter(|size| first < *size && *size < top)
        .collect();
    Zone { tiers, holes }
}

/// The room test's numbers on a sale, as the rule computes them from the request.
struct SaleRoom {
    p36: U512,
    band: U512,
    need: U512,
    grid: U512,
}

impl SaleRoom {
    fn of(fake: &Fake) -> Self {
        let request = fake.request(Side::Sale);
        let (decimals_in, decimals_out) = fake.decimals(Side::Sale);
        let p36 = U512::from(request.usd_in) * wide_pow10(decimals_out) * wide_pow10(36) /
            (U512::from(request.usd_out) * wide_pow10(decimals_in));
        Self {
            p36,
            band: U512::from(
                fake.row()
                    .band_wad
                    .min(request.band_wad),
            ),
            need: U512::from(2) + U512::from(3) * (p36 / wide_pow10(36) + U512::from(1)),
            grid: wide_pow10(36 - decimals_out),
        }
    }

    /// An answer's least room inside the band's two edges.
    fn least(&self, used: U256, out: U256) -> U512 {
        let value = U512::from(used) * self.p36;
        let (wad, scale) = (U512::from(WAD), wide_pow10(36) * U512::from(WAD));
        let low = value * (wad - self.band) / scale + U512::from(1);
        let high = value * (wad + self.band) / scale;
        let out = U512::from(out);
        out.saturating_sub(low)
            .min(high.saturating_sub(out))
    }

    fn margin(&self, used: U256) -> U512 {
        self.need + U512::from(used) / self.grid + U512::from(1)
    }
}

#[test]
fn every_client_runs_the_same_bytes_built_from_the_same_source() {
    assert_eq!(
        keccak256(SAMPLER_RUNTIME),
        SAMPLER_RUNTIME_KECCAK,
        "rebuild: assets/build_sampler.py"
    );
    assert_eq!(keccak256(SAMPLER_SOURCE), SAMPLER_SOURCE_KECCAK);
    assert_eq!(keccak256(POOL_RUNTIME), POOL_RUNTIME_KECCAK);
    assert_eq!(keccak256(POOL_SOURCE), POOL_SOURCE_KECCAK);
    println!(
        "runtime: the sampler {} bytes, the pool {}",
        SAMPLER_RUNTIME.len(),
        POOL_RUNTIME.len()
    );
    for code in [SAMPLER_RUNTIME, POOL_RUNTIME] {
        assert!(!uses_opcode(code, PUSH0), "built for paris: no PUSH0");
        assert!(!uses_opcode(code, MCOPY), "built for paris: no MCOPY");
    }
    assert_eq!(FlammSampler::sampleCall::SELECTOR, [0x61, 0x1b, 0x77, 0xfc]);
    assert_eq!(FlammSampler::TAGCall::SELECTOR, [0x47, 0x22, 0x1a, 0xc7]);
    assert_eq!(previewSwapCall::SELECTOR, [0xbb, 0xba, 0xff, 0xab]);
    assert_eq!(pairRequestCall::SELECTOR, [0x6a, 0x19, 0x82, 0x09]);
    assert_eq!(reason_of("ReadStarved()"), 0x6752_1dd6);
    assert_eq!(reason_of("GasShort()"), 0x882f_2cb8);

    let evm = Evm::new(&Fake::base());
    let data = FlammSampler::TAGCall {}.abi_encode();
    let ExecutionResult::Success { output: Output::Call(output), .. } =
        evm.call(Address::ZERO, taker(), data.into(), 100_000)
    else {
        panic!("TAG() did not return");
    };
    let mut tag = [0u8; 32];
    tag[..4].copy_from_slice(&SAMPLER_TAG);
    assert_eq!(output[..], tag[..]);
}

/// The pool and its hook see the address whose code the sampler is, so they price as they do for
/// the router's swap; from anywhere else the pool refuses every probe and nothing is read.
#[test]
fn the_pool_sees_the_taker_as_its_caller() {
    let fake = Fake::base().fault(Fault::Caller);
    let evm = Evm::new(&fake);
    let asks = fake.asks(Seeds::Sweep);
    // `sample` asks that every call is a STATICCALL of the pool from the address sampled at
    let at_taker = evm.sample(taker(), &asks, &limits(), DEFAULT_SAMPLE_GAS);
    for (side, walk) in SIDES.into_iter().zip(at_taker.walks()) {
        assert!(
            walk.probes
                .iter()
                .all(|probe| status(probe) == Status::Priced),
            "{walk:?}"
        );
        assert_eq!(read(walk), fake.request(side));
        let requests = at_taker.calls_of(pairRequestCall::SELECTOR, side);
        let [request] = requests.as_slice() else { panic!("{} requests", requests.len()) };
        assert_eq!(
            Address::from_word(word(&request.output, 0).into()),
            taker(),
            "the request's word 0, its sender"
        );
    }

    let not_taker = FixedBytes::from(reason_of("NotTaker()").to_be_bytes());
    let elsewhere = evm.sample(ELSEWHERE, &asks, &limits(), DEFAULT_SAMPLE_GAS);
    assert!(!elsewhere.calls.is_empty());
    for walk in elsewhere.walks() {
        assert_eq!(read(walk), Request::default(), "elsewhere nothing is read");
        assert_eq!(walk.probes.len(), MAX_PROBES_PER_DIRECTION);
        for probe in &walk.probes {
            assert_eq!((status(probe), probe.reason), (Status::Refused, not_taker));
        }
    }
}

/// However the pool answers, the sampler answers: each answer classed, the walk stopped where the
/// class says, and the callee held to its cap.
#[rstest]
#[case::a_burner(Fake::base().fault(Fault::Burn), Status::Starved, 0, 1, false)]
#[case::a_burner_that_leaves_680k(
    Fake::base().fault(Fault::Burn).with(BURN_LEAVES, BURNER_LEAVES),
    Status::Priced,
    0,
    5,
    true
)]
#[case::a_return_bomb_of_1_mib(
    Fake::base().fault(Fault::ReturnBomb).with(BOMB_BYTES, MIB),
    Status::Malformed,
    0,
    1,
    false
)]
#[case::a_revert_bomb_of_1_mib(
    Fake::base().fault(Fault::RevertBomb).with(BOMB_BYTES, MIB),
    Status::Refused,
    0,
    9,
    true
)]
#[case::a_64_byte_answer(Fake::base().fault(Fault::Short), Status::Malformed, 0, 1, false)]
#[case::a_read_short_of_gas(
    Fake::base().fault(Fault::Reason).with(REVERT_REASON, reason_of("ReadStarved()")),
    Status::Starved,
    reason_of("ReadStarved()"),
    1,
    false
)]
#[case::a_gas_short_revert(
    Fake::base().fault(Fault::Reason).with(REVERT_REASON, reason_of("GasShort()")),
    Status::Starved,
    reason_of("GasShort()"),
    1,
    false
)]
#[case::a_revert_carrying_the_samplers_tag(
    Fake::base().fault(Fault::Reason).with(REVERT_REASON, tag_reason()),
    Status::Refused,
    tag_reason(),
    MAX_PROBES_PER_DIRECTION,
    false
)]
#[case::a_bare_revert(
    Fake::base().fault(Fault::Bare),
    Status::Refused,
    0,
    MAX_PROBES_PER_DIRECTION,
    false
)]
fn a_hostile_pool_never_makes_the_sampler_revert(
    #[case] fake: Fake,
    #[case] want: Status,
    #[case] reason: u32,
    #[case] probes: usize,
    #[case] truncated: bool,
) {
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let reason = FixedBytes::from(reason.to_be_bytes());
    for walk in sampled.walks() {
        assert_eq!((walk.probes.len(), walk.truncated), (probes, truncated), "{walk:?}");
        for probe in &walk.probes {
            assert_eq!((status(probe), probe.reason), (want, reason), "{probe:?}");
        }
    }
    let [sale, buy] = sampled.walks() else { panic!("two walks") };
    println!(
        "{} + {} probes, first gas {} and {}, call {}, answer {} bytes",
        sale.probes.len(),
        buy.probes.len(),
        sale.probes[0].gasUsed,
        buy.probes[0].gasUsed,
        sampled.gas_used,
        sampled.size
    );
}

/// A burner's callee is held to its cap, and the sampler's count of the call adds only the call's
/// own cost: a cold account's access on the first call to the pool, a warm one's on the next.
#[test]
fn a_burner_costs_its_cap_and_the_calls_access() {
    let fake = Fake::base().fault(Fault::Burn);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let spent: Vec<U256> = sampled
        .walks()
        .iter()
        .map(|walk| walk.probes[0].gasUsed)
        .collect();
    assert_eq!(
        spent,
        [
            U256::from(DEFAULT_PROBE_GAS + CALL_OVERHEAD),
            U256::from(DEFAULT_PROBE_GAS + WARM_CALL_OVERHEAD)
        ]
    );
    assert_eq!(sampled.calls.len(), 2);
    for call in &sampled.calls {
        assert_eq!((call.gas_limit, call.gas_used), (DEFAULT_PROBE_GAS, DEFAULT_PROBE_GAS));
    }
}

/// The sampler copies at most 96 bytes of an answer: the callee pays for a bomb's memory, and the
/// call costs little more than its probes, where a copy would cost the bomb's size again.
#[test]
fn a_return_bomb_is_never_copied() {
    let fake = Fake::base()
        .fault(Fault::ReturnBomb)
        .with(BOMB_BYTES, MIB);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let spent: u64 = sampled
        .walks()
        .iter()
        .flat_map(|walk| &walk.probes)
        .map(|probe| probe.gasUsed.to::<u64>())
        .sum();
    println!("a return bomb: probes {spent} gas, the call {}", sampled.gas_used);
    assert!(sampled.gas_used < spent + 200_000, "{} for probes of {spent}", sampled.gas_used);
}

#[rstest]
#[case::the_caller_check(Fake::base().fault(Fault::Caller))]
#[case::a_burner(Fake::base().fault(Fault::Burn))]
#[case::a_burner_that_leaves_gas(Fake::base().fault(Fault::Burn).with(BURN_LEAVES, BURNER_LEAVES))]
#[case::a_return_bomb(Fake::base().fault(Fault::ReturnBomb).with(BOMB_BYTES, MIB))]
#[case::a_revert_bomb(Fake::base().fault(Fault::RevertBomb).with(BOMB_BYTES, MIB))]
#[case::a_64_byte_answer(Fake::base().fault(Fault::Short))]
#[case::a_bare_revert(Fake::base().fault(Fault::Bare))]
#[case::equal_labels(Fake::base().fault(Fault::EqualFee).with(SALE + EDGE, 15_000_000u64))]
#[case::the_core_band(Fake::base().fault(Fault::CoreBand))]
#[case::used_within_1_bp(Fake::base().fault(Fault::UsedOneBp))]
#[case::a_request_that_reverts(Fake::base().fault(Fault::RequestRevert))]
#[case::a_lev_state_that_reverts(Fake::base().fault(Fault::LevStateRevert))]
#[case::a_short_lev_state(Fake::base().fault(Fault::LevStateShort))]
#[case::a_dropped_edge(Fake::base().with(SALE + EDGE, 15_000_000u64))]
#[case::hole_rows(hole_row_a())]
#[case::the_far_edge_lottery(Fake::base().with(SALE + JITTER, 2_000u64).with(BUY + JITTER, 2_000u64))]
#[case::a_floor_flicker(floor_flicker())]
#[case::a_fall(Fake::base().with(BUY + FALL_AT, JOIN_AT).with(BUY + FALL_BY, 500_000u64))]
#[case::a_token_ceiling(token_ceiling())]
#[case::a_clip(Fake::base().with(SALE + CLIP, 15_000_000u64).with(SALE + EDGE, 15_000_000u64))]
#[case::every_fault_at_once(every_fault())]
fn no_switch_makes_the_sampler_revert(#[case] fake: Fake) {
    let evm = Evm::new(&fake);
    // `sample` asks that each call answers the tag, the block and a walk per ask
    for seeds in [Seeds::Sweep, Seeds::Rungs] {
        for at in [taker(), ELSEWHERE] {
            evm.sample(at, &fake.asks(seeds), &limits(), DEFAULT_SAMPLE_GAS);
        }
    }
    evm.sample(taker(), &fake.asks(Seeds::Sweep), &refining(), DEFAULT_SAMPLE_GAS);
}

/// At the defaults the gas gate stops no walk, from either seeds, and every call the sampler makes
/// gets the whole cap.
#[rstest]
fn the_gate_passes_at_the_defaults(#[values(Seeds::Rungs, Seeds::Sweep)] seeds: Seeds) {
    let fake = Fake::base();
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(seeds), &limits(), DEFAULT_SAMPLE_GAS);
    assert!(sampled
        .walks()
        .iter()
        .all(|walk| !walk.truncated));
    assert!(sampled
        .calls
        .iter()
        .all(|call| call.gas_limit == DEFAULT_PROBE_GAS));
    let [sale, buy] = sampled.walks() else { panic!("two walks") };
    println!(
        "{seeds:?} seeds: {} + {} probes, {} calls, call gas {}, answer {} bytes",
        sale.probes.len(),
        buy.probes.len(),
        sampled.calls.len(),
        sampled.gas_used,
        sampled.size
    );
}

/// What the gas gate leaves of the walks at a call's budget.
#[derive(Clone, Copy, Debug)]
enum Budget {
    Whole,
    /// The probes each walk makes, the sale's first, as the sampler takes turns between them.
    Part([usize; 2]),
    Nothing,
}

impl Budget {
    /// The probes walk `k` is cut to; `None` where it is whole.
    fn probes(self, k: usize) -> Option<usize> {
        match self {
            Self::Whole => None,
            Self::Part(probes) => Some(probes[k]),
            Self::Nothing => Some(0),
        }
    }
}

/// The gate stops a walk before a call it cannot give the whole cap, so a short budget leaves each
/// walk the probes it made, each answer the whole walk's at its ask, and no read.
#[rstest]
#[case::twelve_million(12_000_000, Budget::Whole)]
#[case::a_few_probes_past_the_gate(PART_GAS, Budget::Part([8, 7]))]
#[case::under_a_first_probe(5_400_000, Budget::Nothing)]
#[case::three_million(3_000_000, Budget::Nothing)]
fn a_short_budget_cuts_the_walks_and_keeps_their_answers(#[case] gas: u64, #[case] want: Budget) {
    let fake = Fake::base();
    let evm = Evm::new(&fake);
    let asks = fake.asks(Seeds::Sweep);
    let whole = evm.sample(taker(), &asks, &limits(), DEFAULT_SAMPLE_GAS);
    let short = evm.sample(taker(), &asks, &limits(), gas);
    for (k, (cut, full)) in short
        .walks()
        .iter()
        .zip(whole.walks())
        .enumerate()
    {
        assert!(!full.truncated, "at the defaults the gate stops no walk");
        match want.probes(k) {
            None => assert_eq!(cut, full, "at {gas} gas the walk is whole"),
            Some(probes) => {
                assert!(cut.truncated, "at {gas} gas the gate stops the walk");
                assert_eq!(cut.bandWad, 0, "at {gas} gas the request is not read");
                assert_eq!(cut.probes.len(), probes, "at {gas} gas");
                for probe in &cut.probes {
                    let same = full
                        .probes
                        .iter()
                        .find(|other| other.ask == probe.ask)
                        .expect("the whole walk probed it");
                    assert_eq!(
                        (same.used, same.out, same.feeWad, same.status, same.reason),
                        (probe.used, probe.out, probe.feeWad, probe.status, probe.reason)
                    );
                }
            }
        }
        println!("at {gas} gas: {} of {} probes", cut.probes.len(), full.probes.len());
    }
}

/// The least call gas the feed sends is the gate at the largest cap a pool is raised to, plus
/// 100,000 for what a call spends before its first probe. At each cap a first probe needs more
/// than the plain gate and no more than that margin over it, from either seeds; at the floor a
/// first probe gets the largest cap whole.
#[test]
fn the_floor_covers_what_a_call_spends_before_its_first_probe() {
    let floor = SAMPLE_GAS_FLOOR;
    assert_eq!(floor, 20_717_460);
    let fake = Fake::base();
    let evm = Evm::new(&fake);
    for seeds in [Seeds::Rungs, Seeds::Sweep] {
        let asks = fake.asks(seeds);
        let whole = evm.sample(taker(), &asks, &limits(), DEFAULT_SAMPLE_GAS);
        for probe_gas in [DEFAULT_PROBE_GAS, MAX_PROBE_GAS] {
            let gate = probe_gas * 64 / 63 + SAMPLER_RESERVE_GAS;
            let first = |gas: u64| {
                evm.sample(taker(), &asks, &capped(probe_gas), gas)
                    .walks()[0]
                    .probes
                    .len()
            };
            assert_eq!(first(gate), 0, "at the plain gate the call's own costs come first");
            let (mut lo, mut hi) = (gate, gate + 100_000);
            assert!(first(hi) > 0, "the floor's margin covers them");
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if first(mid) > 0 {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            println!(
                "{seeds:?} seeds, a cap of {probe_gas}: a first probe needs {hi} call gas, {} over \
                 the plain gate {gate}",
                hi - gate
            );
            let at_defaults = evm.sample(taker(), &asks, &capped(probe_gas), DEFAULT_SAMPLE_GAS);
            for (walk, full) in at_defaults
                .walks()
                .iter()
                .zip(whole.walks())
            {
                assert!(
                    !walk.truncated,
                    "at the default call gas a cap of {probe_gas} stops no walk"
                );
                assert_eq!(walk.probes.len(), full.probes.len());
            }
        }
        let at_floor = evm.sample(taker(), &asks, &capped(MAX_PROBE_GAS), floor);
        assert!(!at_floor.walks()[0].probes.is_empty());
        assert!(at_floor
            .calls
            .iter()
            .all(|call| call.gas_limit == MAX_PROBE_GAS));
    }
}

/// A truncated walk is a usable ladder: on a curve alone, where nothing needs the request, it
/// settles and quotes under the pool.
#[test]
fn a_truncated_walk_settles_under_the_pool() {
    let fake = Fake::base()
        .with(SALE + LEVER_R, 0u64)
        .with(BUY + LEVER_R, 0u64);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), PART_GAS);
    for (side, walk) in SIDES.into_iter().zip(sampled.walks()) {
        assert!(walk.truncated);
        let ladder = settle(&probes_of(walk), &fake.terms(side, walk)).ladder;
        let mut previews = Previews::new(&evm, side);
        let switches = switches_of(&mut previews, walk);
        let res = audit(&mut previews, &ladder, &switches);
        println!(
            "a curve alone, truncated at {PART_GAS} gas, {side:?}: {} probes, {res}",
            walk.probes.len()
        );
        assert!(res.rungs > 0);
        assert_eq!(res.over, 0, "{res}");
    }
}

/// Both walks carry a tier label over dust, so each reads the request once, at the lowest ask a
/// tier label pays at least `DUST_OUT` at, and keeps its words; the buy reads the hook's bid from
/// the request's 26 words as they came; neither reads the curve's fee, as each shows two labels.
#[test]
fn the_request_is_read_once_a_walk() {
    let fake = Fake::base();
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let tiers = fake.row().tiers();
    for (side, walk) in SIDES.into_iter().zip(sampled.walks()) {
        let requests = sampled.calls_of(pairRequestCall::SELECTOR, side);
        let [request] = requests.as_slice() else { panic!("{} requests", requests.len()) };
        assert_eq!(request.output.len(), 32 * 26);
        assert_eq!(Address::from_word(word(&request.output, 0).into()), taker());
        let index_in = match side {
            Side::Sale => 0u64,
            Side::Buy => 1,
        };
        assert_eq!(word(&request.output, 3), U256::from(index_in), "word 3 is 1 on a buy");
        let lowest = walk
            .probes
            .iter()
            .find(|probe| {
                matches!(status(probe), Status::Priced | Status::Clipped) &&
                    probe.out >= U256::from(DUST_OUT) &&
                    tiers.contains(probe.feeWad)
            })
            .expect("a tier label pays");
        assert_eq!(request.arg(2), lowest.ask);
        assert_eq!(read(walk), fake.request(side));
        assert_eq!(walk.curveFeeWad, U256::ZERO, "two labels: no curve fee");
        assert!(sampled
            .calls_of(QUOTE_PAIR_SELECTOR, side)
            .is_empty());
        let lev_states = sampled.calls_of(LEV_STATE_SELECTOR, side);
        match side {
            Side::Sale => assert!(lev_states.is_empty(), "a sale reads no bid"),
            Side::Buy => {
                let [lev_state] = lev_states.as_slice() else { panic!("{lev_states:?}") };
                assert_eq!(lev_state.input[..4], LEV_STATE_SELECTOR);
                assert_eq!(lev_state.input[4..], request.output[..]);
                assert_eq!(word(&lev_state.output, 6), walk.bidWad);
                assert_eq!(walk.bidWad, fake.get(BID));
            }
        }
    }
}

/// A walk of one tier label reads the curve's fee once, on the request's words with word 13, the
/// input, zeroed.
#[test]
fn one_tier_label_reads_the_curve_fee_on_the_request_less_its_input() {
    let fake = Fake::base().with(SALE + CURVE_R, 0u64);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let requests = sampled.calls_of(pairRequestCall::SELECTOR, Side::Sale);
    let [request] = requests.as_slice() else { panic!("{} requests", requests.len()) };
    let quotes = sampled.calls_of(QUOTE_PAIR_SELECTOR, Side::Sale);
    let [quote] = quotes.as_slice() else { panic!("{quotes:?}") };
    let mut zeroed = request.output.to_vec();
    zeroed[13 * 32..14 * 32].fill(0);
    assert_ne!(zeroed[..], request.output[..]);
    assert_eq!(quote.input[..4], QUOTE_PAIR_SELECTOR);
    assert_eq!(quote.input[4..], zeroed[..]);
    assert_eq!(sampled.walks()[0].curveFeeWad, fake.get(CURVE_FEE));
}

/// What a failed read keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kept {
    Nothing,
    TheRequest,
}

/// A failed read leaves its words zero.
#[rstest]
#[case::a_request_that_reverts(Fault::RequestRevert, Kept::Nothing)]
#[case::a_lev_state_that_reverts(Fault::LevStateRevert, Kept::TheRequest)]
#[case::a_lev_state_of_seven_words(Fault::LevStateShort, Kept::TheRequest)]
fn a_failed_read_leaves_its_words_zero(#[case] fault: Fault, #[case] kept: Kept) {
    let fake = Fake::base().fault(fault);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    for (side, walk) in SIDES.into_iter().zip(sampled.walks()) {
        let want = match kept {
            Kept::Nothing => Request::default(),
            Kept::TheRequest => Request { bid_wad: U256::ZERO, ..fake.request(side) },
        };
        assert_eq!(read(walk), want, "{side:?}");
        assert_eq!(walk.curveFeeWad, U256::ZERO);
        assert_eq!(
            sampled
                .calls_of(pairRequestCall::SELECTOR, side)
                .len(),
            1
        );
        let want_reads = match (kept, side) {
            (Kept::Nothing, _) | (Kept::TheRequest, Side::Sale) => 0,
            (Kept::TheRequest, Side::Buy) => 1,
        };
        assert_eq!(
            sampled
                .calls_of(LEV_STATE_SELECTOR, side)
                .len(),
            want_reads
        );
    }
}

/// A buy where the curve wins only under `DUST_OUT` and leverage above it: the dust probe on the
/// curve's label does not keep the curve's fee from being read, and with the budget spent by the
/// seeds the probe above still runs, one past it.
#[test]
fn a_dust_probe_on_the_curve_does_not_block_the_curve_fee() {
    let fake = Fake::base()
        .with(BUY + CURVE_R, 7_000u64)
        .with(BUY + CURVE_X, 5_000_000u64)
        .with(BUY + LEVER_R, 1_320_000_000_000u64)
        .with(BUY + LEVER_X, 1_000_000_000_000_000u64);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let buy = &sampled.walks()[1];
    let curve = fake.get(CURVE_FEE);
    let (dust, over): (Vec<_>, Vec<_>) = buy
        .probes
        .iter()
        .filter(|probe| matches!(status(probe), Status::Priced | Status::Clipped))
        .partition(|probe| probe.out < U256::from(DUST_OUT));
    assert!(dust
        .iter()
        .any(|probe| probe.feeWad == curve));
    assert!(!over.is_empty());
    assert!(over
        .iter()
        .all(|probe| probe.feeWad == fake.get(P_SPREAD_DN)));
    assert_eq!(buy.curveFeeWad, curve);
    assert_eq!(
        sampled
            .calls_of(QUOTE_PAIR_SELECTOR, Side::Buy)
            .len(),
        1
    );
    assert_eq!(
        buy.probes.len(),
        MAX_PROBES_PER_DIRECTION + 1,
        "the probe above, past a spent budget"
    );
    let last = buy.probes[MAX_PROBES_PER_DIRECTION].ask;
    let top = buy
        .probes
        .iter()
        .rev()
        .filter(|probe| probe.ask != last)
        .find(|probe| good(probe))
        .expect("a priced top");
    assert_eq!(above(top), last, "at used + (used >> 10) + 1 of the top");
    println!("the probe above on the buy at {last}");
}

/// A buy whose leverage wins only on dust reads nothing; the sale still reads.
#[test]
fn nothing_is_read_where_tier_labels_pay_only_dust() {
    let fake = Fake::base()
        .with(BUY + LEVER_R, 7_000u64)
        .with(BUY + LEVER_X, 5_000_000u64);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let [sale, buy] = sampled.walks() else { panic!("two walks") };
    assert_eq!(read(buy), Request::default());
    assert!(sampled
        .calls_of(pairRequestCall::SELECTOR, Side::Buy)
        .is_empty());
    assert_ne!(sale.bandWad, 0);
}

/// Whole-unit outputs under an 8 % band: small buys alternate between a price and a refusal, and
/// those refusals are dust, so the walk goes on past them through its whole budget of seeds.
#[test]
fn a_walk_reaches_past_the_dust_windows() {
    let fake = Fake::base()
        .fault(Fault::CoreBand)
        .with(BUY + LEVER_R, 0u64);
    let evm = Evm::new(&fake);
    let asks = fake.asks(Seeds::Sweep);
    let sampled = evm.sample(taker(), &asks, &limits(), DEFAULT_SAMPLE_GAS);
    let buy = &sampled.walks()[1];
    let head: Vec<Status> = buy.probes[..4]
        .iter()
        .map(status)
        .collect();
    assert_eq!(head, [Status::Refused, Status::Priced, Status::Refused, Status::Priced]);
    let asked: Vec<U256> = buy
        .probes
        .iter()
        .map(|probe| probe.ask)
        .collect();
    assert_eq!(asked, asks[1].seeds[..MAX_PROBES_PER_DIRECTION]);
    let top = buy
        .probes
        .iter()
        .rev()
        .find(|probe| good(probe))
        .expect("a priced top");
    assert!(top.out > U256::from(100_000_000), "the walk reaches past the windows: {top:?}");
    println!("dust windows: the buy walk's top pays {}", top.out);
}

/// Leverage alone on the sale: every seed priced on a tier label, the budget spent, and the probe
/// above made last, once, at `used + (used >> 10) + 1` of the top tier price. The buy's top is the
/// curve's, so nothing runs above it.
#[test]
fn the_probe_above_runs_once_over_a_tier_labelled_top() {
    let fake = Fake::base().with(SALE + CURVE_R, 0u64);
    let evm = Evm::new(&fake);
    let sampled = evm.sample(taker(), &fake.asks(Seeds::Sweep), &limits(), DEFAULT_SAMPLE_GAS);
    let [sale, buy] = sampled.walks() else { panic!("two walks") };
    assert_eq!(sale.probes.len(), MAX_PROBES_PER_DIRECTION + 1);
    let asks = sampled.preview_asks(Side::Sale);
    let last = *asks.last().expect("probes");
    assert_eq!(
        asks.iter()
            .filter(|ask| **ask == last)
            .count(),
        1,
        "the probe above runs once"
    );
    let top = sale
        .probes
        .iter()
        .rev()
        .filter(|probe| probe.ask != last)
        .find(|probe| good(probe))
        .expect("a priced top");
    assert_eq!(top.feeWad, fake.get(P_SPREAD));
    assert_eq!(above(top), last);
    println!("the probe above at {last}");

    let top = buy
        .probes
        .iter()
        .rev()
        .find(|probe| good(probe))
        .expect("a priced top");
    assert_eq!(top.feeWad, fake.get(CURVE_FEE));
    assert!(!buy
        .probes
        .iter()
        .any(|probe| probe.ask == above(top)));
}

/// An ask at or past `2^120` is never made, so a tier-labelled top at `2^120 - 2^100` asks nothing
/// above it.
#[test]
fn the_probe_above_is_never_asked_at_or_past_2_to_the_120() {
    let fake = Fake::base()
        .with(SALE + CURVE_R, 0u64)
        .with(SALE + LEVER_R, U256::from(1) << 119)
        .with(SALE + LEVER_X, U256::from(1) << 120);
    let evm = Evm::new(&fake);
    let top = (U256::from(1) << 120) - (U256::from(1) << 100);
    let seeds = vec![pow10(6), pow10(12), pow10(20), pow10(30), top];
    let asks = [fake.ask(Side::Sale, seeds.clone()), fake.ask(Side::Buy, sweep(6))];
    let sampled = evm.sample(taker(), &asks, &limits(), DEFAULT_SAMPLE_GAS);
    let sale = &sampled.walks()[0];
    let asked: Vec<U256> = sale
        .probes
        .iter()
        .map(|probe| probe.ask)
        .collect();
    assert_eq!(asked, seeds);
    let highest = &sale.probes[seeds.len() - 1];
    assert_eq!((status(highest), highest.feeWad), (Status::Priced, fake.get(P_SPREAD)));
}

/// Refine splits a span at the midpoint of its asks, which differs from the midpoint of `used`
/// where `used` sits 1 bp under the ask, and the probe above still comes last. This feed's
/// settings leave refine off.
#[test]
fn refine_splits_at_the_midpoint_of_the_asks() {
    let fake = Fake::base()
        .fault(Fault::UsedOneBp)
        .with(SALE + CURVE_R, 0u64);
    let evm = Evm::new(&fake);
    let (low, high) = (U256::from(1_000_000), U256::from(30_000_000));
    let asks = [
        fake.ask(Side::Sale, vec![low, high, U256::from(90_000_000)]),
        fake.ask(Side::Buy, vec![U256::from(10_000_000)]),
    ];
    let less_1_bp = |ask: U256| ask - ask / U256::from(10_000);
    let refined = evm.sample(taker(), &asks, &refining(), DEFAULT_SAMPLE_GAS);
    let sale = &refined.walks()[0];
    for probe in sale
        .probes
        .iter()
        .filter(|probe| status(probe) == Status::Priced)
    {
        assert_eq!(probe.used, less_1_bp(probe.ask));
    }
    let asked: BTreeSet<U256> = sale
        .probes
        .iter()
        .map(|probe| probe.ask)
        .collect();
    let mid_ask = (low + high) / U256::from(2);
    let mid_used = (less_1_bp(low) + less_1_bp(high)) / U256::from(2);
    assert_eq!((mid_ask, mid_used), (U256::from(15_500_000), U256::from(15_498_450)));
    assert!(asked.contains(&mid_ask), "the midpoint of the asks 1e6 and 3e7");
    assert!(!asked.contains(&mid_used), "not the midpoint of their used");
    let top = sale
        .probes
        .iter()
        .rev()
        .skip(1)
        .find(|probe| good(probe))
        .expect("a priced top");
    assert_eq!(refined.preview_asks(Side::Sale).last(), Some(&above(top)));

    let plain = evm.sample(taker(), &asks, &limits(), DEFAULT_SAMPLE_GAS);
    assert!(!plain.walks()[0]
        .probes
        .iter()
        .any(|probe| probe.ask == mid_ask));
}

/// The plain states: the rule stays under the pool from the feed's rungs and from the sweep.
#[rstest]
#[case::leverage_under_the_curve_on_a_sale(Fake::base(), Side::Sale)]
#[case::leverage_under_the_curve_on_a_buy(Fake::base(), Side::Buy)]
#[case::a_dropped_edge(Fake::base().with(SALE + EDGE, 15_000_000u64), Side::Sale)]
#[case::a_clip_reasked_onto_leverage(
    Fake::base().with(SALE + CLIP, 15_000_000u64).with(SALE + EDGE, 15_000_000u64),
    Side::Sale
)]
#[case::dust_windows_on_a_buy(Fake::base().fault(Fault::CoreBand).with(BUY + LEVER_R, 0u64), Side::Buy)]
fn the_rule_stays_under_the_pool(
    #[case] fake: Fake,
    #[case] side: Side,
    #[values(Seeds::Rungs, Seeds::Sweep)] seeds: Seeds,
) {
    let seeds = fake.seeds(side, seeds);
    assert_case(Case::new(fake, side, seeds));
}

/// Leverage answers by lottery within a jitter of the band's far line, at or under the room test's
/// fixed part, and the curve fills where it refuses: the rule stays under the pool, and so does
/// either of its two tests alone; with both off, chords between the zone's tier sizes cross holes
/// the curve fills, and go over.
#[test]
fn the_far_edge_lottery_stays_under_the_pool_through_either_test() {
    let fake = Fake::base()
        .with(SALE + CURVE_R, 740_000_000_000u64)
        .with(SALE + CURVE_X, 1_000_000_000u64)
        .with(SALE + JITTER, 2_000u64);
    let evm = Evm::new(&fake);
    let mut previews = Previews::new(&evm, Side::Sale);
    let zone = lottery_zone(
        &mut previews,
        U256::from(100_000_000),
        U256::from(103_000_000),
        1_500,
        fake.get(P_SPREAD),
    );
    println!("lottery zone: {} tier-labelled sizes, {} holes", zone.tiers.len(), zone.holes.len());
    assert!(!zone.holes.is_empty());
    let top = &zone.tiers[zone.tiers.len().saturating_sub(8)..];
    let mut seeds = sweep_under(8, zone.tiers[0], MAX_PROBES_PER_DIRECTION - top.len());
    seeds.extend(top);
    let mut case = Case::new(fake, Side::Sale, seeds);
    case.extra = zone
        .holes
        .iter()
        .chain(&zone.tiers)
        .copied()
        .collect();
    case.covers = zone.holes;
    case.controls =
        vec![(Control::RoomOff, false), (Control::AboveOff, false), (Control::BothOff, true)];
    assert_case(case);
}

/// The same lottery on an 18/18 pair at 3,000 USD, its jitter past the margin's fixed part and
/// inside the whole margin, with rungs forced whose room passes the fixed part and not the whole
/// margin, a hole between each pair: only the whole margin keeps them off the ladder.
#[test]
fn the_far_edge_lottery_on_18_decimals_stays_under_the_pool_through_the_whole_margin() {
    let e = |exponent: u32| 10u128.pow(exponent);
    let fake = Fake::priced([18, 18], 3_000)
        .with(SALE + CURVE_R, 2_900 * e(23))
        .with(SALE + CURVE_X, e(23))
        .with(SALE + LEVER_R, 2_980 * 3 * e(23))
        .with(SALE + LEVER_X, 3 * e(23))
        .with(SALE + JITTER, 15_000u64)
        .with(LEGS_0, e(22))
        .with(LEGS_0 + 2, 3 * e(21))
        .with(LEGS_1 + 3, e(25))
        .with(CV, 6 * e(25));
    let evm = Evm::new(&fake);
    let mut previews = Previews::new(&evm, Side::Sale);
    let zone = lottery_zone(
        &mut previews,
        U256::from(13_600 * e(18)),
        U256::from(13_760 * e(18)),
        1_600,
        fake.get(P_SPREAD),
    );
    let room = SaleRoom::of(&fake);
    let mut between = Vec::new();
    for &size in &zone.tiers {
        let (used, out, _) = previews
            .at(size)
            .expect("a tier size answers");
        let least = room.least(used, out);
        if room.need < least && least < room.margin(used) {
            between.push(size);
        }
    }
    let pairs: Vec<(U256, U256)> = between
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|&(lo, hi)| {
            zone.holes
                .iter()
                .any(|&hole| lo < hole && hole < hi)
        })
        .take(3)
        .collect();
    let forced: BTreeSet<U256> = pairs
        .iter()
        .flat_map(|&(lo, hi)| [lo, hi])
        .collect();
    println!(
        "lottery zone: {} tier-labelled sizes, {} holes, need {}, {} rungs between the need and \
         the margin, rungs forced {forced:?}",
        zone.tiers.len(),
        zone.holes.len(),
        room.need,
        between.len()
    );
    assert!(!forced.is_empty(), "no rungs between the fixed part and the margin");
    // the largest of the sweep under the zone, so that every forced rung fits the budget
    let mut seeds = sweep_under(18, zone.tiers[0], MAX_PROBES_PER_DIRECTION - forced.len());
    seeds.extend(&forced);
    let mut case = Case::new(fake, Side::Sale, seeds);
    case.extra = zone
        .holes
        .iter()
        .chain(&zone.tiers)
        .copied()
        .collect();
    for (lo, hi) in pairs {
        let mut size = lo;
        while size <= hi {
            case.extra.push(size);
            size += U256::from(1);
        }
    }
    case.covers = forced.into_iter().collect();
    case.controls = vec![(Control::NeedOnly, true), (Control::AboveOff, false)];
    assert_case(case);
}

/// Leverage refuses one size between two it prices, and the curve fills there: the probe above
/// lands on the curve, whose ray lowers the span; without it the chord across that size goes over.
#[rstest]
#[case::a_floor_flicker(floor_flicker())]
#[case::a_token_ceiling(token_ceiling())]
fn a_one_size_refusal_stays_under_the_pool_through_the_probe_above(#[case] fake: Fake) {
    let at = U256::from(SWITCH_AT);
    let one = U256::from(1);
    {
        let evm = Evm::new(&fake);
        let mut previews = Previews::new(&evm, Side::Sale);
        let tier = fake.get(P_SPREAD);
        assert!(previews.labelled(at - one, tier) && previews.labelled(at + one, tier));
        assert!(!previews.labelled(at, tier), "leverage prices the switch");
    }
    let mut seeds = sweep_under(8, at - one, MAX_PROBES_PER_DIRECTION - 2);
    seeds.extend([at - one, at + one]);
    let mut case = Case::new(fake, Side::Sale, seeds);
    case.extra = (0..6u64)
        .map(|k| at - one + U256::from(k))
        .collect();
    case.covers = vec![at];
    case.controls = vec![(Control::AboveOff, true)];
    assert_case(case);
}

/// The lever-down's output falls 500,000 units past a size and stays on its label, and the hook's
/// bid word reads zero: the rule cuts the buy before its tier points, so it quotes nothing; with
/// the word forced positive, the chord across the fall goes over.
#[test]
fn the_join_stays_dark_while_the_bid_word_reads_zero() {
    let fake = Fake::base()
        .with(BUY + FALL_AT, JOIN_AT)
        .with(BUY + FALL_BY, 500_000u64)
        .with(BID, 0u64);
    let at = U256::from(JOIN_AT);
    let seeds = fake.seeds(Side::Buy, Seeds::Sweep);
    let mut case = Case::new(fake, Side::Buy, seeds);
    case.extra = (0..42u64)
        .map(|k| at - U256::from(2) + U256::from(k))
        .chain((3..=9).map(|exponent| at + pow10(exponent)))
        .collect();
    case.dark = true;
    case.controls = vec![(Control::BidForced, true)];
    assert_case(case);
}

/// Leverage refuses every size 3 mod 7 and the curve fills there, a hole a chord over two leverage
/// answers cannot see. Such a row fails its test, so the rule cuts its ladders before their tier
/// points and quotes nothing; trusted anyway, they go over.
#[rstest]
#[case::row_a_on_a_sale(hole_row_a(), Side::Sale)]
#[case::row_a_on_a_buy(hole_row_a(), Side::Buy)]
#[case::row_b_on_a_sale(hole_row_b(), Side::Sale)]
#[case::row_b_on_a_buy(hole_row_b(), Side::Buy)]
fn a_hole_row_fails_its_test_and_stays_dark(#[case] fake: Fake, #[case] side: Side) {
    let every = U256::from(LATTICE);
    // the seeds stepped off the refused lattice
    let seeds: Vec<U256> = fake
        .seeds(side, Seeds::Sweep)
        .into_iter()
        .map(
            |size| {
                if size % every == U256::from(LATTICE_AT) {
                    size + U256::from(1)
                } else {
                    size
                }
            },
        )
        .collect();
    let mut case = Case::new(fake, side, seeds);
    case.extra_of = Some(lattice_above);
    case.gated = true;
    case.dark = true;
    case.controls = vec![(Control::TrustedForced, true)];
    assert_case(case);
}
