//! StableSwapSTETH — Lido stETH/ETH custom StableSwap (0xDC24316b9AE028F1497c275EB9192a3Ea0f67022).
//!
//! a_precision=100, -1 offset, fee before denormalize, static fee — same shape as StableSwapV2.
//! Vyper: https://github.com/curvefi/curve-contract/blob/master/contracts/pools/steth/StableSwapSTETH.vy
//!
//! The one integer-math difference from the generic base/plain template (`stableswap_v2`) is in
//! `get_d`: the steth pool computes each `D_P` step with a `+ 1` added to the divisor
//! (`D_P = D_P * D / (x * N_COINS + 1)`, the on-chain "+1 is to prevent /0" guard). The base/plain
//! template omits that `+1`. The extra unit changes the truncation of `D_P` and therefore the
//! converged `D` in some balance regimes, so the steth pool needs its own `get_d`. `get_y` is
//! identical to the base/plain template.

use alloy_primitives::U256;

pub const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
pub const A_PRECISION: U256 = U256::from_limbs([100, 0, 0, 0]);
const MAX_ITERATIONS: usize = 255;

pub fn get_d(xp: &[U256], amp: U256) -> Option<U256> {
    let n = U256::from(xp.len());

    let sum: U256 = xp
        .iter()
        .try_fold(U256::ZERO, |acc, b| acc.checked_add(*b))?;
    if sum.is_zero() {
        return Some(U256::ZERO);
    }

    let ann = amp.checked_mul(n)?;
    let mut d = sum;

    for _ in 0..MAX_ITERATIONS {
        let mut d_p = d;
        // steth get_D: D_P = D_P * D / (_x * N_COINS + 1). The trailing `+1` is the on-chain
        // "+1 is to prevent /0" guard and is the sole math divergence from the base/plain template.
        for balance in xp {
            d_p = d_p.checked_mul(d)?.checked_div(
                balance
                    .checked_mul(n)?
                    .checked_add(U256::from(1))?,
            )?;
        }

        let d_prev = d;

        let numerator = ann
            .checked_mul(sum)?
            .checked_div(A_PRECISION)?
            .checked_add(d_p.checked_mul(n)?)?
            .checked_mul(d)?;

        let denominator = ann
            .checked_sub(A_PRECISION)?
            .checked_mul(d)?
            .checked_div(A_PRECISION)?
            .checked_add(
                n.checked_add(U256::from(1))?
                    .checked_mul(d_p)?,
            )?;

        if denominator.is_zero() {
            return None;
        }

        d = numerator.checked_div(denominator)?;

        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff <= U256::from(1) {
            return Some(d);
        }
    }

    None
}

pub fn get_y(i: usize, j: usize, x_new: U256, xp: &[U256], d: U256, amp: U256) -> Option<U256> {
    let n_coins = xp.len();
    let n = U256::from(n_coins);
    let ann = amp.checked_mul(n)?;

    let mut s_prime = U256::ZERO;
    let mut c = d;

    #[allow(clippy::needless_range_loop)]
    for k in 0..n_coins {
        let x_k = if k == i {
            x_new
        } else if k != j {
            xp[k]
        } else {
            continue;
        };
        s_prime = s_prime.checked_add(x_k)?;
        c = c
            .checked_mul(d)?
            .checked_div(x_k.checked_mul(n)?)?;
    }
    c = c
        .checked_mul(d)?
        .checked_mul(A_PRECISION)?
        .checked_div(ann.checked_mul(n)?)?;

    let b = s_prime.checked_add(
        d.checked_mul(A_PRECISION)?
            .checked_div(ann)?,
    )?;

    let mut y = d;

    for _ in 0..MAX_ITERATIONS {
        let y_prev = y;

        let numerator = y.checked_mul(y)?.checked_add(c)?;
        let denominator = y
            .checked_mul(U256::from(2))?
            .checked_add(b)?
            .checked_sub(d)?;

        if denominator.is_zero() {
            return None;
        }

        y = numerator.checked_div(denominator)?;

        let diff = if y > y_prev { y - y_prev } else { y_prev - y };
        if diff <= U256::from(1) {
            return Some(y);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wad() -> U256 {
        PRECISION
    }

    #[test]
    fn get_d_zero_returns_zero() {
        let amp = U256::from(100u64) * A_PRECISION;
        assert_eq!(get_d(&[U256::ZERO, U256::ZERO], amp), Some(U256::ZERO));
    }

    #[test]
    fn get_d_balanced_pool() {
        let balance = U256::from(1_000_000u64) * wad();
        let amp = U256::from(100u64) * A_PRECISION;
        let d = get_d(&[balance, balance], amp).expect("converge");
        let expected = balance * U256::from(2u64);
        let diff = if d > expected { d - expected } else { expected - d };
        // The `+1` divisor guard perturbs D by at most a few wei for large balances.
        assert!(diff <= U256::from(4));
    }

    #[test]
    fn get_y_roundtrip() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(100u64) * A_PRECISION;
        let d = get_d(&xp, amp).expect("d");
        let y = get_y(0, 1, xp[0], &xp, d, amp).expect("y");
        let diff = if y > xp[1] { y - xp[1] } else { xp[1] - y };
        assert!(diff <= U256::from(4));
    }

    /// The `+1` divisor guard is the only divergence from the base/plain template. For the
    /// degenerate single-wei balances regime where `x * N_COINS` is tiny, the `+1` shifts the
    /// truncation; confirm `get_d` stays finite and non-panicking there.
    #[test]
    fn get_d_tiny_balances_converges() {
        let amp = U256::from(90_000u64);
        let d = get_d(&[U256::from(1u64), U256::from(1u64)], amp);
        assert!(d.is_some());
    }
}
