// Copyright (c) 2026 Everlong Labs Limited

//! Wei-exact port of the fill-fee law in `EverlongStrategy.sol` (c104 @ `80abd43`,
//! `src/hooks/everlong/EverlongStrategy.sol:132-198`): [`reduction_g`], [`vol_multiplier`],
//! [`log_ratio_abs_wad`] (over Solady's [`ln_wad`]) and [`fill_fee`]. The law carries no hot floor:
//! the per-asset fee floor and cap are applied by the pool core, outside the hook.
//!
//! This is the c104 form of the fee law:
//! - the directional skew is a continuous ramp over the book's own displacement `|w - 1/2|`,
//!   reaching `dirSkew` at `max(invSkewBand, SKEW_RAMP_WAD)` instead of switching on `spot <
//!   anchor`;
//! - a zero reduction coefficient (zero curvature, or a one-sided book) returns `outFee` bare;
//! - the spot input is the hook's `priceAtX(x) * reservationPrice / WAD`, never a sqrt round trip.

use alloy::primitives::{I256, U256};

use super::{
    error::FlammError,
    math::{checked_add, checked_sub, mul_div, sqrt, HALF_WAD, WAD},
};

/// `EverlongStrategy.SKEW_RAMP_WAD` (`EverlongStrategy.sol:136`): the floor on the displacement
/// width over which the directional skew is earned.
pub const SKEW_RAMP_WAD: U256 = U256::from_limbs([30_000_000_000_000_000, 0, 0, 0]);
/// The `1e9` that scales `sqrt(rv)` back to WAD (`EverlongStrategy.sol:156`).
const SIGMA_SCALE: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);

/// `EverlongStrategy.FeeParams` (`EverlongStrategy.sol:28-37`). Every field is `uint64` on chain;
/// the values are carried as `U256` and the one `uint64` subtraction the law performs (`midFeeWad -
/// outFeeWad`) is checked as on chain.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeeParams {
    pub mid_fee_wad: U256,
    pub out_fee_wad: U256,
    pub gamma_wad: U256,
    pub sigma_ref_wad: U256,
    pub vol_beta_wad: U256,
    pub vol_min_wad: U256,
    pub vol_max_wad: U256,
    pub dir_skew_wad: U256,
}

/// `EverlongStrategy.FeeState` (`EverlongStrategy.sol:44-50`): the deployed legs (idle excluded),
/// the reservation price, the live spot in the same frame and the realized-variance EMA.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeeState {
    pub reserve_stable: U256,
    pub reserve_volatile: U256,
    pub anchor_wad: U256,
    pub spot_wad: U256,
    pub rv_wad: U256,
}

/// `EverlongStrategy.reductionG` (`EverlongStrategy.sol:142-150`): `K = 4 Vs Vv / (Vs + Vv)^2` with
/// the total divided twice (so a small book cannot floor `total^2` away), `g = gK / (gK + WAD -
/// K)`, and the volatile value weight `Vv / (Vs + Vv)`. A one-sided book returns `(0, 0)`. The
/// total is summed (checked) before the one-sided test, as on chain.
///
/// Returns `(g, volatile_weight)`.
pub fn reduction_g(s: &FeeState, gamma_wad: U256) -> Result<(U256, U256), FlammError> {
    let vs = s.reserve_stable;
    let vv = mul_div(s.reserve_volatile, s.anchor_wad, WAD)?;
    let total = checked_add(vs, vv)?;
    if vs.is_zero() || vv.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }
    let four_vs = vs
        .checked_mul(U256::from(4))
        .ok_or(FlammError::PanicArithmetic)?;
    let k = mul_div(mul_div(four_vs, WAD, total)?, vv, total)?;
    let gk = mul_div(gamma_wad, k, WAD)?;
    // gK + WAD - K, checked left to right; zero exactly when gamma == 0 on a perfectly
    // value-balanced book (K == WAD), where the division panics.
    let denom = checked_sub(checked_add(gk, WAD)?, k)?;
    let g = mul_div(gk, WAD, denom)?;
    let volatile_weight = mul_div(vv, WAD, total)?;
    Ok((g, volatile_weight))
}

