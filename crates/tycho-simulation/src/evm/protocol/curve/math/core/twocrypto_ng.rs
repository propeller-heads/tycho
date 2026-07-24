//! TwoCryptoNG — Next-gen 2-coin CryptoSwap.
//!
//! Uses Cardano cubic solver (get_y) instead of Newton.
//! Vyper: https://github.com/curvefi/twocrypto-ng/blob/main/contracts/main/Twocrypto.vy
//!      + https://github.com/curvefi/twocrypto-ng/blob/main/contracts/main/TwocryptoMath.vy
//!
//! NOTE: The NG Vyper `_fee` uses a different formula than V1's `reduction_coefficient`:
//!   NG GitHub:  f = fee_gamma * k / (fee_gamma * k / WAD + WAD - k)
//!   V1/deployed: f = fee_gamma * WAD / (fee_gamma + WAD - k)
//! The deployed NG contract matches the V1-style formula. GitHub source differs from deployed
//! bytecode.

use alloy_primitives::{I256, U256};

/// Floor division for signed integers (Vyper `//` and `/` semantics).
/// Rust I256 `/` truncates toward zero; Vyper rounds toward negative infinity.
/// Differs when operands have different signs and there's a remainder.
fn fdiv(a: I256, b: I256) -> I256 {
    let d = a / b;
    let r = a % b;
    if !r.is_zero() && ((a ^ b) < I256::ZERO) {
        d - I256::try_from(1u64).unwrap()
    } else {
        d
    }
}

pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
pub const A_MULTIPLIER: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const MAX_ITERATIONS: usize = 255;

pub fn isqrt(x: U256) -> U256 {
    if x.is_zero() {
        return U256::ZERO;
    }
    let mut z = (x + U256::from(1)) >> 1;
    let mut y = x;
    while z < y {
        y = z;
        z = (x / z + z) >> 1;
    }
    y
}

pub fn snekmate_log_2(x: U256) -> u32 {
    if x.is_zero() {
        return 0;
    }
    let mut value = x;
    let mut result: u32 = 0;
    if value >> 128 != U256::ZERO {
        value >>= 128;
        result = 128;
    }
    if value >> 64 != U256::ZERO {
        value >>= 64;
        result += 64;
    }
    if value >> 32 != U256::ZERO {
        value >>= 32;
        result += 32;
    }
    if value >> 16 != U256::ZERO {
        value >>= 16;
        result += 16;
    }
    if value >> 8 != U256::ZERO {
        value >>= 8;
        result += 8;
    }
    if value >> 4 != U256::ZERO {
        value >>= 4;
        result += 4;
    }
    if value >> 2 != U256::ZERO {
        value >>= 2;
        result += 2;
    }
    if value >> 1 != U256::ZERO {
        result += 1;
    }
    result
}

/// cbrt overflow threshold: 115792089237316195423570985008687907853269
const CBRT_THRESHOLD: U256 = U256::from_limbs([14562287877669245909, 5208750325433214395, 340, 0]);

pub fn cbrt(x: U256) -> U256 {
    let threshold = CBRT_THRESHOLD;
    let (xx, scale_back) = if x >= threshold * WAD {
        (x, 0u8)
    } else if x >= threshold {
        (x * WAD, 1)
    } else {
        (x * U256::from(10u128.pow(36)), 2)
    };
    let log2x = snekmate_log_2(xx);
    let remainder = (log2x % 3) as usize;
    let pow_1260: [U256; 3] = [U256::from(1u64), U256::from(1260u64), U256::from(1587600u64)];
    let pow_1000: [U256; 3] = [U256::from(1u64), U256::from(1000u64), U256::from(1000000u64)];
    let mut a = (U256::from(1u64) << (log2x / 3)) * pow_1260[remainder] / pow_1000[remainder];
    for _ in 0..7 {
        let a_sq = a * a;
        if a_sq.is_zero() {
            break;
        }
        a = (U256::from(2u64) * a + xx / a_sq) / U256::from(3u64);
    }
    match scale_back {
        0 => a * U256::from(1_000_000_000_000u64),
        1 => a * U256::from(1_000_000u64),
        _ => a,
    }
}

