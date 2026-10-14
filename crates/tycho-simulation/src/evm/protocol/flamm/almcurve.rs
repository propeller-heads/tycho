// Copyright (c) 2026 Everlong Labs Limited

//! Wei-exact port of the swap/book half of `AlmCurve.sol` (c104 @ `80abd43`,
//! `src/hooks/everlong/AlmCurve.sol`): the normalized reservation curve the `EverlongHook` trades
//! on.
//!
//! Normalized inventory (`x` volatile, `y` stable) sits on `y^2 + (x + c) y - k / x = 0` with
//! `c = 1/(2A) - 1` and `4k = 1/(2A)`; the hook holds a band of it ([`Support`]) scaled by `kappa`
//! and located by an anchor sqrt price. The recenter-only solvers (`reseed`, `reseedAt`,
//! `xAtValueRatio`, `_scaleAt`) are not on the swap path and are not ported.
//!
//! Every division floors unless named `_ceil` and the square root is the floor root, exactly as on
//! chain. Two properties are load-bearing at the wei scale and must survive any edit:
//!
//! - [`y_at_x`] forms the radicand as ONE sum before the square root (`AlmCurve.sol:135-142`). Two
//!   independently floored terms move in opposite directions in `x`, so their sum can step UP as
//!   `x` increases, a non-monotone `y`, which the stable-in bisection brackets on.
//! - `c` is negative for every valid `A` (`A > WAD/2`), so `b = x + c` is carried as a magnitude
//!   plus sign to avoid the catastrophic cancellation of the naive `(-b + root) / 2` form
//!   (`AlmCurve.sol:117-118`, `:125-127`).

use alloy::primitives::U256;

use super::{
    error::FlammError,
    math::{mul_div, mul_div_up, sqrt, Q96, WAD, WAD_SQUARED},
};

/// `AlmCurve.MIN_X_WAD` (`AlmCurve.sol:24`).
pub const MIN_X_WAD: U256 = U256::from_limbs([1_000_000_000_000_000, 0, 0, 0]);
/// `AlmCurve.MAX_X_WAD` (`AlmCurve.sol:25`).
pub const MAX_X_WAD: U256 = U256::from_limbs([1_999_000_000_000_000_000, 0, 0, 0]);
/// `AlmCurve.MIN_A_WAD = 5e17 + 1` (`AlmCurve.sol:19`).
pub const MIN_A_WAD: U256 = U256::from_limbs([500_000_000_000_000_001, 0, 0, 0]);
/// `AlmCurve.MAX_A_WAD = 1000e18` (`AlmCurve.sol:20`).
pub const MAX_A_WAD: U256 = U256::from_limbs([0x35c9_adc5_dea0_0000, 0x36, 0, 0]);

/// The conservative bracket around the `x <-> y` symmetry seed (`AlmCurve.sol:276`).
const SEED_WINDOW: u64 = 1 << 16;

/// `AlmCurve.Support` (`AlmCurve.sol:46-54`): the funded band `[x_lo, x_hi]` of the curve for
/// amplification `a_wad`, with `y_hi = y(x_hi)` the stable offset.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Support {
    pub a_wad: U256,
    /// High-price edge; the volatile leg is exactly zero here.
    pub x_lo: U256,
    /// Low-price edge; the stable leg is exactly zero here.
    pub x_hi: U256,
    /// `y(x_hi)`, the stable offset; equal to `x_lo` iff the span is symmetric.
    pub y_hi: U256,
}

/// A normalized exact-input fill (`AlmCurve.swapExactIn`): the gross output on the other leg, the
/// new coordinate and the input the funded band left unused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NormalizedFill {
    pub amount_out: U256,
    pub x_after: U256,
    pub input_unused: U256,
}

/// A token-unit exact-input fill (`AlmCurve.swapExactInX96`): `amount_out` gross of any fee and
/// `amount_in_unspent` the part of the input the fill did not charge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenFill {
    pub amount_out: U256,
    pub x_after: U256,
    pub amount_in_unspent: U256,
}

