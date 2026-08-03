//! Pool-level get_amount_out for StableSwapNG (plain + meta NG pools).
//!
//! -1 offset. Fee before denormalize. Dynamic fee with avg xp.

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::stableswap_ng::{
    dynamic_fee, get_d, get_y, A_PRECISION, FEE_DENOMINATOR, PRECISION,
};

pub fn get_amount_out(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
    offpeg_fee_multiplier: U256,
    i: usize,
    j: usize,
    dx: U256,
) -> Option<U256> {
    if dx.is_zero() {
        return None;
    }

    let precision = U256::from(PRECISION);

    let xp: Vec<U256> = balances
        .iter()
        .zip(rates.iter())
        .map(|(b, r)| *b * *r / precision)
        .collect();

    let d = get_d(&xp, amp)?;
    let x_new = xp[i] + dx * rates[i] / precision;
    let y_new = get_y(i, j, x_new, &xp, d, amp)?;

    if xp[j] <= y_new {
        return None;
    }

    // Vyper: dy = xp[j] - y - 1
    let dy = xp[j] - y_new - U256::from(1);

    // Vyper: fee = _dynamic_fee((xp[i]+x)/2, (xp[j]+y)/2, ...) * dy / FEE_DENOM
    let fee_rate = dynamic_fee(
        (xp[i] + x_new) / U256::from(2u64),
        (xp[j] + y_new) / U256::from(2u64),
        fee,
        offpeg_fee_multiplier,
    );
    let fee_amount = fee_rate * dy / U256::from(FEE_DENOMINATOR);

    // Vyper: return (dy - fee) * PRECISION / rates[j]
    let result = (dy - fee_amount) * precision / rates[j];
    if result.is_zero() {
        return None;
    }

    Some(result)
}

pub fn get_amount_in(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
    offpeg_fee_multiplier: U256,
    i: usize,
    j: usize,
    desired_output: U256,
) -> Option<U256> {
    if desired_output.is_zero() {
        return None;
    }
    let precision = U256::from(PRECISION);
    let fee_denom = U256::from(FEE_DENOMINATOR);
    let xp: Vec<U256> = balances
        .iter()
        .zip(rates.iter())
        .map(|(b, r)| *b * *r / precision)
        .collect();
    let d = get_d(&xp, amp)?;

    // Reverse denorm
    let dy_after_fee_internal = desired_output * rates[j] / precision;

    // First pass: use base fee as estimate (round up)
    let fee_complement = fee_denom - fee;
    let dy_internal =
        (dy_after_fee_internal * fee_denom + fee_complement - U256::from(1)) / fee_complement;
    if xp[j] <= dy_internal + U256::from(1) {
        return None;
    }
    let y_new = xp[j] - dy_internal - U256::from(1);
    let x_new = get_y(j, i, y_new, &xp, d, amp)?;

    // Second pass: recompute with actual dynamic fee
    let actual_fee = dynamic_fee(
        (xp[i] + x_new) / U256::from(2u64),
        (xp[j] + y_new) / U256::from(2u64),
        fee,
        offpeg_fee_multiplier,
    );
    let actual_complement = fee_denom - actual_fee;
    let dy_internal =
        (dy_after_fee_internal * fee_denom + actual_complement - U256::from(1)) / actual_complement;
    if xp[j] <= dy_internal + U256::from(1) {
        return None;
    }
    let y_new = xp[j] - dy_internal - U256::from(1);
    let x_new = get_y(j, i, y_new, &xp, d, amp)?;
    if x_new <= xp[i] {
        return None;
    }
    let dx = (x_new - xp[i]) * precision / rates[i] + U256::from(1);
    Some(dx)
}

