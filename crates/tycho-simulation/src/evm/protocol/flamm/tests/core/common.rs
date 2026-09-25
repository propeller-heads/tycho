// Copyright (c) 2026 Everlong Labs Limited

//! Fixture support: the Solidity-generated state dumps (`testdata/core_e2e_*`,
//! `testdata/edges/core_edge_*`, documented in `testdata/README.md` section 4) decoded the way the
//! Go tracker decodes them (`state_reads.go` `flammReads.state` -> `flammState`), into the composed
//! [`Flamm`] state over the real modules: the swap hook's storage, the Router record with every
//! venue's Morpho market, the stateless leverage hook.

#![allow(dead_code)]

use std::str::FromStr;

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Deserializer};

pub use super::super::fixtures::{fixture_lines, read_fixture};
use crate::evm::protocol::flamm::{
    almcurve::Support,
    deps::EverlongLeverageV1,
    fee::FeeParams,
    gate::{self, LoanCfg, Pool},
    hook::HookState,
    morpho::{Market, Position, VenueMarket},
    pricefeed::{FeedRound, FeedToken, PriceFeedState},
    router::{Loan, Router as MmRouter, Venue},
    state::{
        priced, HookKind, LeverageHookSlot, PoolHooks, SpreadHookSlot, SpreadHookState,
        SwapHookSlot,
    },
    Flamm, FlammError,
};

pub type State = Flamm;

pub const E2E_BLOCKS: &[&str] = &["51302915", "51313000", "51324800"];
pub const EDGE_BLOCKS: &[&str] = &["51302915", "51324800", "51326000"];

/// A 256-bit word as the fixtures carry it: a decimal string (the e2e dumps) or a JSON number of
/// any width (the edge dumps); a negative decimal is an `int256` and reads as its two's-complement
/// word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Word(pub U256);

/// A number (a narrow one; the fixture loader quotes every wide one) or a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawWord {
    N(serde_json::Number),
    S(String),
}

impl RawWord {
    fn text(&self) -> String {
        match self {
            Self::N(n) => n.to_string(),
            Self::S(s) => s.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for Word {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        parse_word(&RawWord::deserialize(d)?.text())
            .map(Word)
            .map_err(serde::de::Error::custom)
    }
}

/// An `int256` as the dump writes it (a decimal string or number, `-` for a negative), kept as
/// text so the sign can be checked before the word is read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntWord(pub String);

impl<'de> Deserialize<'de> for IntWord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(IntWord(RawWord::deserialize(d)?.text()))
    }
}

pub fn parse_word(s: &str) -> Result<U256, String> {
    let s = s.trim();
    if let Some(neg) = s.strip_prefix('-') {
        let abs = U256::from_str_radix(neg, 10).map_err(|e| format!("{s}: {e}"))?;
        return Ok(U256::ZERO.wrapping_sub(abs));
    }
    if let Some(h) = s.strip_prefix("0x") {
        return U256::from_str_radix(h, 16).map_err(|e| format!("{s}: {e}"));
    }
    U256::from_str_radix(s, 10).map_err(|e| format!("{s}: {e}"))
}

