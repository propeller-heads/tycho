// Copyright (c) 2026 Everlong Labs Limited

//! Shared fixture plumbing for the financing replays: the Solidity-generated fixtures (gzipped
//! JSON and JSON lines, decimal strings for every uint256, raw revert data for every refusal), the
//! revert vocabulary of the fixture generators, ABI word decoding, and the loaders that turn each
//! fixture's state shape into the port's [`Router`] / [`Pool`] / [`VenueMarket`].

#![allow(dead_code)]

use std::fmt;

use alloy::primitives::U256;
use serde::{de, Deserialize, Deserializer};

use super::super::fixtures::read_fixture;
use crate::evm::protocol::flamm::{
    error::{error_string_payload, FlammError},
    gate::{GateInt, LoanCfg, Pool},
    morpho::{Market, Position, VenueMarket},
    router::{Loan, Router, Venue},
};

/// Decodes one JSON fixture.
pub fn load<T: de::DeserializeOwned>(name: &str) -> T {
    let raw = read_fixture(name);
    serde_json::from_slice(&raw).unwrap_or_else(|e| panic!("decode {name}: {e}"))
}

/// Decodes one JSON-lines fixture, one value per non-empty line.
pub fn load_lines<T: de::DeserializeOwned>(name: &str) -> Vec<T> {
    let raw = read_fixture(name);
    let text = std::str::from_utf8(&raw).expect("utf8");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("decode {name} line {i}: {e}"))
        })
        .collect()
}

// ------------------------------------------------------------------ scalars

/// A `uint256` as the generators write it: a quoted decimal (or `0x` hex), or a bare JSON number.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Dec(pub U256);

impl fmt::Debug for Dec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for Dec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Dec;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a decimal string or an integer")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Dec, E> {
                parse_u256(s)
                    .map(Dec)
                    .ok_or_else(|| E::custom(format!("bad uint256 {s:?}")))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Dec, E> {
                Ok(Dec(U256::from(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Dec, E> {
                u64::try_from(v)
                    .map(|v| Dec(U256::from(v)))
                    .map_err(|_| E::custom("negative uint256"))
            }
        }
        d.deserialize_any(V)
    }
}

pub fn parse_u256(s: &str) -> Option<U256> {
    if let Some(h) = s.strip_prefix("0x") {
        return U256::from_str_radix(h, 16).ok();
    }
    U256::from_str_radix(s, 10).ok()
}

/// A signed decimal (`"-123"`) as the gate's sign-magnitude `int256`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct SignedDec(pub GateInt);

impl fmt::Debug for SignedDec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", gate_int_string(&self.0))
    }
}

impl<'de> Deserialize<'de> for SignedDec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let (neg, mag) = match s.strip_prefix('-') {
            Some(m) => (true, m),
            None => (false, s.as_str()),
        };
        let abs = parse_u256(mag).ok_or_else(|| de::Error::custom(format!("bad int256 {s:?}")))?;
        Ok(SignedDec(GateInt { neg: neg && !abs.is_zero(), abs }))
    }
}

pub fn gate_int_string(x: &GateInt) -> String {
    if x.neg {
        format!("-{}", x.abs)
    } else {
        x.abs.to_string()
    }
}

/// An integer the generators write either bare or as a quoted decimal (`json:",string"` in the Go
/// structs).
pub fn de_int<'de, D: Deserializer<'de>, T: TryFrom<u64>>(d: D) -> Result<T, D::Error> {
    let v = Dec::deserialize(d)?.0;
    let u = u64::try_from(v).map_err(|_| de::Error::custom("integer too wide"))?;
    T::try_from(u).map_err(|_| de::Error::custom("integer out of range"))
}

pub fn de_int_default<'de, D: Deserializer<'de>, T: TryFrom<u64> + Default>(
    d: D,
) -> Result<T, D::Error> {
    de_int(d)
}

/// A hex string (`0x...`) as bytes; an empty or missing string is empty data.
pub fn hex_bytes(h: &str) -> Vec<u8> {
    let h = h.strip_prefix("0x").unwrap_or(h);
    if h.is_empty() {
        return Vec::new();
    }
    hex::decode(h).unwrap_or_else(|e| panic!("hex {h:?}: {e}"))
}

/// ABI words of a return payload (a trailing partial word is dropped).
pub fn abi_words(b: &[u8]) -> Vec<U256> {
    b.as_chunks::<32>()
        .0
        .iter()
        .map(|w| U256::from_be_slice(w))
        .collect()
}

/// The dynamic `uint256[]` whose head offset sits in word `k`.
pub fn dyn_array(w: &[U256], k: usize) -> Vec<U256> {
    let off = usize::try_from(w[k]).expect("offset") / 32;
    let n = usize::try_from(w[off]).expect("length");
    w[off + 1..off + 1 + n].to_vec()
}

/// Strips the `bytes` return of the harness's `gate(bytes4)` wrapper.
pub fn unwrap_bytes(b: &[u8]) -> Vec<u8> {
    let w = abi_words(b);
    let off = usize::try_from(w[0]).expect("offset");
    let n = usize::try_from(w[off / 32]).expect("length");
    b[off + 32..off + 32 + n].to_vec()
}

pub fn bool_word(b: bool) -> U256 {
    if b {
        U256::from(1)
    } else {
        U256::ZERO
    }
}

// ------------------------------------------------------------------ reverts

