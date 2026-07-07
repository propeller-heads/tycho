//! Pool-level get_amount_out for StableSwapV1 (3pool, ren, sbtc, hbtc).
//!
//! -1 offset. Denorm FIRST, then fee.

use alloy_primitives::U256;

use crate::evm::protocol::curve::math::core::stableswap_v1::{
    get_d, get_y, get_y_d, A_PRECISION, FEE_DENOMINATOR, PRECISION,
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
    // -1 offset. Denorm FIRST, then fee.
    let dy = (xp[j] - y_new - U256::from(1)) * precision / rates[j];
    let fee_amount = fee * dy / U256::from(FEE_DENOMINATOR);
    let result = dy - fee_amount;
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
    // Reverse fee (round up)
    let fee_complement = fee_denom - fee;
    let dy = (desired_output * fee_denom + fee_complement - U256::from(1)) / fee_complement;
    // Reverse denorm
    let dy_internal = dy * rates[j] / precision;
    if xp[j] <= dy_internal + U256::from(1) {
        return None;
    }
    // -1 offset: forward was dy_internal = xp[j] - y_new - 1
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
    let numerator = num_xp
        .checked_mul(balances[j])?
        .checked_mul(fee_denom - fee)?;
    let denominator = den_xp
        .checked_mul(balances[i])?
        .checked_mul(fee_denom)?;
    Some((numerator, denominator))
}

/// Calculate the amount of coin `i` received when burning `token_amount` LP tokens.
///
/// Matches Curve's on-chain `calc_withdraw_one_coin` for StableSwapV1 (3pool).
///
/// Vyper: `_calc_withdraw_one_coin(_token_amount, i)` in StableSwap3Pool.vy
pub fn calc_withdraw_one_coin(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
    token_amount: U256,
    i: usize,
    total_supply: U256,
) -> Option<U256> {
    if token_amount.is_zero() || total_supply.is_zero() || i >= balances.len() {
        return None;
    }

    let precision = U256::from(PRECISION);
    let fee_denom = U256::from(FEE_DENOMINATOR);
    let n = balances.len();
    let n_u256 = U256::from(n);

    // Vyper: _fee = self.fee * N_COINS / (4 * (N_COINS - 1))
    let reduced_fee = fee
        .checked_mul(n_u256)?
        .checked_div(U256::from(4).checked_mul(U256::from(n - 1))?)?;

    // Vyper: xp = self._xp()
    let xp: Vec<U256> = balances
        .iter()
        .zip(rates.iter())
        .map(|(b, r)| *b * *r / precision)
        .collect();

    // Vyper: D0 = self.get_D(xp, amp)
    let d0 = get_d(&xp, amp)?;

    // Vyper: D1 = D0 - _token_amount * D0 / total_supply
    let d1 = d0.checked_sub(
        token_amount
            .checked_mul(d0)?
            .checked_div(total_supply)?,
    )?;

    // Vyper: new_y = self.get_y_D(amp, i, xp, D1)
    let new_y = get_y_d(i, &xp, d1, amp)?;

    // Vyper: xp_reduced = xp (copy), then apply imbalance fees
    let mut xp_reduced = xp.clone();
    for j in 0..n {
        // Vyper:
        //   if j == i: dx_expected = xp[j] * D1 / D0 - new_y
        //   else:      dx_expected = xp[j] - xp[j] * D1 / D0
        let dx_expected = if j == i {
            xp[j]
                .checked_mul(d1)?
                .checked_div(d0)?
                .checked_sub(new_y)?
        } else {
            xp[j].checked_sub(xp[j].checked_mul(d1)?.checked_div(d0)?)?
        };
        // Vyper: xp_reduced[j] -= _fee * dx_expected / FEE_DENOMINATOR
        xp_reduced[j] = xp_reduced[j].checked_sub(
            reduced_fee
                .checked_mul(dx_expected)?
                .checked_div(fee_denom)?,
        )?;
    }

    // Vyper: dy = xp_reduced[i] - self.get_y_D(amp, i, xp_reduced, D1)
    let new_y_reduced = get_y_d(i, &xp_reduced, d1, amp)?;
    let dy = xp_reduced[i].checked_sub(new_y_reduced)?;

    // Vyper: dy = (dy - 1) / precisions[i]
    // precisions[i] = PRECISION_MUL[i] = rates[i] / PRECISION
    // so dy / precisions[i] = dy * PRECISION / rates[i]
    let result = dy
        .checked_sub(U256::from(1))?
        .checked_mul(precision)?
        .checked_div(rates[i])?;

    if result.is_zero() {
        return None;
    }

    Some(result)
}

