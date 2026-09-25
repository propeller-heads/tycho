// Copyright (c) 2026 Everlong Labs Limited

//! `FLAMMGateLib` (`src/core/flamm/FLAMMGateLib.sol` @ c104 `80abd43`): the credit gate
//! `U <= ltv * gross` over per-loan-asset exposure (never netted across assets), the sell room, the
//! hook frame, the NAV and posting laws, and the entry / exit gates.
//!
//! Pure functions over a [`Book`] (the pool's tracked physical and liquid plus the Router's
//! aggregates) and a [`Pool`] (the ledger, dials and the feed's peeks). The signed `int256`
//! arithmetic of `netL18` / `netPW` / the anchor is carried as sign-magnitude ([`GateInt`]) with
//! `int256`'s own semantics: explicit `uint256 -> int256` casts wrap and every signed add / sub /
//! negation is checked. The storage composites read the Router through [`GateReads`], which the
//! tracked Router record implements.

use alloy::primitives::{Address, U256};

use super::{
    context::PoolContext,
    error::FlammError,
    math::{checked_add, checked_mul, div_ceil, mul_div, mul_div_up, WAD},
    router::{Positions, Quarantine},
};

/// `FLAMMStore.MONOTONE_SLACK_WAD = 1e9` (`FLAMMStore.sol:204`).
pub const MONOTONE_SLACK_WAD: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);
/// `FLAMMStore.RELEASE_HYSTERESIS_WAD = 0.1e18` (`FLAMMStore.sol:203`).
pub const RELEASE_HYSTERESIS_WAD: U256 = U256::from_limbs([100_000_000_000_000_000, 0, 0, 0]);
/// `WAD + MONOTONE_SLACK_WAD`.
pub const WAD_PLUS_SLACK: U256 = U256::from_limbs([1_000_000_001_000_000_000, 0, 0, 0]);
/// `FLAMMStore.FEATURE_SUPPLY_LENDING = 1 << 3` (`FLAMMStore.sol:197`).
pub const FEATURE_SUPPLY_LENDING: U256 = U256::from_limbs([1 << 3, 0, 0, 0]);
/// `|type(int256).min| = 2^255`.
const INT_MIN_ABS: U256 = U256::from_limbs([0, 0, 0, 1 << 63]);

/// An `int256` as sign and magnitude; zero is never negative and the value always lies in
/// `[-2^255, 2^255 - 1]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateInt {
    pub neg: bool,
    pub abs: U256,
}

impl GateInt {
    /// The explicit `int256(x)` of a `uint256`: a two's-complement reinterpretation, unchecked, so
    /// `x >= 2^255` reads as `x - 2^256`.
    pub fn cast(x: U256) -> Self {
        if x < INT_MIN_ABS {
            return Self { neg: false, abs: x };
        }
        // 2^256 - x, in [1, 2^255]
        Self { neg: true, abs: U256::ZERO.wrapping_sub(x) }
    }

    /// `a - b` of two magnitudes as a signed value (never out of range).
    fn diff(a: U256, b: U256) -> Self {
        if a < b {
            Self { neg: true, abs: b - a }
        } else {
            Self { neg: false, abs: a - b }
        }
    }

    /// `int256 > 0`.
    pub fn positive(&self) -> bool {
        !self.neg && !self.abs.is_zero()
    }

    /// Solidity's checked `int256` result: `Panic(0x11)` outside `[-2^255, 2^255 - 1]`.
    fn checked(self) -> Result<Self, FlammError> {
        if (self.neg && self.abs > INT_MIN_ABS) || (!self.neg && self.abs >= INT_MIN_ABS) {
            return Err(FlammError::PanicArithmetic);
        }
        Ok(self)
    }

    /// The checked `int256` `x + y`.
    pub fn checked_add(self, y: Self) -> Result<Self, FlammError> {
        if self.neg != y.neg {
            return if self.neg { Self::diff(y.abs, self.abs) } else { Self::diff(self.abs, y.abs) }
                .checked();
        }
        Self { neg: self.neg, abs: checked_add(self.abs, y.abs)? }.checked()
    }

