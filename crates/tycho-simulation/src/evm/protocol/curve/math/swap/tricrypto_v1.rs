//! Pool-level get_amount_out for TriCryptoV1 (tricrypto2: USDT/WBTC/WETH).

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::tricrypto_v1::{
    crypto_fee, newton_y_3, FEE_DENOMINATOR, WAD,
};

pub fn get_amount_out(
    balances: &[U256; 3],
    precisions: &[U256; 3],
    price_scale: &[U256; 2],
    d: U256,
    ann: U256,
    gamma: U256,
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
    i: usize,
    j: usize,
    dx: U256,
) -> Option<U256> {
    if dx.is_zero() {
        return None;
    }

    let wad = U256::from(WAD);

    // Vyper: xp = balances; xp[i] += dx
    let mut bal = *balances;
    bal[i] += dx;

    // Vyper: xp[0] *= precisions[0]; xp[k] = xp[k] * price_scale[k-1] * precisions[k] / PRECISION
    let xp: [U256; 3] = [
        bal[0] * precisions[0],
        bal[1] * price_scale[0] * precisions[1] / wad,
        bal[2] * price_scale[1] * precisions[2] / wad,
    ];

    // Vyper: y = newton_y(A, gamma, xp, D, j)
    let y = newton_y_3(ann, gamma, xp, d, j)?;

    if xp[j] <= y {
        return None;
    }

    // Vyper: dy = xp[j] - y - 1
    let mut dy = xp[j] - y - U256::from(1);

    // Vyper: xp[j] = y (for fee calc)
    let mut xp_after = xp;
    xp_after[j] = y;

    // Vyper: if j > 0: dy = dy * PRECISION / price_scale[j-1]
    if j > 0 {
        dy = dy * wad / price_scale[j - 1];
    }
    // Vyper: dy /= precisions[j]
    dy /= precisions[j];

    // Vyper: dy -= fee_calc(xp) * dy / 10**10
    let fee = crypto_fee(&xp_after, mid_fee, out_fee, fee_gamma)?;
    let fee_amount = fee * dy / U256::from(FEE_DENOMINATOR);

    let result = dy - fee_amount;
    if result.is_zero() {
        return None;
    }

    Some(result)
}

pub fn get_amount_in(
    balances: &[U256; 3],
    precisions: &[U256; 3],
    price_scale: &[U256; 2],
    d: U256,
    ann: U256,
    gamma: U256,
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
    i: usize,
    j: usize,
    desired_output: U256,
) -> Option<U256> {
    if desired_output.is_zero() {
        return None;
    }

    let wad = U256::from(WAD);
    let fee_denom = U256::from(FEE_DENOMINATOR);

    let xp_orig: [U256; 3] = [
        balances[0] * precisions[0],
        balances[1] * price_scale[0] * precisions[1] / wad,
        balances[2] * price_scale[1] * precisions[2] / wad,
    ];

    // First pass: estimate fee from pre-swap state
    let fee_est = crypto_fee(&xp_orig, mid_fee, out_fee, fee_gamma)?;
    let complement_est = fee_denom - fee_est;
    let dy_native = (desired_output * fee_denom + complement_est - U256::from(1)) / complement_est;

    // Reverse denorm: dy /= precisions[j], and if j > 0: dy = dy * WAD / price_scale[j-1]
    let mut dy_internal = dy_native * precisions[j];
    if j > 0 {
        dy_internal = (dy_internal * price_scale[j - 1] + wad - U256::from(1)) / wad;
    }
    dy_internal += U256::from(1); // +1 for -1 offset
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let x_new = newton_y_3(ann, gamma, xp_mod, d, i)?;

    // Second pass: recompute fee with actual xp_after
    let mut xp_after = xp_orig;
    xp_after[i] = x_new;
    xp_after[j] = y;
    let fee_actual = crypto_fee(&xp_after, mid_fee, out_fee, fee_gamma)?;
    let complement_actual = fee_denom - fee_actual;
    let dy_native =
        (desired_output * fee_denom + complement_actual - U256::from(1)) / complement_actual;
    let mut dy_internal = dy_native * precisions[j];
    if j > 0 {
        dy_internal = (dy_internal * price_scale[j - 1] + wad - U256::from(1)) / wad;
    }
    dy_internal += U256::from(1);
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let x_new = newton_y_3(ann, gamma, xp_mod, d, i)?;

    if x_new <= xp_orig[i] {
        return None;
    }
    let mut dx = x_new - xp_orig[i];
    if i > 0 {
        dx = dx * wad / price_scale[i - 1];
    }
    dx = dx / precisions[i] + U256::from(1);
    let forward = |amt: U256| {
        get_amount_out(
            balances,
            precisions,
            price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            i,
            j,
            amt,
        )
    };
    match forward(dx) {
        Some(dy_check) if dy_check >= desired_output => return Some(dx),
        _ => {}
    }
    let mut lo = dx;
    let mut hi = dx;
    for _ in 0..64 {
        hi = hi + hi / U256::from(10u64) + U256::from(1u64);
        if let Some(dy_check) = forward(hi) {
            if dy_check >= desired_output {
                break;
            }
        }
    }
    for _ in 0..256 {
        if lo >= hi {
            break;
        }
        let mid = (lo + hi) / U256::from(2u64);
        if mid == lo {
            break;
        }
        match forward(mid) {
            Some(dy_check) if dy_check >= desired_output => hi = mid,
            _ => lo = mid + U256::from(1u64),
        }
    }
    Some(hi)
}

