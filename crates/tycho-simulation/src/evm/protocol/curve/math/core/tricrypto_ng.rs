//! TriCryptoNG — Next-gen 3-coin CryptoSwap (USDC/WBTC/WETH).
//!
//! Uses hybrid cubic+Newton solver (get_y) instead of pure Newton.
//! Vyper: https://github.com/curvefi/tricrypto-ng/blob/main/contracts/main/CurveTricryptoOptimized.vy
//!      + https://github.com/curvefi/tricrypto-ng/blob/main/contracts/main/CurveCryptoMathOptimized3.vy

use alloy_primitives::{I256, U256};

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

/// Domain guard ported from the Vyper math contract's `get_y` safety asserts
/// (`dev: unsafe values D` / `Unsafe values x[i]`):
/// <https://github.com/curvefi/tricrypto-ng/blob/ecaa8161c240f21dd7c3712eefc5637e1dac742b/contracts/main/CurveCryptoMathOptimized3.vy#L48-L57>
/// (`_newton_y` re-asserts the balance bounds at L250).
///
/// The unchecked arithmetic in the solvers below is only sound inside this domain; the
/// deployed contract reverts outside it, so quoting returns `None`. Without this guard,
/// out-of-domain balances (e.g. an absurdly large `dx`) wrap the I256 cubic coefficients
/// and drive `newton_y_3` into a division by zero.
///
/// The A and gamma asserts from the same block are intentionally not ported: pool
/// parameters come from the deployed contract, which already enforces them at deploy
/// time, while `D` and the balances are recomputed during simulation and can leave the
/// domain through caller-supplied amounts.
fn check_solver_domain(x: &[U256; 3], d: U256, i: usize) -> Option<()> {
    let p = |exp: u32| -> U256 { U256::from(10u64).pow(U256::from(exp)) };
    if d < p(17) || d > p(33) {
        return None;
    }
    for (k, x_k) in x.iter().enumerate() {
        if k == i {
            continue;
        }
        let frac = x_k.checked_mul(WAD)? / d;
        if frac < p(16) || frac > p(20) {
            return None;
        }
    }
    Some(())
}

/// EVM `unsafe_div` semantics: division by zero yields zero instead of reverting.
fn unsafe_div(a: U256, b: U256) -> U256 {
    if b.is_zero() {
        U256::ZERO
    } else {
        a / b
    }
}

/// Vyper `_sort`: three balances in descending order.
fn sort_3_desc(unsorted_x: [U256; 3]) -> [U256; 3] {
    let mut x = unsorted_x;
    x.sort_unstable_by(|a, b| b.cmp(a));
    x
}

/// Vyper `_geometric_mean` for three sorted balances (1e18-scaled), via `cbrt`.
/// Returns `None` where the deployed contract would revert on overflow.
fn geometric_mean_3(x: [U256; 3]) -> Option<U256> {
    let prod = x[0].checked_mul(x[1])? / WAD;
    let prod = prod.checked_mul(x[2])? / WAD;
    if prod.is_zero() {
        return Some(U256::ZERO);
    }
    Some(cbrt(prod))
}

