//! StableSwapALend — Aave-style lending pools (Aave, sAAVE, IB, aETH).
//!
//! a_precision=100, no -1 offset, fee after denormalize, dynamic fee with avg xp.
//! Deployed Vyper (the actual on-chain pools, not the later generic template):
//! https://github.com/curvefi/curve-contract/blob/master/contracts/pools/aave/StableSwapAave.vy
//! https://github.com/curvefi/curve-contract/blob/master/contracts/pools/saave/StableSwapSAAVE.vy
//!
//! IMPORTANT: Uses PRECISION_MUL directly (not stored_rates/PRECISION).
//! Normalization: xp[k] = balance[k] * precision_mul[k]
//!
//! `get_d` uses the `+1` divisor guard (`_x * N_COINS + 1`) present in the deployed Aave/sAAVE
//! pools. The generic `pool-templates/a/SwapTemplateA.vy` later dropped that `+1`; the deployed
//! pools kept it, so it is required for wei-exactness in low-A / near-zero-balance regimes.

use alloy_primitives::U256;

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
        // Deployed Aave/sAAVE get_D: D_P = D_P * D / (_x * N_COINS + 1). The trailing `+1` is the
        // on-chain "+1 is to prevent /0" guard; it only changes the result when a normalized
        // balance is near zero, but is required to match the deployed pools exactly.
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

pub fn dynamic_fee(xpi: U256, xpj: U256, fee: U256, fee_multiplier: U256) -> U256 {
    if fee_multiplier <= FEE_DENOMINATOR {
        return fee;
    }
    let xps2 = (xpi + xpj) * (xpi + xpj);
    if xps2.is_zero() {
        return fee;
    }
    (fee_multiplier * fee) /
        ((fee_multiplier - FEE_DENOMINATOR) * U256::from(4u64) * xpi * xpj / xps2 +
            FEE_DENOMINATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_d_balanced_pool() {
        let balance = U256::from(1_000_000_000_000_000_000u128);
        let amp = U256::from(100u64) * A_PRECISION;
        let d = get_d(&[balance, balance, balance], amp).expect("converge");
        let expected = balance * U256::from(3u64);
        let diff = if d > expected { d - expected } else { expected - d };
        assert!(diff <= U256::from(1));
    }

    #[test]
    fn get_y_roundtrip() {
        let balance = U256::from(1_000_000_000_000_000_000u128);
        let xp = [balance, balance, balance];
        let amp = U256::from(100u64) * A_PRECISION;
        let d = get_d(&xp, amp).expect("d");
        let y = get_y(0, 1, xp[0], &xp, d, amp).expect("y");
        let diff = if y > xp[1] { y - xp[1] } else { xp[1] - y };
        assert!(diff <= U256::from(1));
    }

    /// Pins the deployed Aave/sAAVE `get_D` `+1` divisor guard. In a near-zero-balance regime the
    /// guard changes the converged D: with `amp = 100 * A_PRECISION` it gives `D = 650`, whereas
    /// dropping the `+1` (the generic `SwapTemplateA.vy` form) gives `D = 602`. The deployed pools
    /// use `+1`. Reference computed from the deployed Vyper integer math.
    #[test]
    fn get_d_plus_one_guard_near_zero_balance() {
        let xp = [U256::from(900u64), U256::from(1u64)];
        let amp = U256::from(100u64) * A_PRECISION;
        assert_eq!(get_d(&xp, amp), Some(U256::from(650u64)));
    }

    #[test]
    fn dynamic_fee_at_peg_returns_base_fee() {
        let fee = U256::from(4_000_000u64);
        let multiplier = U256::from(20_000_000_000u64);
        let xp = U256::from(1_000_000_000_000_000_000u128);
        // When xpi == xpj, 4*xpi*xpj/xps2 = 1, so fee should equal base fee
        let result = dynamic_fee(xp, xp, fee, multiplier);
        assert_eq!(result, fee);
    }

    #[test]
    fn dynamic_fee_off_peg_increases() {
        let fee = U256::from(4_000_000u64);
        let multiplier = U256::from(20_000_000_000u64);
        let xpi = U256::from(2_000_000_000_000_000_000u128);
        let xpj = U256::from(500_000_000_000_000_000u128);
        let result = dynamic_fee(xpi, xpj, fee, multiplier);
        assert!(result > fee);
    }

    #[test]
    fn dynamic_fee_low_multiplier_returns_base() {
        let fee = U256::from(4_000_000u64);
        let fee_denom = FEE_DENOMINATOR;
        let xpi = U256::from(2_000_000_000_000_000_000u128);
        let xpj = U256::from(500_000_000_000_000_000u128);
        // multiplier <= FEE_DENOMINATOR should return base fee
        let result = dynamic_fee(xpi, xpj, fee, fee_denom);
        assert_eq!(result, fee);
    }
}