/// `-cWad(aWad) = WAD - WAD^2 / (2A)`, the magnitude of the always-negative `c`, together with
/// `fourK = WAD^2 / (2A) = c + WAD` (`AlmCurve.cWad`, `AlmCurve.sol:119-122`, and
/// `AlmCurve.sol:134`). Reverts `CurveAmplification` outside `[MIN_A_WAD, MAX_A_WAD]`.
///
/// Returns `(neg_c, four_k)`.
pub fn neg_c(a_wad: U256) -> Result<(U256, U256), FlammError> {
    if a_wad < MIN_A_WAD || a_wad > MAX_A_WAD {
        return Err(FlammError::CurveAmplification);
    }
    // Math.mulDiv(WAD, WAD, 2 * aWad): a 256-bit product over a non-zero denominator.
    let four_k = WAD_SQUARED / (a_wad << 1);
    Ok((WAD - four_k, four_k))
}

/// `AlmCurve.cWad` as the `int256` the contract returns, in two's complement
/// (`AlmCurve.sol:119-122`).
pub fn c_wad_raw(a_wad: U256) -> Result<U256, FlammError> {
    let (_, four_k) = neg_c(a_wad)?;
    Ok(four_k.wrapping_sub(WAD))
}

/// `AlmCurve.yAtX` (`AlmCurve.sol:128-144`): the positive root of `y^2 + (x + c) y - k / x = 0`, as
/// `(root -/+ |b|) / 2` branching on `sign(b)`. The domain check precedes the amplification check,
/// as on chain.
pub fn y_at_x(x: U256, a_wad: U256) -> Result<U256, FlammError> {
    if x < MIN_X_WAD || x > MAX_X_WAD {
        return Err(FlammError::CurveDomain);
    }
    let (neg_c, four_k) = neg_c(a_wad)?;
    // b = x + c = x - |c| (:131-132).
    let b_neg = x < neg_c;
    let abs_b = if b_neg { neg_c - x } else { x - neg_c };
    // ONE floor in the radicand (:142): abs_b^2 is exact (<= 4e36), the k-term floors once (<=
    // 1e39).
    let term = mul_div(four_k, WAD_SQUARED, x)?;
    let root = sqrt(abs_b * abs_b + term);
    let y = if b_neg { root + abs_b } else { root - abs_b };
    Ok(y >> 1)
}

/// `AlmCurve.priceAtX` (`AlmCurve.sol:151-158`): the normalized marginal price
/// `F_x / F_y = y (2x + y + c) / (x (x + 2y + c))`, exactly `WAD` at `x = WAD/2`. A non-positive
/// numerator or denominator reverts `CurveDomain`.
pub fn price_at_x(x: U256, a_wad: U256) -> Result<U256, FlammError> {
    let y = y_at_x(x, a_wad)?;
    let (neg_c, _) = neg_c(a_wad)?;
    // num = y * (2x + y + c), den = x * (x + 2y + c) (:154-155); every term is < 2^70, so the
    // signed sums are formed as unsigned sums compared against |c|.
    let s_num = (x << 1) + y;
    let s_den = (y << 1) + x;
    if s_num <= neg_c || s_den <= neg_c || y.is_zero() {
        return Err(FlammError::CurveDomain);
    }
    let num = y * (s_num - neg_c);
    let den = x * (s_den - neg_c);
    mul_div(num, WAD, den)
}

