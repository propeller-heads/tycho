//! Pool-level get_amount_out for TwoCryptoNG (crvUSD/FXN, Cardano cubic solver).

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::twocrypto_ng::{get_y_2_ng, FEE_DENOMINATOR, WAD};

fn crypto_fee(xp: &[U256], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> Option<U256> {
    let wad = U256::from(WAD);
    let s: U256 = xp
        .iter()
        .try_fold(U256::ZERO, |acc, v| acc.checked_add(*v))?;
    if s.is_zero() {
        return None;
    }
    let n = U256::from(xp.len());
    let mut k = wad;
    for x_i in xp {
        k = k * n * (*x_i) / s;
    }
    // fee_gamma * WAD / (fee_gamma + WAD - K)
    // Matches both v2.0.0 and v2.1.0 deployed pool.fee_calc() / get_dy.
    let f = if fee_gamma > U256::ZERO { fee_gamma * wad / (fee_gamma + wad - k) } else { k };
    Some((mid_fee * f + out_fee * (wad - f)) / wad)
}

pub fn get_amount_out(
    balances: &[U256; 2],
    precisions: &[U256; 2],
    price_scale: U256,
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
    let price_scale_local = price_scale * precisions[1];

    let mut bal = *balances;
    bal[i] += dx;
    let xp: [U256; 2] = [bal[0] * precisions[0], bal[1] * price_scale_local / wad];

    // NG uses Cardano cubic solver
    let (y, _k0) = get_y_2_ng(ann, gamma, xp, d, j)?;

    if xp[j] <= y {
        return None;
    }

    let dy = xp[j] - y - U256::from(1);
    let xp_after: [U256; 2] = if j == 0 { [y, xp[1]] } else { [xp[0], y] };

    // Vyper: two sequential divisions (dy * WAD // price_scale // precisions[j])
    // NOT single division by (price_scale * precisions[j])
    let dy_native = if j > 0 { dy * wad / price_scale / precisions[j] } else { dy / precisions[0] };

    let fee = crypto_fee(&xp_after, mid_fee, out_fee, fee_gamma)?;
    let fee_amount = fee * dy_native / U256::from(FEE_DENOMINATOR);
    let result = dy_native - fee_amount;

    if result.is_zero() {
        return None;
    }

    Some(result)
}

pub fn get_amount_in(
    balances: &[U256; 2],
    precisions: &[U256; 2],
    price_scale: U256,
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
    let price_scale_local = price_scale * precisions[1];

    let xp_orig: [U256; 2] = [balances[0] * precisions[0], balances[1] * price_scale_local / wad];

    // First pass: estimate fee from pre-swap state
    let fee_est = crypto_fee(&xp_orig, mid_fee, out_fee, fee_gamma)?;
    let complement_est = fee_denom - fee_est;
    let dy_native = (desired_output * fee_denom + complement_est - U256::from(1)) / complement_est;

    let dy_internal = if j > 0 {
        (dy_native * price_scale_local + wad - U256::from(1)) / wad
    } else {
        dy_native * precisions[0]
    } + U256::from(1);
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let (x_new, _) = get_y_2_ng(ann, gamma, xp_mod, d, i)?;

    // Second pass: recompute fee with actual xp_after
    let mut xp_after = [U256::ZERO; 2];
    xp_after[i] = x_new;
    xp_after[j] = y;
    let fee_actual = crypto_fee(&xp_after, mid_fee, out_fee, fee_gamma)?;
    let complement_actual = fee_denom - fee_actual;
    let dy_native =
        (desired_output * fee_denom + complement_actual - U256::from(1)) / complement_actual;
    let dy_internal = if j > 0 {
        (dy_native * price_scale_local + wad - U256::from(1)) / wad
    } else {
        dy_native * precisions[0]
    } + U256::from(1);
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let (x_new, _) = get_y_2_ng(ann, gamma, xp_mod, d, i)?;

    if x_new <= xp_orig[i] {
        return None;
    }
    let dx = if i > 0 {
        (x_new - xp_orig[i]) * wad / price_scale_local
    } else {
        (x_new - xp_orig[i]) / precisions[0]
    } + U256::from(1);
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
    balances: &[U256; 2],
    precisions: &[U256; 2],
    price_scale: U256,
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
        let balances = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64)];
        let price_scale = wad;
        let d = U256::from(10000u64) * wad;
        let ann = U256::from(540_000u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let dx = U256::from(1u64) * wad;
        let dy = get_amount_out(
            &balances,
            &precisions,
            price_scale,
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
            price_scale,
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
            price_scale,
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
        let balances = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64)];
        let price_scale = wad;
        let d = U256::from(10000u64) * wad;
        let ann = U256::from(540_000u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let dx = U256::from(1_000_000_000_000_000u128);
        let dy = get_amount_out(
            &balances,
            &precisions,
            price_scale,
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
            price_scale,
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
    fn crypto_fee_balanced() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let xp = [U256::from(100_000u64) * wad, U256::from(100_000u64) * wad];
        let fee = crypto_fee(&xp, mid_fee, out_fee, fee_gamma).expect("fee");
        assert!(fee >= mid_fee);
        assert!(fee < out_fee);
    }

    #[test]
    fn spot_price_nonzero() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        let balances = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64)];
        let price_scale = wad;
        let d = U256::from(10000u64) * wad;
        let ann = U256::from(540_000u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let mid_fee = U256::from(3_000_000u64);
        let out_fee = U256::from(30_000_000u64);
        let fee_gamma = U256::from(230_000_000_000_000u64);
        let (num, den) = spot_price(
            &balances,
            &precisions,
            price_scale,
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
