//! Pool-level get_amount_out for TwoCryptoNG pools using StableSwap MATH (v0.1.0).
//!
//! These pools are deployed from the TwoCryptoNG factory but use StableSwap invariant.
//! Normalization and fee are CryptoSwap-style (price_scale, crypto_fee).
//! Core math is StableSwap (gamma is ignored).

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::twocrypto_stable::{get_d, get_y};

pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);

fn crypto_fee(xp: &[U256], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> Option<U256> {
    let wad = WAD;
    let s: U256 = xp
        .iter()
        .try_fold(U256::ZERO, |acc, v| acc.checked_add(*v))?;
    if s.is_zero() {
        return None;
    }
    let n = U256::from(xp.len());
    // Vyper V1 _fee: K = WAD * N^N * xp[0] / S * xp[1] / S
    let mut nn = U256::from(1u64);
    for _ in 0..xp.len() {
        nn *= n;
    }
    let mut k = wad * nn;
    for x_i in xp {
        k = k * (*x_i) / s;
    }
    // NG fee formula: f = fee_gamma * k / (fee_gamma * k / WAD + WAD - k)
    let f =
        if fee_gamma > U256::ZERO { fee_gamma * k / (fee_gamma * k / wad + wad - k) } else { k };
    Some((mid_fee * f + out_fee * (wad - f)) / wad)
}

/// Recompute the invariant D from the current balances, as the deployed views contract does
/// while an A ramp is active. The pool template is TwoCrypto-NG, so the ramp branch calls
/// `MATH.newton_D` — here the StableswapMath flavour (`get_d`), guarded by its `!balance`
/// assert (`xp[0] > 0 and xp[1] > 0 and max/min < 10_000`).
pub fn recompute_d(
    balances: &[U256; 2],
    precisions: &[U256; 2],
    price_scale: U256,
    ann: U256,
) -> Option<U256> {
    let wad = WAD;
    let price_scale_local = price_scale.checked_mul(precisions[1])?;
    let xp: [U256; 2] = [
        balances[0].checked_mul(precisions[0])?,
        balances[1].checked_mul(price_scale_local)? / wad,
    ];
    if xp[0].is_zero() ||
        xp[1].is_zero() ||
        xp[0].max(xp[1]) / xp[0].min(xp[1]) >= U256::from(10_000u64)
    {
        return None;
    }
    get_d(&xp, ann)
}

pub fn get_amount_out(
    balances: &[U256; 2],
    precisions: &[U256; 2],
    price_scale: U256,
    d: U256,
    ann: U256,
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

    let wad = WAD;
    let price_scale_local = price_scale * precisions[1];

    // CryptoSwap normalization — compute xp BEFORE and AFTER dx
    let xp_orig: [U256; 2] = [balances[0] * precisions[0], balances[1] * price_scale_local / wad];
    let mut bal = *balances;
    bal[i] += dx;
    let xp: [U256; 2] = [bal[0] * precisions[0], bal[1] * price_scale_local / wad];

    // StableSwap core — use stored D (passed from on-chain), get_y with new xp[i]
    let y_new = get_y(i, j, xp[i], &xp_orig, d, ann)?;

    if xp[j] <= y_new {
        return None;
    }

    let dy = xp[j] - y_new - U256::from(1);
    let xp_after: [U256; 2] = if j == 0 { [y_new, xp[1]] } else { [xp[0], y_new] };

    // CryptoSwap denormalization
    let dy_native = if j > 0 { dy * wad / price_scale / precisions[j] } else { dy / precisions[0] };

    // CryptoSwap fee
    let fee = crypto_fee(&xp_after, mid_fee, out_fee, fee_gamma)?;
    let fee_amount = fee * dy_native / FEE_DENOMINATOR;
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

    let wad = WAD;
    let fee_denom = FEE_DENOMINATOR;
    let price_scale_local = price_scale * precisions[1];

    let xp_orig: [U256; 2] = [balances[0] * precisions[0], balances[1] * price_scale_local / wad];

    // Estimate fee from pre-swap state
    let fee_est = crypto_fee(&xp_orig, mid_fee, out_fee, fee_gamma)?;
    let complement = fee_denom - fee_est;

    // Reverse fee (ceiling division)
    let dy_native = (desired_output * fee_denom + complement - U256::from(1)) / complement;

    // Renormalize to internal space + reverse -1 offset
    let dy_internal = if j > 0 {
        (dy_native * price_scale_local + wad - U256::from(1)) / wad
    } else {
        dy_native * precisions[0]
    } + U256::from(1);

    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y_new = xp_orig[j] - dy_internal;

    // Solve for x_new using StableSwap get_y (swap i and j)
    let x_new = get_y(j, i, y_new, &xp_orig, d, ann)?;
    if x_new <= xp_orig[i] {
        return None;
    }

    // Denormalize dx
    let dx = if i > 0 {
        (x_new - xp_orig[i]) * wad / price_scale_local
    } else {
        (x_new - xp_orig[i]) / precisions[0]
    } + U256::from(1);

    // Binary search to ensure get_amount_out(dx) >= desired_output
    let forward = |amt: U256| {
        get_amount_out(
            balances,
            precisions,
            price_scale,
            d,
            ann,
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
    mid_fee: U256,
    out_fee: U256,
    fee_gamma: U256,
    i: usize,
    j: usize,
) -> Option<(U256, U256)> {
    let dx = U256::from(1_000_000_000_000_000u64);
    let dy = get_amount_out(
        balances,
        precisions,
        price_scale,
        d,
        ann,
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
    fn basic_swap() {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        // Realistic params from crvUSD/WETH pool using MATH v0.1.0
        let balances = [
            U256::from_str_radix("16913758444575087263963692", 10).unwrap(),
            U256::from_str_radix("13702674829211830926174", 10).unwrap(),
        ];
        let precisions = [U256::from(1u64), U256::from(1u64)];
        let price_scale = U256::from_str_radix("2797050192554649390777", 10).unwrap();
        let ann = U256::from(25000u64);
        let mid_fee = U256::from(60000000u64);
        let out_fee = U256::from(220000000u64);
        let fee_gamma = U256::from_str_radix("1395000000000000", 10).unwrap();
        // Compute D from xp (simulating stored D)
        let price_scale_local = price_scale * precisions[1];
        let xp_orig: [U256; 2] =
            [balances[0] * precisions[0], balances[1] * price_scale_local / wad];
        let d = crate::evm::protocol::curve::math::core::twocrypto_stable::get_d(&xp_orig, ann)
            .expect("D");
        let dx = wad; // 1 crvUSD
        let dy = get_amount_out(
            &balances,
            &precisions,
            price_scale,
            d,
            ann,
            mid_fee,
            out_fee,
            fee_gamma,
            0,
            1,
            dx,
        )
        .expect("swap");
        assert!(dy > U256::ZERO);
    }
}
