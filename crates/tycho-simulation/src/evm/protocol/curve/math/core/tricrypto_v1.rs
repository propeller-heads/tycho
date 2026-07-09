//! TriCryptoV1 — Legacy 3-coin CryptoSwap (tricrypto2: USDT/WBTC/WETH).
//!
//! Vyper: https://github.com/curvefi/curve-crypto-contract/blob/master/contracts/tricrypto/CurveCryptoMath3.vy (newton_y)
//!      + https://github.com/curvefi/curve-crypto-contract/blob/master/contracts/tricrypto/CurveCryptoSwap3.vy
//!        (_fee)

use alloy_primitives::U256;

pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
pub const A_MULTIPLIER: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const MAX_ITERATIONS: usize = 255;

pub fn newton_y_3(ann: U256, gamma: U256, x: [U256; 3], d: U256, j: usize) -> Option<U256> {
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
    // others is sorted descending [large, small], iterate reversed (small first)
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

    // Realistic tricrypto2 params (USDT/WBTC/WETH, normalized)
    fn realistic_params() -> (U256, U256, [U256; 3], U256) {
        let wad = WAD;
        let ann = U256::from(1707629u64) * A_MULTIPLIER;
        let gamma = U256::from(11_809_167_828_997u64);
        // Balanced in internal space after price_scale normalization
        let balance = U256::from(10_000u64) * wad;
        let x = [balance, balance, balance];
        let d = U256::from(30_000u64) * wad;
        (ann, gamma, x, d)
    }

    #[test]
    fn newton_y_3_convergence() {
        let (ann, gamma, x, d) = realistic_params();
        let y = newton_y_3(ann, gamma, x, d, 2).expect("converge");
        assert!(y > U256::ZERO);
        assert!(y < d);
    }

    #[test]
    fn newton_y_3_swap_reduces() {
        let wad = WAD;
        let (ann, gamma, x, d) = realistic_params();
        let dx = U256::from(10u64) * wad;
        let y_before = newton_y_3(ann, gamma, x, d, 2).expect("before");
        let y_after = newton_y_3(ann, gamma, [x[0] + dx, x[1], x[2]], d, 2).expect("after");
        assert!(y_after < y_before);
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
}
