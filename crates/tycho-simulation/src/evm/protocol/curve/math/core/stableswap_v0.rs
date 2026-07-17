//! StableSwapV0 — Oldest pools (sUSD, Compound, USDT, y, BUSD).
//!
//! a_precision=1, no -1 offset, fee after denormalize, static fee.
//! Vyper: https://github.com/curvefi/curve-contract/blob/master/contracts/pools/susd/StableSwapSUSD.vy

use alloy_primitives::U256;

pub const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
pub const A_PRECISION: U256 = U256::from_limbs([1, 0, 0, 0]);
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
        for balance in xp {
            d_p = d_p.checked_mul(d)?.checked_div(
                balance
                    .checked_mul(n)?
                    .checked_add(U256::from(1))?,
            )?;
        }
        let d_prev = d;
        let num = ann
            .checked_mul(sum)?
            .checked_div(A_PRECISION)?
            .checked_add(d_p.checked_mul(n)?)?
            .checked_mul(d)?;
        let den = (ann
            .checked_div(A_PRECISION)?
            .checked_sub(U256::from(1))?)
        .checked_mul(d)?
        .checked_add(
            n.checked_add(U256::from(1))?
                .checked_mul(d_p)?,
        )?;
        if den.is_zero() {
            return None;
        }
        d = num.checked_div(den)?;
        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff <= U256::from(1) {
            return Some(d);
        }
    }
    None
}

pub fn get_y(i: usize, j: usize, x_new: U256, xp: &[U256], d: U256, amp: U256) -> Option<U256> {
    let n = U256::from(xp.len());
    let ann = amp.checked_mul(n)?;
    let mut s_prime = U256::ZERO;
    let mut c = d;
    #[allow(clippy::needless_range_loop)]
    for k in 0..xp.len() {
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
        let num = y.checked_mul(y)?.checked_add(c)?;
        let den = y
            .checked_mul(U256::from(2))?
            .checked_add(b)?
            .checked_sub(d)?;
        if den.is_zero() {
            return None;
        }
        y = num.checked_div(den)?;
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
        assert_eq!(get_d(&[U256::ZERO, U256::ZERO], U256::from(100u64)), Some(U256::ZERO));
    }

    #[test]
    fn get_d_balanced_pool() {
        let balance = U256::from(1_000_000u64) * wad();
        let d = get_d(&[balance, balance], U256::from(100u64)).expect("converge");
        let expected = balance * U256::from(2u64);
        let diff = if d > expected { d - expected } else { expected - d };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_d_four_coins() {
        let balance = U256::from(1_000_000u64) * wad();
        let d = get_d(&[balance, balance, balance, balance], U256::from(100u64)).expect("converge");
        let expected = balance * U256::from(4u64);
        let diff = if d > expected { d - expected } else { expected - d };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_roundtrip() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(100u64);
        let d = get_d(&xp, amp).expect("d");
        let y = get_y(0, 1, xp[0], &xp, d, amp).expect("y");
        let diff = if y > xp[1] { y - xp[1] } else { xp[1] - y };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_swap_reduces_output() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(100u64);
        let d = get_d(&xp, amp).expect("d");
        let dx = U256::from(1000u64) * wad();
        let y = get_y(0, 1, xp[0] + dx, &xp, d, amp).expect("y");
        let dy = xp[1] - y;
        assert!(dy < dx);
        assert!(dy > U256::ZERO);
    }

    #[test]
    fn get_y_symmetry() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(100u64);
        let d = get_d(&xp, amp).expect("d");
        let dx = U256::from(1000u64) * wad();
        let y01 = get_y(0, 1, xp[0] + dx, &xp, d, amp).expect("y01");
        let y10 = get_y(1, 0, xp[1] + dx, &xp, d, amp).expect("y10");
        assert_eq!(y01, y10);
    }

    #[test]
    fn get_d_imbalanced_pool() {
        let b0 = U256::from(2_000_000u64) * wad();
        let b1 = U256::from(500_000u64) * wad();
        let d = get_d(&[b0, b1], U256::from(100u64)).expect("converge");
        assert!(d > U256::ZERO);
        assert!(d <= b0 + b1);
    }
}
