//! Native swap-to-price solver for StableSwap pool variants.
//!
//! Not part of the vendored `curve-math` tree. Finds the input amount that moves a StableSwap
//! pool's marginal (spot) price down to a target, using the pool's own invariant math instead of
//! the generic numerical search over `ProtocolSim::get_amount_out`.
//!
//! Each candidate evaluation costs one `get_y` (to quote the swap output with a pre-computed
//! invariant `D`) plus one `get_d` on the post-swap balances (to price the resulting state with
//! the exact analytic spot-price fraction used by `Pool::spot_price`). An Illinois-damped
//! false-position iteration on the input amount typically converges in a handful of evaluations.

use alloy_primitives::{U256, U512};

use crate::evm::protocol::{
    curve::math::{core, Pool},
    u256_num::u256_to_f64,
};

const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
const MAX_ITERATIONS: usize = 32;
/// Attempts to shrink the upper search bound when the pool math fails at the full input balance.
const MAX_BOUND_HALVINGS: usize = 4;

/// Error returned by [`Pool::swap_to_price`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapToPriceError {
    /// The pool variant has no native solver (CryptoSwap variants).
    UnsupportedVariant,
    /// The target price is strictly above the current spot price and cannot be reached by
    /// selling `token_in` into the pool.
    TargetAboveSpot,
    /// The target price is below the price reachable within the pool's input-side balance.
    TargetBelowLimit,
    /// The pool math failed to converge or overflowed.
    MathFailed,
}

impl std::fmt::Display for SwapToPriceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVariant => {
                f.write_str("no native swap-to-price solver for this pool variant")
            }
            Self::TargetAboveSpot => f.write_str("target price is above the current spot price"),
            Self::TargetBelowLimit => {
                f.write_str("target price is below the pool's reachable limit")
            }
            Self::MathFailed => f.write_str("pool math failed to converge"),
        }
    }
}

impl std::error::Error for SwapToPriceError {}

/// Per-variant math hooks and quoting semantics for the StableSwap family.
struct VariantMath {
    get_d: fn(&[U256], U256) -> Option<U256>,
    get_y: fn(usize, usize, U256, &[U256], U256, U256) -> Option<U256>,
    a_precision: U256,
    /// Whether the variant subtracts 1 wei from `xp[j] - y_new` (V1/V2/STETH/NG/Meta).
    minus_one_offset: bool,
    /// Whether the fee is charged in normalized (xp) space before denormalizing (V2-era and
    /// later) or on the denormalized output (V0/V1/ALend).
    fee_before_denorm: bool,
    /// Dynamic fee hook `(xp_i, xp_j, fee, offpeg_fee_multiplier) -> fee` for NG/ALend.
    dynamic_fee: Option<fn(U256, U256, U256, U256) -> U256>,
}

/// A StableSwap pool normalized to a common representation for the solver.
///
/// `rates` are 1e18-scaled so that `xp[k] = balances[k] * rates[k] / PRECISION` for every
/// variant; ALend's `precision_mul` is folded in as `precision_mul * PRECISION`.
struct StableCtx<'p> {
    balances: &'p [U256],
    rates: Vec<U256>,
    amp: U256,
    fee: U256,
    offpeg: U256,
    math: VariantMath,
}