pub fn addr(s: &str) -> Address {
    Address::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

/// ABI-encoded return data as 32-byte words.
pub fn return_words(h: &str) -> Vec<U256> {
    let b = hex::decode(h.trim_start_matches("0x")).expect("hex");
    b.chunks(32)
        .map(U256::from_be_slice)
        .collect()
}

/// Classifies recorded revert data the way the Go replays do (`core_e2e_test.go` `e2eRevert`,
/// `core_edges_test.go` `coreEdgeRevert`): empty data is `Math.mulDiv`'s bare require, the raw
/// strings `"oracle down"` / `"down"` are the mocked market oracle's / rate model's own reverts
/// bubbled by Morpho, a 36-byte `Panic(uint256)` is a panic, an `Error(string)` is one of Morpho
/// Blue's require strings, and any other data is a custom error by its 4-byte selector, whatever
/// its arguments (`StalePrice(address)`, `FeatureDisabled(uint8)`, ... carry 36 bytes). `None` is
/// unmapped data, which fails the row.
///
/// Everything after the mocks' raw strings is [`FlammError::from_revert_data`], which dispatches
/// on the selector at the recorded length: the panic, the `Error(string)` and the
/// argument-carrying custom errors are one call, not three branches over the length.
pub fn revert_class(h: &str) -> Option<FlammError> {
    let b = hex::decode(h.trim_start_matches("0x")).expect("hex");
    match &b[..] {
        [] => return Some(FlammError::MulDivOverflow),
        b"oracle down" => return Some(FlammError::MorphoOracleReverted),
        b"down" => return Some(FlammError::MorphoIrmReverted),
        _ => {}
    }
    FlammError::from_revert_data(&b)
}

/// The recorded outcome of one call: the return words, or the revert class (`None`: unmapped
/// data).
pub type Chain = Result<Vec<U256>, Option<FlammError>>;

pub fn outcome(r: Option<&Vec<Word>>, e: Option<&String>) -> Chain {
    match (r, e) {
        (_, Some(e)) => Err(revert_class(e)),
        (Some(r), None) => Ok(r.iter().map(|w| w.0).collect()),
        (None, None) => Ok(Vec::new()),
    }
}

pub fn outcome_hex(r: Option<&String>, e: Option<&String>) -> Chain {
    match (r, e) {
        (_, Some(e)) => Err(revert_class(e)),
        (Some(r), None) => Ok(return_words(r)),
        (None, None) => Ok(Vec::new()),
    }
}

// ------------------------------------------------------------------ the dump, in the shape of the
// reads

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reads {
    pub block: u64,
    pub timestamp: u64,
    pub pool: PoolReads,
    pub hook: Option<HookReads>,
    pub spread: Option<SpreadReads>,
    pub feed: FeedReads,
    pub router: RouterReads,
    #[serde(default)]
    pub views: Option<Views>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolReads {
    pub asset: String,
    pub paused: bool,
    pub features: Word,
    pub lev_paused: bool,
    pub hooks: Vec<String>,
    pub physical: Word,
    pub gross: Word,
    pub total_supply: Word,
    pub phi_wad: Word,
    pub ltv_wad: Word,
    pub room_epsilon_wad: Word,
    pub fee_floor_wad: Word,
    pub fee_cap_wad: Word,
    pub loans: Vec<LoanReads>,
    pub last_lever_spread_ppm: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoanReads {
    pub token: String,
    pub decimals: u8,
    pub swap_price_band_wad: Word,
    pub fee_floor_wad: Word,
    pub max_swap_notional: Word,
    pub reserve_target: Word,
    pub liquid: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookReads {
    pub a_wad: Word,
    pub fee: Vec<Word>,
    pub inv_skew_kappa_wad: Word,
    pub inv_skew_band_wad: Word,
    pub support: Vec<Word>,
    pub anchor_sqrt_x96: Word,
    pub reservation_price_wad: Word,
    pub kappa: Word,
    pub x_wad: Word,
    pub reserve_stable: Word,
    pub idle_stable: Word,
    pub reserve_volatile: Word,
    pub idle_volatile: Word,
    pub rv_wad: Word,
    pub loan_scale: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpreadReads {
    pub spread: Word,
    pub max_spread_age: Word,
    pub last_set_ts: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundReads {
    pub ok: bool,
    pub round_id: Word,
    pub answer: Word,
    pub started_at: Word,
    pub updated_at: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedTokenReads {
    pub token: String,
    pub known: bool,
    pub heartbeat: Word,
    pub scale: Word,
    pub unit: Word,
    pub peg_band_wad: Word,
    pub round: RoundReads,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedReads {
    pub sequencer_feed: String,
    pub sequencer_grace: Word,
    pub sequencer: RoundReads,
    pub tokens: Vec<FeedTokenReads>,
}

/// `state_reads.go` `routerLoanReads`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterLoanReads {
    pub decimals: u8,
    pub loan_scale: Word,
    pub debt_cap: Word,
    pub supply_cap: Word,
    pub borrow_enabled: bool,
    pub retired: bool,
}

/// `morpho.go` `mmMarket`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketReads {
    pub tsa: Word,
    pub tss: Word,
    pub tba: Word,
    pub tbs: Word,
    pub last_update: Word,
    pub fee: Word,
}

/// `morpho.go` `mmPosition`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionReads {
    pub supply_shares: Word,
    pub borrow_shares: Word,
    pub collateral: Word,
}

/// `state_reads.go` `venueReads`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenueReads {
    pub kind: u8,
    pub loan_index: u8,
    pub lltv_wad: Word,
    pub borrow_enabled: bool,
    pub supply_enabled: bool,
    pub retired: bool,
    pub debt_cap: Word,
    pub supply_cap: Word,
    pub max_borrow_rate_wad: Word,
    pub managed_collateral: Word,
    pub managed_supply_shares: Word,
    pub market: MarketReads,
    pub position: PositionReads,
    pub irm: String,
    pub market_lltv: Word,
    /// `int256`, refused when negative.
    pub rate_at_target: IntWord,
    pub has_irm: bool,
    pub irm_readable: bool,
    pub oracle_ok: bool,
    pub oracle_price: Word,
    pub oracle_zero: bool,
}

/// `state_reads.go` `routerReads`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterReads {
    pub global_paused: bool,
    pub pin_ltv_wad: Word,
    pub safety_gap_wad: Word,
    pub oracle_band_wad: Word,
    pub max_drawn_assets: u8,
    pub loans: Vec<RouterLoanReads>,
    pub borrow_order: Vec<u16>,
    pub supply_order: Vec<u16>,
    pub withdraw_order: Vec<u16>,
    pub repay_order: Vec<u16>,
    pub venues: Vec<VenueReads>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Views {
    pub peek_cross: PeekCrossView,
    pub peg_ok: bool,
    pub loan_position: Vec<Word>,
    pub positions: PositionsView,
    pub total_assets: TotalAssetsView,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeekCrossView {
    pub ok: bool,
    pub price_wad: Word,
    pub ts: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionsView {
    pub coll: Vec<Word>,
    pub sup: Vec<Word>,
    pub debt: Vec<Word>,
    pub total_coll: Word,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotalAssetsView {
    pub v: Option<Word>,
    pub err: Option<String>,
}

/// The c104 hook registry (`hook_registry.go`): kind by address.
pub const C104_SWAP_HOOK: &str = "0x65CBD227cBC61248ae77a5fC813A29C54C092134";
pub const C104_LEVERAGE_HOOK: &str = "0xE0A98d8e60035832B8BaD7f7af7B9B0b3A7308F3";
pub const C104_SPREAD_HOOK: &str = "0x04988aF54ec88D2de77b191025EAef2fe488f93b";
/// The only rate model the port prices (`state_reads.go` `adaptiveCurveIrm`).
pub const ADAPTIVE_CURVE_IRM: &str = "0x46415998764C29aB2a25CbeA6254146D50D22687";

/// A state dump decoded from its JSON value (the edge dumps carry 256-bit words as JSON numbers,
/// which the fixture loader has already quoted, so a wide word reads as its decimal string).
pub fn decode_reads(raw: &serde_json::Value) -> Reads {
    serde_json::from_value(raw.clone()).expect("state dump")
}

/// `flammReads.state` (`state_reads.go`) over the real modules, with its consistency refusals as
/// panics (a dump this harness cannot evaluate is a harness bug, not a row).
pub fn build_state(r: &Reads) -> State {
    let p = &r.pool;
    let n = p.loans.len();
    assert!(n > 0, "a pool with no loan asset");
    assert_eq!(r.router.loans.len(), n, "the Router's loan set disagrees with the pool's");
    let mut loans = Vec::with_capacity(n);
    for (i, l) in p.loans.iter().enumerate() {
        assert!(l.decimals <= 18);
        let scale = U256::from(10u64).pow(U256::from(18 - l.decimals));
        assert_eq!(r.router.loans[i].loan_scale.0, scale, "loan {i}: the Router's scale");
        loans.push(LoanCfg {
            token: addr(&l.token),
            scale,
            swap_price_band_wad: l.swap_price_band_wad.0,
            fee_floor_wad: l.fee_floor_wad.0,
            max_swap_notional: l.max_swap_notional.0,
            reserve_target: l.reserve_target.0,
            liquid: l.liquid.0,
        });
    }
    let pool = Pool {
        physical: p.physical.0,
        loans,
        ltv_wad: p.ltv_wad.0,
        phi_wad: p.phi_wad.0,
        room_epsilon_wad: p.room_epsilon_wad.0,
        features: p.features.0,
        price_wad: Vec::new(),
        cross_wad: Vec::new(),
    };

    let mut addrs = [Address::ZERO; 7];
    for (i, a) in p.hooks.iter().enumerate().take(7) {
        addrs[i] = addr(a);
    }
    // `everlongSwapV1.build`: the hook's storage; its LOAN_SCALE must be loan asset 0's.
    let swap = match (&r.hook, addrs[0] == addr(C104_SWAP_HOOK)) {
        (Some(h), true) => {
            assert_eq!(h.fee.len(), 8);
            assert_eq!(h.support.len(), 4);
            assert_eq!(h.loan_scale.0, pool.loans[0].scale, "the hook's LOAN_SCALE");
            SwapHookSlot {
                kind: HookKind::EverlongSwapV1,
                everlong_swap: Some(HookState {
                    a_wad: h.a_wad.0,
                    support: Support {
                        a_wad: h.support[0].0,
                        x_lo: h.support[1].0,
                        x_hi: h.support[2].0,
                        y_hi: h.support[3].0,
                    },
                    anchor_sqrt_x96: h.anchor_sqrt_x96.0,
                    reservation_price_wad: h.reservation_price_wad.0,
                    kappa: h.kappa.0,
                    x_wad: h.x_wad.0,
                    reserve_stable: h.reserve_stable.0,
                    idle_stable: h.idle_stable.0,
                    reserve_volatile: h.reserve_volatile.0,
                    idle_volatile: h.idle_volatile.0,
                    rv_wad: h.rv_wad.0,
                    fee: FeeParams {
                        mid_fee_wad: h.fee[0].0,
                        out_fee_wad: h.fee[1].0,
                        gamma_wad: h.fee[2].0,
                        sigma_ref_wad: h.fee[3].0,
                        vol_beta_wad: h.fee[4].0,
                        vol_min_wad: h.fee[5].0,
                        vol_max_wad: h.fee[6].0,
                        dir_skew_wad: h.fee[7].0,
                    },
                    inv_skew_kappa_wad: h.inv_skew_kappa_wad.0,
                    inv_skew_band_wad: h.inv_skew_band_wad.0,
                    loan_scale: h.loan_scale.0,
                }),
            }
        }
        _ => panic!("a dump without the c104 swap hook"),
    };
    let leverage = if addrs[4] == addr(C104_LEVERAGE_HOOK) {
        LeverageHookSlot {
            kind: HookKind::EverlongLeverageV1,
            everlong_leverage: Some(EverlongLeverageV1 {}),
        }
    } else {
        assert_eq!(addrs[4], Address::ZERO, "an unknown leverage hook");
        LeverageHookSlot::default()
    };
    let spread = match (&r.spread, addrs[5] == addr(C104_SPREAD_HOOK)) {
        (Some(s), true) => SpreadHookSlot {
            kind: HookKind::EverlongSpreadV1,
            everlong_spread: Some(SpreadHookState {
                spread: s.spread.0,
                max_spread_age: s.max_spread_age.0,
                last_set_ts: s.last_set_ts.0,
            }),
        },
        (None, false) => SpreadHookSlot::default(),
        _ => panic!("spread reads and the listed spread hook disagree"),
    };

    let round = |x: &RoundReads| FeedRound {
        ok: x.ok,
        round_id: x.round_id.0,
        answer: x.answer.0,
        started_at: x.started_at.0,
        updated_at: x.updated_at.0,
    };
    let token = |a: Address| -> FeedToken {
        for t in &r.feed.tokens {
            if addr(&t.token) == a && t.known {
                return FeedToken {
                    known: true,
                    heartbeat: t.heartbeat.0,
                    scale: t.scale.0,
                    unit: t.unit.0,
                    peg_band_wad: t.peg_band_wad.0,
                    round: round(&t.round),
                };
            }
        }
        FeedToken::default()
    };
    let pool_asset = addr(&p.asset);
    let feed = PriceFeedState {
        has_sequencer: addr(&r.feed.sequencer_feed) != Address::ZERO,
        sequencer_grace: r.feed.sequencer_grace.0,
        sequencer: round(&r.feed.sequencer),
        asset: token(pool_asset),
        loans: pool
            .loans
            .iter()
            .map(|l| token(l.token))
            .collect(),
    };

    let rr = &r.router;
    let router = MmRouter {
        global_paused: rr.global_paused,
        pin_ltv_wad: rr.pin_ltv_wad.0,
        safety_gap_wad: rr.safety_gap_wad.0,
        oracle_band_wad: rr.oracle_band_wad.0,
        max_drawn_assets: rr.max_drawn_assets,
        loans: rr
            .loans
            .iter()
            .map(|l| Loan {
                decimals: l.decimals,
                loan_scale: l.loan_scale.0,
                debt_cap: l.debt_cap.0,
                supply_cap: l.supply_cap.0,
                borrow_enabled: l.borrow_enabled,
                retired: l.retired,
            })
            .collect(),
        venues: rr
            .venues
            .iter()
            .enumerate()
            .map(|(i, v)| {
                assert!((v.loan_index as usize) < n, "venue {i}: loanIndex past the loan set");
                assert!(!v.rate_at_target.0.starts_with('-'), "venue {i}: a negative rateAtTarget");
                assert!(
                    !v.has_irm || addr(&v.irm) == addr(ADAPTIVE_CURVE_IRM),
                    "venue {i}: an unsupported rate model"
                );
                Venue {
                    morpho: VenueMarket {
                        market: Market {
                            total_supply_assets: v.market.tsa.0,
                            total_supply_shares: v.market.tss.0,
                            total_borrow_assets: v.market.tba.0,
                            total_borrow_shares: v.market.tbs.0,
                            last_update: v.market.last_update.0,
                            fee: v.market.fee.0,
                        },
                        position: Position {
                            supply_shares: v.position.supply_shares.0,
                            borrow_shares: v.position.borrow_shares.0,
                            collateral: v.position.collateral.0,
                        },
                        lltv: v.market_lltv.0,
                        has_irm: v.has_irm,
                        irm_readable: v.irm_readable,
                        rate_at_target: parse_word(&v.rate_at_target.0).expect("rateAtTarget"),
                        oracle_ok: v.oracle_ok,
                        oracle_price: v.oracle_price.0,
                        oracle_zero: v.oracle_zero,
                    },
                    kind: v.kind,
                    loan_index: v.loan_index,
                    lltv_wad: v.lltv_wad.0,
                    borrow_enabled: v.borrow_enabled,
                    supply_enabled: v.supply_enabled,
                    retired: v.retired,
                    debt_cap: v.debt_cap.0,
                    supply_cap: v.supply_cap.0,
                    max_borrow_rate_wad: v.max_borrow_rate_wad.0,
                    managed_collateral: v.managed_collateral.0,
                    managed_supply_shares: v.managed_supply_shares.0,
                }
            })
            .collect(),
        borrow_order: rr.borrow_order.clone(),
        supply_order: rr.supply_order.clone(),
        withdraw_order: rr.withdraw_order.clone(),
        repay_order: rr.repay_order.clone(),
        transient_repay: Default::default(),
    };
    for order in [&rr.borrow_order, &rr.supply_order, &rr.withdraw_order, &rr.repay_order] {
        for id in order {
            assert!((*id as usize) < rr.venues.len(), "a priority past the venue set");
        }
    }

    Flamm {
        block: r.block,
        timestamp: r.timestamp,
        pool_asset,
        pool,
        paused: p.paused,
        lev_paused: p.lev_paused,
        fee_floor_wad: p.fee_floor_wad.0,
        fee_cap_wad: p.fee_cap_wad.0,
        share_supply: p.total_supply.0,
        last_lever_spread_ppm: p.last_lever_spread_ppm.0,
        hooks: PoolHooks { swap, leverage, spread },
        router,
        feed,
    }
}

// ------------------------------------------------------------------ comparison

/// Tallies of one replay: every compared call or view is a check; a sensitivity run is `quiet`
/// (counts mismatches, reports nothing) and stops at the first.
#[derive(Debug, Default)]
pub struct Report {
    pub checks: usize,
    pub failures: Vec<String>,
    pub quiet: bool,
}

impl Report {
    pub fn fail(&mut self, msg: String) {
        self.failures.push(msg);
    }

    pub fn stop(&self) -> bool {
        self.quiet && !self.failures.is_empty()
    }

    /// One call: both answer with the same words (the chain's prefix of the port's length, as the
    /// Go replays index them), or both refuse with the same revert class.
    pub fn compare(&mut self, where_: &str, chain: Chain, got: Result<Vec<U256>, FlammError>) {
        self.checks += 1;
        match (chain, got) {
            (Err(None), got) => {
                self.fail(format!("{where_}: unmapped chain revert (port {got:?})"));
            }
            (Err(Some(c)), Err(e)) => {
                if c != e {
                    self.fail(format!("{where_}: chain reverts {c:?}, port {e:?}"));
                }
            }
            (Err(Some(c)), Ok(w)) => {
                self.fail(format!("{where_}: chain reverts {c:?}, port answers {w:?}"));
            }
            (Ok(c), Err(e)) => {
                self.fail(format!("{where_}: chain answers {c:?}, port refuses {e:?}"));
            }
            (Ok(c), Ok(w)) => {
                if c.len() < w.len() {
                    self.fail(format!(
                        "{where_}: chain returned {} words, port {}",
                        c.len(),
                        w.len()
                    ));
                } else if let Some(i) = (0..w.len()).find(|&i| c[i] != w[i]) {
                    self.fail(format!(
                        "{where_}: word {i} chain {} port {} (chain {:?} port {w:?})",
                        c[i],
                        w[i],
                        &c[..w.len()]
                    ));
                }
            }
        }
    }

    pub fn finish(&self, label: &str) {
        eprintln!("{label}: {} checks, {} mismatches", self.checks, self.failures.len());
        for f in self.failures.iter().take(40) {
            eprintln!("  {f}");
        }
        assert!(self.failures.is_empty(), "{label}: {} mismatches", self.failures.len());
    }
}

/// The revert class of a recorded revert, for the class tallies (`(*row.E + "0000000000")[:10]`).
pub fn class_tag(e: Option<&String>) -> String {
    match e {
        None => "ok".to_string(),
        Some(e) => {
            let mut s = e.clone();
            s.push_str("0000000000");
            s[..10].to_string()
        }
    }
}

/// The differing fields of two states, each as `path: port X chain Y` with the Go port's field
/// paths (`core_edges_test.go` `coreEdgeDiff`), the per-call price frame and the transient repay
/// snapshot excluded, `s.Block` / `s.Timestamp` included (the caller zeroes them when they are not
/// state). A path in `skip` excludes itself and its subtree.
pub fn diff_state(got: &State, want: &State, skip: &[&str]) -> Vec<String> {
    let mut d = Vec::new();
    let skipped = |path: &str| {
        skip.iter().any(|s| {
            path == *s ||
                path.strip_prefix(s)
                    .is_some_and(|r| r.starts_with('.') || r.starts_with('['))
        })
    };
    macro_rules! cmp {
        ($path:expr, $a:expr, $b:expr) => {{
            let p: String = $path;
            if !skipped(&p) && $a != $b {
                d.push(format!("{p}: port {:?} chain {:?}", $a, $b));
            }
        }};
    }
    macro_rules! cmpu {
        ($path:expr, $a:expr, $b:expr) => {{
            let p: String = $path;
            if !skipped(&p) && $a != $b {
                d.push(format!("{p}: port {} chain {}", $a, $b));
            }
        }};
    }
    let (g, w) = (got, want);
    cmp!("s.Block".into(), g.block, w.block);
    cmp!("s.Timestamp".into(), g.timestamp, w.timestamp);
    cmp!("s.PoolAsset".into(), g.pool_asset, w.pool_asset);
    cmpu!("s.Pool.Physical".into(), g.pool.physical, w.pool.physical);
    if g.pool.loans.len() != w.pool.loans.len() {
        d.push(format!(
            "s.Pool.Loans: len port {} chain {}",
            g.pool.loans.len(),
            w.pool.loans.len()
        ));
    } else {
        for (i, (a, b)) in g
            .pool
            .loans
            .iter()
            .zip(&w.pool.loans)
            .enumerate()
        {
            let p = format!("s.Pool.Loans[{i}]");
            cmp!(format!("{p}.Token"), a.token, b.token);
            cmpu!(format!("{p}.Scale"), a.scale, b.scale);
            cmpu!(format!("{p}.SwapPriceBandWad"), a.swap_price_band_wad, b.swap_price_band_wad);
            cmpu!(format!("{p}.FeeFloorWad"), a.fee_floor_wad, b.fee_floor_wad);
            cmpu!(format!("{p}.MaxSwapNotional"), a.max_swap_notional, b.max_swap_notional);
            cmpu!(format!("{p}.ReserveTarget"), a.reserve_target, b.reserve_target);
            cmpu!(format!("{p}.Liquid"), a.liquid, b.liquid);
        }
    }
    cmpu!("s.Pool.LtvWad".into(), g.pool.ltv_wad, w.pool.ltv_wad);
    cmpu!("s.Pool.PhiWad".into(), g.pool.phi_wad, w.pool.phi_wad);
    cmpu!("s.Pool.RoomEpsilonWad".into(), g.pool.room_epsilon_wad, w.pool.room_epsilon_wad);
    cmpu!("s.Pool.Features".into(), g.pool.features, w.pool.features);
    cmp!("s.Paused".into(), g.paused, w.paused);
    cmp!("s.LevPaused".into(), g.lev_paused, w.lev_paused);
    cmpu!("s.FeeFloorWad".into(), g.fee_floor_wad, w.fee_floor_wad);
    cmpu!("s.FeeCapWad".into(), g.fee_cap_wad, w.fee_cap_wad);
    cmpu!("s.ShareSupply".into(), g.share_supply, w.share_supply);
    cmpu!("s.LastLeverSpreadPpm".into(), g.last_lever_spread_ppm, w.last_lever_spread_ppm);
    cmp!("s.Hooks.Swap.Kind".into(), g.hooks.swap.kind, w.hooks.swap.kind);
    match (&g.hooks.swap.everlong_swap, &w.hooks.swap.everlong_swap) {
        (Some(a), Some(b)) => {
            let p = "s.Hooks.Swap.EverlongSwap";
            cmpu!(format!("{p}.AWad"), a.a_wad, b.a_wad);
            cmp!(format!("{p}.Support"), a.support, b.support);
            cmpu!(format!("{p}.AnchorSqrtX96"), a.anchor_sqrt_x96, b.anchor_sqrt_x96);
            cmpu!(
                format!("{p}.ReservationPriceWad"),
                a.reservation_price_wad,
                b.reservation_price_wad
            );
            cmpu!(format!("{p}.Kappa"), a.kappa, b.kappa);
            cmpu!(format!("{p}.XWad"), a.x_wad, b.x_wad);
            cmpu!(format!("{p}.ReserveStable"), a.reserve_stable, b.reserve_stable);
            cmpu!(format!("{p}.IdleStable"), a.idle_stable, b.idle_stable);
            cmpu!(format!("{p}.ReserveVolatile"), a.reserve_volatile, b.reserve_volatile);
            cmpu!(format!("{p}.IdleVolatile"), a.idle_volatile, b.idle_volatile);
            cmpu!(format!("{p}.RvWad"), a.rv_wad, b.rv_wad);
            cmp!(format!("{p}.Fee"), a.fee, b.fee);
            cmpu!(format!("{p}.InvSkewKappaWad"), a.inv_skew_kappa_wad, b.inv_skew_kappa_wad);
            cmpu!(format!("{p}.InvSkewBandWad"), a.inv_skew_band_wad, b.inv_skew_band_wad);
            cmpu!(format!("{p}.LoanScale"), a.loan_scale, b.loan_scale);
        }
        (a, b) => cmp!("s.Hooks.Swap.EverlongSwap".into(), a.is_some(), b.is_some()),
    }
    cmp!("s.Hooks.Leverage.Kind".into(), g.hooks.leverage.kind, w.hooks.leverage.kind);
    cmp!("s.Hooks.Spread.Kind".into(), g.hooks.spread.kind, w.hooks.spread.kind);
    match (&g.hooks.spread.everlong_spread, &w.hooks.spread.everlong_spread) {
        (Some(a), Some(b)) => {
            let p = "s.Hooks.Spread.EverlongSpread";
            cmpu!(format!("{p}.Spread"), a.spread, b.spread);
            cmpu!(format!("{p}.MaxSpreadAge"), a.max_spread_age, b.max_spread_age);
            cmpu!(format!("{p}.LastSetTs"), a.last_set_ts, b.last_set_ts);
        }
        (a, b) => cmp!("s.Hooks.Spread.EverlongSpread".into(), a.is_some(), b.is_some()),
    }
    // Feed.
    {
        let (a, b) = (&g.feed, &w.feed);
        cmp!("s.Feed.HasSequencer".into(), a.has_sequencer, b.has_sequencer);
        cmpu!("s.Feed.SequencerGrace".into(), a.sequencer_grace, b.sequencer_grace);
        cmp!("s.Feed.Sequencer".into(), a.sequencer, b.sequencer);
        cmp!("s.Feed.Asset".into(), a.asset, b.asset);
        if a.loans.len() != b.loans.len() {
            d.push(format!("s.Feed.Loans: len port {} chain {}", a.loans.len(), b.loans.len()));
        } else {
            for (i, (x, y)) in a.loans.iter().zip(&b.loans).enumerate() {
                cmp!(format!("s.Feed.Loans[{i}]"), x, y);
            }
        }
    }
    // Router.
    {
        let (a, b) = (&g.router, &w.router);
        cmp!("s.Router.GlobalPaused".into(), a.global_paused, b.global_paused);
        cmpu!("s.Router.PinLtvWad".into(), a.pin_ltv_wad, b.pin_ltv_wad);
        cmpu!("s.Router.SafetyGapWad".into(), a.safety_gap_wad, b.safety_gap_wad);
        cmpu!("s.Router.OracleBandWad".into(), a.oracle_band_wad, b.oracle_band_wad);
        cmp!("s.Router.MaxDrawnAssets".into(), a.max_drawn_assets, b.max_drawn_assets);
        if a.loans.len() != b.loans.len() {
            d.push(format!("s.Router.Loans: len port {} chain {}", a.loans.len(), b.loans.len()));
        } else {
            for (i, (x, y)) in a.loans.iter().zip(&b.loans).enumerate() {
                let p = format!("s.Router.Loans[{i}]");
                cmp!(format!("{p}.Decimals"), x.decimals, y.decimals);
                cmpu!(format!("{p}.LoanScale"), x.loan_scale, y.loan_scale);
                cmpu!(format!("{p}.DebtCap"), x.debt_cap, y.debt_cap);
                cmpu!(format!("{p}.SupplyCap"), x.supply_cap, y.supply_cap);
                cmp!(format!("{p}.BorrowEnabled"), x.borrow_enabled, y.borrow_enabled);
                cmp!(format!("{p}.Retired"), x.retired, y.retired);
            }
        }
        if a.venues.len() != b.venues.len() {
            d.push(format!(
                "s.Router.Venues: len port {} chain {}",
                a.venues.len(),
                b.venues.len()
            ));
        } else {
            for (i, (x, y)) in a
                .venues
                .iter()
                .zip(&b.venues)
                .enumerate()
            {
                let p = format!("s.Router.Venues[{i}]");
                cmp!(format!("{p}.Kind"), x.kind, y.kind);
                cmp!(format!("{p}.LoanIndex"), x.loan_index, y.loan_index);
                cmpu!(format!("{p}.LltvWad"), x.lltv_wad, y.lltv_wad);
                cmp!(format!("{p}.BorrowEnabled"), x.borrow_enabled, y.borrow_enabled);
                cmp!(format!("{p}.SupplyEnabled"), x.supply_enabled, y.supply_enabled);
                cmp!(format!("{p}.Retired"), x.retired, y.retired);
                cmpu!(format!("{p}.DebtCap"), x.debt_cap, y.debt_cap);
                cmpu!(format!("{p}.SupplyCap"), x.supply_cap, y.supply_cap);
                cmpu!(
                    format!("{p}.MaxBorrowRateWad"),
                    x.max_borrow_rate_wad,
                    y.max_borrow_rate_wad
                );
                cmpu!(format!("{p}.ManagedCollateral"), x.managed_collateral, y.managed_collateral);
                cmpu!(
                    format!("{p}.ManagedSupplyShares"),
                    x.managed_supply_shares,
                    y.managed_supply_shares
                );
                let (m, n) = (&x.morpho, &y.morpho);
                let q = format!("{p}.Morpho");
                cmpu!(
                    format!("{q}.Market.TotalSupplyAssets"),
                    m.market.total_supply_assets,
                    n.market.total_supply_assets
                );
                cmpu!(
                    format!("{q}.Market.TotalSupplyShares"),
                    m.market.total_supply_shares,
                    n.market.total_supply_shares
                );
                cmpu!(
                    format!("{q}.Market.TotalBorrowAssets"),
                    m.market.total_borrow_assets,
                    n.market.total_borrow_assets
                );
                cmpu!(
                    format!("{q}.Market.TotalBorrowShares"),
                    m.market.total_borrow_shares,
                    n.market.total_borrow_shares
                );
                cmpu!(format!("{q}.Market.LastUpdate"), m.market.last_update, n.market.last_update);
                cmpu!(format!("{q}.Market.Fee"), m.market.fee, n.market.fee);
                cmpu!(
                    format!("{q}.Position.SupplyShares"),
                    m.position.supply_shares,
                    n.position.supply_shares
                );
                cmpu!(
                    format!("{q}.Position.BorrowShares"),
                    m.position.borrow_shares,
                    n.position.borrow_shares
                );
                cmpu!(
                    format!("{q}.Position.Collateral"),
                    m.position.collateral,
                    n.position.collateral
                );
                cmpu!(format!("{q}.Lltv"), m.lltv, n.lltv);
                cmp!(format!("{q}.HasIrm"), m.has_irm, n.has_irm);
                cmp!(format!("{q}.IrmReadable"), m.irm_readable, n.irm_readable);
                cmpu!(format!("{q}.RateAtTarget"), m.rate_at_target, n.rate_at_target);
                cmp!(format!("{q}.OracleOk"), m.oracle_ok, n.oracle_ok);
                cmpu!(format!("{q}.OraclePrice"), m.oracle_price, n.oracle_price);
                cmp!(format!("{q}.OracleZero"), m.oracle_zero, n.oracle_zero);
            }
        }
        cmp!("s.Router.BorrowOrder".into(), a.borrow_order, b.borrow_order);
        cmp!("s.Router.SupplyOrder".into(), a.supply_order, b.supply_order);
        cmp!("s.Router.WithdrawOrder".into(), a.withdraw_order, b.withdraw_order);
        cmp!("s.Router.RepayOrder".into(), a.repay_order, b.repay_order);
    }
    d
}

/// The deployed views a dump carries, recomputed from the built state (`core_e2e_test.go`
/// `e2eAttest`): `peekCross`, `pegOk(loan0)`, `positions`, `gross`, `loanPosition(0)` and
/// `totalAssets`.
pub fn attest(where_: &str, s: &State, r: &Reads, rep: &mut Report) {
    let Some(v) = &r.views else {
        return;
    };
    let now = r.timestamp;
    rep.checks += 1;
    let (ok, p, ts) = s
        .feed
        .peek_cross(&s.feed.asset, &s.feed.loans[0], now);
    if ok != v.peek_cross.ok || p != v.peek_cross.price_wad.0 || U256::from(ts) != v.peek_cross.ts.0
    {
        rep.fail(format!(
            "{where_}: peekCross chain ({}, {}, {}) port ({ok}, {p}, {ts})",
            v.peek_cross.ok, v.peek_cross.price_wad.0, v.peek_cross.ts.0
        ));
    }
    match s.peg_ok(0, now) {
        Ok(peg) if peg == v.peg_ok => {}
        other => rep.fail(format!("{where_}: pegOk chain {} port {other:?}", v.peg_ok)),
    }
    let pos = match s.router.positions(now) {
        Ok(pos) => pos,
        Err(e) => {
            rep.fail(format!("{where_}: positions: {e:?}"));
            return;
        }
    };
    let words = |w: &[Word]| {
        w.iter()
            .map(|x| x.0)
            .collect::<Vec<_>>()
    };
    if pos.coll != words(&v.positions.coll) ||
        pos.sup != words(&v.positions.sup) ||
        pos.debt != words(&v.positions.debt) ||
        pos.total_coll != v.positions.total_coll.0
    {
        rep.fail(format!("{where_}: positions chain {:?} port {pos:?}", v.positions));
    }
    let gross = s.pool.physical + pos.total_coll;
    if gross != r.pool.gross.0 {
        rep.fail(format!("{where_}: gross chain {} port {gross}", r.pool.gross.0));
    }
    match s.router.position(0, now) {
        Ok((_, sup, debt))
            if s.pool.loans[0].liquid == v.loan_position[0].0 &&
                sup == v.loan_position[1].0 &&
                debt == v.loan_position[2].0 => {}
        other => rep.fail(format!(
            "{where_}: loanPosition chain {:?} port (liquid {}, {other:?})",
            v.loan_position, s.pool.loans[0].liquid
        )),
    }
    let mut pool = s.pool.clone();
    let nav = priced(&s.feed, &s.router, &mut pool, now).and_then(|b| gate::nav_at(&b));
    match (&v.total_assets.v, &v.total_assets.err) {
        (Some(want), _) => {
            if nav != Ok(want.0) {
                rep.fail(format!("{where_}: totalAssets chain {} port {nav:?}", want.0));
            }
        }
        (None, Some(e)) => match (revert_class(e), &nav) {
            (Some(c), Err(got)) if c == *got => {}
            _ => rep.fail(format!("{where_}: totalAssets chain revert {e} port {nav:?}")),
        },
        (None, None) => rep.fail(format!("{where_}: totalAssets view carries nothing")),
    }
}

/// `TestCoreE2ESequences`' `e2eDiff`: the port's carried state against the chain's post dump, the
/// stamp set aside (entries run at the step's timestamp; the stamp is the reader's).
pub fn e2e_diff(got: &State, want: &State) -> Vec<String> {
    let mut g = got.clone();
    g.timestamp = want.timestamp;
    diff_state(&g, want, &[])
}

/// `coreEdgeDiff`: every field but the snapshot stamps.
pub fn edge_diff(got: &State, want: &State, skip: &[&str]) -> Vec<String> {
    let mut g = got.clone();
    g.block = want.block;
    g.timestamp = want.timestamp;
    diff_state(&g, want, skip)
}

/// A bool as the event word.
pub fn bool_word(b: bool) -> U256 {
    U256::from(b as u64)
}