    /// The checked `int256` `x - y`; `y = type(int256).min` is valid here (the flipped magnitude
    /// never escapes).
    pub fn checked_sub(self, mut y: Self) -> Result<Self, FlammError> {
        if !y.abs.is_zero() {
            y.neg = !y.neg;
        }
        self.checked_add(y)
    }

    /// The checked `int256` `-x`: only `type(int256).min` reverts.
    pub fn checked_neg(mut self) -> Result<Self, FlammError> {
        if !self.abs.is_zero() {
            self.neg = !self.neg;
        }
        self.checked()
    }

    /// The two's-complement word of the value, as the contract returns an `int256`.
    pub fn to_word(&self) -> U256 {
        if self.neg {
            U256::ZERO.wrapping_sub(self.abs)
        } else {
            self.abs
        }
    }
}

/// `FLAMMGateLib.Leg` (`FLAMMGateLib.sol:22-29`): one loan asset's position (native units) and
/// frame. `price_wad` is its checked poolAsset cross (zero: unchecked); `cross_wad` is `q_i`,
/// numeraire value per L18 of the asset (`WAD` for asset 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Leg {
    pub liquid: U256,
    pub supplied: U256,
    pub debt: U256,
    pub scale: U256,
    pub price_wad: U256,
    pub cross_wad: U256,
}

/// `FLAMMGateLib.Book` (`FLAMMGateLib.sol:31-35`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Book {
    pub physical: U256,
    pub posted: U256,
    pub legs: Vec<Leg>,
}

/// `FLAMMStore.LoanCfg` (`FLAMMStore.sol:208`): one loan asset's binding, swap policy and liquid.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoanCfg {
    pub token: Address,
    /// `10**(18 - decimals)`.
    pub scale: U256,
    /// The fill's band around the checked cross.
    pub swap_price_band_wad: U256,
    /// This asset's own fee floor.
    pub fee_floor_wad: U256,
    /// Native; zero is uncapped.
    pub max_swap_notional: U256,
    /// Liquid kept back from `supplyCascade`.
    pub reserve_target: U256,
    /// Tracked unlent custody.
    pub liquid: U256,
}

/// The pool ledger and dials the gate and settlement read, plus the feed's peeks at the snapshot:
/// `price_wad[i]` is `peekCross(poolAsset, loan_i)` (zero when not ok) and `cross_wad[i]` is `WAD`
/// for `i == 0`, else `Math.mulDiv(usd_i, WAD, usd_0)` when both USD peeks are ok and `usd_0 != 0`,
/// else zero (`FLAMMGateLib.priceIn`, `FLAMMGateLib.sol:179-193`).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Pool {
    pub physical: U256,
    pub loans: Vec<LoanCfg>,
    pub ltv_wad: U256,
    pub phi_wad: U256,
    pub room_epsilon_wad: U256,
    pub features: U256,
    pub price_wad: Vec<U256>,
    pub cross_wad: Vec<U256>,
}

/// The two Router views the storage-reading gate functions make (`IMMRouter.positions` and
/// `IMMRouter.quarantine`); the tracked Router record answers them from its venues.
pub trait GateReads {
    fn positions(&self, now: u64) -> Result<Positions, FlammError>;
    fn quarantine(&self, idx: u8, now: u64) -> Result<Quarantine, FlammError>;
}

// ------------------------------------------------------------------ pure law

/// `FLAMMGateLib.netL18` (`FLAMMGateLib.sol:39-41`): `int256(debt * scale) - int256((supplied +
/// liquid) * scale)`, positive when a net borrower. The casts wrap and the subtraction is checked.
pub fn net_l18(l: &Leg) -> Result<GateInt, FlammError> {
    let d = checked_mul(l.debt, l.scale)?;
    let a = checked_mul(checked_add(l.supplied, l.liquid)?, l.scale)?;
    GateInt::cast(d).checked_sub(GateInt::cast(a))
}