impl Pool {
    /// Computes the amount of coin `i` to sell into the pool so that the post-swap marginal
    /// price of coin `i` -> coin `j` lands in `[target, target * (1 + tolerance)]`.
    ///
    /// The target is the fraction `target_num / target_den` in native token units (coin `j`
    /// received per coin `i` sold), matching the convention of [`Pool::spot_price`]. The
    /// returned amount is in coin `i`'s native units; feeding it to [`Pool::get_amount_out`]
    /// and applying the balance changes yields a pool whose `spot_price(i, j)` satisfies the
    /// target. If the tolerance band cannot be hit exactly (e.g. zero tolerance), the largest
    /// evaluated amount whose post-swap price stays at or above the target is returned.
    ///
    /// Only StableSwap variants are supported; CryptoSwap variants return
    /// [`SwapToPriceError::UnsupportedVariant`].
    pub fn swap_to_price(
        &self,
        i: usize,
        j: usize,
        target_num: U256,
        target_den: U256,
        tolerance: f64,
    ) -> Result<U256, SwapToPriceError> {
        let ctx = match self {
            Pool::StableSwapV0 { balances, rates, amp, fee } => StableCtx {
                balances,
                rates: rates.clone(),
                amp: *amp,
                fee: *fee,
                offpeg: U256::ZERO,
                math: VariantMath {
                    get_d: core::stableswap_v0::get_d,
                    get_y: core::stableswap_v0::get_y,
                    a_precision: core::stableswap_v0::A_PRECISION,
                    minus_one_offset: false,
                    fee_before_denorm: false,
                    dynamic_fee: None,
                },
            },
            Pool::StableSwapV1 { balances, rates, amp, fee } => StableCtx {
                balances,
                rates: rates.clone(),
                amp: *amp,
                fee: *fee,
                offpeg: U256::ZERO,
                math: VariantMath {
                    get_d: core::stableswap_v1::get_d,
                    get_y: core::stableswap_v1::get_y,
                    a_precision: core::stableswap_v1::A_PRECISION,
                    minus_one_offset: true,
                    fee_before_denorm: false,
                    dynamic_fee: None,
                },
            },
            Pool::StableSwapV2 { balances, rates, amp, fee } => StableCtx {
                balances,
                rates: rates.clone(),
                amp: *amp,
                fee: *fee,
                offpeg: U256::ZERO,
                math: VariantMath {
                    get_d: core::stableswap_v2::get_d,
                    get_y: core::stableswap_v2::get_y,
                    a_precision: core::stableswap_v2::A_PRECISION,
                    minus_one_offset: true,
                    fee_before_denorm: true,
                    dynamic_fee: None,
                },
            },
            Pool::StableSwapSTETH { balances, rates, amp, fee } => StableCtx {
                balances,
                rates: rates.clone(),
                amp: *amp,
                fee: *fee,
                offpeg: U256::ZERO,
                math: VariantMath {
                    get_d: core::stableswap_steth::get_d,
                    get_y: core::stableswap_steth::get_y,
                    a_precision: core::stableswap_steth::A_PRECISION,
                    minus_one_offset: true,
                    fee_before_denorm: true,
                    dynamic_fee: None,
                },
            },
            Pool::StableSwapALend { balances, precision_mul, amp, fee, offpeg_fee_multiplier } => {
                StableCtx {
                    balances,
                    rates: precision_mul
                        .iter()
                        .map(|p| *p * PRECISION)
                        .collect(),
                    amp: *amp,
                    fee: *fee,
                    offpeg: *offpeg_fee_multiplier,
                    math: VariantMath {
                        get_d: core::stableswap_alend::get_d,
                        get_y: core::stableswap_alend::get_y,
                        a_precision: core::stableswap_alend::A_PRECISION,
                        minus_one_offset: false,
                        fee_before_denorm: false,
                        dynamic_fee: Some(core::stableswap_alend::dynamic_fee),
                    },
                }
            }
            Pool::StableSwapNG { balances, rates, amp, fee, offpeg_fee_multiplier } => StableCtx {
                balances,
                rates: rates.clone(),
                amp: *amp,
                fee: *fee,
                offpeg: *offpeg_fee_multiplier,
                math: VariantMath {
                    get_d: core::stableswap_ng::get_d,
                    get_y: core::stableswap_ng::get_y,
                    a_precision: core::stableswap_ng::A_PRECISION,
                    minus_one_offset: true,
                    fee_before_denorm: true,
                    dynamic_fee: Some(core::stableswap_ng::dynamic_fee),
                },
            },
            Pool::StableSwapMeta { balances, rates, amp, fee } => StableCtx {
                balances,
                rates: rates.clone(),
                amp: *amp,
                fee: *fee,
                offpeg: U256::ZERO,
                math: VariantMath {
                    get_d: core::stableswap_meta::get_d,
                    get_y: core::stableswap_meta::get_y,
                    a_precision: core::stableswap_meta::A_PRECISION,
                    minus_one_offset: true,
                    fee_before_denorm: true,
                    dynamic_fee: None,
                },
            },
            Pool::TwoCryptoV1 { .. } |
            Pool::TwoCryptoNG { .. } |
            Pool::TwoCryptoStable { .. } |
            Pool::TriCryptoV1 { .. } |
            Pool::TriCryptoNG { .. } => return Err(SwapToPriceError::UnsupportedVariant),
        };
        ctx.solve(i, j, target_num, target_den, tolerance)
    }
}

