//! Pool-level get_amount_out for StableSwapSTETH (Lido stETH/ETH).
//!
//! -1 offset. Fee before denormalize. Same swap shape as StableSwapV2; only the underlying
//! `get_d` differs (see `core::stableswap_steth`). The stETH pool's coins are both 18-decimal, so
//! the rate normalization (`10^(36-18) = 10^18`) is the identity, matching the on-chain pool that
//! uses raw balances.

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::stableswap_steth::{
    get_d, get_y, A_PRECISION, FEE_DENOMINATOR, PRECISION,
};

pub fn get_amount_out(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
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

    let dy = xp[j] - y_new - U256::from(1);
    let fee_amount = fee * dy / U256::from(FEE_DENOMINATOR);
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
    let dy_after_fee_internal = desired_output * rates[j] / precision;
    let complement = fee_denom - fee;
    let dy_internal = (dy_after_fee_internal * fee_denom + complement - U256::from(1)) / complement;
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
pub fn spot_price(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
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
    let mut d_p = d;
    for x_k in &xp {
        d_p = d_p
            .checked_mul(d)?
            .checked_div(x_k.checked_mul(n)?)?;
    }
    let num_xp = ann_eff
        .checked_mul(xp[i])?
        .checked_add(d_p)?;
    let den_xp = ann_eff
        .checked_mul(xp[j])?
        .checked_add(d_p)?;
    if den_xp.is_zero() {
        return None;
    }
    let numerator = num_xp
        .checked_mul(balances[j])?
        .checked_mul(fee_denom - fee)?;
    let denominator = den_xp
        .checked_mul(balances[i])?
        .checked_mul(fee_denom)?;
    Some((numerator, denominator))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE_18: u128 = 1_000_000_000_000_000_000;

    /// Wei-exact differential vs on-chain `get_dy` for the Lido stETH/ETH pool
    /// (0xDC24316b9AE028F1497c275EB9192a3Ea0f67022) at block 25401200.
    /// amp = 90000 (A=900, A_PRECISION=100), fee = 1_000_000. Both coins 18-decimal.
    #[test]
    fn steth_matches_onchain_get_dy() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let balances = [u("18442503510792261301777"), u("22475254954595626231709")];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(90_000u64);
        let fee = U256::from(1_000_000u64);

        let cases: [(usize, usize, U256, U256); 6] = [
            (0, 1, u("1000000000000000000"), u("1000123023002245111")),
            (1, 0, u("1000000000000000000"), u("999676911804004156")),
            (0, 1, u("100000000000000000000"), u("100011734178369415039")),
            (1, 0, u("100000000000000000000"), u("99967121249884639048")),
            (0, 1, u("5000000000000000000000"), u("4999229397531533957465")),
            (1, 0, u("5000000000000000000000"), u("4996698666409888617280")),
        ];
        for (i, j, dx, expected) in cases {
            let got = get_amount_out(&balances, &rates, amp, fee, i, j, dx)
                .expect("steth get_amount_out");
            assert_eq!(got, expected, "steth {i}->{j} dx={dx}");
        }
    }

    #[test]
    fn zero_returns_none() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        assert!(get_amount_out(
            &[b, b],
            &[U256::from(RATE_18), U256::from(RATE_18)],
            U256::from(90_000u64),
            U256::from(1_000_000u64),
            0,
            1,
            U256::ZERO,
        )
        .is_none());
    }

    #[test]
    fn roundtrip() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let balances = [u("18442503510792261301777"), u("22475254954595626231709")];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(90_000u64);
        let fee = U256::from(1_000_000u64);
        let dx = u("1000000000000000000");
        let dy = get_amount_out(&balances, &rates, amp, fee, 0, 1, dx).expect("out");
        let dx_recovered = get_amount_in(&balances, &rates, amp, fee, 0, 1, dy).expect("in");
        assert!(dx_recovered >= dx);
        assert!(dx_recovered <= dx + U256::from(2));
    }

    #[test]
    fn spot_price_balanced_near_one() {
        let b = U256::from(1_000_000_000_000_000_000_000_000u128);
        let balances = [b, b];
        let rates = [U256::from(RATE_18), U256::from(RATE_18)];
        let amp = U256::from(90_000u64);
        let fee = U256::from(1_000_000u64);
        let (num, den) = spot_price(&balances, &rates, amp, fee, 0, 1).expect("price");
        let diff = if num > den { num - den } else { den - num };
        assert!(diff * U256::from(1000) < den, "spot price not near 1");
    }
}