/// Maps raw revert data onto the port's error vocabulary: empty data is `Math.mulDiv`'s bare
/// require; panics and custom errors by selector; Morpho's `Error(string)` requires by message,
/// together with the mock oracle / IRM strings the fixture generators revert with (`"dead"`, `"irm
/// dead"`, `"irm down"`, `"oracle down"`, raw or ABI-encoded). `None` is an unmapped revert, which
/// fails the row.
pub fn revert_of(data: &[u8]) -> Option<FlammError> {
    match data {
        b"" => return Some(FlammError::MulDivOverflow),
        b"dead" | b"oracle down" => return Some(FlammError::MorphoOracleReverted),
        b"irm dead" | b"irm down" => return Some(FlammError::MorphoIrmReverted),
        _ => {}
    }
    // A custom error carrying arguments (`InsufficientLiquidity(uint256)` and its kin) is
    // classified by its selector at its recorded length: `from_revert_data` dispatches on the
    // selector, not the length, so there is nothing left for a `data[..4]` retry to catch.
    if let Some(e) = FlammError::from_revert_data(data) {
        return Some(e);
    }
    match error_string_payload(data)? {
        "dead" | "oracle down" => Some(FlammError::MorphoOracleReverted),
        "irm dead" | "irm down" => Some(FlammError::MorphoIrmReverted),
        _ => None,
    }
}

pub fn revert_of_hex(h: &str) -> Option<FlammError> {
    revert_of(&hex_bytes(h))
}

/// The `uint256` payload of a one-argument custom error (`InsufficientLiquidity(shortfall)` and its
/// kin), when the data carries one.
pub fn revert_payload(data: &[u8]) -> Option<U256> {
    if data.len() != 36 {
        return None;
    }
    Some(U256::from_be_slice(&data[4..36]))
}

/// A recorded call: `ok` with ABI return words, or a revert.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Call {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub ret: String,
}

impl Call {
    pub fn bytes(&self) -> Vec<u8> {
        hex_bytes(&self.ret)
    }

    pub fn words(&self) -> Vec<U256> {
        abi_words(&self.bytes())
    }
}

/// Collects mismatches so one run reports them all; the test asserts zero at the end.
pub struct Report {
    pub area: String,
    pub n: usize,
    pub limit: usize,
}

impl Report {
    pub fn new(area: &str) -> Self {
        Self { area: area.to_string(), n: 0, limit: 40 }
    }

    pub fn add(&mut self, msg: impl fmt::Display) {
        self.n += 1;
        if self.n <= self.limit {
            eprintln!("[{}] {}", self.area, msg);
        }
    }

    pub fn finish(&self, rows: usize) {
        eprintln!("[{}] {} rows, {} mismatches", self.area, rows, self.n);
        assert_eq!(self.n, 0, "{}: {} mismatches", self.area, self.n);
    }

    /// Compares one call with the port's words (or its error), like the Go `checkWords`.
    pub fn check_words(&mut self, ctx: &str, want: &Call, got: Result<Vec<U256>, FlammError>) {
        if !want.ok {
            let w = revert_of(&want.bytes());
            match (&got, w) {
                (Err(e), Some(w)) if *e == w => {}
                _ => self.add(format!(
                    "{ctx}: port={} sol=revert {} ({:?})",
                    err_class(&got),
                    want.ret,
                    w
                )),
            }
            return;
        }
        let got = match got {
            Ok(g) => g,
            Err(e) => {
                self.add(format!("{ctx}: port={e} sol=ok {}", want.ret));
                return;
            }
        };
        let w = want.words();
        if w.len() != got.len() {
            self.add(format!("{ctx}: port={} words sol={} words", got.len(), w.len()));
            return;
        }
        for i in 0..w.len() {
            if w[i] != got[i] {
                self.add(format!("{ctx}: word {i} port={} sol={}", got[i], w[i]));
                return;
            }
        }
    }

    /// Like `check_words`, but the chain may return more words than the port compares (a prefix
    /// match), as the Go `finSeqSameResult` does.
    pub fn check_prefix(&mut self, ctx: &str, want: &Call, got: Result<Vec<U256>, FlammError>) {
        if want.ok != got.is_ok() {
            self.add(format!(
                "{ctx}: chain ok={} ret={}, port {}",
                want.ok,
                want.ret,
                err_class(&got)
            ));
            return;
        }
        if !want.ok {
            let w = revert_of(&want.bytes());
            if w.is_none() || got.as_ref().err() != w.as_ref() {
                self.add(format!(
                    "{ctx}: chain revert {} ({w:?}), port {}",
                    want.ret,
                    err_class(&got)
                ));
            }
            return;
        }
        let got = got.unwrap();
        let w = want.words();
        if w.len() < got.len() {
            self.add(format!("{ctx}: chain returned {} words, port {}", w.len(), got.len()));
            return;
        }
        for i in 0..got.len() {
            if w[i] != got[i] {
                self.add(format!("{ctx}: word {i} chain {} port {}", w[i], got[i]));
            }
        }
    }
}