/// Calculate LP tokens minted when depositing `amounts` into the pool.
///
/// Matches Curve's on-chain `add_liquidity` mint calculation for StableSwapV1 (3pool).
///
/// Vyper: `add_liquidity(amounts, min_mint_amount)` in StableSwap3Pool.vy
pub fn calc_add_liquidity(
    balances: &[U256],
    rates: &[U256],
    amp: U256,
    fee: U256,
    amounts: &[U256],
    total_supply: U256,
) -> Option<U256> {
    if amounts.len() != balances.len() {
        return None;
    }
    if amounts.iter().all(|a| a.is_zero()) {
        return None;
    }

    let precision = U256::from(PRECISION);
    let fee_denom = U256::from(FEE_DENOMINATOR);
    let n = balances.len();
    let n_u256 = U256::from(n);

    // Vyper: _fee = self.fee * N_COINS / (4 * (N_COINS - 1))
    let reduced_fee = fee
        .checked_mul(n_u256)?
        .checked_div(U256::from(4).checked_mul(U256::from(n - 1))?)?;

    // Normalize helper
    let normalize = |bals: &[U256]| -> Vec<U256> {
        bals.iter()
            .zip(rates.iter())
            .map(|(b, r)| *b * *r / precision)
            .collect()
    };

    // Vyper: D0 = get_D_mem(old_balances, amp)
    let d0 = if total_supply > U256::ZERO { get_d(&normalize(balances), amp)? } else { U256::ZERO };

    // Vyper: new_balances = old_balances + amounts
    let new_balances: Vec<U256> = balances
        .iter()
        .zip(amounts.iter())
        .map(|(b, a)| *b + *a)
        .collect();

    // Vyper: D1 = get_D_mem(new_balances, amp)
    let d1 = get_d(&normalize(&new_balances), amp)?;
    if d1 <= d0 {
        return None;
    }

    // First deposit: mint_amount = D1
    if total_supply.is_zero() {
        return Some(d1);
    }

    // Vyper: Apply imbalance fees, compute D2
    let mut new_balances_after_fee = new_balances.clone();
    for idx in 0..n {
        // Vyper: ideal_balance = D1 * old_balances[i] / D0
        let ideal = d1
            .checked_mul(balances[idx])?
            .checked_div(d0)?;
        let difference = if ideal > new_balances[idx] {
            ideal - new_balances[idx]
        } else {
            new_balances[idx] - ideal
        };
        // Vyper: fees[i] = _fee * difference / FEE_DENOMINATOR
        let fee_i = reduced_fee
            .checked_mul(difference)?
            .checked_div(fee_denom)?;
        // Vyper: new_balances[i] -= fees[i]
        new_balances_after_fee[idx] = new_balances_after_fee[idx].checked_sub(fee_i)?;
    }

    // Vyper: D2 = get_D_mem(new_balances, amp)
    let d2 = get_d(&normalize(&new_balances_after_fee), amp)?;

    // Vyper: mint_amount = token_supply * (D2 - D0) / D0
    let mint_amount = total_supply
        .checked_mul(d2.checked_sub(d0)?)?
        .checked_div(d0)?;

    if mint_amount.is_zero() {
        return None;
    }

    Some(mint_amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let rate18 = U256::from(1_000_000_000_000_000_000u128);
        let balances = [
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
        ];
        let rates = [rate18, rate18, rate18];
        let amp = U256::from(2000u64);
        let fee = U256::from(1_000_000u64);
        let dx = U256::from(1_000_000_000_000_000_000_000u128);
        let dy = get_amount_out(&balances, &rates, amp, fee, 0, 1, dx).expect("out");
        let dx_recovered = get_amount_in(&balances, &rates, amp, fee, 0, 1, dy).expect("in");
        assert!(dx_recovered >= dx);
        assert!(dx_recovered <= dx + U256::from(2));
        let dy_check =
            get_amount_out(&balances, &rates, amp, fee, 0, 1, dx_recovered).expect("check");
        assert!(dy_check >= dy);
    }

    #[test]
    fn spot_price_balanced_near_one() {
        let rate18 = U256::from(1_000_000_000_000_000_000u128);
        let balances = [
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
        ];
        let rates = [rate18, rate18, rate18];
        let amp = U256::from(2000u64);
        let fee = U256::from(1_000_000u64);
        let (num, den) = spot_price(&balances, &rates, amp, fee, 0, 1).expect("price");
        let diff = if num > den { num - den } else { den - num };
        assert!(diff * U256::from(1000) < den, "spot price not near 1");
    }

    #[test]
    fn spot_price_symmetry() {
        let rate18 = U256::from(1_000_000_000_000_000_000u128);
        let balances = [
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
        ];
        let rates = [rate18, rate18, rate18];
        let amp = U256::from(2000u64);
        let fee = U256::from(1_000_000u64);
        let (num_ij, den_ij) = spot_price(&balances, &rates, amp, fee, 0, 1).expect("price_ij");
        let (num_ji, den_ji) = spot_price(&balances, &rates, amp, fee, 1, 0).expect("price_ji");
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
        let rate18 = U256::from(1_000_000_000_000_000_000u128);
        let balances = [
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
            U256::from(50_000_000_000_000_000_000_000_000u128),
        ];
        let rates = [rate18, rate18, rate18];
        let amp = U256::from(2000u64);
        let fee = U256::from(1_000_000u64);
        let dx = U256::from(1_000_000_000_000_000u128);
        let dy = get_amount_out(&balances, &rates, amp, fee, 0, 1, dx).expect("out");
        let (num, den) = spot_price(&balances, &rates, amp, fee, 0, 1).expect("price");
        let lhs = dy * den;
        let rhs = dx * num;
        let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
        assert!(diff * U256::from(100) < rhs, "spot price inconsistent with swap");
    }

    /// 3pool at block 24669924: on-chain calc_withdraw_one_coin(1e18, 1) = 1039789
    #[test]
    fn calc_withdraw_one_coin_matches_onchain() {
        // 3pool state at block 24669924
        let balances = [
            U256::from(63975337809806329031583135u128), // DAI (18 dec)
            U256::from(61219263170093u128),             // USDC (6 dec)
            U256::from(37832425459809u128),             // USDT (6 dec)
        ];
        let rates = [
            U256::from(1_000_000_000_000_000_000u128), // 1e18 (DAI 18 dec)
            U256::from(1_000_000_000_000_000_000_000_000_000_000u128), // 1e30 (USDC 6 dec)
            U256::from(1_000_000_000_000_000_000_000_000_000_000u128), // 1e30 (USDT 6 dec)
        ];
        let amp = U256::from(4000u64);
        let fee = U256::from(1500000u64);
        let total_supply = U256::from(156782246573983669718736356u128);

        let token_amount = U256::from(1_000_000_000_000_000_000u128); // 1 LP token
        let result =
            calc_withdraw_one_coin(&balances, &rates, amp, fee, token_amount, 1, total_supply)
                .expect("should succeed");

        // On-chain result: 1039789 (1.039789 USDC)
        assert_eq!(result, U256::from(1039789u64), "must match on-chain exactly");
    }

    #[test]
    fn calc_add_liquidity_and_withdraw_roundtrip() {
        let balances = [
            U256::from(63975337809806329031583135u128),
            U256::from(61219263170093u128),
            U256::from(37832425459809u128),
        ];
        let rates = [
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000_000_000_000_000u128),
        ];
        let amp = U256::from(4000u64);
        let fee = U256::from(1500000u64);
        let total_supply = U256::from(156782246573983669718736356u128);

        // Deposit 1000 USDC
        let deposit = U256::from(1_000_000_000u64); // 1000 USDC (6 dec)
        let amounts = [U256::ZERO, deposit, U256::ZERO];
        let lp_minted = calc_add_liquidity(&balances, &rates, amp, fee, &amounts, total_supply)
            .expect("add should succeed");

        // Withdraw the LP tokens as USDC
        let new_supply = total_supply + lp_minted;
        let withdrawn = calc_withdraw_one_coin(
            // Approximate: add deposit to balance (not exact but close enough for roundtrip check)
            &[balances[0], balances[1] + deposit, balances[2]],
            &rates,
            amp,
            fee,
            lp_minted,
            1,
            new_supply,
        )
        .expect("withdraw should succeed");

        // Should get back less than deposited (fees taken both ways)
        assert!(withdrawn < deposit, "should lose some to fees");
        // But not too much (< 1% loss for balanced pool)
        assert!(
            withdrawn > deposit * U256::from(99) / U256::from(100),
            "too much fee loss: deposited {deposit}, withdrew {withdrawn}"
        );
    }
}