/// `EverlongStrategy.volMultiplier` (`EverlongStrategy.sol:154-161`):
/// `clamp((sqrt(rv) * 1e9 / sigmaRef) * (1 + beta * disloc), vMin, vMax)`; `sigmaRef == 0` disables
/// the term (`WAD`, unclamped).
pub fn vol_multiplier(p: &FeeParams, rv_wad: U256, disloc_wad: U256) -> Result<U256, FlammError> {
    if p.sigma_ref_wad.is_zero() {
        return Ok(WAD);
    }
    // sqrt(2^256) * 1e9 < 2^158: the product cannot overflow.
    let sigma = sqrt(rv_wad) * SIGMA_SCALE;
    let boost = checked_add(WAD, mul_div(p.vol_beta_wad, disloc_wad, WAD)?)?;
    let ratio = mul_div(sigma, WAD, p.sigma_ref_wad)?;
    let mut v = mul_div(ratio, boost, WAD)?;
    if v < p.vol_min_wad {
        v = p.vol_min_wad;
    } else if v > p.vol_max_wad {
        v = p.vol_max_wad;
    }
    Ok(v)
}

/// `EverlongStrategy.logRatioAbsWad` (`EverlongStrategy.sol:78-84`): `|ln(a / b)|` in WAD, zero
/// when either input is zero or the ratio floors to zero. The ratio is reinterpreted as `int256`
/// exactly as the cast does, so a ratio at or above `2^255` reads negative and reverts
/// `LnWadUndefined`.
pub fn log_ratio_abs_wad(a_wad: U256, b_wad: U256) -> Result<U256, FlammError> {
    if a_wad.is_zero() || b_wad.is_zero() {
        return Ok(U256::ZERO);
    }
    let ratio = mul_div(a_wad, WAD, b_wad)?;
    if ratio.is_zero() {
        return Ok(U256::ZERO);
    }
    let r = ln_wad(ratio)?;
    Ok(if r.bit(255) { r.wrapping_neg() } else { r })
}

/// Solady `FixedPointMathLib.lnWad`'s rational-approximation constants
/// (`lib/solady/src/utils/FixedPointMathLib.sol:307-343`), as the raw 256-bit words the opcodes
/// see.
mod ln {
    use alloy::primitives::{uint, U256};

    pub const P0: U256 = uint!(43456485725739037958740375743393_U256);
    pub const P1: U256 = uint!(24828157081833163892658089445524_U256);
    pub const P2: U256 = uint!(3273285459638523848632254066296_U256);
    pub const P3: U256 = uint!(11111509109440967052023855526967_U256);
    pub const P4: U256 = uint!(45023709667254063763336534515857_U256);
    pub const P5: U256 = uint!(14706773417378608786704636184526_U256);
    /// `shl(96, 795164235651350426258249787498)`.
    pub const P6: U256 = uint!(795164235651350426258249787498_U256).wrapping_shl(96);
    pub const Q0: U256 = uint!(5573035233440673466300451813936_U256);
    pub const Q1: U256 = uint!(71694874799317883764090561454958_U256);
    pub const Q2: U256 = uint!(283447036172924575727196451306956_U256);
    pub const Q3: U256 = uint!(401686690394027663651624208769553_U256);
    pub const Q4: U256 = uint!(204048457590392012362485061816622_U256);
    pub const Q5: U256 = uint!(31853899698501571402653359427138_U256);
    pub const Q6: U256 = uint!(909429971244387300277376558375_U256);
    /// The scale factor `s * 5**18 * 2**96`.
    pub const S: U256 = uint!(1677202110996718588342820967067443963516166_U256);
    /// `ln(2) * 5**18 * 2**192`.
    pub const K: U256 =
        uint!(16597577552685614221487285958193947469193820559219878177908093499208371_U256);
    /// `ln(2**96 / 10**18) * 5**18 * 2**192`.
    pub const C: U256 =
        uint!(600920179829731861736702779321621459595472258049074101567377883020018308_U256);
}