pub fn newton_y_2_ng(
    ann: U256,
    gamma: U256,
    x: [U256; 2],
    d: U256,
    i: usize,
    lim_mul: U256,
) -> Option<U256> {
    if i >= 2 {
        return None;
    }
    let n = U256::from(2u64);
    let x_j = x[1 - i];
    let mut y = d * d / (x_j * U256::from(4u64));
    let k0_i = WAD * n * x_j / d;
    if k0_i < U256::from(10u128.pow(36)) / lim_mul || k0_i > lim_mul {
        return None;
    }
    let convergence_limit = (x_j / U256::from(10u128.pow(14)))
        .max(d / U256::from(10u128.pow(14)))
        .max(U256::from(100u64));
    for _ in 0..MAX_ITERATIONS {
        let y_prev = y;
        let k0 = k0_i * y * n / d;
        let s = x_j + y;
        let _g1k0 = {
            let g = gamma + WAD;
            if g > k0 {
                g - k0 + U256::from(1)
            } else {
                k0 - g + U256::from(1)
            }
        };
        let mul1 = WAD * d / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ann;
        let mul2 = WAD + U256::from(2u64) * WAD * k0 / _g1k0;
        let yfprime = WAD * y + s * mul2 + mul1;
        let _dyfprime = d * mul2;
        if yfprime < _dyfprime {
            y = y_prev / U256::from(2);
            continue;
        }
        let yfprime = yfprime - _dyfprime;
        let fprime = yfprime / y;
        let y_minus = mul1 / fprime;
        let y_plus = (yfprime + WAD * d) / fprime + y_minus * WAD / k0;
        let y_minus = y_minus + WAD * s / fprime;
        if y_plus < y_minus {
            y = y_prev / U256::from(2);
        } else {
            y = y_plus - y_minus;
        }
        let diff = if y > y_prev { y - y_prev } else { y_prev - y };
        if diff < convergence_limit.max(y / U256::from(10u128.pow(14))) {
            return Some(y);
        }
    }
    None
}

