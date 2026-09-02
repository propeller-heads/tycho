//! TwoCryptoV1 — Legacy 2-coin CryptoSwap (CurveCryptoSwap2 / CurveCryptoSwap2ETH).
//!
//! Two deployed Vyper flavours of the same legacy 2-coin CryptoSwap solver differ only in the
//! `mul2` grouping inside the Newton `get_y` solver:
//!   ETH variant  (CurveCryptoSwap2ETH.vy): `mul2 = 10**18 + (2 * 10**18) * K0 / _g1k0`
//!     — division binds to the second term only, then `10**18` is added.
//!   non-ETH      (CurveCryptoSwap2.vy):     `mul2 = unsafe_div(10**18 + (2 * 10**18) * K0, _g1k0)`
//!     — division applies to the whole `(10**18 + 2*10**18*K0)` numerator.
//! Both share the identical `mul1`, `_g1k0`, and iteration body; only this single integer-division
//! grouping differs, which can shift `mul2` (and therefore the converged `y`) by a few wei and so
//! diverge `get_dy` on some pools. `eth_variant = true` selects the ETH grouping.
//!
//! Vyper ETH:     https://github.com/curvefi/curve-crypto-contract/blob/master/contracts/two/CurveCryptoSwap2ETH.vy
//! Vyper non-ETH: https://github.com/curvefi/curve-crypto-contract/blob/master/contracts/two/CurveCryptoSwap2.vy

use alloy_primitives::U256;

pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
pub const A_MULTIPLIER: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const MAX_ITERATIONS: usize = 255;

