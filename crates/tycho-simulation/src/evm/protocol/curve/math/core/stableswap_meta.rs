//! StableSwapMeta — Meta pools (GUSD/3CRV, HUSD, factory meta).
//!
//! a_precision=100, -1 offset, fee before denormalize, static fee.
//! Vyper: https://github.com/curvefi/curve-contract/blob/master/contracts/pool-templates/meta/SwapTemplateMeta.vy

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
        let den = ann
            .checked_sub(A_PRECISION)?
            .checked_mul(d)?
            .checked_div(A_PRECISION)?
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
    fn get_d_balanced_meta_pool() {
        let balance = U256::from(1_000_000u64) * wad();
        let amp = U256::from(500u64) * A_PRECISION;
        let d = get_d(&[balance, balance], amp).expect("converge");
        let expected = balance * U256::from(2u64);
        let diff = if d > expected { d - expected } else { expected - d };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_roundtrip() {
        let balance = U256::from(1_000_000u64) * wad();
        let xp = [balance, balance];
        let amp = U256::from(500u64) * A_PRECISION;
        let d = get_d(&xp, amp).expect("d");
        let y = get_y(0, 1, xp[0], &xp, d, amp).expect("y");
        let diff = if y > xp[1] { y - xp[1] } else { xp[1] - y };
        assert!(diff <= U256::from(1));
    }
}