impl StableCtx<'_> {
    fn solve(
        &self,
        i: usize,
        j: usize,
        target_num: U256,
        target_den: U256,
        tolerance: f64,
    ) -> Result<U256, SwapToPriceError> {
        let n = self.balances.len();
        if i >= n || j >= n || i == j || target_num.is_zero() || target_den.is_zero() {
            return Err(SwapToPriceError::MathFailed);
        }

        let xp = self
            .xp(self.balances)
            .ok_or(SwapToPriceError::MathFailed)?;
        let d = (self.math.get_d)(&xp, self.amp).ok_or(SwapToPriceError::MathFailed)?;
        let spot = self
            .price_fraction(&xp, self.balances, d, i, j)
            .ok_or(SwapToPriceError::MathFailed)?;

        match fraction_cmp(&spot, target_num, target_den) {
            std::cmp::Ordering::Less => return Err(SwapToPriceError::TargetAboveSpot),
            std::cmp::Ordering::Equal => return Ok(U256::ZERO),
            std::cmp::Ordering::Greater => {}
        }

        // Upper search bound: the pool's own input-side balance, the same soft limit that
        // `get_limits` reports. Shrink it if the pool math cannot quote that deep.
        let mut hi = self.balances[i];
        let mut hi_point = None;
        for _ in 0..MAX_BOUND_HALVINGS {
            if hi.is_zero() {
                break;
            }
            if let Some(point) = self.eval(&xp, d, i, j, hi) {
                hi_point = Some(point);
                break;
            }
            hi /= U256::from(2);
        }
        let (limit_price, _limit_dy) = hi_point.ok_or(SwapToPriceError::MathFailed)?;
        match fraction_cmp(&limit_price, target_num, target_den) {
            std::cmp::Ordering::Greater => return Err(SwapToPriceError::TargetBelowLimit),
            std::cmp::Ordering::Equal => return Ok(hi),
            std::cmp::Ordering::Less => {}
        }

        let target_f =
            fraction_to_f64(&(target_num, target_den)).ok_or(SwapToPriceError::MathFailed)?;
        // Accept only the lower half of the caller's tolerance band so the result stays safely
        // inside it after any f64 rounding on the caller's side.
        let accept_upper = target_f * (1.0 + 0.5 * tolerance);
        let aim = target_f * (1.0 + 0.25 * tolerance);

        let spot_f = fraction_to_f64(&spot).ok_or(SwapToPriceError::MathFailed)?;
        let limit_f = fraction_to_f64(&limit_price).ok_or(SwapToPriceError::MathFailed)?;

        let mut lo = U256::ZERO;
        // Illinois false position on g(dx) = price(dx) - aim, bracketed by
        // [lo: g > 0, hi: g < 0]. `side` tracks which endpoint moved last; moving the same
        // endpoint twice halves the retained residual to avoid one-sided stagnation.
        let mut g_lo = spot_f - aim;
        let mut g_hi = limit_f - aim;
        let mut best = U256::ZERO;
        let mut side = 0i8;

        for _ in 0..MAX_ITERATIONS {
            if hi.saturating_sub(lo) <= U256::from(1) {
                break;
            }
            let dx = next_dx(lo, hi, g_lo, g_hi);
            let Some((price, dy)) = self.eval(&xp, d, i, j, dx) else {
                // Quoting failed at this depth; treat it as beyond the reachable side.
                hi = dx;
                g_hi = f64::NAN;
                side = 0;
                continue;
            };
            let price_f = fraction_to_f64(&price).ok_or(SwapToPriceError::MathFailed)?;
            if fraction_cmp(&price, target_num, target_den) == std::cmp::Ordering::Less {
                if side == -1 {
                    g_lo *= 0.5;
                }
                hi = dx;
                g_hi = price_f - aim;
                side = -1;
            } else {
                if !dy.is_zero() {
                    if price_f <= accept_upper {
                        return Ok(dx);
                    }
                    best = dx;
                }
                if side == 1 {
                    g_hi *= 0.5;
                }
                lo = dx;
                g_lo = price_f - aim;
                side = 1;
            }
        }

        // Bracket exhausted without hitting the band (e.g. zero tolerance): return the largest
        // evaluated amount whose post-swap price stayed at or above the target.
        Ok(best)
    }

    /// Normalized balances `xp[k] = balances[k] * rates[k] / PRECISION`.
    fn xp(&self, balances: &[U256]) -> Option<Vec<U256>> {
        balances
            .iter()
            .zip(self.rates.iter())
            .map(|(b, r)| b.checked_mul(*r).map(|v| v / PRECISION))
            .collect()
    }

    /// Quotes the swap output for `dx` with a pre-computed invariant, mirroring the variant's
    /// `get_amount_out` semantics (offset, fee placement, dynamic fee) exactly.
    fn quote(&self, xp: &[U256], d: U256, i: usize, j: usize, dx: U256) -> Option<U256> {
        if dx.is_zero() {
            return Some(U256::ZERO);
        }
        let x_new = xp[i].checked_add(dx.checked_mul(self.rates[i])? / PRECISION)?;
        let y_new = (self.math.get_y)(i, j, x_new, xp, d, self.amp)?;
        if xp[j] <= y_new {
            return None;
        }
        let offset = if self.math.minus_one_offset { U256::from(1) } else { U256::ZERO };
        let gross = (xp[j] - y_new).checked_sub(offset)?;
        let fee_rate = match self.math.dynamic_fee {
            Some(dynamic_fee) => dynamic_fee(
                xp[i].checked_add(x_new)? / U256::from(2),
                xp[j].checked_add(y_new)? / U256::from(2),
                self.fee,
                self.offpeg,
            ),
            None => self.fee,
        };
        if self.math.fee_before_denorm {
            let fee_amount = fee_rate.checked_mul(gross)? / FEE_DENOMINATOR;
            Some(
                gross
                    .checked_sub(fee_amount)?
                    .checked_mul(PRECISION)? /
                    self.rates[j],
            )
        } else {
            let dy = gross.checked_mul(PRECISION)? / self.rates[j];
            let fee_amount = fee_rate.checked_mul(dy)? / FEE_DENOMINATOR;
            dy.checked_sub(fee_amount)
        }
    }

    /// Evaluates the exact post-swap spot price for input `dx`: quotes the output, applies the
    /// balance changes, and prices the resulting state the same way `Pool::spot_price` would
    /// (fresh `get_d` on the post-swap balances, fee retained by the pool).
    fn eval(
        &self,
        xp: &[U256],
        d: U256,
        i: usize,
        j: usize,
        dx: U256,
    ) -> Option<((U256, U256), U256)> {
        let dy = self.quote(xp, d, i, j, dx)?;
        let mut post_balances = self.balances.to_vec();
        post_balances[i] = post_balances[i].checked_add(dx)?;
        post_balances[j] = post_balances[j].checked_sub(dy)?;
        if post_balances[j].is_zero() {
            return None;
        }
        let post_xp = self.xp(&post_balances)?;
        let post_d = (self.math.get_d)(&post_xp, self.amp)?;
        let price = self.price_fraction(&post_xp, &post_balances, post_d, i, j)?;
        Some((price, dy))
    }

    /// Fee-inclusive marginal price dy/dx as an integer fraction, identical to the per-variant
    /// `spot_price` implementations in `swap/`.
    fn price_fraction(
        &self,
        xp: &[U256],
        balances: &[U256],
        d: U256,
        i: usize,
        j: usize,
    ) -> Option<(U256, U256)> {
        let n = U256::from(xp.len());
        let ann_eff = self.amp.checked_mul(n)? / self.math.a_precision;
        let mut d_p = d;
        for x_k in xp {
            d_p = d_p
                .checked_mul(d)?
                .checked_div(x_k.checked_mul(n)?)?;
        }
        let num_xp = ann_eff
            .checked_mul(xp[i])?
            .checked_add(d_p)?;
        let den_xp = ann_eff
            .checked_mul(xp[j])?
            .checked_add(d_p)?;
        if den_xp.is_zero() {
            return None;
        }
        let effective_fee = match self.math.dynamic_fee {
            Some(dynamic_fee) => dynamic_fee(xp[i], xp[j], self.fee, self.offpeg),
            None => self.fee,
        };
        let numerator = num_xp
            .checked_mul(balances[j])?
            .checked_mul(FEE_DENOMINATOR - effective_fee)?;
        let denominator = den_xp
            .checked_mul(balances[i])?
            .checked_mul(FEE_DENOMINATOR)?;
        Some((numerator, denominator))
    }
}