/// `FLAMMGateLib.netPW` (`FLAMMGateLib.sol:45-53`): the net in poolAsset-WAD, the borrower side
/// ceiled and the surplus floored; a positioned leg at an unchecked feed reverts `PriceUnchecked`.
/// `int256(ceilDiv(uint256(n) * WAD, p))` and `-int256((uint256(-n) * WAD) / p)`: both casts wrap,
/// both negations are checked.
pub fn net_pw(l: &Leg) -> Result<GateInt, FlammError> {
    let n = net_l18(l)?;
    if l.price_wad.is_zero() {
        if !n.abs.is_zero() {
            return Err(FlammError::PriceUnchecked);
        }
        return Ok(GateInt::default());
    }
    if !n.neg {
        let w = checked_mul(n.abs, WAD)?;
        return Ok(GateInt::cast(div_ceil(w, l.price_wad)?));
    }
    let m = n.checked_neg()?;
    let w = checked_mul(m.abs, WAD)?;
    GateInt::cast(w / l.price_wad).checked_neg()
}

/// `FLAMMGateLib.exposurePW` (`FLAMMGateLib.sol:56-61`): `U = sum of max(0, netPW_i)`.
pub fn exposure_pw(b: &Book) -> Result<U256, FlammError> {
    let mut u = U256::ZERO;
    for l in &b.legs {
        let n = net_pw(l)?;
        if n.positive() {
            u = checked_add(u, n.abs)?;
        }
    }
    Ok(u)
}

/// `FLAMMGateLib.gross` (`FLAMMGateLib.sol:63-65`): `physical + posted`.
pub fn gross(b: &Book) -> Result<U256, FlammError> {
    checked_add(b.physical, b.posted)
}

/// `FLAMMGateLib.boundPW` (`FLAMMGateLib.sol:68-70`): `gross * ltv`.
pub fn bound_pw(gross: U256, ltv_wad: U256) -> Result<U256, FlammError> {
    checked_mul(gross, ltv_wad)
}

/// `FLAMMGateLib.boundOf` (`FLAMMGateLib.sol:73-75`): `mulDiv(gross * price, ltv, WAD)`.
pub fn bound_of(gross: U256, price_wad: U256, ltv_wad: U256) -> Result<U256, FlammError> {
    mul_div(checked_mul(gross, price_wad)?, ltv_wad, WAD)
}

/// `FLAMMGateLib.lift` (`FLAMMGateLib.sol:78-80`): `mulDiv(head, WAD, WAD - mulDiv(ltv, phi,
/// WAD))`.
pub fn lift(head: U256, ltv_wad: U256, phi_wad: U256) -> Result<U256, FlammError> {
    let lp = mul_div(ltv_wad, phi_wad, WAD)?;
    if lp > WAD {
        return Err(FlammError::PanicArithmetic);
    }
    mul_div(head, WAD, WAD - lp)
}

/// `FLAMMGateLib.roomWad` (`FLAMMGateLib.sol:84-97`): `(ltv*C - u)^+ / (1 - ltv*phi)`, a surplus
/// `u` adding to the headroom (`uint256(-u)`, the negation checked).
pub fn room_wad(
    u: GateInt,
    gross: U256,
    price_wad: U256,
    ltv_wad: U256,
    phi_wad: U256,
) -> Result<U256, FlammError> {
    let ltv_c = bound_of(gross, price_wad, ltv_wad)?;
    let headroom = if !u.neg {
        ltv_c.saturating_sub(u.abs)
    } else {
        let m = u.checked_neg()?;
        checked_add(ltv_c, m.abs)?
    };
    lift(headroom, ltv_wad, phi_wad)
}

/// `FLAMMGateLib.headOf` (`FLAMMGateLib.sol:101-107`): the credit-zero headroom of leg `idx` in L18
/// of that asset -- the standing bound less every asset's exposure, plus this asset's own surplus
/// (another asset's surplus never counts).
pub fn head_of(b: &Book, idx: usize, u: U256, ltv_wad: U256) -> Result<U256, FlammError> {
    let bound = bound_pw(gross(b)?, ltv_wad)?;
    let mut head_pw = bound.saturating_sub(u);
    let l = b
        .legs
        .get(idx)
        .ok_or(FlammError::PanicIndex)?;
    let n = net_pw(l)?;
    if n.neg {
        let m = n.checked_neg()?;
        head_pw = checked_add(head_pw, m.abs)?;
    }
    mul_div(head_pw, l.price_wad, WAD)
}

