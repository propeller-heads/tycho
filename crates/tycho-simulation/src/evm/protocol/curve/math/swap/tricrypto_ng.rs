//! Pool-level get_amount_out for TriCryptoNG (USDC/WBTC/WETH, hybrid cubic+Newton).

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::tricrypto_ng::{
    crypto_fee, get_y_3_ng, newton_d_3, FEE_DENOMINATOR, WAD,
};

/// Recompute the invariant D from the current balances, as the deployed views contract does
/// while an A/gamma ramp is active (`CurveCryptoViews3Optimized._calc_D_ramp`):
/// `xp` is built exactly like [`get_amount_out`]'s (without any swap input), then
/// `newton_D(A, gamma, xp, 0)`.
pub fn recompute_d(
    balances: &[U256; 3],
    precisions: &[U256; 3],
    price_scale: &[U256; 2],
    ann: U256,
    gamma: U256,
) -> Option<U256> {
    let wad = U256::from(WAD);
    let xp: [U256; 3] = [
        balances[0].checked_mul(precisions[0])?,
        balances[1]
            .checked_mul(price_scale[0])?
            .checked_mul(precisions[1])? /
            wad,
        balances[2]
            .checked_mul(price_scale[1])?
            .checked_mul(precisions[2])? /
            wad,
    ];
    newton_d_3(ann, gamma, xp)
}

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
    // Vyper pool contract: `assert i != j and i < N_COINS and j < N_COINS
    //                        # dev: coin index out of range`
    if i >= 3 || j >= 3 || i == j {
        return None;
    }
    if dx.is_zero() {
        return None;
    }

    let wad = U256::from(WAD);

    let mut bal = *balances;
    bal[i] = bal[i].checked_add(dx)?;

    // Checked: an oversized dx must yield None, not wrap into a plausible-looking xp.
    let xp: [U256; 3] = [
        bal[0].checked_mul(precisions[0])?,
        bal[1]
            .checked_mul(price_scale[0])?
            .checked_mul(precisions[1])? /
            wad,
        bal[2]
            .checked_mul(price_scale[1])?
            .checked_mul(precisions[2])? /
            wad,
    ];

    // NG uses hybrid cubic+Newton solver
    let (y, _k0) = get_y_3_ng(ann, gamma, xp, d, j)?;

    if xp[j] <= y {
        return None;
    }

    let mut dy = xp[j] - y - U256::from(1);
    let mut xp_after = xp;
    xp_after[j] = y;

    if j > 0 {
        dy = dy * wad / price_scale[j - 1];
    }
    dy /= precisions[j];

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
    // Vyper pool contract: `assert i != j and i < N_COINS and j < N_COINS
    //                        # dev: coin index out of range`
    if i >= 3 || j >= 3 || i == j {
        return None;
    }
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
    // Checked: an oversized desired_output must yield None, not wrap.
    let dy_native =
        (desired_output.checked_mul(fee_denom)? + complement_est - U256::from(1)) / complement_est;

    let mut dy_internal = dy_native.checked_mul(precisions[j])?;
    if j > 0 {
        dy_internal = (dy_internal.checked_mul(price_scale[j - 1])? + wad - U256::from(1)) / wad;
    }
    dy_internal += U256::from(1);
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let (x_new, _) = get_y_3_ng(ann, gamma, xp_mod, d, i)?;

    // Second pass: recompute fee with actual xp_after
    let mut xp_after = xp_orig;
    xp_after[i] = x_new;
    xp_after[j] = y;
    let fee_actual = crypto_fee(&xp_after, mid_fee, out_fee, fee_gamma)?;
    let complement_actual = fee_denom - fee_actual;
    let dy_native = (desired_output.checked_mul(fee_denom)? + complement_actual - U256::from(1)) /
        complement_actual;
    let mut dy_internal = dy_native.checked_mul(precisions[j])?;
    if j > 0 {
        dy_internal = (dy_internal.checked_mul(price_scale[j - 1])? + wad - U256::from(1)) / wad;
    }
    dy_internal += U256::from(1);
    if xp_orig[j] <= dy_internal {
        return None;
    }
    let y = xp_orig[j] - dy_internal;
    let mut xp_mod = xp_orig;
    xp_mod[j] = y;
    let (x_new, _) = get_y_3_ng(ann, gamma, xp_mod, d, i)?;

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

    /// TricryptoUSDC-shaped pool: USDC (6 dec) / WBTC (8 dec) / WETH (18 dec).
    fn usdc_pool_amount_out(dx: U256) -> Option<U256> {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        get_amount_out(
            &[
                U256::from(7_000_000_000_000u64), // 7M USDC
                U256::from(6_000_000_000u64),     // 60 WBTC
                U256::from(1840u64) * wad,        // 1840 WETH
            ],
            &[U256::from(1_000_000_000_000u64), U256::from(10_000_000_000u64), U256::from(1u64)],
            &[U256::from(118_000u64) * wad, U256::from(3_800u64) * wad],
            U256::from(21u64) * U256::from(10u64).pow(U256::from(24u64)),
            U256::from(1707629u64) * U256::from(10_000u64),
            U256::from(11_809_167_828_997u64),
            U256::from(3_000_000u64),
            U256::from(30_000_000u64),
            U256::from(230_000_000_000_000u64),
            0,
            2,
            dx,
        )
    }

    #[test]
    fn get_amount_out_rejects_absurd_amount_in() {
        // Quote amount from the 2026-07-23 prod incident: drove the balances outside the
        // contract's safe domain and panicked the solver (division by zero in newton_y_3).
        let poison: U256 = "79274803880378691797489662351102500"
            .parse()
            .expect("literal");
        assert!(usdc_pool_amount_out(poison).is_none());
        assert!(usdc_pool_amount_out(U256::from(10u64).pow(U256::from(38u64))).is_none());
    }

    #[test]
    fn get_amount_out_rejects_amount_overflowing_xp() {
        assert!(usdc_pool_amount_out(U256::MAX).is_none());
    }

    fn usdc_pool_with_indices(i: usize, j: usize, forward: bool, amount: U256) -> Option<U256> {
        let wad = U256::from(1_000_000_000_000_000_000u128);
        let balances = [
            U256::from(7_000_000_000_000u64),
            U256::from(6_000_000_000u64),
            U256::from(1840u64) * wad,
        ];
        let precisions =
            [U256::from(1_000_000_000_000u64), U256::from(10_000_000_000u64), U256::from(1u64)];
        let price_scale = [U256::from(118_000u64) * wad, U256::from(3_800u64) * wad];
        let d = U256::from(21u64) * U256::from(10u64).pow(U256::from(24u64));
        let ann = U256::from(1707629u64) * U256::from(10_000u64);
        let gamma = U256::from(11_809_167_828_997u64);
        let (mid_fee, out_fee, fee_gamma) = (
            U256::from(3_000_000u64),
            U256::from(30_000_000u64),
            U256::from(230_000_000_000_000u64),
        );
        if forward {
            get_amount_out(
                &balances,
                &precisions,
                &price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
                i,
                j,
                amount,
            )
        } else {
            get_amount_in(
                &balances,
                &precisions,
                &price_scale,
                d,
                ann,
                gamma,
                mid_fee,
                out_fee,
                fee_gamma,
                i,
                j,
                amount,
            )
        }
    }

    #[test]
    fn rejects_out_of_range_coin_indices() {
        let dx = U256::from(1_000_000_000u64);
        for forward in [true, false] {
            assert!(usdc_pool_with_indices(3, 2, forward, dx).is_none());
            assert!(usdc_pool_with_indices(0, 3, forward, dx).is_none());
            assert!(usdc_pool_with_indices(7, 9, forward, dx).is_none());
            assert!(usdc_pool_with_indices(1, 1, forward, dx).is_none());
        }
    }

    #[test]
    fn get_amount_out_accepts_normal_amounts() {
        let dy = usdc_pool_amount_out(U256::from(1_000_000_000u64)).expect("1k USDC quote");
        assert!(dy > U256::ZERO);
    }

    #[test]
    fn roundtrip() {
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
            2,
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
            2,
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
            2,
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
            2,
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
            2,
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
            2,
        )
        .expect("price");
        assert!(!num.is_zero(), "numerator is zero");
        assert!(!den.is_zero(), "denominator is zero");
    }
}