/// EVM `sar(n, x)`: the arithmetic shift right of a two's-complement word.
fn sar(x: U256, n: usize) -> U256 {
    I256::from_raw(x).asr(n).into_raw()
}

/// EVM `sdiv(a, b)`: signed division truncating toward zero, zero on a zero divisor and `MIN` for
/// `MIN / -1`.
fn sdiv(a: U256, b: U256) -> U256 {
    if b.is_zero() {
        return U256::ZERO;
    }
    I256::from_raw(a)
        .wrapping_div(I256::from_raw(b))
        .into_raw()
}

/// Solady `FixedPointMathLib.lnWad` (`lib/solady/src/utils/FixedPointMathLib.sol:277-347`), opcode
/// for opcode: `x` and the result are `int256` in two's complement carried as raw words, every
/// `add`, `sub` and `mul` wraps mod `2^256`, and `sar` / `sdiv` are signed, as in the EVM. A
/// non-positive `x` (zero, or a word with the top bit set) reverts `LnWadUndefined`.
pub fn ln_wad(x: U256) -> Result<U256, FlammError> {
    if x.is_zero() || x.bit(255) {
        return Err(FlammError::LnWadUndefined);
    }
    // r = 255 ^ log2(x) = 256 - bitlen(x) for a positive int256 (:286-298).
    let r = 256 - x.bit_len();
    // Reduce the range of x to (1, 2) * 2^96 (:302).
    let xn = (x << r) >> 159;

    // p = sar(96, (P0 + sar(96, (P1 + sar(96, (P2 + x) * x)) * x)) * x) - P3, and on (:307-314).
    let mut p = sar(ln::P2.wrapping_add(xn).wrapping_mul(xn), 96);
    p = sar(ln::P1.wrapping_add(p).wrapping_mul(xn), 96);
    p = sar(ln::P0.wrapping_add(p).wrapping_mul(xn), 96).wrapping_sub(ln::P3);
    p = sar(p.wrapping_mul(xn), 96).wrapping_sub(ln::P4);
    p = sar(p.wrapping_mul(xn), 96).wrapping_sub(ln::P5);
    p = p.wrapping_mul(xn).wrapping_sub(ln::P6);

    // q is monic by convention (:318-324).
    let mut q = ln::Q0.wrapping_add(xn);
    for c in [ln::Q1, ln::Q2, ln::Q3, ln::Q4, ln::Q5, ln::Q6] {
        q = c.wrapping_add(sar(xn.wrapping_mul(q), 96));
    }

    // Finalization (:336-345).
    p = sdiv(p, q);
    p = ln::S.wrapping_mul(p);
    let k = U256::from(159).wrapping_sub(U256::from(r));
    p = ln::K.wrapping_mul(k).wrapping_add(p);
    p = ln::C.wrapping_add(p);
    Ok(sar(p, 174))
}