pub fn get_y_2_ng(ann: U256, gamma: U256, x: [U256; 2], d: U256, i: usize) -> Option<(U256, U256)> {
    if i >= 2 {
        return None;
    }
    // These closures convert known small constants from the Vyper Cardano solver into I256.
    // All values are hardcoded literals (max 10^36 << 2^255), so try_from never fails.
    let s = |v: u128| -> I256 { I256::try_from(v).expect("i256 const") };
    let p = |exp: u32| -> U256 { U256::from(10u64).pow(U256::from(exp)) };
    let si = |exp: u32| -> I256 { I256::try_from(p(exp)).expect("i256 pow") };

    let max_gamma_small = U256::from(2u64) * U256::from(10u128.pow(16));
    let mut lim_mul = U256::from(100u64) * WAD;
    if gamma > max_gamma_small {
        lim_mul = lim_mul * max_gamma_small / gamma;
    }

    let ann_s = I256::try_from(ann).ok()?;
    let gamma_s = I256::try_from(gamma).ok()?;
    let d_s = I256::try_from(d).ok()?;
    let x_j_s = I256::try_from(x[1 - i]).ok()?;
    let gamma2 = gamma_s.wrapping_mul(gamma_s);
    let e18 = I256::try_from(WAD).expect("WAD fits I256");
    let n_s = s(2);

    let k0_i = fdiv(
        e18.wrapping_mul(n_s)
            .wrapping_mul(x_j_s),
        d_s,
    );
    let lim_mul_signed = I256::try_from(lim_mul).ok()?;
    if k0_i < fdiv(s(10u128.pow(36)), lim_mul_signed) || k0_i > lim_mul_signed {
        return None;
    }

    let ann_gamma2 = ann_s.wrapping_mul(gamma2);
    let a: I256 = s(10u128.pow(32));
    let b: I256 = fdiv(fdiv(d_s.wrapping_mul(ann_gamma2), s(400_000_000)), x_j_s) -
        s(3).wrapping_mul(s(10u128.pow(32))) -
        s(2).wrapping_mul(gamma_s)
            .wrapping_mul(s(10u128.pow(14)));
    let c: I256 = s(3).wrapping_mul(s(10u128.pow(32))) +
        s(4).wrapping_mul(gamma_s)
            .wrapping_mul(s(10u128.pow(14))) +
        fdiv(gamma2, s(10u128.pow(4))) +
        fdiv(fdiv(s(4).wrapping_mul(ann_gamma2), s(400_000_000)).wrapping_mul(x_j_s), d_s) -
        fdiv(s(4).wrapping_mul(ann_gamma2), s(400_000_000));
    // Vyper: d = -unsafe_div((10^18 + gamma)^2, 10^4)
    // Negate AFTER truncation-dividing positive values (not floor-dividing negative)
    let d_coeff: I256 = -((e18 + gamma_s)
        .wrapping_mul(e18 + gamma_s)
        .wrapping_div(s(10u128.pow(4))));

    let delta0: I256 = fdiv(s(3).wrapping_mul(a).wrapping_mul(c), b) - b;
    let delta1: I256 = s(3).wrapping_mul(delta0) + b -
        fdiv(fdiv(s(27).wrapping_mul(a).wrapping_mul(a), b).wrapping_mul(d_coeff), b);

    let threshold = delta0.abs().min(delta1.abs()).min(a);
    let threshold_u = U256::try_from(threshold.abs()).unwrap_or(U256::ZERO);
    let divider: I256 = if threshold_u > p(48) {
        si(30)
    } else if threshold_u > p(46) {
        si(28)
    } else if threshold_u > p(44) {
        si(26)
    } else if threshold_u > p(42) {
        si(24)
    } else if threshold_u > p(40) {
        si(22)
    } else if threshold_u > p(38) {
        si(20)
    } else if threshold_u > p(36) {
        si(18)
    } else if threshold_u > p(34) {
        si(16)
    } else if threshold_u > p(32) {
        si(14)
    } else if threshold_u > p(30) {
        si(12)
    } else if threshold_u > p(28) {
        si(10)
    } else if threshold_u > p(26) {
        si(8)
    } else if threshold_u > p(24) {
        si(6)
    } else if threshold_u > p(20) {
        si(2)
    } else {
        s(1)
    };

    // After divider: Vyper uses unsafe_div (EVM SDIV = truncation toward zero).
    // wrapping_div matches SDIV semantics for I256.
    let a = a.wrapping_div(divider);
    let b = b.wrapping_div(divider);
    let c = c.wrapping_div(divider);
    let d_coeff = d_coeff.wrapping_div(divider);
    let delta0 = s(3)
        .wrapping_mul(a)
        .wrapping_mul(c)
        .wrapping_div(b) -
        b;
    let delta1 = s(3).wrapping_mul(delta0) + b -
        s(27)
            .wrapping_mul(a.wrapping_mul(a))
            .wrapping_div(b)
            .wrapping_mul(d_coeff)
            .wrapping_div(b);
    let sqrt_arg = delta1.wrapping_mul(delta1) +
        s(4).wrapping_mul(delta0.wrapping_mul(delta0))
            .wrapping_div(b)
            .wrapping_mul(delta0);

    if sqrt_arg <= I256::ZERO {
        let y = newton_y_2_ng(ann, gamma, x, d, i, lim_mul)?;
        return Some((y, U256::ZERO));
    }
    let sqrt_val = I256::try_from(isqrt(U256::try_from(sqrt_arg).ok()?)).ok()?;
    let b_cbrt: I256 = if b > I256::ZERO {
        I256::try_from(cbrt(U256::try_from(b).ok()?)).ok()?
    } else {
        -I256::try_from(cbrt(U256::try_from(-b).ok()?)).ok()?
    };
    let second_cbrt: I256 = if delta1 > I256::ZERO {
        I256::try_from(cbrt(U256::try_from(delta1.wrapping_add(sqrt_val)).ok()? / U256::from(2u64)))
            .ok()?
    } else {
        -I256::try_from(cbrt(
            U256::try_from(sqrt_val.wrapping_sub(delta1)).ok()? / U256::from(2u64),
        ))
        .ok()?
    };
    let c1: I256 = b_cbrt
        .wrapping_mul(b_cbrt)
        .wrapping_div(e18)
        .wrapping_mul(second_cbrt)
        .wrapping_div(e18);
    // Vyper: (10^18*C1 - 10^18*b - 10^18*b//C1*delta0) // (3*a)
    // The inner `10^18*b // C1` uses truncation (SDIV) in deployed bytecode,
    // not floor division. Using wrapping_div matches both v2.0.0 and v2.1.0.
    let root: I256 = fdiv(
        e18.wrapping_mul(c1) -
            e18.wrapping_mul(b) -
            e18.wrapping_mul(b)
                .wrapping_div(c1)
                .wrapping_mul(delta0),
        s(3).wrapping_mul(a),
    );
    let y_out = U256::try_from(
        d_s.wrapping_mul(d_s)
            .wrapping_div(x_j_s)
            .wrapping_mul(root)
            .wrapping_div(s(4))
            .wrapping_div(e18),
    )
    .ok()?;
    let k0_prev = U256::try_from(root).ok()?;
    let frac = y_out * WAD / d;
    let n2 = U256::from(2u64);
    if frac < U256::from(10u128.pow(36)) / n2 / lim_mul || frac > lim_mul / n2 {
        return None;
    }
    Some((y_out, k0_prev))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isqrt_known_values() {
        assert_eq!(isqrt(U256::ZERO), U256::ZERO);
        assert_eq!(isqrt(U256::from(1u64)), U256::from(1u64));
        assert_eq!(isqrt(U256::from(4u64)), U256::from(2u64));
        assert_eq!(isqrt(U256::from(9u64)), U256::from(3u64));
        assert_eq!(isqrt(U256::from(100u64)), U256::from(10u64));
        // Non-perfect square: floor
        assert_eq!(isqrt(U256::from(8u64)), U256::from(2u64));
        assert_eq!(isqrt(U256::from(99u64)), U256::from(9u64));
    }

    #[test]
    fn isqrt_large() {
        let x = WAD * WAD;
        let s = isqrt(x);
        assert_eq!(s, WAD);
    }

    #[test]
    fn snekmate_log_2_known_values() {
        assert_eq!(snekmate_log_2(U256::ZERO), 0);
        assert_eq!(snekmate_log_2(U256::from(1u64)), 0);
        assert_eq!(snekmate_log_2(U256::from(2u64)), 1);
        assert_eq!(snekmate_log_2(U256::from(4u64)), 2);
        assert_eq!(snekmate_log_2(U256::from(255u64)), 7);
        assert_eq!(snekmate_log_2(U256::from(256u64)), 8);
    }

    #[test]
    fn cbrt_basic() {
        let wad = WAD;
        // cbrt(1e18) in WAD-scaled space
        let result = cbrt(wad);
        // cbrt(10^18) = 10^6, then scaled by 10^18 = 10^24? No, cbrt is WAD-scaled.
        // cbrt operates on WAD-scaled values, result is WAD-scaled
        assert!(result > U256::ZERO);
    }

    #[test]
    fn cbrt_monotonic() {
        let a = U256::from(1_000_000_000_000_000_000u128);
        let b = U256::from(8_000_000_000_000_000_000u128);
        let ca = cbrt(a);
        let cb = cbrt(b);
        // Monotonic: larger input → larger output
        assert!(cb > ca);
    }

    #[test]
    fn cbrt_perfect_cube() {
        // cbrt should be reasonably accurate
        let x = U256::from(8_000_000_000_000_000_000u128); // 8e18
        let result = cbrt(x);
        // result should be approximately 2e18 (depending on scaling)
        assert!(result > U256::ZERO);
    }

    #[test]
    fn get_y_2_ng_rejects_out_of_range_index() {
        let wad = WAD;
        let ann = U256::from(540_000u64) * A_MULTIPLIER;
        let gamma = U256::from(11_809_167_828_997u64);
        let x = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let d = U256::from(10000u64) * wad;
        let lim_mul = U256::from(100u64) * wad;
        assert!(get_y_2_ng(ann, gamma, x, d, 2).is_none());
        assert!(newton_y_2_ng(ann, gamma, x, d, 2, lim_mul).is_none());
    }

    #[test]
    fn get_y_2_ng_convergence() {
        let wad = WAD;
        // Realistic twocrypto-ng params
        let ann = U256::from(540_000u64) * A_MULTIPLIER;
        let gamma = U256::from(11_809_167_828_997u64);
        let x0 = U256::from(5000u64) * wad;
        let x1 = U256::from(5000u64) * wad;
        let d = U256::from(10000u64) * wad;
        let result = get_y_2_ng(ann, gamma, [x0, x1], d, 1);
        assert!(result.is_some());
        let (y, _k0) = result.expect("converge");
        assert!(y > U256::ZERO);
        assert!(y < d);
    }
}