/// `FLAMMGateLib.structuralDistWad` (`FLAMMGateLib.sol:110-113`): `1 - ltv/lltv` with the quotient
/// ceiled.
pub fn structural_dist_wad(ltv_wad: U256, lltv_wad: U256) -> U256 {
    if lltv_wad.is_zero() || ltv_wad >= lltv_wad {
        return U256::ZERO;
    }
    // ltv < lltv, so the rounded-up quotient is at most WAD and cannot overflow
    let q = mul_div_up(ltv_wad, WAD, lltv_wad).unwrap_or(WAD);
    WAD - q
}

/// `FLAMMGateLib.requiredPosted` (`FLAMMGateLib.sol:116-123`): `mulDivUp(debt * scale, WAD, ltv *
/// price)` poolAsset units.
pub fn required_posted(
    gross_debt: U256,
    loan_scale: U256,
    ltv_wad: U256,
    price_wad: U256,
) -> Result<U256, FlammError> {
    if gross_debt.is_zero() {
        return Ok(U256::ZERO);
    }
    let num = checked_mul(gross_debt, loan_scale)?;
    let den = checked_mul(ltv_wad, price_wad)?;
    mul_div_up(num, WAD, den)
}

/// `FLAMMGateLib.requiredPostedAll` (`FLAMMGateLib.sol:126-133`): the posting law over every
/// indebted asset.
pub fn required_posted_all(b: &Book, ltv_wad: U256) -> Result<U256, FlammError> {
    let mut need = U256::ZERO;
    for l in &b.legs {
        if l.debt.is_zero() {
            continue;
        }
        if l.price_wad.is_zero() {
            return Err(FlammError::PriceUnchecked);
        }
        need = checked_add(need, required_posted(l.debt, l.scale, ltv_wad, l.price_wad)?)?;
    }
    Ok(need)
}

/// `FLAMMGateLib.navAt` (`FLAMMGateLib.sol:136-149`): `physical + posted + sum floor(assets_i /
/// p_i) - sum ceil(debt_i / p_i)`, floored at 0.
pub fn nav_at(b: &Book) -> Result<U256, FlammError> {
    let mut plus = gross(b)?;
    let mut minus = U256::ZERO;
    for l in &b.legs {
        if l.price_wad.is_zero() {
            if !l.liquid.is_zero() || !l.supplied.is_zero() || !l.debt.is_zero() {
                return Err(FlammError::PriceUnchecked);
            }
            continue;
        }
        let a = checked_mul(checked_add(l.liquid, l.supplied)?, l.scale)? / l.price_wad;
        plus = checked_add(plus, a)?;
        let d = checked_mul(l.debt, l.scale)?;
        minus = checked_add(minus, div_ceil(d, l.price_wad)?)?;
    }
    Ok(plus.saturating_sub(minus))
}

/// `FLAMMGateLib.context` (`FLAMMGateLib.sol:222-245`): the poolAsset side native, the loan side
/// aggregated into the numeraire (N18, loan asset 0's value) with liquid and supplied floored and
/// debt ceiled; an idle leg at an unchecked cross contributes nothing, a positioned one reverts
/// `PriceUnchecked`.
pub fn context(
    b: &Book,
    price_wad: U256,
    price_ts: u64,
    supply: U256,
) -> Result<PoolContext, FlammError> {
    let mut c = PoolContext {
        physical_pool_asset: b.physical,
        posted_pool_asset: b.posted,
        share_supply: supply,
        price_wad,
        price_ts,
        loan_count: b.legs.len() as u8,
        ..Default::default()
    };
    for l in &b.legs {
        if l.cross_wad.is_zero() {
            if !l.liquid.is_zero() || !l.supplied.is_zero() || !l.debt.is_zero() {
                return Err(FlammError::PriceUnchecked);
            }
            continue;
        }
        c.liquid_loan_asset = checked_add(
            c.liquid_loan_asset,
            mul_div(checked_mul(l.liquid, l.scale)?, l.cross_wad, WAD)?,
        )?;
        c.supplied_loan_asset = checked_add(
            c.supplied_loan_asset,
            mul_div(checked_mul(l.supplied, l.scale)?, l.cross_wad, WAD)?,
        )?;
        c.debt_loan_asset = checked_add(
            c.debt_loan_asset,
            mul_div_up(checked_mul(l.debt, l.scale)?, l.cross_wad, WAD)?,
        )?;
    }
    Ok(c)
}

