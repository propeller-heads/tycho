//! Pool-level get_amount_out for TwoCryptoV1 (CRV/ETH legacy CurveCryptoSwap2).

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::twocrypto_v1::{
    crypto_fee, newton_d_2_v1, newton_y_2, FEE_DENOMINATOR, WAD,
};

/// Recompute the invariant D from the current balances, as the deployed legacy pool's `get_dy`
/// does whenever `future_A_gamma_time > 0`:
/// `D = self.newton_D(A, gamma, self.xp())` with
/// `xp() = [balances[0] * PRECISIONS[0], balances[1] * PRECISIONS[1] * price_scale / PRECISION]`.
pub fn recompute_d(
    balances: &[U256; 2],
    precisions: &[U256; 2],
    price_scale: U256,
    ann: U256,
    gamma: U256,
) -> Option<U256> {
    let wad = U256::from(WAD);
    let xp: [U256; 2] = [
        balances[0].checked_mul(precisions[0])?,
        balances[1]
            .checked_mul(precisions[1])?
            .checked_mul(price_scale)? /
            wad,
    ];
    newton_d_2_v1(ann, gamma, xp)
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
    eth_variant: bool,
    i: usize,
    j: usize,
    dx: U256,
) -> Option<U256> {
    if dx.is_zero() {
        return None;
    }

    let wad = U256::from(WAD);

    // Vyper: price_scale_local = self.price_scale * PRECISIONS[1]
    let price_scale_local = price_scale * precisions[1];

    // Vyper: xp = self.balances; xp[i] += dx
    // xp = [xp[0]*PRECISIONS[0], xp[1]*price_scale_local/PRECISION]
    let mut bal = *balances;
    bal[i] += dx;
    let xp: [U256; 2] = [bal[0] * precisions[0], bal[1] * price_scale_local / wad];

    // Vyper: y = newton_y(A, gamma, xp, D, j)
    let y = newton_y_2(ann, gamma, xp, d, j, eth_variant)?;

    if xp[j] <= y {
        return None;
    }

    // Vyper: dy = xp[j] - y - 1
    let dy = xp[j] - y - U256::from(1);

    // Vyper: xp[j] = y  (for fee calc)
    let xp_after: [U256; 2] = if j == 0 { [y, xp[1]] } else { [xp[0], y] };

    // Vyper: denormalize
    let dy_native = if j > 0 { dy * wad / price_scale_local } else { dy / precisions[0] };

    // Vyper: dy -= _fee(xp) * dy / 10**10
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
    eth_variant: bool,
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

    // Original xp (before swap)
    let xp_orig: [U256; 2] = [balances[0] * precisions[0], balances[1] * price_scale_local / wad];

    // First pass: estimate fee from pre-swap state
    let fee_est = crypto_fee(&xp_orig, mid_fee, out_fee, fee_gamma)?;
    let complement_est = fee_denom - fee_est;
    let dy_native = (desired_output * fee_denom + complement_est - U256::from(1)) / complement_est;

    // Renormalize to internal space (round up + 1 for -1 offset)
    let dy_internal = if j > 0 {
        (dy_native * price_scale_local + wad - U256::from(1)) / wad
    } else {
        dy_native * precisions[0]
    } + U256::from(1);
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;

    // Solve for x_new at index i
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let x_new = newton_y_2(ann, gamma, xp_mod, d, i, eth_variant)?;

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
    let x_new = newton_y_2(ann, gamma, xp_mod, d, i, eth_variant)?;

    if x_new <= xp_orig[i] {
        return None;
    }
    // Denormalize dx
    let dx = if i > 0 {
        (x_new - xp_orig[i]) * wad / price_scale_local
    } else {
        (x_new - xp_orig[i]) / precisions[0]
    } + U256::from(1);
    // Verify with forward pass and binary search if Newton tolerance caused undershoot
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
            eth_variant,
            i,
            j,
            amt,
        )
    };
    match forward(dx) {
        Some(dy_check) if dy_check >= desired_output => return Some(dx),
        _ => {}
    }
    // Binary search: dx is too low, find upper bound then bisect
    let mut lo = dx;
    let mut hi = dx;
    for _ in 0..64 {
        hi = hi + hi / U256::from(10u64) + U256::from(1u64); // grow by 10% + 1
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
    eth_variant: bool,
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
        eth_variant,
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
        // Use params from core tests that are known to converge
        let balances = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64)];
        let price_scale = wad;
        let d = U256::from(10000u64) * wad;
        let ann = U256::from(540_000u64) * U256::from(10_000u64);
        let gamma = U256::from(28_000_000_000_000u64);
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
            true,
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
            true,
            0,
            1,
            dy,
        )
        .expect("in");
        // Verify the recovered dx produces at least the desired output
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
            true,
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
        let gamma = U256::from(28_000_000_000_000u64);
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
            true,
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
            true,
            0,
            1,
        )
        .expect("price");
        // dy/dx ≈ num/den → dy * den ≈ dx * num (within 1%)
        let lhs = dy * den;
        let rhs = dx * num;
        let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
        assert!(diff * U256::from(100) < rhs, "spot price inconsistent with swap");
    }

    #[test]
    fn spot_price_nonzero() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        let balances = [U256::from(5000u64) * wad, U256::from(5000u64) * wad];
        let precisions = [U256::from(1u64), U256::from(1u64)];
        let price_scale = wad;
        let d = U256::from(10000u64) * wad;
        let ann = U256::from(540_000u64) * U256::from(10_000u64);
        let gamma = U256::from(28_000_000_000_000u64);
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
            true,
            0,
            1,
        )
        .expect("price");
        assert!(!num.is_zero(), "numerator is zero");
        assert!(!den.is_zero(), "denominator is zero");
    }
}