/// Spot price dy/dx including fee, returned as (numerator, denominator).
/// Numerical: compute get_amount_out with a small dx for marginal price.
pub fn spot_price(
    balances: &[U256; 3],
    precisions: &[U256; 3],
    price_scale: &[U256; 2],
    d: U256,
    ann: U256,
    gamma: U256,
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
    i: usize,
    j: usize,
) -> Option<(U256, U256)> {
    let dx = U256::from(1_000_000_000_000_000u64); // 10^15 = 0.001 tokens for 18-dec
    let dy = get_amount_out(
        balances,
        precisions,
        price_scale,
        d,
        ann,
        gamma,
        mid_fee,
        out_fee,
        fee_gamma,
        i,
        j,
        dx,
    )?;
    Some((dy, dx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        // All 18-dec, 1:1:1 pricing for simplicity
        let balances =
            [U256::from(5000u64) * wad, U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64), U256::from(1u64)];
        let price_scale = [wad, wad];
        let d = U256::from(15000u64) * wad;
        let ann = U256::from(1707629u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let dx = U256::from(1u64) * wad;
        let dy = get_amount_out(
            &balances,
            &precisions,
            &price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
            dx,
        )
        .expect("out");
        let dx_recovered = get_amount_in(
            &balances,
            &precisions,
            &price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
            dy,
        )
        .expect("in");
        let dy_check = get_amount_out(
            &balances,
            &precisions,
            &price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
            dx_recovered,
        )
        .expect("check");
        assert!(dy_check >= dy);
    }

    #[test]
    fn spot_price_consistent_with_swap() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        let balances =
            [U256::from(5000u64) * wad, U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64), U256::from(1u64)];
        let price_scale = [wad, wad];
        let d = U256::from(15000u64) * wad;
        let ann = U256::from(1707629u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let dx = U256::from(1_000_000_000_000_000u128);
        let dy = get_amount_out(
            &balances,
            &precisions,
            &price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
            dx,
        )
        .expect("out");
        let (num, den) = spot_price(
            &balances,
            &precisions,
            &price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
        )
        .expect("price");
        let lhs = dy * den;
        let rhs = dx * num;
        let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
        assert!(diff * U256::from(100) < rhs, "spot price inconsistent with swap");
    }

    #[test]
    fn spot_price_nonzero() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        let balances =
            [U256::from(5000u64) * wad, U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64), U256::from(1u64)];
        let price_scale = [wad, wad];
        let d = U256::from(15000u64) * wad;
        let ann = U256::from(1707629u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let (num, den) = spot_price(
            &balances,
            &precisions,
            &price_scale,
            d,
            ann,
            gamma,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
        )
        .expect("price");
        assert!(!num.is_zero(), "numerator is zero");
        assert!(!den.is_zero(), "denominator is zero");
    }
}