/// Invariant solver ported from the deployed TriCrypto-NG MATH contract
/// (`CurveTricryptoMathOptimized.vy` v2.0.0, mainnet `0xcBFf3004a20dBfE2731543AA38599A526e0fD6eE`),
/// `newton_D` with `K0_prev = 0` (the only form the pool and views contracts use for
/// ramp-time D recomputation).
///
/// Wei-exact transcription: Vyper `unsafe_*` operations map to wrapping arithmetic (with
/// division-by-zero yielding zero), checked Vyper operations map to `checked_*` returning
/// `None` where the contract would revert, and the convergence bound is the contract's
/// `diff * 10**14 < max(10**16, D)`.
pub fn newton_d_3(ann: U256, gamma: U256, x_unsorted: [U256; 3]) -> Option<U256> {
    let p = |exp: u32| -> U256 { U256::from(10u64).pow(U256::from(exp)) };
    let n = U256::from(3u64);
    let x = sort_3_desc(x_unsorted);
    // assert x[0] < max_value(uint256) / 10**18 * N_COINS**N_COINS  # dev: out of limits
    if x[0] >= U256::MAX / WAD * U256::from(27u64) {
        return None;
    }
    // assert x[0] > 0  # dev: empty pool
    if x[0].is_zero() {
        return None;
    }
    let s = x[0]
        .wrapping_add(x[1])
        .wrapping_add(x[2]);
    let mut d = n.wrapping_mul(geometric_mean_3(x)?);
    for _ in 0..MAX_ITERATIONS {
        let d_prev = d;
        // K0 = 10**18 * x[0] * N / D * x[1] * N / D * x[2] * N / D (all unsafe math)
        let k0 = unsafe_div(
            unsafe_div(
                unsafe_div(WAD.wrapping_mul(x[0]).wrapping_mul(n), d)
                    .wrapping_mul(x[1])
                    .wrapping_mul(n),
                d,
            )
            .wrapping_mul(x[2])
            .wrapping_mul(n),
            d,
        );
        let _g1k0 = {
            let g = gamma.wrapping_add(WAD);
            if g > k0 {
                g.wrapping_sub(k0)
                    .wrapping_add(U256::from(1))
            } else {
                k0.wrapping_sub(g)
                    .wrapping_add(U256::from(1))
            }
        };
        // mul1 = 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN
        let mul1 = unsafe_div(
            unsafe_div(unsafe_div(WAD.wrapping_mul(d), gamma).wrapping_mul(_g1k0), gamma)
                .wrapping_mul(_g1k0)
                .wrapping_mul(A_MULTIPLIER),
            ann,
        );
        // mul2 = (2 * 10**18) * N_COINS * K0 / _g1k0
        let mul2 = unsafe_div(
            U256::from(2u64)
                .wrapping_mul(WAD)
                .wrapping_mul(n)
                .wrapping_mul(k0),
            _g1k0,
        );
        // neg_fprime = (S + S * mul2 / 10**18) + mul1 * N_COINS / K0 - mul2 * D / 10**18
        let neg_fprime = s
            .wrapping_add(unsafe_div(s.wrapping_mul(mul2), WAD))
            .wrapping_add(unsafe_div(mul1.wrapping_mul(n), k0))
            .wrapping_sub(unsafe_div(mul2.wrapping_mul(d), WAD));
        // D_plus = D * (neg_fprime + S) / neg_fprime  (outer mul is checked in Vyper)
        let d_plus = unsafe_div(d.checked_mul(neg_fprime.wrapping_add(s))?, neg_fprime);
        // D_minus = D * D / neg_fprime
        let mut d_minus = unsafe_div(d.checked_mul(d)?, neg_fprime);
        // The += / -= adjustments and the `D * ...` products are checked in Vyper.
        let adj = unsafe_div(
            d.checked_mul(unsafe_div(mul1, neg_fprime))?
                .wrapping_div(WAD)
                .wrapping_mul(if WAD > k0 { WAD - k0 } else { k0 - WAD }),
            k0,
        );
        if WAD > k0 {
            d_minus = d_minus.checked_add(adj)?;
        } else {
            d_minus = d_minus.checked_sub(adj)?;
        }
        d = if d_plus > d_minus {
            d_plus.wrapping_sub(d_minus)
        } else {
            d_minus.wrapping_sub(d_plus) / U256::from(2)
        };
        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff.wrapping_mul(p(14)) < d.max(p(16)) {
            // Final domain guard mirrors `assert frac >= 10**16 - 1 and frac < 10**20 + 1`.
            for x_i in x {
                let frac = unsafe_div(x_i.wrapping_mul(WAD), d);
                if frac < p(16) - U256::from(1) || frac >= p(20) + U256::from(1) {
                    return None;
                }
            }
            return Some(d);
        }
    }
    None
}