/// `EverlongStrategy.fillFee` (`EverlongStrategy.sol:169-198`): `min(out + (mid - out) g v, mid)`
/// times the directional multiplier `(1 -/+ skew(|w - 1/2|)) + invSkewKappa * max(0, |w - 1/2| -
/// band)` (the surcharge only on fills that increase the displacement), clamped at 100%. `skew` is
/// proportional to the displacement up to `max(band, SKEW_RAMP_WAD)` and `dirSkew` beyond, so both
/// legs quote the same fee at the tie. A zero `g` (zero curvature or a one-sided book) returns
/// `outFee` bare.
///
/// `loan_asset_in` is true when the taker pays the stable leg (a buy of the volatile asset).
pub fn fill_fee(
    p: &FeeParams,
    s: &FeeState,
    loan_asset_in: bool,
    inv_skew_kappa_wad: U256,
    inv_skew_band_wad: U256,
) -> Result<U256, FlammError> {
    let (g, w) = reduction_g(s, p.gamma_wad)?;
    if g.is_zero() {
        return Ok(p.out_fee_wad);
    }
    // The dislocation is evaluated before volMultiplier runs, sigmaRef == 0 included (:178).
    let disloc = log_ratio_abs_wad(s.spot_wad, s.anchor_wad)?;
    let v = vol_multiplier(p, s.rv_wad, disloc)?;
    // `p.midFeeWad - p.outFeeWad` is a checked uint64 subtraction (:179).
    let span = checked_sub(p.mid_fee_wad, p.out_fee_wad)?;
    let gv = mul_div(g, v, WAD)?;
    let mut f = checked_add(p.out_fee_wad, mul_div(span, gv, WAD)?)?;
    if f > p.mid_fee_wad {
        f = p.mid_fee_wad;
    }

    // Which way the fill pushes the book, read once off the value weight (:188): paying stable
    // removes volatile, so a buy increases the displacement only below one half.
    let increasing = if loan_asset_in { w < HALF_WAD } else { w > HALF_WAD };
    let dev = if w > HALF_WAD { w - HALF_WAD } else { HALF_WAD - w };
    let ramp = if inv_skew_band_wad > SKEW_RAMP_WAD { inv_skew_band_wad } else { SKEW_RAMP_WAD };
    let skew = if dev >= ramp { p.dir_skew_wad } else { mul_div(p.dir_skew_wad, dev, ramp)? };
    // `WAD + skew` and `WAD - skew` are checked (:192); only the subtraction can fail on a uint64
    // skew.
    let mut multiplier = if increasing { checked_add(WAD, skew)? } else { checked_sub(WAD, skew)? };
    if increasing && !inv_skew_kappa_wad.is_zero() && dev > inv_skew_band_wad {
        let surcharge = mul_div(inv_skew_kappa_wad, dev - inv_skew_band_wad, WAD)?;
        multiplier = checked_add(multiplier, surcharge)?;
    }
    f = mul_div(f, multiplier, WAD)?;
    if f > WAD {
        f = WAD;
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ln_wad_edges() {
        // ln(1) == 0 and the non-positive input revert (fee_test.go TestFeeLnWadEdges).
        assert_eq!(ln_wad(WAD), Ok(U256::ZERO));
        for x in [U256::ZERO, U256::from(1) << 255, U256::MAX] {
            assert_eq!(ln_wad(x), Err(FlammError::LnWadUndefined));
        }
        // ln(2) to the value Solady prints; ln(1/2) is negative and one wei past -ln(2), since
        // `sar` floors toward minus infinity, and logRatioAbsWad negates it as the cast
        // does.
        let two = WAD * U256::from(2);
        let ln2 = U256::from(693_147_180_559_945_309u64);
        assert_eq!(ln_wad(two), Ok(ln2));
        let half = WAD / U256::from(2);
        assert_eq!(ln_wad(half), Ok((ln2 + U256::from(1)).wrapping_neg()));
        assert_eq!(log_ratio_abs_wad(half, WAD), Ok(ln2 + U256::from(1)));
        assert_eq!(log_ratio_abs_wad(WAD, half), Ok(ln2));
    }

    #[test]
    fn ln_constants() {
        assert_eq!(
            ln::P6,
            U256::from_str_radix("795164235651350426258249787498", 10).unwrap() << 96
        );
        assert_eq!(SKEW_RAMP_WAD, U256::from(3u64) * U256::from(10u64).pow(U256::from(16)));
    }

    // ------------------------------------------------------------------ DirSkewRamp.t.sol

    fn e(v: u128) -> U256 {
        U256::from(v)
    }

    /// The sealed e0_anchor fee row (fee_test.go feeTestRow).
    fn row() -> FeeParams {
        FeeParams {
            mid_fee_wad: e(30_000_000_000_000_000),
            out_fee_wad: e(5_000_000_000_000_000),
            gamma_wad: e(50_000_000_000_000_000),
            sigma_ref_wad: e(400_000_000_000_000),
            vol_beta_wad: e(4_000_000_000_000_000_000),
            vol_min_wad: e(500_000_000_000_000_000),
            vol_max_wad: e(2_000_000_000_000_000_000),
            dir_skew_wad: e(150_000_000_000_000_000),
        }
    }

    /// DirSkewRampTest._state: a 1e24 book whose volatile value weight is `w`, at the anchor WAD,
    /// with the given spot and rv 1e12.
    fn state(w: U256, spot: U256) -> FeeState {
        let total = U256::from(10u64).pow(U256::from(24));
        let vv = mul_div(total, w, WAD).unwrap();
        FeeState {
            reserve_stable: total - vv,
            reserve_volatile: vv,
            anchor_wad: WAD,
            spot_wad: spot,
            rv_wad: e(1_000_000_000_000),
        }
    }

    /// DirSkewRampTest._old: the law as it shipped before the ramp, verbatim.
    fn old_law(p: &FeeParams, s: &FeeState, loan_in: bool, kappa: U256, band: U256) -> U256 {
        let (g, w) = reduction_g(s, p.gamma_wad).unwrap();
        if g.is_zero() {
            return p.out_fee_wad;
        }
        let disloc = log_ratio_abs_wad(s.spot_wad, s.anchor_wad).unwrap();
        let v = vol_multiplier(p, s.rv_wad, disloc).unwrap();
        let span = p.mid_fee_wad - p.out_fee_wad;
        let gv = mul_div(g, v, WAD).unwrap();
        let mut f = mul_div(span, gv, WAD).unwrap() + p.out_fee_wad;
        if f > p.mid_fee_wad {
            f = p.mid_fee_wad;
        }
        let restoring = (s.spot_wad < s.anchor_wad) == loan_in;
        let mut m = if restoring { WAD - p.dir_skew_wad } else { WAD + p.dir_skew_wad };
        let increasing = if loan_in { w < HALF_WAD } else { w > HALF_WAD };
        if increasing && !kappa.is_zero() {
            let dev = if w > HALF_WAD { w - HALF_WAD } else { HALF_WAD - w };
            if dev > band {
                m += mul_div(kappa, dev - band, WAD).unwrap();
            }
        }
        f = mul_div(f, m, WAD).unwrap();
        if f > WAD {
            f = WAD;
        }
        f
    }

    fn new_law(p: &FeeParams, s: &FeeState, loan_in: bool, kappa: U256, band: U256) -> U256 {
        fill_fee(p, s, loan_in, kappa, band).unwrap()
    }

    fn abs_diff(a: U256, b: U256) -> U256 {
        if a > b {
            a - b
        } else {
            b - a
        }
    }

    fn w(above: bool, dev: u128) -> U256 {
        if above {
            HALF_WAD + U256::from(dev)
        } else {
            HALF_WAD - U256::from(dev)
        }
    }

    /// A: crossing the tie by a nano-weight no longer moves the quoted buy fee by more than dust.
    #[test]
    fn dir_skew_ramp_tie_is_no_longer_a_step() {
        let p = row();
        let (kappa, band) = (e(4_000_000_000_000_000_000), e(60_000_000_000_000_000));
        let lo = state(w(false, 1_000_000_000), WAD + U256::from(1));
        let hi = state(w(true, 1_000_000_000), WAD - U256::from(1));
        let buy_old =
            abs_diff(old_law(&p, &lo, true, kappa, band), old_law(&p, &hi, true, kappa, band));
        let buy_new =
            abs_diff(new_law(&p, &lo, true, kappa, band), new_law(&p, &hi, true, kappa, band));
        assert!(buy_old > e(8_000_000_000_000_000), "the shipped law steps ~90 bp");
        assert!(buy_new < e(10_000_000_000_000), "the ramped law does not step");
    }

    /// B: from max(band, SKEW_RAMP_WAD) outward the ramped law is the shipped law to the wei.
    #[test]
    fn dir_skew_ramp_identical_outside_the_ramp() {
        let p = row();
        let kappa = e(4_000_000_000_000_000_000);
        for b in [0u128, 30_000_000_000_000_000, 60_000_000_000_000_000] {
            let band = e(b);
            let ramp = b.max(30_000_000_000_000_000);
            for i in 1..=40u128 {
                let dev = ramp + i * 10_000_000_000_000_000 / 4;
                if dev >= 500_000_000_000_000_000 {
                    break;
                }
                for above in [false, true] {
                    let spot = if above { WAD - U256::from(1) } else { WAD + U256::from(1) };
                    let s = state(w(above, dev), spot);
                    for loan_in in [true, false] {
                        assert_eq!(
                            old_law(&p, &s, loan_in, kappa, band),
                            new_law(&p, &s, loan_in, kappa, band)
                        );
                    }
                }
            }
        }
    }

    /// C: calm two-sided flow pays what it paid: the two legs' sum is conserved to 2 wei inside the
    /// ramp.
    #[test]
    fn dir_skew_ramp_two_sided_sum_is_conserved() {
        let p = row();
        for i in 0..=20u128 {
            let s = state(w(true, i * 1_000_000_000_000_000), WAD - U256::from(1));
            let sum_old = old_law(&p, &s, true, U256::ZERO, U256::ZERO) +
                old_law(&p, &s, false, U256::ZERO, U256::ZERO);
            let sum_new = new_law(&p, &s, true, U256::ZERO, U256::ZERO) +
                new_law(&p, &s, false, U256::ZERO, U256::ZERO);
            assert!(abs_diff(sum_old, sum_new) < U256::from(3), "i={i}");
        }
    }

    /// D: the widening leg is never cheaper, the envelope is unchanged and the quote has no step.
    #[test]
    fn dir_skew_ramp_continuous_and_bounded() {
        let p = row();
        let (kappa, band) = (e(4_000_000_000_000_000_000), e(60_000_000_000_000_000));
        let mut prev_widen = U256::ZERO;
        for i in 0..=200u128 {
            let dev = i * 1_000_000_000_000_000;
            if dev >= 500_000_000_000_000_000 {
                break;
            }
            let s = state(w(true, dev), WAD - U256::from(1));
            let widen = new_law(&p, &s, false, kappa, band);
            let restore = new_law(&p, &s, true, kappa, band);
            assert!(widen >= restore, "i={i}");
            // bare = old(0,0) * WAD / (WAD - dirSkew); widen <= bare * (WAD + dirSkew + kappa) /
            // WAD + 1
            let bare_old = old_law(&p, &s, true, U256::ZERO, U256::ZERO);
            let bare = bare_old * WAD / (WAD - p.dir_skew_wad);
            let env = bare * (WAD + p.dir_skew_wad + kappa) / WAD + U256::from(1);
            assert!(widen <= env, "i={i}");
            if i > 0 {
                assert!(abs_diff(widen, prev_widen) < e(2_000_000_000_000_000), "i={i}");
            }
            prev_widen = widen;
        }
    }

    /// E: a one-sided book pays the bare out fee on both legs.
    #[test]
    fn dir_skew_ramp_one_sided_book_unchanged() {
        let p = row();
        let s = FeeState {
            reserve_volatile: WAD,
            anchor_wad: WAD,
            spot_wad: WAD,
            ..Default::default()
        };
        for loan_in in [true, false] {
            let f =
                new_law(&p, &s, loan_in, e(4_000_000_000_000_000_000), e(60_000_000_000_000_000));
            assert_eq!(f, p.out_fee_wad);
        }
    }

    /// F: the peak fee the live row can quote is unchanged by the ramp.
    #[test]
    fn dir_skew_ramp_reachable_maximum_unchanged() {
        let p = row();
        let (kappa, band) = (e(4_000_000_000_000_000_000), e(60_000_000_000_000_000));
        let (mut max_old, mut max_new) = (U256::ZERO, U256::ZERO);
        for i in 1..490u128 {
            let s = state(w(true, i * 1_000_000_000_000_000), WAD - U256::from(1));
            for loan_in in [true, false] {
                max_old = max_old.max(old_law(&p, &s, loan_in, kappa, band));
                max_new = max_new.max(new_law(&p, &s, loan_in, kappa, band));
            }
        }
        assert_eq!(max_old, max_new);
    }
}