/// Spot price dy/dx including fee, returned as (numerator, denominator).
/// Analytical: from implicit differentiation of StableSwap invariant.
/// Uses dynamic fee at current pool state (zero trade size).
pub fn spot_price(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
    offpeg_fee_multiplier: U256,
    i: usize,
    j: usize,
) -> Option<(U256, U256)> {
    let precision = U256::from(PRECISION);
    let fee_denom = U256::from(FEE_DENOMINATOR);
    let n = U256::from(balances.len());
    let ann_eff = amp.checked_mul(n)? / U256::from(A_PRECISION);
    let xp: Vec<U256> = balances
        .iter()
        .zip(rates.iter())
        .map(|(b, r)| *b * *r / precision)
        .collect();
    let d = get_d(&xp, amp)?;
    // D_P = D^(N+1) / (N^N * prod(xp)), computed iteratively
    let mut d_p = d;
    for x_k in &xp {
        d_p = d_p
            .checked_mul(d)?
            .checked_div(x_k.checked_mul(n)?)?;
    }
    // Implicit differentiation of StableSwap invariant at constant D:
    // dy/dx = (A_n + D_P/xp[i]) / (A_n + D_P/xp[j])
    // As integer fraction: (A_n*xp[i] + D_P) * bal[j] / ((A_n*xp[j] + D_P) * bal[i])
    let num_xp = ann_eff
        .checked_mul(xp[i])?
        .checked_add(d_p)?;
    let den_xp = ann_eff
        .checked_mul(xp[j])?
        .checked_add(d_p)?;
    if den_xp.is_zero() {
        return None;
    }
    let effective_fee = dynamic_fee(xp[i], xp[j], fee, offpeg_fee_multiplier);
    let numerator = num_xp
        .checked_mul(balances[j])?
        .checked_mul(fee_denom - effective_fee)?;
    let denominator = den_xp
        .checked_mul(balances[i])?
        .checked_mul(fee_denom)?;
    Some((numerator, denominator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::protocol::curve::math::core::stableswap_ng::A_PRECISION;

    const RATE_18: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn basic_swap() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        let out = get_amount_out(
            &[b, b],
            &[U256::from(RATE_18), U256::from(RATE_18)],
            U256::from(400u64) * A_PRECISION,
            U256::from(4_000_000u64),
            U256::from(20_000_000_000u64),
            0,
            1,
            U256::from(1_000_000_000_000_000_000_000u128),
        )
        .expect("swap");
        assert!(out > U256::ZERO);
    }

    #[test]
    fn roundtrip() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        let balances = [b, b];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(400u64) * A_PRECISION;
        let fee = U256::from(4_000_000u64);
        let offpeg = U256::from(20_000_000_000u64);
        let dx = U256::from(1_000_000_000_000_000_000_000u128);
        let dy = get_amount_out(&balances, &rates, amp, fee, offpeg, 0, 1, dx).expect("out");
        let dx_recovered =
            get_amount_in(&balances, &rates, amp, fee, offpeg, 0, 1, dy).expect("in");
        assert!(dx_recovered >= dx);
        assert!(dx_recovered <= dx + U256::from(2));
        let dy_check =
            get_amount_out(&balances, &rates, amp, fee, offpeg, 0, 1, dx_recovered).expect("check");
        assert!(dy_check >= dy);
    }

    #[test]
    fn spot_price_balanced_near_one() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        let balances = [b, b];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(400u64) * A_PRECISION;
        let fee = U256::from(4_000_000u64);
        let offpeg = U256::from(20_000_000_000u64);
        let (num, den) = spot_price(&balances, &rates, amp, fee, offpeg, 0, 1).expect("price");
        let diff = if num > den { num - den } else { den - num };
        assert!(diff * U256::from(1000) < den, "spot price not near 1");
    }

    #[test]
    fn spot_price_symmetry() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        let balances = [b, b];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(400u64) * A_PRECISION;
        let fee = U256::from(4_000_000u64);
        let offpeg = U256::from(20_000_000_000u64);
        let (num_ij, den_ij) =
            spot_price(&balances, &rates, amp, fee, offpeg, 0, 1).expect("price_ij");
        let (num_ji, den_ji) =
            spot_price(&balances, &rates, amp, fee, offpeg, 1, 0).expect("price_ji");
        let product_num = num_ij * num_ji;
        let product_den = den_ij * den_ji;
        let diff = if product_num > product_den {
            product_num - product_den
        } else {
            product_den - product_num
        };
        assert!(diff * U256::from(1000) < product_den, "symmetry violated");
    }

    #[test]
    fn spot_price_consistent_with_swap() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        let balances = [b, b];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(400u64) * A_PRECISION;
        let fee = U256::from(4_000_000u64);
        let offpeg = U256::from(20_000_000_000u64);
        let dx = U256::from(1_000_000_000_000_000u128);
        let dy = get_amount_out(&balances, &rates, amp, fee, offpeg, 0, 1, dx).expect("out");
        let (num, den) = spot_price(&balances, &rates, amp, fee, offpeg, 0, 1).expect("price");
        let lhs = dy * den;
        let rhs = dx * num;
        let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
        assert!(diff * U256::from(100) < rhs, "spot price inconsistent with swap");
    }
}
