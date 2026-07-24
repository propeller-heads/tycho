//! StableSwapV2 — Base/plain template (FRAX/USDC, stETH, factory plain pools).
//!
//! a_precision=100, -1 offset, fee before denormalize, static fee.
//! Vyper: https://github.com/curvefi/curve-contract/blob/master/contracts/pool-templates/base/SwapTemplateBase.vy

use alloy_primitives::U256;

pub const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
pub const A_PRECISION: U256 = U256::from_limbs([100, 0, 0, 0]);
const MAX_ITERATIONS: usize = 255;

pub fn get_d(xp: &[U256], amp: U256) -> Option<U256> {
    let n_coins = xp.len();
    let n = U256::from(n_coins);

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
        if n_coins == 2 {
            // Plain-2 factory Vyper uses unrolled D_P with different truncation:
            //   D_P = D * D / xp[0] * D / xp[1] / N_COINS²
            // Divides by each xp separately, then by N² at the end.
            for balance in xp {
                d_p = d_p
                    .checked_mul(d)?
                    .checked_div(*balance)?;
            }
            d_p = d_p.checked_div(n.checked_mul(n)?)?;
        } else {
            // Plain-3/4 factory Vyper uses per-element loop (same as V0/V1):
            //   D_P = D_P * D / (x * N_COINS)
            for balance in xp {
                d_p = d_p
                    .checked_mul(d)?
                    .checked_div(balance.checked_mul(n)?)?;
            }
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
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_roundtrip() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(100u64) * A_PRECISION;
        let d = get_d(&xp, amp).expect("d");
        let y = get_y(0, 1, xp[0], &xp, d, amp).expect("y");
        let diff = if y > xp[1] { y - xp[1] } else { xp[1] - y };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_d_monotonic_with_amp() {
        let b0 = U256::from(2_000_000u64) * wad();
        let b1 = U256::from(500_000u64) * wad();
        let d_low = get_d(&[b0, b1], U256::from(10u64) * A_PRECISION).expect("d_low");
        let d_high = get_d(&[b0, b1], U256::from(1000u64) * A_PRECISION).expect("d_high");
        assert!(d_high >= d_low);
    }

    #[test]
    fn get_y_larger_input_gives_more_output() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(200u64) * A_PRECISION;
        let d = get_d(&xp, amp).expect("d");
        let small_dx = U256::from(100u64) * wad();
        let large_dx = U256::from(10_000u64) * wad();
        let y_small = get_y(0, 1, xp[0] + small_dx, &xp, d, amp).expect("y_small");
        let y_large = get_y(0, 1, xp[0] + large_dx, &xp, d, amp).expect("y_large");
        assert!(y_small > y_large);
    }
}