/// `FLAMMGateLib.roomNative` (`FLAMMGateLib.sol:248-252`): `lift(headOf)` less the epsilon shave (a
/// checked subtraction), in native units of asset `idx`.
pub fn room_native(p: &Pool, b: &Book, idx: usize, u: U256) -> Result<U256, FlammError> {
    let head = head_of(b, idx, u, p.ltv_wad)?;
    let raw = lift(head, p.ltv_wad, p.phi_wad)?;
    let shave = mul_div(raw, p.room_epsilon_wad, WAD)?;
    if shave > raw {
        return Err(FlammError::PanicArithmetic);
    }
    let raw = raw - shave;
    let scale = b
        .legs
        .get(idx)
        .ok_or(FlammError::PanicIndex)?
        .scale;
    if scale.is_zero() {
        return Err(FlammError::PanicDivZero);
    }
    Ok(raw / scale)
}

// ------------------------------------------------------------------ storage composites

/// `FLAMMGateLib.book` (`FLAMMGateLib.sol:153-163`): tracked physical and liquid with the Router's
/// per-asset aggregates, price-free.
pub fn book_of<R: GateReads>(p: &Pool, r: &R, now: u64) -> Result<Book, FlammError> {
    let pos = r.positions(now)?;
    let mut b = Book {
        physical: p.physical,
        posted: pos.total_coll,
        legs: Vec::with_capacity(p.loans.len()),
    };
    for (i, c) in p.loans.iter().enumerate() {
        if i >= pos.sup.len() || i >= pos.debt.len() {
            return Err(FlammError::PanicIndex);
        }
        b.legs.push(Leg {
            liquid: c.liquid,
            supplied: pos.sup[i],
            debt: pos.debt[i],
            scale: c.scale,
            ..Default::default()
        });
    }
    Ok(b)
}

/// `FLAMMGateLib.priced` (`FLAMMGateLib.sol:196-199`): the book with every leg's cross and
/// numeraire frame (`priceIn`, `:179-193`, from the pool's recorded peeks).
pub fn priced<R: GateReads>(p: &Pool, r: &R, now: u64) -> Result<Book, FlammError> {
    let mut b = book_of(p, r, now)?;
    for (i, l) in b.legs.iter_mut().enumerate() {
        l.price_wad = p
            .price_wad
            .get(i)
            .copied()
            .ok_or(FlammError::PanicIndex)?;
        l.cross_wad = if i == 0 {
            WAD
        } else {
            p.cross_wad
                .get(i)
                .copied()
                .ok_or(FlammError::PanicIndex)?
        };
    }
    Ok(b)
}

/// `FLAMMGateLib.priceWads` / `priceVector` (`FLAMMGateLib.sol:202-218`): the per-asset crosses
/// reclaim takes.
pub fn price_wads(b: &Book) -> Vec<U256> {
    b.legs
        .iter()
        .map(|l| l.price_wad)
        .collect()
}

/// `FLAMMGateLib.assertGate` (`FLAMMGateLib.sol:255-258`): `exposurePW > gross * ltv` reverts
/// `LedgerBoundBreached`.
pub fn assert_gate<R: GateReads>(p: &Pool, r: &R, now: u64) -> Result<(), FlammError> {
    let b = priced(p, r, now)?;
    assert_bound(&b, p.ltv_wad)
}

fn assert_bound(b: &Book, ltv_wad: U256) -> Result<(), FlammError> {
    let u = exposure_pw(b)?;
    let bound = bound_pw(gross(b)?, ltv_wad)?;
    if u > bound {
        return Err(FlammError::LedgerBoundBreached);
    }
    Ok(())
}