pub fn newton_y_3(ann: U256, gamma: U256, x: [U256; 3], d: U256, j: usize) -> Option<U256> {
    if j >= 3 {
        return None;
    }
    check_solver_domain(&x, d, j)?;
    let n = U256::from(3u64);
    let mut others: Vec<U256> = x
        .iter()
        .enumerate()
        .filter(|(k, _)| *k != j)
        .map(|(_, v)| *v)
        .collect();
    others.sort_unstable_by(|a, b| b.cmp(a));
    let (x_0, x_1) = (others[0], others[1]);
    // Vyper: y = D/N, then for each other coin (small first): y = y*D/(x*N)
    let mut y = d / n;
    for &other in others.iter().rev() {
        y = y * d / (other * n);
    }
    let k0_i = WAD * n * x_0 / d * n * x_1 / d;
    let s_i = x_0 + x_1;
    let convergence_limit = (others
        .iter()
        .max()
        .copied()
        .unwrap_or(U256::ZERO) /
        U256::from(10u128.pow(14)))
    .max(d / U256::from(10u128.pow(14)))
    .max(U256::from(100u64));
    let __g1k0 = gamma + WAD;
    for _ in 0..MAX_ITERATIONS {
        let y_prev = y;
        let k0 = k0_i * y * n / d;
        let s = s_i + y;
        let _g1k0 =
            if __g1k0 > k0 { __g1k0 - k0 + U256::from(1) } else { k0 - __g1k0 + U256::from(1) };
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
            let frac = y * WAD / d;
            if frac < U256::from(10u128.pow(16)) || frac > U256::from(10u128.pow(20)) {
                return None;
            }
            return Some(y);
        }
    }
    None
}

pub fn get_y_3_ng(ann: U256, gamma: U256, x: [U256; 3], d: U256, i: usize) -> Option<(U256, U256)> {
    check_solver_domain(&x, d, i)?;
    // These closures convert known small constants from the Vyper Cardano solver into I256.
    // All values are hardcoded literals (max 10^36 << 2^255), so try_from never fails.
    let s = |v: u128| -> I256 { I256::try_from(v).expect("i256 const") };
    let p = |exp: u32| -> U256 { U256::from(10u64).pow(U256::from(exp)) };
    let si = |exp: u32| -> I256 { I256::try_from(p(exp)).expect("i256 pow") };
    let (j_idx, k_idx) = match i {
        0 => (1usize, 2usize),
        1 => (0, 2),
        2 => (0, 1),
        _ => return None,
    };
    let ann_s = I256::try_from(ann).ok()?;
    let gamma_s = I256::try_from(gamma).ok()?;
    let d_s = I256::try_from(d).ok()?;
    let x_j = I256::try_from(x[j_idx]).ok()?;
    let x_k = I256::try_from(x[k_idx]).ok()?;
    let gamma2 = gamma_s.wrapping_mul(gamma_s);
    let e18 = I256::try_from(WAD).expect("WAD fits I256");
    let a_mul_s = I256::try_from(A_MULTIPLIER).expect("A_MULTIPLIER fits I256");
    let a: I256 = si(36) / s(27);
    let b: I256 = si(36) / s(9) +
        s(2).wrapping_mul(e18)
            .wrapping_mul(gamma_s) /
            s(27) -
        d_s.wrapping_mul(d_s) / x_j * gamma2 * ann_s / s(27 * 27) / a_mul_s / x_k;
    let c: I256 = si(36) / s(9) +
        gamma_s.wrapping_mul(gamma_s + s(4).wrapping_mul(e18)) / s(27) +
        gamma2 * (x_j + x_k - d_s) / d_s * ann_s / s(27) / a_mul_s;
    let d_coeff: I256 = (e18 + gamma_s).wrapping_mul(e18 + gamma_s) / s(27);
    let d0: I256 = (s(3).wrapping_mul(a).wrapping_mul(c) / b - b).abs();
    let d0_u = U256::try_from(d0).unwrap_or(U256::ZERO);
    let divider: I256 = if d0_u > p(48) {
        si(30)
    } else if d0_u > p(44) {
        si(26)
    } else if d0_u > p(40) {
        si(22)
    } else if d0_u > p(36) {
        si(18)
    } else if d0_u > p(32) {
        si(14)
    } else if d0_u > p(28) {
        si(10)
    } else if d0_u > p(24) {
        si(6)
    } else if d0_u > p(20) {
        si(2)
    } else {
        s(1)
    };
    let (a, b, c, d_coeff) = if a.abs() > b.abs() {
        let ap = (a / b).abs();
        (
            a.wrapping_mul(ap) / divider,
            (b * ap) / divider,
            (c * ap) / divider,
            (d_coeff * ap) / divider,
        )
    } else {
        let ap = (b / a).abs();
        (a / ap / divider, b / ap / divider, c / ap / divider, d_coeff / ap / divider)
    };
    let _3ac = s(3).wrapping_mul(a).wrapping_mul(c);
    let delta0 = _3ac / b - b;
    let delta1 = s(3).wrapping_mul(_3ac) / b -
        s(2).wrapping_mul(b) -
        s(27).wrapping_mul(a.wrapping_mul(a)) / b * d_coeff / b;
    let sqrt_arg =
        delta1.wrapping_mul(delta1) + s(4).wrapping_mul(delta0.wrapping_mul(delta0)) / b * delta0;
    if sqrt_arg <= I256::ZERO {
        let y = newton_y_3(ann, gamma, x, d, i)?;
        return Some((y, U256::ZERO));
    }
    let sqrt_val = I256::try_from(isqrt(U256::try_from(sqrt_arg).ok()?)).ok()?;
    let b_cbrt: I256 = if b >= I256::ZERO {
        I256::try_from(cbrt(U256::try_from(b).ok()?)).ok()?
    } else {
        -I256::try_from(cbrt(U256::try_from(-b).ok()?)).ok()?
    };
    let second_cbrt: I256 = if delta1 > I256::ZERO {
        I256::try_from(cbrt(U256::try_from(delta1 + sqrt_val).ok()? / U256::from(2u64))).ok()?
    } else {
        -I256::try_from(cbrt(U256::try_from(-(delta1 - sqrt_val)).ok()? / U256::from(2u64))).ok()?
    };
    let c1: I256 = b_cbrt
        .wrapping_mul(b_cbrt)
        .wrapping_div(e18)
        .wrapping_mul(second_cbrt)
        .wrapping_div(e18);
    let root_k0: I256 = (b + b * delta0 / c1 - c1) / s(3);
    let root: I256 = d_s.wrapping_mul(d_s) / s(27) / x_k * d_s / x_j * root_k0 / a;
    let y_out = U256::try_from(root).ok()?;
    let k0_prev = U256::try_from(e18.wrapping_mul(root_k0) / a).ok()?;
    let frac = y_out * WAD / d;
    if frac < p(16) - U256::from(1) || frac >= p(20) + U256::from(1) {
        return None;
    }
    Some((y_out, k0_prev))
}