/// `AlmCurve.xAtPrice` (`AlmCurve.sol:167-179`): the coordinate whose marginal price is
/// `target_price_wad`, by bisection over the whole domain (clamping at the edges). Used only by
/// [`support_for`] on the swap path.
pub fn x_at_price(target_price_wad: U256, a_wad: U256) -> Result<U256, FlammError> {
    let mut lo = MIN_X_WAD;
    let mut hi = MAX_X_WAD;
    if target_price_wad >= price_at_x(lo, a_wad)? {
        return Ok(lo);
    }
    if target_price_wad <= price_at_x(hi, a_wad)? {
        return Ok(hi);
    }
    for _ in 0..128 {
        let mid = (lo + hi) >> 1;
        if mid == lo {
            break;
        }
        if price_at_x(mid, a_wad)? > target_price_wad {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok((lo + hi) >> 1)
}

/// `AlmCurve.supportFor(aWad, spanUpWad, spanDnWad)` (`AlmCurve.sol:78-87`): the funded band from a
/// price span, rejecting a span the domain clamp would silently widen.
pub fn support_for(
    a_wad: U256,
    span_up_wad: U256,
    span_dn_wad: U256,
) -> Result<Support, FlammError> {
    if span_up_wad <= WAD || span_dn_wad <= WAD {
        return Err(FlammError::CurveSpan);
    }
    let x_lo = x_at_price(span_up_wad, a_wad)?;
    let x_hi = x_at_price(mul_div(WAD, WAD, span_dn_wad)?, a_wad)?;
    if x_lo <= MIN_X_WAD || x_hi >= MAX_X_WAD || x_lo >= x_hi {
        return Err(FlammError::CurveSpan);
    }
    let y_hi = y_at_x(x_hi, a_wad)?;
    Ok(Support { a_wad, x_lo, x_hi, y_hi })
}

/// `AlmCurve.heldAt` (`AlmCurve.sol:99-106`): the normalized inventory held at `x_wad`, clamped to
/// the band: volatile `x - x_lo` and stable `y(x) - y_hi` (zero-guarded).
///
/// Returns `(volatile_wad, stable_wad)`.
pub fn held_at(sup: &Support, x_wad: U256) -> Result<(U256, U256), FlammError> {
    let x = if x_wad < sup.x_lo {
        sup.x_lo
    } else if x_wad > sup.x_hi {
        sup.x_hi
    } else {
        x_wad
    };
    // Only a malformed band (x_hi < x_lo) reaches this checked subtraction (:101).
    let volatile_wad = x
        .checked_sub(sup.x_lo)
        .ok_or(FlammError::PanicArithmetic)?;
    let y = y_at_x(x, sup.a_wad)?;
    Ok((volatile_wad, y.saturating_sub(sup.y_hi)))
}

/// `AlmCurve.reservesAt` (`AlmCurve.sol:311-320`): token reserves at `x_wad` for anchor and scale,
/// `stable = kappa * held * a / Q96` and `volatile = kappa * held * Q96 / a`, both legs floored
/// twice.
///
/// Returns `(stable, volatile_amount)`.
pub fn reserves_at(
    sup: &Support,
    anchor_sqrt_x96: U256,
    kappa: U256,
    x_wad: U256,
) -> Result<(U256, U256), FlammError> {
    if anchor_sqrt_x96.is_zero() {
        return Err(FlammError::CurveDomain);
    }
    let (volatile_held, stable_held) = held_at(sup, x_wad)?;
    let stable = mul_div(mul_div(kappa, stable_held, WAD)?, anchor_sqrt_x96, Q96)?;
    let volatile_amount = mul_div(mul_div(kappa, volatile_held, WAD)?, Q96, anchor_sqrt_x96)?;
    Ok((stable, volatile_amount))
}

/// `AlmCurve.swapExactIn` (`AlmCurve.sol:191-217`): an exact-input fill in normalized units.
/// Volatile-in (`x` rises) is closed form; stable-in inverts `y -> x` with the seeded bisection.
/// `input_unused` is non-zero only where the funded band truncates the fill.
pub fn swap_exact_in(
    sup: &Support,
    x_wad: U256,
    volatile_in: bool,
    amount_in_wad: U256,
) -> Result<NormalizedFill, FlammError> {
    // A no-input fill MUST be a no-op (:205): the stable-in bisection lands on the leftmost
    // coordinate of a floored-y tread, so without this guard a zero input could move the
    // coordinate.
    if amount_in_wad.is_zero() {
        return Ok(NormalizedFill {
            amount_out: U256::ZERO,
            x_after: x_wad,
            input_unused: U256::ZERO,
        });
    }
    let y = y_at_x(x_wad, sup.a_wad)?;
    if !volatile_in {
        return stable_in(sup, x_wad, y, amount_in_wad);
    }
    let room = sup.x_hi.saturating_sub(x_wad);
    let used = if amount_in_wad > room { room } else { amount_in_wad };
    let input_unused = amount_in_wad - used;
    let x_after = x_wad + used;
    let y_after = y_at_x(x_after, sup.a_wad)?;
    Ok(NormalizedFill { amount_out: y.saturating_sub(y_after), x_after, input_unused })
}

/// `AlmCurve._stableIn` (`AlmCurve.sol:225-252`): paying the stable leg raises `y` and lowers `x`;
/// `y` is non-increasing in `x`, so the root sits where the predicate flips true -> false
/// (`AlmCurve.sol:280`). The returned `x_after` is `hi`, the right endpoint of the final bracket,
/// at which the floored `y` no longer exceeds `y + used`. Nothing is claimed about `lo`: when the
/// fill saturates the leg, `y(lo)` equals the target rather than exceeding it, and `x_after` is
/// then one step to the right of the leftmost non-exceeding coordinate.
fn stable_in(
    sup: &Support,
    x_wad: U256,
    y: U256,
    amount_in_wad: U256,
) -> Result<NormalizedFill, FlammError> {
    let y_max = y_at_x(sup.x_lo, sup.a_wad)?;
    let reachable = y_max.saturating_sub(y);
    let used = if amount_in_wad > reachable { reachable } else { amount_in_wad };
    let input_unused = amount_in_wad - used;
    if used.is_zero() {
        return Ok(NormalizedFill {
            amount_out: U256::ZERO,
            x_after: x_wad,
            input_unused: amount_in_wad,
        });
    }
    let mut lo = sup.x_lo;
    let mut hi = x_wad;
    let y_target = y + used;
    seed_bracket(sup, &mut lo, &mut hi, y_target)?;
    for _ in 0..128 {
        let mid = (lo + hi) >> 1;
        if mid == lo {
            break;
        }
        if y_at_x(mid, sup.a_wad)? > y_target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let x_after = hi;
    Ok(NormalizedFill { amount_out: x_wad.saturating_sub(x_after), x_after, input_unused })
}

/// `AlmCurve._seedBracket` (`AlmCurve.sol:269-284`): narrow `[lo, hi]` around `yAtX(yTarget)`,
/// valid because the level set is symmetric under swapping the coordinates, and adopt the window
/// ONLY if it brackets the root under the bisection's own predicate. It never changes the
/// bisection's answer.
fn seed_bracket(
    sup: &Support,
    lo: &mut U256,
    hi: &mut U256,
    y_target: U256,
) -> Result<(), FlammError> {
    if y_target < MIN_X_WAD || y_target > MAX_X_WAD {
        return Ok(());
    }
    let seed = y_at_x(y_target, sup.a_wad)?;
    let window = U256::from(SEED_WINDOW);
    // `lo` is inside the domain (yAtX(lo) answered above) and `seed` is below 2^70, so neither sum
    // can overflow the checked `+` (:277-278).
    let lo_seed = if seed > *lo + window { seed - window } else { *lo };
    let hi_seed = if seed + window < *hi { seed + window } else { *hi };
    if lo_seed >= hi_seed {
        return Ok(());
    }
    if y_at_x(lo_seed, sup.a_wad)? <= y_target {
        return Ok(());
    }
    if y_at_x(hi_seed, sup.a_wad)? > y_target {
        return Ok(());
    }
    *lo = lo_seed;
    *hi = hi_seed;
    Ok(())
}

/// `AlmCurve._toNormalized` (`AlmCurve.sol:418-425`): token amount to normalized inventory,
/// FLOORING; the inverse of [`to_token`]'s two `mulDiv`s, applied in the same order.
pub fn to_normalized(
    amount: U256,
    anchor_sqrt_x96: U256,
    kappa: U256,
    stable: bool,
) -> Result<U256, FlammError> {
    let t = mul_div(amount, WAD, kappa)?;
    if stable {
        mul_div(t, Q96, anchor_sqrt_x96)
    } else {
        mul_div(t, anchor_sqrt_x96, Q96)
    }
}

/// `AlmCurve._toToken` (`AlmCurve.sol:429-432`): normalized inventory to a token amount, FLOORING;
/// mirrors [`reserves_at`], one anchor factor per leg.
pub fn to_token(
    norm: U256,
    anchor_sqrt_x96: U256,
    kappa: U256,
    stable: bool,
) -> Result<U256, FlammError> {
    let t = mul_div(kappa, norm, WAD)?;
    if stable {
        mul_div(t, anchor_sqrt_x96, Q96)
    } else {
        mul_div(t, Q96, anchor_sqrt_x96)
    }
}

/// `AlmCurve._toTokenCeil` (`AlmCurve.sol:436-445`): [`to_token`] rounding UP at both steps, used
/// only to price a truncated fill's charge.
pub fn to_token_ceil(
    norm: U256,
    anchor_sqrt_x96: U256,
    kappa: U256,
    stable: bool,
) -> Result<U256, FlammError> {
    let t = mul_div_up(kappa, norm, WAD)?;
    if stable {
        mul_div_up(t, anchor_sqrt_x96, Q96)
    } else {
        mul_div_up(t, Q96, anchor_sqrt_x96)
    }
}

/// `AlmCurve.swapExactInX96` (`AlmCurve.sol:379-414`): the exact-input fill in TOKEN units,
/// `amount_out` gross of any fee. An input below normalized resolution (about `kappa / WAD` in the
/// leg's units) is reported entirely unspent; an untruncated fill charges `amount_in` exactly; a
/// fill truncated by the band charges the used movement rounded UP, capped at `amount_in`.
pub fn swap_exact_in_x96(
    sup: &Support,
    anchor_sqrt_x96: U256,
    kappa: U256,
    x_wad: U256,
    stable_in: bool,
    amount_in: U256,
) -> Result<TokenFill, FlammError> {
    // A retracted book (kappa == 0) must fail as a named revert, not a bare panic (:390).
    if anchor_sqrt_x96.is_zero() || kappa.is_zero() {
        return Err(FlammError::CurveDomain);
    }
    let in_norm = to_normalized(amount_in, anchor_sqrt_x96, kappa, stable_in)?;
    if in_norm.is_zero() {
        return Ok(TokenFill {
            amount_out: U256::ZERO,
            x_after: x_wad,
            amount_in_unspent: amount_in,
        });
    }
    // The curve's flag is VOLATILE-in; this one is STABLE-in: opposites, as are the output legs
    // (:408).
    let fill = swap_exact_in(sup, x_wad, !stable_in, in_norm)?;
    let amount_out = to_token(fill.amount_out, anchor_sqrt_x96, kappa, !stable_in)?;
    if fill.input_unused.is_zero() {
        return Ok(TokenFill { amount_out, x_after: fill.x_after, amount_in_unspent: U256::ZERO });
    }
    let used_tok = to_token_ceil(in_norm - fill.input_unused, anchor_sqrt_x96, kappa, stable_in)?;
    Ok(TokenFill {
        amount_out,
        x_after: fill.x_after,
        amount_in_unspent: amount_in.saturating_sub(used_tok),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(MAX_A_WAD, U256::from(1000u64) * WAD);
        assert_eq!(MIN_A_WAD, WAD / U256::from(2) + U256::from(1));
        assert_eq!(MIN_X_WAD, U256::from(10u64).pow(U256::from(15)));
        assert_eq!(MAX_X_WAD, U256::from(1999u64) * U256::from(10u64).pow(U256::from(15)));
    }

    /// Zero input never moves `x`, and a stable-in fill never lands right of the starting
    /// coordinate (`almcurve_test.go` `TestAlmSwapZeroInputIsNoOp`).
    #[test]
    fn swap_zero_input_is_no_op() {
        let a = U256::from(34u64) * WAD;
        let six = U256::from(6u64) * WAD;
        let sup = support_for(a, six, six).unwrap();
        let x = U256::from(532_533_306_204_662_064u64);
        for stable_in in [true, false] {
            let f = swap_exact_in(&sup, x, stable_in, U256::ZERO).unwrap();
            assert!(f.amount_out.is_zero() && f.input_unused.is_zero());
            assert_eq!(f.x_after, x);
        }
        let f = swap_exact_in(&sup, x, false, U256::from(1)).unwrap();
        assert!(f.x_after <= x);
    }

    #[test]
    fn price_is_wad_at_the_anchor() {
        for a in [MIN_A_WAD, WAD, U256::from(34u64) * WAD, MAX_A_WAD] {
            assert_eq!(price_at_x(super::super::math::HALF_WAD, a).unwrap(), WAD, "{a}");
        }
    }

    #[test]
    fn c_is_negative_for_every_valid_a() {
        for a in [MIN_A_WAD, WAD, MAX_A_WAD] {
            let (neg_c, four_k) = neg_c(a).unwrap();
            assert!(!neg_c.is_zero());
            assert_eq!(neg_c + four_k, WAD);
            assert!(c_wad_raw(a).unwrap().bit(255));
        }
        assert_eq!(neg_c(MIN_A_WAD - U256::from(1)), Err(FlammError::CurveAmplification));
        assert_eq!(neg_c(MAX_A_WAD + U256::from(1)), Err(FlammError::CurveAmplification));
    }
}