/// Compares the fraction `frac.0 / frac.1` against `num / den` without loss of precision.
fn fraction_cmp(frac: &(U256, U256), num: U256, den: U256) -> std::cmp::Ordering {
    let lhs = U512::from(frac.0) * U512::from(den);
    let rhs = U512::from(num) * U512::from(frac.1);
    lhs.cmp(&rhs)
}

fn fraction_to_f64(frac: &(U256, U256)) -> Option<f64> {
    let num = u256_to_f64(frac.0).ok()?;
    let den = u256_to_f64(frac.1).ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

/// Next candidate input via false position on the residuals, clamped strictly inside the
/// bracket; falls back to bisection when the residuals are unusable.
fn next_dx(lo: U256, hi: U256, g_lo: f64, g_hi: f64) -> U256 {
    let width = hi - lo;
    let mid = lo + width / U256::from(2);
    if !g_lo.is_finite() || !g_hi.is_finite() || g_lo <= 0.0 || g_hi >= 0.0 {
        return mid;
    }
    let ratio = g_lo / (g_lo - g_hi);
    if !ratio.is_finite() || ratio <= 0.0 || ratio >= 1.0 {
        return mid;
    }
    // Fixed-point scale of 2^32 keeps the interpolation in integer space.
    let scaled = (ratio * 4_294_967_296.0) as u64;
    let Some(offset) = width
        .checked_mul(U256::from(scaled))
        .map(|v| v >> 32)
    else {
        return mid;
    };
    let dx: U256 = lo + offset;
    let min_dx = lo + U256::from(1);
    let max_dx = hi - U256::from(1);
    dx.clamp(min_dx, max_dx)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const WAD: u128 = 1_000_000_000_000_000_000;
    const RATE_6_DEC: u128 = 1_000_000_000_000_000_000_000_000_000_000;

    fn v1_two_coin() -> Pool {
        Pool::StableSwapV1 {
            balances: vec![U256::from(50_000_000u128 * WAD), U256::from(48_000_000u128 * WAD)],
            rates: vec![U256::from(WAD), U256::from(WAD)],
            amp: U256::from(2000u64),
            fee: U256::from(1_000_000u64),
        }
    }

    fn v1_three_coin_mixed_decimals() -> Pool {
        // 3pool state at block 24669924 (DAI 18 dec, USDC 6 dec, USDT 6 dec).
        Pool::StableSwapV1 {
            balances: vec![
                U256::from(63_975_337_809_806_329_031_583_135u128),
                U256::from(61_219_263_170_093u128),
                U256::from(37_832_425_459_809u128),
            ],
            rates: vec![U256::from(WAD), U256::from(RATE_6_DEC), U256::from(RATE_6_DEC)],
            amp: U256::from(4000u64),
            fee: U256::from(1_500_000u64),
        }
    }

    fn ng_dynamic_fee() -> Pool {
        // Imbalanced so the offpeg multiplier actually raises the fee.
        Pool::StableSwapNG {
            balances: vec![U256::from(1_500_000u128 * WAD), U256::from(700_000u128 * WAD)],
            rates: vec![U256::from(WAD), U256::from(WAD)],
            amp: U256::from(40_000u64),
            fee: U256::from(4_000_000u64),
            offpeg_fee_multiplier: U256::from(20_000_000_000u64),
        }
    }

    fn meta_with_virtual_price() -> Pool {
        // rates[1] carries the base pool's virtual price (1.03).
        Pool::StableSwapMeta {
            balances: vec![U256::from(500_000u128 * WAD), U256::from(480_000u128 * WAD)],
            rates: vec![U256::from(WAD), U256::from(1_030_000_000_000_000_000u128)],
            amp: U256::from(50_000u64),
            fee: U256::from(4_000_000u64),
        }
    }

    fn alend_precision_mul() -> Pool {
        Pool::StableSwapALend {
            balances: vec![
                U256::from(20_000_000u128 * WAD),
                U256::from(18_000_000_000_000u128), // 6 decimals
            ],
            precision_mul: vec![U256::from(1u64), U256::from(1_000_000_000_000u128)],
            amp: U256::from(20_000u64),
            fee: U256::from(2_000_000u64),
            offpeg_fee_multiplier: U256::from(20_000_000_000u64),
        }
    }

    fn spot(pool: &Pool, i: usize, j: usize) -> (U256, U256) {
        pool.spot_price(i, j)
            .expect("spot price")
    }

    /// Applies the swap `dx` to a clone of the pool and returns the post-swap spot fraction.
    fn post_swap_spot(pool: &Pool, i: usize, j: usize, dx: U256) -> (U256, U256) {
        let dy = pool
            .get_amount_out(i, j, dx)
            .expect("get_amount_out");
        let mut post = pool.clone();
        let balances = post.balances().to_vec();
        post.set_balance(i, balances[i] + dx)
            .expect("set balance in");
        post.set_balance(j, balances[j] - dy)
            .expect("set balance out");
        spot(&post, i, j)
    }

    /// Scales a spot fraction by `multiplier` in integer space (parts per billion).
    fn scaled_target(spot: &(U256, U256), multiplier: f64) -> (U256, U256) {
        let ppb = U256::from((multiplier * 1e9) as u64);
        (spot.0 * ppb, spot.1 * U256::from(1_000_000_000u64))
    }

    fn assert_in_band(price: &(U256, U256), target: &(U256, U256), tolerance: f64) {
        let price_f = fraction_to_f64(price).expect("price f64");
        let target_f = fraction_to_f64(target).expect("target f64");
        assert!(price_f >= target_f, "post-swap price {price_f} fell below target {target_f}");
        assert!(
            price_f <= target_f * (1.0 + tolerance),
            "post-swap price {price_f} above tolerance band of target {target_f}"
        );
    }

    #[rstest]
    #[case::v1_two_coin_shallow(v1_two_coin(), 0, 1, 0.9999)]
    #[case::v1_two_coin_deep(v1_two_coin(), 0, 1, 0.999)]
    #[case::v1_two_coin_reverse(v1_two_coin(), 1, 0, 0.999)]
    #[case::v1_mixed_decimals_18_to_6(v1_three_coin_mixed_decimals(), 0, 1, 0.999)]
    #[case::v1_mixed_decimals_6_to_18(v1_three_coin_mixed_decimals(), 1, 0, 0.999)]
    #[case::v1_mixed_decimals_6_to_6(v1_three_coin_mixed_decimals(), 2, 1, 0.9995)]
    #[case::ng_dynamic_fee(ng_dynamic_fee(), 0, 1, 0.999)]
    #[case::ng_dynamic_fee_reverse(ng_dynamic_fee(), 1, 0, 0.999)]
    #[case::meta_virtual_price(meta_with_virtual_price(), 0, 1, 0.999)]
    #[case::meta_virtual_price_reverse(meta_with_virtual_price(), 1, 0, 0.999)]
    #[case::alend_precision_mul(alend_precision_mul(), 0, 1, 0.999)]
    #[case::alend_precision_mul_reverse(alend_precision_mul(), 1, 0, 0.999)]
    fn converges_to_target(
        #[case] pool: Pool,
        #[case] i: usize,
        #[case] j: usize,
        #[case] multiplier: f64,
    ) {
        let tolerance = 0.001;
        let current = spot(&pool, i, j);
        let target = scaled_target(&current, multiplier);

        let dx = pool
            .swap_to_price(i, j, target.0, target.1, tolerance)
            .expect("solver should converge");
        assert!(dx > U256::ZERO, "expected a non-zero swap amount");

        let post = post_swap_spot(&pool, i, j, dx);
        assert_in_band(&post, &target, tolerance);
    }

    #[rstest]
    #[case::v1(v1_two_coin())]
    #[case::ng(ng_dynamic_fee())]
    fn target_above_spot_is_rejected(#[case] pool: Pool) {
        let current = spot(&pool, 0, 1);
        let target = scaled_target(&current, 1.01);
        let result = pool.swap_to_price(0, 1, target.0, target.1, 0.001);
        assert_eq!(result, Err(SwapToPriceError::TargetAboveSpot));
    }

    #[test]
    fn target_below_limit_is_rejected() {
        let pool = v1_two_coin();
        let current = spot(&pool, 0, 1);
        // Far deeper than the input-side balance can move a high-amp stable pool.
        let target = (current.0, current.1 * U256::from(100u64));
        let result = pool.swap_to_price(0, 1, target.0, target.1, 0.001);
        assert_eq!(result, Err(SwapToPriceError::TargetBelowLimit));
    }

    #[test]
    fn target_equal_to_spot_returns_zero() {
        let pool = v1_two_coin();
        let current = spot(&pool, 0, 1);
        let dx = pool
            .swap_to_price(0, 1, current.0, current.1, 0.001)
            .expect("equal target");
        assert_eq!(dx, U256::ZERO);
    }

    #[test]
    fn crypto_variant_is_unsupported() {
        let wad = U256::from(WAD);
        let pool = Pool::TwoCryptoNG {
            balances: [U256::from(5000u64) * wad, U256::from(5000u64) * wad],
            precisions: [U256::from(1u64), U256::from(1u64)],
            price_scale: wad,
            d: U256::from(10000u64) * wad,
            ann: U256::from(540_000u64) * U256::from(10_000u64),
            gamma: U256::from(11_809_167_828_997u64),
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(230_000_000_000_000u64),
        };
        let result = pool.swap_to_price(0, 1, U256::from(1u64), U256::from(2u64), 0.001);
        assert_eq!(result, Err(SwapToPriceError::UnsupportedVariant));
    }

    #[test]
    fn zero_tolerance_returns_best_effort_at_or_above_target() {
        let pool = v1_two_coin();
        let current = spot(&pool, 0, 1);
        let target = scaled_target(&current, 0.999);
        let dx = pool
            .swap_to_price(0, 1, target.0, target.1, 0.0)
            .expect("solver should return best effort");
        assert!(dx > U256::ZERO);
        let post = post_swap_spot(&pool, 0, 1, dx);
        assert_ne!(
            fraction_cmp(&post, target.0, target.1),
            std::cmp::Ordering::Less,
            "best-effort result must not undershoot the target"
        );
        // Even best-effort should land very close for a smooth stable pool.
        assert_in_band(&post, &target, 1e-6);
    }
}