/// `FLAMMGateLib.anchor` (`FLAMMGateLib.sol:263-267`): the READABLE venues' signed per-asset
/// exposure (a quarantined venue's counted debt subtracted), the gross poolAsset, and whether any
/// venue is quarantined. Returns `(u0, gross0, quarantined)`.
pub fn anchor<R: GateReads>(
    p: &Pool,
    r: &R,
    now: u64,
) -> Result<(Vec<GateInt>, U256, bool), FlammError> {
    let b = book_of(p, r, now)?;
    let (q, u) = readable_u(r, &b, now)?;
    let g = gross(&b)?;
    Ok((u, g, q))
}

/// `FLAMMGateLib._readableU` (`FLAMMGateLib.sol:269-276`).
fn readable_u<R: GateReads>(r: &R, b: &Book, now: u64) -> Result<(bool, Vec<GateInt>), FlammError> {
    let mut quarantined = false;
    let mut u = Vec::with_capacity(b.legs.len());
    for (i, l) in b.legs.iter().enumerate() {
        let qr = r.quarantine(i as u8, now)?;
        if qr.any {
            quarantined = true;
        }
        let n = net_l18(l)?;
        let f = checked_mul(qr.frozen_debt, l.scale)?;
        u.push(n.checked_sub(GateInt::cast(f))?);
    }
    Ok((quarantined, u))
}

/// `FLAMMGateLib.assertEntryGate` (`FLAMMGateLib.sol:296-330`): the absolute bound, else no drawn
/// asset's `u / gross` may worsen (per asset, 1e-9 slack). A quarantined anchor restores one frame
/// on both sides: the frozen collateral is added back to both grosses and the anchor's frozen-debt
/// subtraction undone.
pub fn assert_entry_gate<R: GateReads>(
    p: &Pool,
    r: &R,
    now: u64,
    u0: &[GateInt],
    mut gross0: U256,
    quarantined: bool,
) -> Result<(), FlammError> {
    let b = priced(p, r, now)?;
    let mut g1 = gross(&b)?;
    let n = b.legs.len();
    let mut f_debt: Vec<U256> = Vec::new();
    if quarantined {
        let mut f_coll = U256::ZERO;
        for i in 0..n {
            let qr = r.quarantine(i as u8, now)?;
            f_debt.push(qr.frozen_debt);
            f_coll = checked_add(f_coll, qr.frozen_coll)?;
        }
        g1 = checked_add(g1, f_coll)?;
        gross0 = checked_add(gross0, f_coll)?;
    }
    let u = exposure_pw(&b)?;
    let bound = bound_pw(g1, p.ltv_wad)?;
    if u <= bound {
        return Ok(());
    }
    for (i, l) in b.legs.iter().enumerate() {
        let u1 = net_l18(l)?;
        if !u1.positive() {
            continue;
        }
        let mut base = *u0
            .get(i)
            .ok_or(FlammError::PanicIndex)?;
        if quarantined {
            let f = checked_mul(f_debt[i], l.scale)?;
            base = base.checked_add(GateInt::cast(f))?;
        }
        if !base.positive() {
            return Err(FlammError::LedgerBoundBreached);
        }
        require_not_worsened(base.abs, gross0, u1.abs, g1, FlammError::LedgerBoundBreached)?;
    }
    Ok(())
}

/// `FLAMMGateLib.assertExitNotWorsened` (`FLAMMGateLib.sol:333-342`): price-free, per asset, over
/// readable venues.
pub fn assert_exit_not_worsened<R: GateReads>(
    p: &Pool,
    r: &R,
    now: u64,
    u0: &[GateInt],
    gross0: U256,
) -> Result<(), FlammError> {
    let b = book_of(p, r, now)?;
    let (_, u1) = readable_u(r, &b, now)?;
    let g1 = gross(&b)?;
    for (i, u1i) in u1.iter().enumerate() {
        if !u1i.positive() {
            continue;
        }
        let u0i = u0
            .get(i)
            .ok_or(FlammError::PanicIndex)?;
        if !u0i.positive() {
            return Err(FlammError::ExitWorsensLedger);
        }
        require_not_worsened(u0i.abs, gross0, u1i.abs, g1, FlammError::ExitWorsensLedger)?;
    }
    Ok(())
}

