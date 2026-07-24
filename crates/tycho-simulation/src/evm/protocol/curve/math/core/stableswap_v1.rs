//! StableSwapV1 — 3pool era (3pool, ren, sbtc, hbtc).
//!
//! a_precision=1, -1 offset, fee after denormalize, static fee.
//! Vyper: https://github.com/curvefi/curve-contract/blob/master/contracts/pools/3pool/StableSwap3Pool.vy

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
            d_p = d_p
                .checked_mul(d)?
                .checked_div(balance.checked_mul(n)?)?;
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

/// Solve for xp[i] when D is reduced (used by `calc_withdraw_one_coin`).
///
/// Like [`get_y`] but without substituting any balance — uses all `xp[k]`
/// except `xp[i]`, and `d` is provided directly (not computed from `xp`).
///
/// Vyper: `get_y_D(A_, i, xp, D)` in StableSwap3Pool.vy
pub fn get_y_d(i: usize, xp: &[U256], d: U256, amp: U256) -> Option<U256> {
    let n = U256::from(xp.len());
    let ann = amp.checked_mul(n)?;
    let mut s_prime = U256::ZERO;
    let mut c = d;
    #[allow(clippy::needless_range_loop)]
    for k in 0..xp.len() {
        if k == i {
            continue;
        }
        s_prime = s_prime.checked_add(xp[k])?;
        c = c
            .checked_mul(d)?
            .checked_div(xp[k].checked_mul(n)?)?;
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
    fn get_d_balanced_three_coins() {
        let balance = U256::from(1_000_000u64) * wad();
        let d = get_d(&[balance, balance, balance], U256::from(200u64)).expect("converge");
        let expected = balance * U256::from(3u64);
        let diff = if d > expected { d - expected } else { expected - d };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_roundtrip() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance, balance];
        let amp = U256::from(200u64);
        let d = get_d(&xp, amp).expect("d");
        let y = get_y(0, 1, xp[0], &xp, d, amp).expect("y");
        let diff = if y > xp[1] { y - xp[1] } else { xp[1] - y };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_swap_reduces_output() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance, balance];
        let amp = U256::from(200u64);
        let d = get_d(&xp, amp).expect("d");
        let dx = U256::from(1000u64) * wad();
        let y = get_y(0, 2, xp[0] + dx, &xp, d, amp).expect("y");
        let dy = xp[2] - y;
        assert!(dy < dx);
        assert!(dy > U256::ZERO);
    }

    #[test]
    fn high_amp_approaches_constant_sum() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(1_000_000u64);
        let d = get_d(&xp, amp).expect("d");
        let dx = U256::from(1000u64) * wad();
        let y = get_y(0, 1, xp[0] + dx, &xp, d, amp).expect("y");
        let dy = xp[1] - y;
        let diff = if dy > dx { dy - dx } else { dx - dy };
        assert!(diff < dx / U256::from(1000u64));
    }
}