/// Newton solver for the legacy 2-coin CryptoSwap `get_y`.
///
/// `eth_variant` selects the deployed Vyper flavour's `mul2` integer-division grouping:
/// `true` for the ETH pool (`10**18 + (2 * 10**18) * K0 / _g1k0`), `false` for the non-ETH pool
/// (`(10**18 + 2*10**18*K0) / _g1k0`). All other terms are identical between flavours.
pub fn newton_y_2(
    ann: U256,
    gamma: U256,
    x: [U256; 2],
    d: U256,
    j: usize,
    eth_variant: bool,
) -> Option<U256> {
    let n = U256::from(2u64);
    let x_j = x[1 - j];
    let mut y = d * d / (x_j * U256::from(4u64));
    let k0_i = WAD * n * x_j / d;
    let convergence_limit = {
        let a = x_j / U256::from(10u128.pow(14));
        let b = d / U256::from(10u128.pow(14));
        a.max(b).max(U256::from(100u64))
    };
    let __g1k0 = gamma + WAD;
    for _ in 0..MAX_ITERATIONS {
        let y_prev = y;
        let k0 = k0_i * y * n / d;
        let s = x_j + y;
        let _g1k0 =
            if __g1k0 > k0 { __g1k0 - k0 + U256::from(1) } else { k0 - __g1k0 + U256::from(1) };
        let mul1 = WAD * d / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ann;
        // The two deployed flavours differ only here. ETH divides the second term alone then adds
        // 10**18; non-ETH divides the whole (10**18 + 2*10**18*K0) numerator by _g1k0.
        let mul2 = if eth_variant {
            WAD + U256::from(2u64) * WAD * k0 / _g1k0
        } else {
            (WAD + U256::from(2u64) * WAD * k0) / _g1k0
        };
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

/// Vyper `geometric_mean(x, False)` from the legacy 2-coin CryptoSwap: Newton iteration on
/// `D = (D + x[0] * x[1] / D) / 2` starting from `x[0]` (caller passes descending-sorted x).
fn geometric_mean_2(x: [U256; 2]) -> Option<U256> {
    let mut d = x[0];
    for _ in 0..MAX_ITERATIONS {
        let d_prev = d;
        d = d
            .checked_add(x[0].checked_mul(x[1])?.checked_div(d)?)?
            .checked_div(U256::from(2))?;
        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff <= U256::from(1) || diff.checked_mul(WAD)? < d {
            return Some(d);
        }
    }
    None
}

/// Invariant solver ported from the deployed legacy 2-coin CryptoSwap
/// (`newton_D` in the CRV/ETH pool `0x8301AE4fc9c624d1D396cbDAa1ed877821D7C511`,
/// `CurveCryptoSwap2ETH.vy` flavour). The ETH/non-ETH flavour split affects only `newton_y`'s
/// `mul2` grouping; `newton_D` is shared, and every in-scope pool uses the ETH flavour anyway
/// (see `adapter::detect_eth_variant`).
///
/// The legacy source uses checked Vyper arithmetic throughout, so every operation maps to
/// `checked_*` returning `None` where the contract would revert. Unlike the NG solver, the
/// initial guess is `N_COINS * geometric_mean(x)` via Newton square root, not `isqrt` — the
/// two can stop on different iterates, so they are ported separately.
pub fn newton_d_2_v1(ann: U256, gamma: U256, x_unsorted: [U256; 2]) -> Option<U256> {
    let p = |exp: u32| -> U256 { U256::from(10u64).pow(U256::from(exp)) };
    let n = U256::from(2u64);
    // assert ANN > MIN_A - 1 and ANN < MAX_A + 1; assert gamma in [MIN_GAMMA, MAX_GAMMA]
    let (min_a, max_a) = (U256::from(4_000u64), U256::from(4_000_000_000u64));
    let (min_gamma, max_gamma) = (p(10), U256::from(2u64) * p(16));
    if ann < min_a || ann > max_a || gamma < min_gamma || gamma > max_gamma {
        return None;
    }
    let x = if x_unsorted[0] < x_unsorted[1] { [x_unsorted[1], x_unsorted[0]] } else { x_unsorted };
    // assert x[0] > 10**9 - 1 and x[0] < 10**15 * 10**18 + 1  # dev: unsafe values x[0]
    if x[0] < p(9) || x[0] > p(15) * WAD {
        return None;
    }
    // assert x[1] * 10**18 / x[0] > 10**14 - 1  # dev: unsafe values x[i] (input)
    if x[1].checked_mul(WAD)? / x[0] < p(14) {
        return None;
    }
    let mut d = n * geometric_mean_2(x)?;
    let s = x[0] + x[1];
    for _ in 0..MAX_ITERATIONS {
        let d_prev = d;
        // K0 = (10**18 * N_COINS**2) * x[0] / D * x[1] / D
        let k0 = p(18)
            .checked_mul(U256::from(4u64))?
            .checked_mul(x[0])?
            .checked_div(d)?
            .checked_mul(x[1])?
            .checked_div(d)?;
        let _g1k0 = {
            let g = gamma + WAD;
            if g > k0 {
                g - k0 + U256::from(1)
            } else {
                k0 - g + U256::from(1)
            }
        };
        // mul1 = 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN
        let mul1 = WAD
            .checked_mul(d)?
            .checked_div(gamma)?
            .checked_mul(_g1k0)?
            .checked_div(gamma)?
            .checked_mul(_g1k0)?
            .checked_mul(A_MULTIPLIER)?
            .checked_div(ann)?;
        // mul2 = (2 * 10**18) * N_COINS * K0 / _g1k0
        let mul2 = p(18)
            .checked_mul(U256::from(4u64))?
            .checked_mul(k0)?
            .checked_div(_g1k0)?;
        // neg_fprime = (S + S * mul2 / 10**18) + mul1 * N_COINS / K0 - mul2 * D / 10**18
        let neg_fprime = s
            .checked_add(s.checked_mul(mul2)?.checked_div(WAD)?)?
            .checked_add(mul1.checked_mul(n)?.checked_div(k0)?)?
            .checked_sub(mul2.checked_mul(d)?.checked_div(WAD)?)?;
        // D_plus = D * (neg_fprime + S) / neg_fprime; D_minus = D * D / neg_fprime
        let d_plus = d
            .checked_mul(neg_fprime.checked_add(s)?)?
            .checked_div(neg_fprime)?;
        let mut d_minus = d
            .checked_mul(d)?
            .checked_div(neg_fprime)?;
        let adj = d
            .checked_mul(mul1.checked_div(neg_fprime)?)?
            .checked_div(WAD)?
            .checked_mul(if WAD > k0 { WAD - k0 } else { k0 - WAD })?
            .checked_div(k0)?;
        if WAD > k0 {
            d_minus = d_minus.checked_add(adj)?;
        } else {
            d_minus = d_minus.checked_sub(adj)?;
        }
        d = if d_plus > d_minus { d_plus - d_minus } else { (d_minus - d_plus) / n };
        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff.checked_mul(p(14))? < d.max(p(16)) {
            // Final domain guard mirrors `assert frac > 10**16 - 1 and frac < 10**20 + 1`.
            for x_i in x {
                let frac = x_i.checked_mul(WAD)?.checked_div(d)?;
                if frac < p(16) || frac >= p(20) + U256::from(1) {
                    return None;
                }
            }
            return Some(d);
        }
    }
    None
}

pub fn crypto_fee(xp: &[U256], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> Option<U256> {
    let s: U256 = xp
        .iter()
        .try_fold(U256::ZERO, |acc, v| acc.checked_add(*v))?;
    if s.is_zero() {
        return None;
    }
    // Vyper V1 _fee computes K inline: (WAD * N^N) * xp[0] / S * xp[1] / S
    // NOT per-element k *= N * x_i / S (which differs in integer truncation)
    let n = U256::from(xp.len());
    let mut nn = U256::from(1u64);
    for _ in 0..xp.len() {
        nn *= n;
    }
    let mut k = WAD * nn;
    for x_i in xp {
        k = k * (*x_i) / s;
    }
    let f = if fee_gamma > U256::ZERO { fee_gamma * WAD / (fee_gamma + WAD - k) } else { k };
    Some((mid_fee * f + out_fee * (WAD - f)) / WAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Realistic CRV/ETH pool parameters (both 18-dec, normalized to internal space)
    fn realistic_params() -> (U256, U256, [U256; 2], U256) {
        let wad = WAD;
        // A=540000, gamma=2.8e13 (typical twocrypto V1)
        let ann = U256::from(540_000u64) * A_MULTIPLIER;
        let gamma = U256::from(28_000_000_000_000u64);
        // ~5000 ETH and ~20M CRV normalized
        let x0 = U256::from(5000u64) * wad;
        let x1 = U256::from(5000u64) * wad; // price_scale-normalized
        let d = U256::from(10000u64) * wad;
        (ann, gamma, [x0, x1], d)
    }

    /// The legacy `newton_D` is `@internal`, so there is no `eth_call` oracle for it; it is
    /// validated end-to-end against on-chain `get_dy` on ever-ramped pools (see the ignored
    /// differential tests in `curve::vm`). These unit tests pin the solver's basic behaviour.
    #[test]
    fn newton_d_2_v1_converges_on_balanced_pool() {
        let wad = WAD;
        let ann = U256::from(400_000u64);
        let gamma = U256::from(145_000_000_000_000u64);
        let bal = U256::from(5000u64) * wad;
        let d = newton_d_2_v1(ann, gamma, [bal, bal]).expect("converge");
        let expected = U256::from(10_000u64) * wad;
        let diff = if d > expected { d - expected } else { expected - d };
        // Balanced pool: D converges to the sum of balances within the solver tolerance.
        assert!(diff * U256::from(10u64).pow(U256::from(14u64)) < expected, "D={d}");
    }

    #[test]
    fn newton_d_2_v1_rejects_out_of_domain_inputs() {
        let wad = WAD;
        let x = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let gamma = U256::from(145_000_000_000_000u64);
        let ann = U256::from(400_000u64);
        // ANN and gamma outside the deployed contract's asserted bounds (legacy MAX_A is 4e9).
        assert!(newton_d_2_v1(U256::from(3_999u64), gamma, x).is_none());
        assert!(newton_d_2_v1(U256::from(4_000_000_001u64), gamma, x).is_none());
        assert!(newton_d_2_v1(ann, U256::from(10u64).pow(U256::from(9u64)), x).is_none());
        // x[0] below 10**9 and balance ratio below 10**14 / 10**18.
        assert!(newton_d_2_v1(ann, gamma, [U256::from(10u64), U256::from(10u64)]).is_none());
        assert!(
            newton_d_2_v1(ann, gamma, [x[0], U256::from(10u64).pow(U256::from(9u64))]).is_none()
        );
    }

    #[test]
    fn newton_y_2_convergence() {
        let (ann, gamma, x, d) = realistic_params();
        let y = newton_y_2(ann, gamma, x, d, 1, true).expect("converge");
        assert!(y > U256::ZERO);
        assert!(y < d);
    }

    #[test]
    fn newton_y_2_with_swap() {
        let wad = WAD;
        let (ann, gamma, x, d) = realistic_params();
        let dx = U256::from(10u64) * wad;
        let y_before = newton_y_2(ann, gamma, x, d, 1, true).expect("before");
        let y_after = newton_y_2(ann, gamma, [x[0] + dx, x[1]], d, 1, true).expect("after");
        assert!(y_after < y_before);
    }

    /// The two flavours agree when `2*10**18*K0` is an exact multiple of `_g1k0` (no truncation),
    /// and can diverge by a wei or two otherwise. Confirm the non-ETH grouping still converges to a
    /// sane `y` and that the selector actually changes the math in at least one regime.
    #[test]
    fn newton_y_2_eth_vs_non_eth() {
        let (ann, gamma, x, d) = realistic_params();
        let y_eth = newton_y_2(ann, gamma, x, d, 1, true).expect("eth converge");
        let y_non = newton_y_2(ann, gamma, x, d, 1, false).expect("non-eth converge");
        assert!(y_non > U256::ZERO && y_non < d);
        // Both are valid roots; they agree to within the per-iteration truncation tolerance.
        let diff = if y_eth > y_non { y_eth - y_non } else { y_non - y_eth };
        assert!(diff < U256::from(1_000u64), "flavours diverge implausibly: {diff}");
    }

    #[test]
    fn crypto_fee_balanced() {
        let wad = WAD;
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let xp = [U256::from(100_000u64) * wad, U256::from(100_000u64) * wad];
        let fee = crypto_fee(&xp, mid_fee, out_fee, fee_gamma).expect("fee");
        // Balanced pool: k ≈ 1, f ≈ 1, fee ≈ mid_fee
        assert!(fee >= mid_fee);
        assert!(fee < out_fee);
    }

    #[test]
    fn crypto_fee_imbalanced() {
        let wad = WAD;
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let balanced = [U256::from(100_000u64) * wad, U256::from(100_000u64) * wad];
        let imbalanced = [U256::from(200_000u64) * wad, U256::from(50_000u64) * wad];
        let fee_b = crypto_fee(&balanced, mid_fee, out_fee, fee_gamma).expect("balanced");
        let fee_i = crypto_fee(&imbalanced, mid_fee, out_fee, fee_gamma).expect("imbalanced");
        // Imbalanced should have higher fee
        assert!(fee_i > fee_b);
    }
}