pub fn err_class<T>(r: &Result<T, FlammError>) -> String {
    match r {
        Ok(_) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

// ------------------------------------------------------------------ market shapes

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct MarketFx {
    #[serde(default)]
    pub tsa: Dec,
    #[serde(default)]
    pub tss: Dec,
    #[serde(default)]
    pub tba: Dec,
    #[serde(default)]
    pub tbs: Dec,
    #[serde(default, rename = "lastUpdate")]
    pub last_update: Dec,
    #[serde(default)]
    pub fee: Dec,
}

impl MarketFx {
    pub fn state(&self) -> Market {
        Market {
            total_supply_assets: self.tsa.0,
            total_supply_shares: self.tss.0,
            total_borrow_assets: self.tba.0,
            total_borrow_shares: self.tbs.0,
            last_update: self.last_update.0,
            fee: self.fee.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PositionFx {
    #[serde(default, rename = "supplyShares")]
    pub supply_shares: Dec,
    #[serde(default, rename = "borrowShares")]
    pub borrow_shares: Dec,
    #[serde(default)]
    pub collateral: Dec,
}

impl PositionFx {
    pub fn state(&self) -> Position {
        Position {
            supply_shares: self.supply_shares.0,
            borrow_shares: self.borrow_shares.0,
            collateral: self.collateral.0,
        }
    }
}

/// The Go port's own `mmVenueMarket` JSON shape (the sequence fixtures and the accrual grid).
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct VenueMarketFx {
    #[serde(default)]
    pub market: MarketFx,
    #[serde(default)]
    pub position: PositionFx,
    #[serde(default, rename = "lltvWad")]
    pub lltv_wad: Dec,
    #[serde(default, rename = "hasIrm")]
    pub has_irm: bool,
    #[serde(default, rename = "irmReadable")]
    pub irm_readable: bool,
    #[serde(default, rename = "rateAtTarget")]
    pub rate_at_target: Dec,
    #[serde(default, rename = "oracleOk")]
    pub oracle_ok: bool,
    #[serde(default, rename = "oraclePrice")]
    pub oracle_price: Dec,
    #[serde(default, rename = "oracleZero")]
    pub oracle_zero: bool,
}

impl VenueMarketFx {
    pub fn state(&self) -> VenueMarket {
        VenueMarket {
            market: self.market.state(),
            position: self.position.state(),
            lltv: self.lltv_wad.0,
            has_irm: self.has_irm,
            irm_readable: self.irm_readable,
            rate_at_target: self.rate_at_target.0,
            oracle_ok: self.oracle_ok,
            oracle_price: self.oracle_price.0,
            oracle_zero: self.oracle_zero,
        }
    }
}

// ------------------------------------------------------------------ Router shapes

/// `testdata/gen/MMFixtureBase.sol`'s Router record (the `mm_*` fixtures).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MmFxLoan {
    #[serde(default, deserialize_with = "de_int_default")]
    pub decimals: u8,
    #[serde(default, rename = "loanScale")]
    pub loan_scale: Dec,
    #[serde(default, rename = "debtCap")]
    pub debt_cap: Dec,
    #[serde(default, rename = "supplyCap")]
    pub supply_cap: Dec,
    #[serde(default, rename = "borrowEnabled")]
    pub borrow_enabled: bool,
    #[serde(default)]
    pub retired: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MmFxVenue {
    #[serde(default, deserialize_with = "de_int_default")]
    pub kind: u8,
    #[serde(default, rename = "loanIndex", deserialize_with = "de_int_default")]
    pub loan_index: u8,
    #[serde(default, rename = "lltvWad")]
    pub lltv_wad: Dec,
    #[serde(default, rename = "borrowEnabled")]
    pub borrow_enabled: bool,
    #[serde(default, rename = "supplyEnabled")]
    pub supply_enabled: bool,
    #[serde(default)]
    pub retired: bool,
    #[serde(default, rename = "debtCap")]
    pub debt_cap: Dec,
    #[serde(default, rename = "supplyCap")]
    pub supply_cap: Dec,
    #[serde(default, rename = "maxBorrowRateWad")]
    pub max_borrow_rate_wad: Dec,
    #[serde(default, rename = "managedCollateral")]
    pub managed_collateral: Dec,
    #[serde(default, rename = "managedSupplyShares")]
    pub managed_supply_shares: Dec,
    #[serde(default)]
    pub market: MarketFx,
    #[serde(default)]
    pub position: PositionFx,
    #[serde(default, rename = "rateAtTarget")]
    pub rate_at_target: Dec,
    #[serde(default, rename = "hasIrm")]
    pub has_irm: bool,
    #[serde(default, rename = "irmReadable")]
    pub irm_readable: bool,
    #[serde(default, rename = "oracleOk")]
    pub oracle_ok: bool,
    #[serde(default, rename = "oraclePrice")]
    pub oracle_price: Dec,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MmFxRouter {
    #[serde(default, rename = "globalPaused")]
    pub global_paused: bool,
    #[serde(default, rename = "pinLtvWad")]
    pub pin_ltv_wad: Dec,
    #[serde(default, rename = "safetyGapWad")]
    pub safety_gap_wad: Dec,
    #[serde(default, rename = "oracleBandWad")]
    pub oracle_band_wad: Dec,
    #[serde(default, rename = "maxDrawnAssets", deserialize_with = "de_int_default")]
    pub max_drawn_assets: u8,
    #[serde(default)]
    pub loans: Vec<MmFxLoan>,
    #[serde(default, rename = "borrowOrder")]
    pub borrow_order: Vec<u16>,
    #[serde(default, rename = "supplyOrder")]
    pub supply_order: Vec<u16>,
    #[serde(default, rename = "withdrawOrder")]
    pub withdraw_order: Vec<u16>,
    #[serde(default, rename = "repayOrder")]
    pub repay_order: Vec<u16>,
    #[serde(default)]
    pub venues: Vec<MmFxVenue>,
}

impl MmFxRouter {
    pub fn state(&self) -> Router {
        Router {
            global_paused: self.global_paused,
            pin_ltv_wad: self.pin_ltv_wad.0,
            safety_gap_wad: self.safety_gap_wad.0,
            oracle_band_wad: self.oracle_band_wad.0,
            max_drawn_assets: self.max_drawn_assets,
            loans: self
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
            venues: self
                .venues
                .iter()
                .map(|v| Venue {
                    morpho: VenueMarket {
                        market: v.market.state(),
                        position: v.position.state(),
                        lltv: v.lltv_wad.0,
                        has_irm: v.has_irm,
                        irm_readable: v.irm_readable,
                        rate_at_target: v.rate_at_target.0,
                        oracle_ok: v.oracle_ok,
                        oracle_price: v.oracle_price.0,
                        oracle_zero: false,
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
                })
                .collect(),
            borrow_order: self.borrow_order.clone(),
            supply_order: self.supply_order.clone(),
            withdraw_order: self.withdraw_order.clone(),
            repay_order: self.repay_order.clone(),
            transient_repay: Default::default(),
        }
    }
}

/// `testdata/gen/FinancingEdgesBase.sol`'s Router state (the `edges/mm_*` and `router_settlement_*`
/// fixtures), integers written as decimal strings.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FinLoan {
    #[serde(default, deserialize_with = "de_int_default")]
    pub decimals: u8,
    #[serde(default)]
    pub scale: Dec,
    #[serde(default, rename = "debtCap")]
    pub debt_cap: Dec,
    #[serde(default, rename = "supplyCap")]
    pub supply_cap: Dec,
    #[serde(default, rename = "borrowEnabled")]
    pub borrow_enabled: bool,
    #[serde(default)]
    pub retired: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FinVenue {
    #[serde(default, rename = "loanIndex", deserialize_with = "de_int_default")]
    pub loan_index: u8,
    #[serde(default)]
    pub lltv: Dec,
    #[serde(default, rename = "borrowEnabled")]
    pub borrow_enabled: bool,
    #[serde(default, rename = "supplyEnabled")]
    pub supply_enabled: bool,
    #[serde(default)]
    pub retired: bool,
    #[serde(default, rename = "debtCap")]
    pub debt_cap: Dec,
    #[serde(default, rename = "supplyCap")]
    pub supply_cap: Dec,
    #[serde(default, rename = "maxRate")]
    pub max_rate: Dec,
    #[serde(default, rename = "managedColl")]
    pub managed_coll: Dec,
    #[serde(default, rename = "managedShares")]
    pub managed_shares: Dec,
    #[serde(default, rename = "hasIrm")]
    pub has_irm: bool,
    #[serde(default, rename = "irmDead")]
    pub irm_dead: bool,
    #[serde(default)]
    pub rat: SignedDec,
    #[serde(default, rename = "oracleOk")]
    pub oracle_ok: bool,
    #[serde(default, rename = "oraclePrice")]
    pub oracle_price: Dec,
    #[serde(default, rename = "oracleZero")]
    pub oracle_zero: bool,
    /// The account's registered LLTV (the router edges); the settlement edges use `lltv` for both.
    #[serde(default, rename = "acctLltv")]
    pub acct_lltv: Option<Dec>,
    #[serde(default)]
    pub market: MarketFx,
    #[serde(default)]
    pub position: PositionFx,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FinRouterState {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub pin: Dec,
    #[serde(default)]
    pub gap: Dec,
    #[serde(default)]
    pub band: Dec,
    #[serde(default, rename = "maxDrawn", deserialize_with = "de_int_default")]
    pub max_drawn: u8,
    #[serde(default, rename = "borrowOrder", alias = "bo")]
    pub borrow_order: Vec<u16>,
    #[serde(default, rename = "supplyOrder", alias = "so")]
    pub supply_order: Vec<u16>,
    #[serde(default, rename = "withdrawOrder", alias = "wo")]
    pub withdraw_order: Vec<u16>,
    #[serde(default, rename = "repayOrder", alias = "ro")]
    pub repay_order: Vec<u16>,
    #[serde(default)]
    pub loans: Vec<FinLoan>,
    #[serde(default)]
    pub venues: Vec<FinVenue>,
}

impl FinRouterState {
    pub fn router(&self) -> Router {
        Router {
            global_paused: self.paused,
            pin_ltv_wad: self.pin.0,
            safety_gap_wad: self.gap.0,
            oracle_band_wad: self.band.0,
            max_drawn_assets: self.max_drawn,
            loans: self
                .loans
                .iter()
                .map(|l| Loan {
                    decimals: l.decimals,
                    loan_scale: l.scale.0,
                    debt_cap: l.debt_cap.0,
                    supply_cap: l.supply_cap.0,
                    borrow_enabled: l.borrow_enabled,
                    retired: l.retired,
                })
                .collect(),
            venues: self
                .venues
                .iter()
                .map(|v| Venue {
                    morpho: VenueMarket {
                        market: v.market.state(),
                        position: v.position.state(),
                        lltv: v.acct_lltv.unwrap_or(v.lltv).0,
                        has_irm: v.has_irm,
                        irm_readable: !v.irm_dead,
                        rate_at_target: v.rat.0.abs,
                        oracle_ok: v.oracle_ok,
                        oracle_price: v.oracle_price.0,
                        oracle_zero: v.oracle_zero,
                    },
                    kind: 0,
                    loan_index: v.loan_index,
                    lltv_wad: v.lltv.0,
                    borrow_enabled: v.borrow_enabled,
                    supply_enabled: v.supply_enabled,
                    retired: v.retired,
                    debt_cap: v.debt_cap.0,
                    supply_cap: v.supply_cap.0,
                    max_borrow_rate_wad: v.max_rate.0,
                    managed_collateral: v.managed_coll.0,
                    managed_supply_shares: v.managed_shares.0,
                })
                .collect(),
            borrow_order: self.borrow_order.clone(),
            supply_order: self.supply_order.clone(),
            withdraw_order: self.withdraw_order.clone(),
            repay_order: self.repay_order.clone(),
            transient_repay: Default::default(),
        }
    }
}

/// The Go port's own `mmRouter` JSON shape (the sequence fixtures).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GoVenue {
    #[serde(default)]
    pub morpho: VenueMarketFx,
    #[serde(default, deserialize_with = "de_int_default")]
    pub kind: u8,
    #[serde(default, rename = "loanIndex", deserialize_with = "de_int_default")]
    pub loan_index: u8,
    #[serde(default, rename = "lltvWad")]
    pub lltv_wad: Dec,
    #[serde(default, rename = "borrowEnabled")]
    pub borrow_enabled: bool,
    #[serde(default, rename = "supplyEnabled")]
    pub supply_enabled: bool,
    #[serde(default)]
    pub retired: bool,
    #[serde(default, rename = "debtCap")]
    pub debt_cap: Dec,
    #[serde(default, rename = "supplyCap")]
    pub supply_cap: Dec,
    #[serde(default, rename = "maxBorrowRateWad")]
    pub max_borrow_rate_wad: Dec,
    #[serde(default, rename = "managedCollateral")]
    pub managed_collateral: Dec,
    #[serde(default, rename = "managedSupplyShares")]
    pub managed_supply_shares: Dec,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GoRouter {
    #[serde(default, rename = "globalPaused")]
    pub global_paused: bool,
    #[serde(default, rename = "pinLtvWad")]
    pub pin_ltv_wad: Dec,
    #[serde(default, rename = "safetyGapWad")]
    pub safety_gap_wad: Dec,
    #[serde(default, rename = "oracleBandWad")]
    pub oracle_band_wad: Dec,
    #[serde(default, rename = "maxDrawnAssets", deserialize_with = "de_int_default")]
    pub max_drawn_assets: u8,
    #[serde(default)]
    pub loans: Vec<MmFxLoan>,
    #[serde(default)]
    pub venues: Vec<GoVenue>,
    #[serde(default, rename = "borrowOrder")]
    pub borrow_order: Vec<u16>,
    #[serde(default, rename = "supplyOrder")]
    pub supply_order: Vec<u16>,
    #[serde(default, rename = "withdrawOrder")]
    pub withdraw_order: Vec<u16>,
    #[serde(default, rename = "repayOrder")]
    pub repay_order: Vec<u16>,
}

impl GoRouter {
    pub fn state(&self) -> Router {
        Router {
            global_paused: self.global_paused,
            pin_ltv_wad: self.pin_ltv_wad.0,
            safety_gap_wad: self.safety_gap_wad.0,
            oracle_band_wad: self.oracle_band_wad.0,
            max_drawn_assets: self.max_drawn_assets,
            loans: self
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
            venues: self
                .venues
                .iter()
                .map(|v| Venue {
                    morpho: v.morpho.state(),
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
                })
                .collect(),
            borrow_order: self.borrow_order.clone(),
            supply_order: self.supply_order.clone(),
            withdraw_order: self.withdraw_order.clone(),
            repay_order: self.repay_order.clone(),
            transient_repay: Default::default(),
        }
    }
}

// ------------------------------------------------------------------ pool shapes

/// `MMFixtureBase.sol`'s pool snapshot (the `mm_*` fixtures).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MmFxPoolLoan {
    #[serde(default)]
    pub scale: Dec,
    #[serde(default)]
    pub liquid: Dec,
    #[serde(default, rename = "reserveTarget")]
    pub reserve_target: Dec,
    #[serde(default, rename = "priceWad")]
    pub price_wad: Dec,
    #[serde(default, rename = "crossWad")]
    pub cross_wad: Dec,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MmFxResult {
    #[serde(default)]
    pub v: Option<Dec>,
    #[serde(default)]
    pub err: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MmFxPool {
    #[serde(default)]
    pub physical: Dec,
    #[serde(default)]
    pub gross: Dec,
    #[serde(default, rename = "phiWad")]
    pub phi_wad: Dec,
    #[serde(default, rename = "ltvWad")]
    pub ltv_wad: Dec,
    #[serde(default, rename = "roomEpsilonWad")]
    pub room_epsilon_wad: Dec,
    #[serde(default)]
    pub features: Dec,
    #[serde(default, rename = "totalSupply")]
    pub total_supply: Dec,
    #[serde(default)]
    pub loans: Vec<MmFxPoolLoan>,
    #[serde(default, rename = "loanPosition")]
    pub loan_position: Vec<Dec>,
    #[serde(default, rename = "totalAssets")]
    pub total_assets: MmFxResult,
}

impl MmFxPool {
    pub fn state(&self) -> Pool {
        Pool {
            physical: self.physical.0,
            loans: self
                .loans
                .iter()
                .map(|l| LoanCfg {
                    scale: l.scale.0,
                    liquid: l.liquid.0,
                    reserve_target: l.reserve_target.0,
                    ..Default::default()
                })
                .collect(),
            ltv_wad: self.ltv_wad.0,
            phi_wad: self.phi_wad.0,
            room_epsilon_wad: self.room_epsilon_wad.0,
            features: self.features.0,
            price_wad: self
                .loans
                .iter()
                .map(|l| l.price_wad.0)
                .collect(),
            cross_wad: self
                .loans
                .iter()
                .map(|l| l.cross_wad.0)
                .collect(),
        }
    }
}

/// The Go port's own `gatePool` JSON shape (the swap settlement sequences).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GoLoanCfg {
    #[serde(default)]
    pub scale: Dec,
    #[serde(default, rename = "swapPriceBandWad")]
    pub swap_price_band_wad: Dec,
    #[serde(default, rename = "feeFloorWad")]
    pub fee_floor_wad: Dec,
    #[serde(default, rename = "maxSwapNotional")]
    pub max_swap_notional: Dec,
    #[serde(default, rename = "reserveTarget")]
    pub reserve_target: Dec,
    #[serde(default)]
    pub liquid: Dec,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GoPool {
    #[serde(default)]
    pub physical: Dec,
    #[serde(default)]
    pub loans: Vec<GoLoanCfg>,
    #[serde(default, rename = "ltvWad")]
    pub ltv_wad: Dec,
    #[serde(default, rename = "phiWad")]
    pub phi_wad: Dec,
    #[serde(default, rename = "roomEpsilonWad")]
    pub room_epsilon_wad: Dec,
    #[serde(default)]
    pub features: Dec,
    #[serde(default, rename = "priceWad")]
    pub price_wad: Vec<Dec>,
    #[serde(default, rename = "crossWad")]
    pub cross_wad: Vec<Dec>,
}

impl GoPool {
    pub fn state(&self) -> Pool {
        Pool {
            physical: self.physical.0,
            loans: self
                .loans
                .iter()
                .map(|l| LoanCfg {
                    scale: l.scale.0,
                    swap_price_band_wad: l.swap_price_band_wad.0,
                    fee_floor_wad: l.fee_floor_wad.0,
                    max_swap_notional: l.max_swap_notional.0,
                    reserve_target: l.reserve_target.0,
                    liquid: l.liquid.0,
                    ..Default::default()
                })
                .collect(),
            ltv_wad: self.ltv_wad.0,
            phi_wad: self.phi_wad.0,
            room_epsilon_wad: self.room_epsilon_wad.0,
            features: self.features.0,
            price_wad: self
                .price_wad
                .iter()
                .map(|d| d.0)
                .collect(),
            cross_wad: self
                .cross_wad
                .iter()
                .map(|d| d.0)
                .collect(),
        }
    }
}

// ------------------------------------------------------------------ state comparison

/// The fields a settlement writes on a venue, compared after every transition.
pub fn venue_diff(have: &Venue, want: &Venue, i: usize) -> Option<String> {
    if have.morpho.market != want.morpho.market ||
        have.morpho.position != want.morpho.position ||
        have.morpho.rate_at_target != want.morpho.rate_at_target ||
        have.managed_collateral != want.managed_collateral ||
        have.managed_supply_shares != want.managed_supply_shares
    {
        return Some(format!(
            "venue {i} port={{m:{:?} p:{:?} rat:{} mc:{} ms:{}}} sol={{m:{:?} p:{:?} rat:{} mc:{} ms:{}}}",
            have.morpho.market,
            have.morpho.position,
            have.morpho.rate_at_target,
            have.managed_collateral,
            have.managed_supply_shares,
            want.morpho.market,
            want.morpho.position,
            want.morpho.rate_at_target,
            want.managed_collateral,
            want.managed_supply_shares
        ));
    }
    None
}

/// Every field of two Router records except the transient snapshot (the Go `finSeqDiffRouter`).
pub fn router_diff(a: &Router, b: &Router) -> Vec<String> {
    let mut d = Vec::new();
    if a.global_paused != b.global_paused ||
        a.pin_ltv_wad != b.pin_ltv_wad ||
        a.safety_gap_wad != b.safety_gap_wad ||
        a.oracle_band_wad != b.oracle_band_wad ||
        a.max_drawn_assets != b.max_drawn_assets
    {
        d.push("record scalars".to_string());
    }
    if a.loans != b.loans {
        d.push(format!("loans {:?} vs {:?}", a.loans, b.loans));
    }
    if a.borrow_order != b.borrow_order ||
        a.supply_order != b.supply_order ||
        a.withdraw_order != b.withdraw_order ||
        a.repay_order != b.repay_order
    {
        d.push("orders".to_string());
    }
    if a.venues.len() != b.venues.len() {
        d.push("venue count".to_string());
        return d;
    }
    for (i, (x, y)) in a
        .venues
        .iter()
        .zip(&b.venues)
        .enumerate()
    {
        if let Some(s) = venue_diff(x, y, i) {
            d.push(s);
        }
        let mut xc = *x;
        let mut yc = *y;
        xc.morpho.market = Default::default();
        yc.morpho.market = Default::default();
        xc.morpho.position = Default::default();
        yc.morpho.position = Default::default();
        xc.morpho.rate_at_target = U256::ZERO;
        yc.morpho.rate_at_target = U256::ZERO;
        xc.managed_collateral = U256::ZERO;
        yc.managed_collateral = U256::ZERO;
        xc.managed_supply_shares = U256::ZERO;
        yc.managed_supply_shares = U256::ZERO;
        if xc != yc {
            d.push(format!("venue {i} config {xc:?} vs {yc:?}"));
        }
    }
    d
}

/// Overwrites the carried fields (market, position, rateAtTarget, managed) of `dst` from `src`.
pub fn router_carry(dst: &mut Router, src: &Router) {
    for (d, s) in dst.venues.iter_mut().zip(&src.venues) {
        d.morpho.market = s.morpho.market;
        d.morpho.position = s.morpho.position;
        d.morpho.rate_at_target = s.morpho.rate_at_target;
        d.managed_collateral = s.managed_collateral;
        d.managed_supply_shares = s.managed_supply_shares;
    }
}

pub fn pool_diff(a: &Pool, b: &Pool) -> Vec<String> {
    let mut d = Vec::new();
    if a.physical != b.physical {
        d.push(format!("physical {} vs {}", a.physical, b.physical));
    }
    for (i, (x, y)) in a.loans.iter().zip(&b.loans).enumerate() {
        if x.liquid != y.liquid {
            d.push(format!("liquid[{i}] {} vs {}", x.liquid, y.liquid));
        }
    }
    d
}

/// A fixed-per-asset Router stand-in for the gate harness (the mock router of `GateMathFixture` and
/// `GateEdges`): `positions` from given per-asset figures, `quarantine` from given flags.
pub struct FakeRouter {
    pub sup: Vec<U256>,
    pub debt: Vec<U256>,
    pub posted: U256,
    pub q: Vec<crate::evm::protocol::flamm::router::Quarantine>,
}

impl crate::evm::protocol::flamm::gate::GateReads for FakeRouter {
    fn positions(
        &self,
        _now: u64,
    ) -> Result<crate::evm::protocol::flamm::router::Positions, FlammError> {
        Ok(crate::evm::protocol::flamm::router::Positions {
            coll: vec![U256::ZERO; self.sup.len()],
            sup: self.sup.clone(),
            debt: self.debt.clone(),
            total_coll: self.posted,
        })
    }

    fn quarantine(
        &self,
        idx: u8,
        _now: u64,
    ) -> Result<crate::evm::protocol::flamm::router::Quarantine, FlammError> {
        Ok(self.q[idx as usize])
    }
}

// ------------------------------------------------------------------ the gate harness rows

/// One row of `GateEdges` / `GateIntEdges` (`testdata/gen/GateEdges.t.sol`): a book with its frame,
/// the harness arguments, and every harness call's result.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GateRowIn {
    #[serde(default)]
    pub tag: String,
    #[serde(default, deserialize_with = "de_int_default")]
    pub n: usize,
    #[serde(default)]
    pub scale: Vec<Dec>,
    #[serde(default)]
    pub liquid: Vec<Dec>,
    #[serde(default)]
    pub supplied: Vec<Dec>,
    #[serde(default)]
    pub debt: Vec<Dec>,
    #[serde(default)]
    pub price: Vec<Dec>,
    #[serde(default)]
    pub cross: Vec<Dec>,
    #[serde(default)]
    pub usd: Vec<Dec>,
    #[serde(default, rename = "okCross")]
    pub ok_cross: Vec<bool>,
    #[serde(default, rename = "okUsd")]
    pub ok_usd: Vec<bool>,
    #[serde(default, rename = "qAny")]
    pub q_any: Vec<bool>,
    #[serde(default, rename = "qDebt")]
    pub q_debt: Vec<Dec>,
    #[serde(default, rename = "qColl")]
    pub q_coll: Vec<Dec>,
    #[serde(default)]
    pub u0: Vec<SignedDec>,
    #[serde(default)]
    pub physical: Dec,
    #[serde(default)]
    pub posted: Dec,
    #[serde(default)]
    pub ltv: Dec,
    #[serde(default)]
    pub phi: Dec,
    #[serde(default)]
    pub eps: Dec,
    #[serde(default)]
    pub gross0: Dec,
    #[serde(default)]
    pub quarantined: bool,
    #[serde(default, rename = "uArg")]
    pub u_arg: SignedDec,
    #[serde(default, rename = "headArg")]
    pub head_arg: Dec,
    #[serde(default, rename = "ltvArg")]
    pub ltv_arg: Dec,
    #[serde(default, rename = "phiArg")]
    pub phi_arg: Dec,
    #[serde(default, rename = "lltvArg")]
    pub lltv_arg: Dec,
    #[serde(default, rename = "routerLegs", deserialize_with = "de_int_default")]
    pub router_legs: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GateRowLeg {
    #[serde(default, rename = "netL18")]
    pub net_l18: Call,
    #[serde(default, rename = "netPW")]
    pub net_pw: Call,
    #[serde(default, rename = "requiredPosted")]
    pub required_posted: Call,
    #[serde(default, rename = "headOf")]
    pub head_of: Call,
    #[serde(default, rename = "roomNative")]
    pub room_native: Call,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GateRow {
    #[serde(default)]
    pub header: bool,
    #[serde(default, rename = "in")]
    pub input: GateRowIn,
    #[serde(default)]
    pub legs: Vec<GateRowLeg>,
    #[serde(default, rename = "exposurePW")]
    pub exposure_pw: Call,
    #[serde(default)]
    pub gross: Call,
    #[serde(default, rename = "boundPW")]
    pub bound_pw: Call,
    #[serde(default, rename = "boundOf")]
    pub bound_of: Call,
    #[serde(default)]
    pub lift: Call,
    #[serde(default, rename = "roomWad")]
    pub room_wad: Call,
    #[serde(default, rename = "structuralDistWad")]
    pub structural_dist_wad: Call,
    #[serde(default, rename = "requiredPostedAll")]
    pub required_posted_all: Call,
    #[serde(default, rename = "navAt")]
    pub nav_at: Call,
    #[serde(default)]
    pub context: Call,
    #[serde(default, rename = "assertGate")]
    pub assert_gate: Call,
    #[serde(default)]
    pub anchor: Call,
    #[serde(default)]
    pub entry: Call,
    #[serde(default)]
    pub exit: Call,
    #[serde(default, rename = "totalAssets")]
    pub total_assets: Call,
}

/// Whether a leg's `debt * scale` or `(supplied + liquid) * scale` lands in `[2^255, 2^256)`, where
/// Solidity's explicit cast in `netL18` wraps; counted to show the wrap region stays covered.
fn gate_wrapped(l: &crate::evm::protocol::flamm::gate::Leg) -> bool {
    let half = U256::from(1) << 255;
    let d = l.debt.checked_mul(l.scale);
    let a = l
        .supplied
        .checked_add(l.liquid)
        .and_then(|a| a.checked_mul(l.scale));
    d.is_some_and(|d| d >= half) || a.is_some_and(|a| a >= half)
}

/// Compares every harness call of every gate row (header element first) with the port, like the Go
/// `finGateReplay`.
pub fn gate_replay(area: &str, rows: &[GateRow]) {
    use crate::evm::protocol::flamm::gate::{self, Book, Leg};
    let mut rep = Report::new(area);
    let mut wrapped = 0usize;
    let wad = crate::evm::protocol::flamm::math::WAD;
    for (ri, r) in rows.iter().enumerate().skip(1) {
        let inp = &r.input;
        let mut b = Book { physical: inp.physical.0, posted: inp.posted.0, legs: Vec::new() };
        let mut wrap = false;
        for i in 0..inp.n {
            let l = Leg {
                liquid: inp.liquid[i].0,
                supplied: inp.supplied[i].0,
                debt: inp.debt[i].0,
                scale: inp.scale[i].0,
                price_wad: inp.price[i].0,
                cross_wad: inp.cross[i].0,
            };
            wrap |= gate_wrapped(&l);
            b.legs.push(l);
        }
        let mut pool = Pool {
            physical: inp.physical.0,
            ltv_wad: inp.ltv.0,
            phi_wad: inp.phi.0,
            room_epsilon_wad: inp.eps.0,
            ..Default::default()
        };
        for i in 0..inp.n {
            pool.loans.push(LoanCfg {
                scale: inp.scale[i].0,
                liquid: inp.liquid[i].0,
                ..Default::default()
            });
            let pw = if inp.ok_cross[i] { inp.price[i].0 } else { U256::ZERO };
            let cw = if i == 0 {
                wad
            } else if inp.ok_usd[0] && inp.ok_usd[i] && !inp.usd[0].0.is_zero() {
                crate::evm::protocol::flamm::math::mul_div(inp.usd[i].0, wad, inp.usd[0].0)
                    .expect("cross fits")
            } else {
                U256::ZERO
            };
            pool.price_wad.push(pw);
            pool.cross_wad.push(cw);
        }
        if wrap {
            wrapped += 1;
        }
        let fr = FakeRouter {
            sup: inp.supplied[..inp.router_legs]
                .iter()
                .map(|d| d.0)
                .collect(),
            debt: inp.debt[..inp.router_legs]
                .iter()
                .map(|d| d.0)
                .collect(),
            posted: inp.posted.0,
            q: (0..inp.q_any.len())
                .map(|i| crate::evm::protocol::flamm::router::Quarantine {
                    any: inp.q_any[i],
                    frozen_debt: inp.q_debt[i].0,
                    frozen_coll: inp.q_coll[i].0,
                })
                .collect(),
        };
        let ctx = format!("row {ri} {}", inp.tag);
        for i in 0..inp.n {
            let lc = format!("{ctx} leg {i}");
            rep.check_words(
                &format!("{lc} netL18"),
                &r.legs[i].net_l18,
                gate::net_l18(&b.legs[i]).map(|n| vec![n.to_word()]),
            );
            rep.check_words(
                &format!("{lc} netPW"),
                &r.legs[i].net_pw,
                gate::net_pw(&b.legs[i]).map(|n| vec![n.to_word()]),
            );
            rep.check_words(
                &format!("{lc} requiredPosted"),
                &r.legs[i].required_posted,
                gate::required_posted(inp.debt[i].0, inp.scale[i].0, inp.ltv_arg.0, inp.price[i].0)
                    .map(|x| vec![x]),
            );
            rep.check_words(
                &format!("{lc} headOf"),
                &r.legs[i].head_of,
                gate::head_of(&b, i, inp.head_arg.0, inp.ltv_arg.0).map(|x| vec![x]),
            );
            rep.check_words(
                &format!("{lc} roomNative"),
                &r.legs[i].room_native,
                gate::room_native(&pool, &b, i, inp.head_arg.0).map(|x| vec![x]),
            );
        }
        rep.check_words(
            &format!("{ctx} exposurePW"),
            &r.exposure_pw,
            gate::exposure_pw(&b).map(|x| vec![x]),
        );
        rep.check_words(&format!("{ctx} gross"), &r.gross, gate::gross(&b).map(|x| vec![x]));
        rep.check_words(
            &format!("{ctx} boundPW"),
            &r.bound_pw,
            gate::bound_pw(inp.physical.0, inp.ltv_arg.0).map(|x| vec![x]),
        );
        rep.check_words(
            &format!("{ctx} boundOf"),
            &r.bound_of,
            gate::bound_of(inp.physical.0, inp.price[0].0, inp.ltv_arg.0).map(|x| vec![x]),
        );
        rep.check_words(
            &format!("{ctx} lift"),
            &r.lift,
            gate::lift(inp.head_arg.0, inp.ltv_arg.0, inp.phi_arg.0).map(|x| vec![x]),
        );
        rep.check_words(
            &format!("{ctx} roomWad"),
            &r.room_wad,
            gate::room_wad(
                inp.u_arg.0,
                inp.physical.0,
                inp.price[0].0,
                inp.ltv_arg.0,
                inp.phi_arg.0,
            )
            .map(|x| vec![x]),
        );
        rep.check_words(
            &format!("{ctx} structuralDistWad"),
            &r.structural_dist_wad,
            Ok(vec![gate::structural_dist_wad(inp.ltv_arg.0, inp.lltv_arg.0)]),
        );
        rep.check_words(
            &format!("{ctx} requiredPostedAll"),
            &r.required_posted_all,
            gate::required_posted_all(&b, inp.ltv_arg.0).map(|x| vec![x]),
        );
        rep.check_words(&format!("{ctx} navAt"), &r.nav_at, gate::nav_at(&b).map(|x| vec![x]));
        let ts = inp.head_arg.0 % (U256::from(1) << 48);
        rep.check_words(
            &format!("{ctx} context"),
            &r.context,
            gate::context(&b, inp.price[0].0, u64::try_from(ts).expect("48 bits"), inp.gross0.0)
                .map(|c| {
                    vec![
                        c.physical_pool_asset,
                        c.posted_pool_asset,
                        c.liquid_loan_asset,
                        c.supplied_loan_asset,
                        c.debt_loan_asset,
                        c.share_supply,
                        c.price_wad,
                        U256::from(c.price_ts),
                        U256::from(c.loan_count),
                    ]
                }),
        );
        let now = 0u64;
        rep.check_words(
            &format!("{ctx} assertGate"),
            &r.assert_gate,
            gate::assert_gate(&pool, &fr, now).map(|_| vec![]),
        );
        match gate::anchor(&pool, &fr, now) {
            Ok((u0, g0, q)) if r.anchor.ok => {
                let w = r.anchor.words();
                let mut got = vec![w[0], g0, bool_word(q), U256::from(u0.len())];
                got.extend(u0.iter().map(|u| u.to_word()));
                rep.check_words(&format!("{ctx} anchor"), &r.anchor, Ok(got));
            }
            res => rep.check_words(&format!("{ctx} anchor"), &r.anchor, res.map(|_| vec![])),
        }
        let in_u0: Vec<GateInt> = inp
            .u0
            .iter()
            .take(inp.n)
            .map(|s| s.0)
            .collect();
        rep.check_words(
            &format!("{ctx} assertEntryGate"),
            &r.entry,
            gate::assert_entry_gate(&pool, &fr, now, &in_u0, inp.gross0.0, inp.quarantined)
                .map(|_| vec![]),
        );
        rep.check_words(
            &format!("{ctx} assertExitNotWorsened"),
            &r.exit,
            gate::assert_exit_not_worsened(&pool, &fr, now, &in_u0, inp.gross0.0).map(|_| vec![]),
        );
        rep.check_words(
            &format!("{ctx} totalAssets"),
            &r.total_assets,
            gate::total_assets(&pool, &fr, now).map(|x| vec![x]),
        );
    }
    eprintln!("[{area}] int256-wrap rows {wrapped}");
    rep.finish(rows.len() - 1);
}