pub fn crypto_fee(xp: &[U256], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> Option<U256> {
    let s: U256 = xp
        .iter()
        .try_fold(U256::ZERO, |acc, v| acc.checked_add(*v))?;
    if s.is_zero() {
        return None;
    }
    let n = U256::from(xp.len());
    let mut k = WAD;
    for x_i in xp {
        k = k * n * (*x_i) / s;
    }
    let f = if fee_gamma > U256::ZERO { fee_gamma * WAD / (fee_gamma + WAD - k) } else { k };
    Some((mid_fee * f + out_fee * (WAD - f)) / WAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isqrt_known_values() {
        assert_eq!(isqrt(U256::from(16u64)), U256::from(4u64));
        assert_eq!(isqrt(U256::from(25u64)), U256::from(5u64));
        assert_eq!(isqrt(U256::from(26u64)), U256::from(5u64));
    }

    #[test]
    fn cbrt_monotonic() {
        let a = U256::from(1_000_000_000_000_000_000u128);
        let b = U256::from(27_000_000_000_000_000_000u128);
        let ca = cbrt(a);
        let cb = cbrt(b);
        assert!(cb > ca);
    }

    fn realistic_params() -> (U256, U256, [U256; 3], U256) {
        let wad = WAD;
        let ann = U256::from(1707629u64) * A_MULTIPLIER;
        let gamma = U256::from(11_809_167_828_997u64);
        let balance = U256::from(10_000u64) * wad;
        let x = [balance, balance, balance];
        let d = U256::from(30_000u64) * wad;
        (ann, gamma, x, d)
    }

    /// Wei-exact against the deployed v2.0.0 MATH contract
    /// (`0xcBFf3004a20dBfE2731543AA38599A526e0fD6eE.newton_D(A, gamma, xp, 0)` via `eth_call`).
    /// First case is TricryptoUSDT (`0xf5f5b976…`) at block 25708548, mid A/gamma ramp — the
    /// exact state whose stored `D()` (11488973604915154442528465) had gone stale.
    #[test]
    fn newton_d_3_matches_deployed_math() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let d = newton_d_3(
            U256::from(357827u64),
            U256::from(130_900_344_682_889u64),
            [
                u("3990103228866000000000000"),
                u("3852702800372147876026991"),
                u("3651547413737580194425789"),
            ],
        );
        assert_eq!(d, Some(u("11488947540905734661415623")));
        let d = newton_d_3(
            U256::from(1707629u64),
            U256::from(11_809_167_828_997u64),
            [
                u("3000000000000000000000000"),
                u("2400000000000000000000000"),
                u("2600000000000000000000000"),
            ],
        );
        assert_eq!(d, Some(u("7966893883621358300290061")));
    }

    #[test]
    fn newton_d_3_rejects_out_of_domain_balances() {
        let (_, gamma, ..) = realistic_params();
        let ann = U256::from(1707629u64);
        assert!(newton_d_3(ann, gamma, [U256::ZERO, U256::ZERO, U256::ZERO]).is_none());
        // Wildly lopsided balances fail the final `Unsafe values x[i]` guard.
        let wad = WAD;
        let x = [U256::from(1u64) * wad, U256::from(10u64).pow(U256::from(40u64)), wad];
        assert!(newton_d_3(ann, gamma, x).is_none());
    }

    #[test]
    fn newton_y_3_convergence() {
        let (ann, gamma, x, d) = realistic_params();
        let y = newton_y_3(ann, gamma, x, d, 0).expect("converge");
        assert!(y > U256::ZERO);
        assert!(y < d);
    }

    #[test]
    fn get_y_3_ng_convergence() {
        let (ann, gamma, x, d) = realistic_params();
        let result = get_y_3_ng(ann, gamma, x, d, 2);
        assert!(result.is_some());
        let (y, _k0) = result.expect("converge");
        assert!(y > U256::ZERO);
        assert!(y < d);
    }

    #[test]
    fn get_y_3_ng_rejects_out_of_domain_balances() {
        // Deployed contract reverts with "Unsafe values x[i]" when a balance is far out of
        // proportion to D; the solver must reject instead of panicking on such inputs.
        let (ann, gamma, x, d) = realistic_params();
        let huge = U256::from(10u64).pow(U256::from(47u64));
        assert!(get_y_3_ng(ann, gamma, [huge, x[1], x[2]], d, 2).is_none());
    }

    #[test]
    fn newton_y_3_rejects_out_of_domain_balances() {
        let (ann, gamma, x, d) = realistic_params();
        let huge = U256::from(10u64).pow(U256::from(47u64));
        assert!(newton_y_3(ann, gamma, [huge, x[1], x[2]], d, 2).is_none());
    }

    #[test]
    fn solvers_reject_out_of_range_index() {
        let (ann, gamma, x, d) = realistic_params();
        assert!(get_y_3_ng(ann, gamma, x, d, 3).is_none());
        assert!(newton_y_3(ann, gamma, x, d, 3).is_none());
    }

    #[test]
    fn get_y_3_ng_rejects_out_of_domain_d() {
        let (ann, gamma, x, _) = realistic_params();
        assert!(get_y_3_ng(ann, gamma, x, U256::from(10u64).pow(U256::from(16u64)), 2).is_none());
        assert!(get_y_3_ng(ann, gamma, x, U256::from(10u64).pow(U256::from(34u64)), 2).is_none());
    }

    #[test]
    fn crypto_fee_three_coins_balanced() {
        let wad = WAD;
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let xp = [
            U256::from(100_000u64) * wad,
            U256::from(100_000u64) * wad,
            U256::from(100_000u64) * wad,
        ];
        let fee = crypto_fee(&xp, mid_fee, out_fee, fee_gamma).expect("fee");
        assert!(fee >= mid_fee);
        assert!(fee < out_fee);
    }

    #[test]
    fn get_y_3_ng_swap_reduces() {
        let wad = WAD;
        let (ann, gamma, x, d) = realistic_params();
        let dx = U256::from(10u64) * wad;
        let (y_before, _) = get_y_3_ng(ann, gamma, x, d, 2).expect("before");
        let (y_after, _) = get_y_3_ng(ann, gamma, [x[0] + dx, x[1], x[2]], d, 2).expect("after");
        assert!(y_after < y_before);
    }
}