/// `FLAMMGateLib._requireNotWorsened` (`FLAMMGateLib.sol:344-354`): `u1 * g0 <= mulDiv(u0 * g1, WAD
/// + slack, WAD)`.
fn require_not_worsened(
    u0: U256,
    g0: U256,
    u1: U256,
    g1: U256,
    fail: FlammError,
) -> Result<(), FlammError> {
    if g0.is_zero() {
        return Ok(());
    }
    let lhs = checked_mul(u1, g0)?;
    let rhs = mul_div(checked_mul(u0, g1)?, WAD_PLUS_SLACK, WAD)?;
    if lhs > rhs {
        return Err(fail);
    }
    Ok(())
}

/// `FLAMMGateLib.totalAssets` (`FLAMMGateLib.sol:358-360`): `navAt` over the priced book.
pub fn total_assets<R: GateReads>(p: &Pool, r: &R, now: u64) -> Result<U256, FlammError> {
    nav_at(&priced(p, r, now)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(neg: bool, abs: u64) -> GateInt {
        GateInt { neg, abs: U256::from(abs) }
    }

    #[test]
    fn constants() {
        assert_eq!(WAD_PLUS_SLACK, WAD + MONOTONE_SLACK_WAD);
        assert_eq!(INT_MIN_ABS, U256::from(1) << 255);
    }

    #[test]
    fn int256_semantics() {
        let min = GateInt { neg: true, abs: INT_MIN_ABS };
        let max = GateInt { neg: false, abs: INT_MIN_ABS - U256::from(1) };
        assert_eq!(GateInt::cast(INT_MIN_ABS), min);
        assert_eq!(GateInt::cast(U256::MAX), g(true, 1));
        assert_eq!(GateInt::cast(INT_MIN_ABS - U256::from(1)), max);
        assert_eq!(min.checked_neg(), Err(FlammError::PanicArithmetic));
        assert_eq!(max.checked_neg().unwrap(), GateInt { neg: true, abs: max.abs });
        assert_eq!(max.checked_add(g(false, 1)), Err(FlammError::PanicArithmetic));
        assert_eq!(min.checked_sub(g(false, 1)), Err(FlammError::PanicArithmetic));
        assert_eq!(g(false, 0).checked_sub(min), Err(FlammError::PanicArithmetic));
        assert_eq!(min.checked_add(g(false, 0)), Ok(min));
        assert_eq!(g(true, 5).checked_add(g(false, 7)), Ok(g(false, 2)));
        assert_eq!(g(false, 5).checked_add(g(true, 7)), Ok(g(true, 2)));
        assert_eq!(g(false, 5).checked_sub(g(false, 5)), Ok(g(false, 0)));
        assert_eq!(g(true, 3).to_word(), U256::MAX - U256::from(2));
        assert!(!g(false, 0).positive() && g(false, 1).positive() && !g(true, 1).positive());
    }

    #[test]
    fn net_pw_rounds_against_the_room() {
        let l = Leg {
            debt: U256::from(3),
            scale: U256::from(1),
            price_wad: U256::from(2_000_000_000_000_000_000u64),
            ..Default::default()
        };
        // 3 / 2 rounds up on the borrower side
        assert_eq!(net_pw(&l), Ok(g(false, 2)));
        let l = Leg { liquid: U256::from(3), ..l };
        // net zero: exactly zero either way
        assert_eq!(net_pw(&l), Ok(g(false, 0)));
        let l = Leg { liquid: U256::from(4), ..l };
        // -1 / 2 floors toward zero on the surplus side
        assert_eq!(net_pw(&l), Ok(g(false, 0)));
        let l = Leg { liquid: U256::from(5), ..l };
        assert_eq!(net_pw(&l), Ok(g(true, 1)));
        let l = Leg { price_wad: U256::ZERO, ..l };
        assert_eq!(net_pw(&l), Err(FlammError::PriceUnchecked));
    }
}
