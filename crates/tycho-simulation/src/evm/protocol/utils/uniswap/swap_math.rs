use alloy::primitives::{aliases::U1024, I256, U256, U512};
use tycho_common::simulation::errors::SimulationError;

use super::sqrt_price_math;
use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_sub_u256, sqrt_u512},
    utils::solidity_math::{mul_div, mul_div_rounding_up},
};

pub(crate) fn compute_swap_step(
    sqrt_ratio_current: U256,
    sqrt_ratio_target: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> Result<(U256, U256, U256, U256), SimulationError> {
    let zero_for_one = sqrt_ratio_current >= sqrt_ratio_target;
    let exact_in = amount_remaining >= I256::from_raw(U256::from(0u64));
    let sqrt_ratio_next: U256;
    let mut amount_in = U256::from(0u64);
    let mut amount_out = U256::from(0u64);

    if exact_in {
        let amount_remaining_less_fee = mul_div(
            amount_remaining.into_raw(),
            U256::from(1_000_000 - fee_pips),
            U256::from(1_000_000),
        )?;
        amount_in = if zero_for_one {
            sqrt_price_math::get_amount0_delta(
                sqrt_ratio_target,
                sqrt_ratio_current,
                liquidity,
                true,
            )?
        } else {
            sqrt_price_math::get_amount1_delta(
                sqrt_ratio_current,
                sqrt_ratio_target,
                liquidity,
                true,
            )?
        };
        if amount_remaining_less_fee >= amount_in {
            sqrt_ratio_next = sqrt_ratio_target
        } else {
            sqrt_ratio_next = sqrt_price_math::get_next_sqrt_price_from_input(
                sqrt_ratio_current,
                liquidity,
                amount_remaining_less_fee,
                zero_for_one,
            )?
        }
    } else {
        amount_out = if zero_for_one {
            sqrt_price_math::get_amount1_delta(
                sqrt_ratio_target,
                sqrt_ratio_current,
                liquidity,
                false,
            )?
        } else {
            sqrt_price_math::get_amount0_delta(
                sqrt_ratio_current,
                sqrt_ratio_target,
                liquidity,
                false,
            )?
        };
        if amount_remaining.abs().into_raw() > amount_out {
            sqrt_ratio_next = sqrt_ratio_target;
        } else {
            sqrt_ratio_next = sqrt_price_math::get_next_sqrt_price_from_output(
                sqrt_ratio_current,
                liquidity,
                amount_remaining.abs().into_raw(),
                zero_for_one,
            )?;
        }
    }

    let max = sqrt_ratio_target == sqrt_ratio_next;

    if zero_for_one {
        amount_in = if max && exact_in {
            amount_in
        } else {
            sqrt_price_math::get_amount0_delta(
                sqrt_ratio_next,
                sqrt_ratio_current,
                liquidity,
                true,
            )?
        };
        amount_out = if max && !exact_in {
            amount_out
        } else {
            sqrt_price_math::get_amount1_delta(
                sqrt_ratio_next,
                sqrt_ratio_current,
                liquidity,
                false,
            )?
        }
    } else {
        amount_in = if max && exact_in {
            amount_in
        } else {
            sqrt_price_math::get_amount1_delta(
                sqrt_ratio_current,
                sqrt_ratio_next,
                liquidity,
                true,
            )?
        };
        amount_out = if max && !exact_in {
            amount_out
        } else {
            sqrt_price_math::get_amount0_delta(
                sqrt_ratio_current,
                sqrt_ratio_next,
                liquidity,
                false,
            )?
        };
    }

    if !exact_in && amount_out > amount_remaining.abs().into_raw() {
        amount_out = amount_remaining.abs().into_raw();
    }

    let fee_amount = if exact_in && sqrt_ratio_next != sqrt_ratio_target {
        safe_sub_u256(amount_remaining.abs().into_raw(), amount_in)?
    } else {
        mul_div_rounding_up(amount_in, U256::from(fee_pips), U256::from(1_000_000 - fee_pips))?
    };
    Ok((sqrt_ratio_next, amount_in, amount_out, fee_amount))
}

/// Computes the gross input (fee included) and output amounts for moving the price across
/// part of a constant-liquidity range.
///
/// # Arguments
/// * `sqrt_ratio_start` - Sqrt price at the start of the move, in Q96 format.
/// * `sqrt_ratio_end` - Sqrt price at the end of the move, in Q96 format.
/// * `liquidity` - The active liquidity of the range.
/// * `fee_pips` - Total swap fee in pips (1/1_000_000).
/// * `zero_for_one` - Swap direction; `true` when token0 is sold for token1.
///
/// # Returns
/// `(amount_in, amount_out)` where `amount_in` is gross of fees (rounded up, like the
/// real swap charges) and `amount_out` is rounded down.
///
/// # Errors
/// * `SimulationError::FatalError` - Arithmetic overflow.
pub(crate) fn trade_price_range_amounts(
    sqrt_ratio_start: U256,
    sqrt_ratio_end: U256,
    liquidity: u128,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<(U256, U256), SimulationError> {
    let (net_amount_in, amount_out) = if zero_for_one {
        (
            sqrt_price_math::get_amount0_delta(sqrt_ratio_end, sqrt_ratio_start, liquidity, true)?,
            sqrt_price_math::get_amount1_delta(sqrt_ratio_end, sqrt_ratio_start, liquidity, false)?,
        )
    } else {
        (
            sqrt_price_math::get_amount1_delta(sqrt_ratio_end, sqrt_ratio_start, liquidity, true)?,
            sqrt_price_math::get_amount0_delta(sqrt_ratio_end, sqrt_ratio_start, liquidity, false)?,
        )
    };
    let fee_amount =
        mul_div_rounding_up(net_amount_in, U256::from(fee_pips), U256::from(1_000_000 - fee_pips))?;
    let amount_in = safe_add_u256(net_amount_in, fee_amount)?;
    Ok((amount_in, amount_out))
}

/// Solves for the sqrt price within a constant-liquidity range at which the cumulative
/// trade price (`total_out / total_in` including this range's contribution) reaches a
/// limit price.
///
/// # Algorithm
///
/// Within a range of liquidity `L` starting at sqrt price `s0` (Q96), with
/// `g = 1_000_000 − fee_pips` and `F = 1_000_000`, the partial amounts as a function of
/// the final sqrt price `s` are (zero-for-one):
/// ```text,no_run
/// in(s)  = L·Q96·(s0−s)/(s·s0) · F/g      (gross of fee)
/// out(s) = L·(s0−s)/Q96
/// ```
///
/// Setting `(O + out(s)) / (I + in(s)) = limit_num/limit_den` — where `I`/`O` are the
/// amounts accumulated over previously consumed ranges — and clearing denominators yields
/// the quadratic `a·s² − b·s + c = 0` with
/// ```text,no_run
/// a = limit_den·L·g·s0
/// b = g·Q96·s0·(limit_den·O − limit_num·I) + a·s0 + c/s0
/// c = limit_num·L·Q192·F·s0
/// ```
///
/// At `I = O = 0` the equation factors as `(s−s0)·(a·s − c/s0) = 0`: the trivial root `s0`
/// is the zero-size trade and the non-trivial root lies below `s0` exactly when the limit
/// is below the fee-adjusted spot price, so the relevant root is the smaller one,
/// `s* = (b − √(b²−4ac)) / 2a`. One-for-zero mirrors the derivation with `limit_num` and
/// `limit_den` swapping places in `a` and `c`; the relevant root there is the larger one.
///
/// With zero accumulated amounts (the first range of every walk) the factored form is
/// solved directly with a single U512 division; otherwise the full quadratic is solved in
/// [`compute_trade_price_quadratic_root`]. In both cases the root is rounded toward `s0`
/// (a smaller swap can only improve the trade price), clamped into the range, and finally
/// a guard loop verifies the achieved cumulative trade price against the limit using the
/// exact integer amounts, stepping further toward `s0` if rounding pushed it across. The
/// ≥-limit guarantee therefore holds by construction rather than by error analysis.
///
/// # Arguments
/// * `sqrt_ratio_current` - Sqrt price at the start of the range, in Q96 format.
/// * `sqrt_ratio_target` - Range boundary the price may not cross, in Q96 format.
/// * `liquidity` - The active liquidity of the range.
/// * `total_amount_in` - Gross input accumulated over previously consumed ranges.
/// * `total_amount_out` - Output accumulated over previously consumed ranges.
/// * `limit_num` - Limit trade price numerator (`token_out`, raw atomic units).
/// * `limit_den` - Limit trade price denominator (`token_in`, raw atomic units).
/// * `fee_pips` - Total swap fee in pips (1/1_000_000).
///
/// # Returns
/// `(sqrt_ratio_next, amount_in, amount_out)` — the stopping sqrt price and this range's
/// partial contribution (gross input, output).
///
/// # Errors
/// * `SimulationError::FatalError` - Arithmetic overflow, or the accumulated amounts already
///   violate the limit on entry (broken caller invariant).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_swap_step_to_trade_price(
    sqrt_ratio_current: U256,
    sqrt_ratio_target: U256,
    liquidity: u128,
    total_amount_in: U256,
    total_amount_out: U256,
    limit_num: U256,
    limit_den: U256,
    fee_pips: u32,
) -> Result<(U256, U256, U256), SimulationError> {
    let zero_for_one = sqrt_ratio_current >= sqrt_ratio_target;

    if liquidity == 0 {
        return Ok((sqrt_ratio_current, U256::ZERO, U256::ZERO));
    }

    let root = if total_amount_in.is_zero() && total_amount_out.is_zero() {
        // With nothing accumulated the quadratic factors as (s−s0)·(a·s − c/s0) = 0 and
        // the non-trivial root reduces to a single division:
        //   zero_for_one:  s* = limit_num·F·Q192 / (limit_den·g·s0)
        //   one_for_zero:  s* = limit_den·g·Q192 / (limit_num·F·s0)
        // Numerators stay under 2^(256+20+192) and denominators under 2^(256+20+160), so
        // plain U512 arithmetic cannot overflow.
        let fee_complement = U512::from(1_000_000 - fee_pips);
        let fee_precision = U512::from(1_000_000u32);
        let q192 = U512::ONE << 192;
        let (numerator, denominator) = if zero_for_one {
            (
                U512::from(limit_num) * fee_precision * q192,
                U512::from(limit_den) * fee_complement * U512::from(sqrt_ratio_current),
            )
        } else {
            (
                U512::from(limit_den) * fee_complement * q192,
                U512::from(limit_num) * fee_precision * U512::from(sqrt_ratio_current),
            )
        };
        if denominator == U512::ZERO {
            return Err(trade_price_overflow());
        }
        if zero_for_one {
            // Rounded up: stopping earlier can only improve the trade price
            (numerator + denominator - U512::ONE) / denominator
        } else {
            // Rounded down by the division for the same reason
            numerator / denominator
        }
    } else {
        compute_trade_price_quadratic_root(
            sqrt_ratio_current,
            liquidity,
            total_amount_in,
            total_amount_out,
            limit_num,
            limit_den,
            fee_pips,
            zero_for_one,
        )?
    };

    let (lower_bound, upper_bound) = if zero_for_one {
        (sqrt_ratio_target, sqrt_ratio_current)
    } else {
        (sqrt_ratio_current, sqrt_ratio_target)
    };
    let clamped_root = root.clamp(U512::from(lower_bound), U512::from(upper_bound));
    let root_limbs = clamped_root.as_limbs();
    let mut sqrt_ratio_next =
        U256::from_limbs([root_limbs[0], root_limbs[1], root_limbs[2], root_limbs[3]]);

    // Guard loop: verify with exact integer amounts, shrinking the swap until the limit
    // is satisfied. The continuous root can fall short because the integer amounts round
    // against the trader (input up, output down); since the cumulative trade price moves
    // proportionally to the sqrt price, the deficit translates directly into a sqrt price
    // correction of about `s·deficit/required`. Doubling the previous step on top makes
    // termination exponential in the worst case, and the zero-size step at the range
    // start preserves the caller's invariant, so the loop always terminates.
    let mut last_step = U256::ZERO;
    loop {
        let (amount_in, amount_out) = trade_price_range_amounts(
            sqrt_ratio_current,
            sqrt_ratio_next,
            liquidity,
            fee_pips,
            zero_for_one,
        )?;
        let achieved_out = U512::from(total_amount_out) + U512::from(amount_out);
        let required_in = U512::from(total_amount_in) + U512::from(amount_in);
        // checked: a 257-bit amount sum times a full-width limit exceeds U512, and this
        // function's contract allows both
        let achieved_side = achieved_out
            .checked_mul(U512::from(limit_den))
            .ok_or_else(trade_price_overflow)?;
        let required_side = required_in
            .checked_mul(U512::from(limit_num))
            .ok_or_else(trade_price_overflow)?;
        if achieved_side >= required_side {
            return Ok((sqrt_ratio_next, amount_in, amount_out));
        }
        if sqrt_ratio_next == sqrt_ratio_current {
            return Err(SimulationError::FatalError(
                "Trade price guard could not satisfy the limit".to_string(),
            ));
        }
        let deficit_ratio = required_side / (required_side - achieved_side);
        let proportional = U512::from(sqrt_ratio_next) / deficit_ratio;
        let proportional_limbs = proportional.as_limbs();
        let step = U256::from_limbs([
            proportional_limbs[0],
            proportional_limbs[1],
            proportional_limbs[2],
            proportional_limbs[3],
        ])
        .max(last_step << 1)
        .max(U256::ONE);
        sqrt_ratio_next = if zero_for_one {
            sqrt_ratio_current.min(safe_add_u256(sqrt_ratio_next, step)?)
        } else {
            sqrt_ratio_current.max(sqrt_ratio_next.saturating_sub(step))
        };
        last_step = step;
    }
}

/// Solves the full trade price quadratic for a range entered with accumulated amounts.
///
/// Coefficient bounds: limit values ≤ 2^256, liquidity ≤ 2^128, fee terms ≤ 2^20, sqrt
/// prices ≤ 2^160, accumulated amounts ≤ 2^256 — every term stays under 2^800, so plain
/// U1024 arithmetic cannot overflow. A shared right-shift then brings the coefficients
/// into U512 (preserving the roots) so the discriminant fits.
#[allow(clippy::too_many_arguments)]
fn compute_trade_price_quadratic_root(
    sqrt_ratio_current: U256,
    liquidity: u128,
    total_amount_in: U256,
    total_amount_out: U256,
    limit_num: U256,
    limit_den: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<U512, SimulationError> {
    let fee_complement = U1024::from(1_000_000 - fee_pips);
    let fee_precision = U1024::from(1_000_000u32);
    let liquidity_wide = U1024::from(liquidity);
    let sqrt_price_start = U1024::from(sqrt_ratio_current);
    let q96 = U1024::ONE << 96;
    let q192 = U1024::ONE << 192;

    // Non-negative while the running trade price still beats the limit; the caller only
    // invokes this solver under that invariant.
    let amount_imbalance = (U1024::from(limit_den) * U1024::from(total_amount_out))
        .checked_sub(U1024::from(limit_num) * U1024::from(total_amount_in))
        .ok_or_else(|| {
            SimulationError::FatalError("Accumulated trade price is below the limit".to_string())
        })?;

    let (coeff_a, coeff_c_over_s0) = if zero_for_one {
        (
            U1024::from(limit_den) * liquidity_wide * fee_complement * sqrt_price_start,
            U1024::from(limit_num) * liquidity_wide * q192 * fee_precision,
        )
    } else {
        (
            U1024::from(limit_num) * liquidity_wide * fee_precision * sqrt_price_start,
            U1024::from(limit_den) * liquidity_wide * fee_complement * q192,
        )
    };
    let coeff_b: U1024 = fee_complement * q96 * sqrt_price_start * amount_imbalance +
        coeff_a * sqrt_price_start +
        coeff_c_over_s0;
    let coeff_c = coeff_c_over_s0 * sqrt_price_start;

    // Shared right-shift keeps the roots intact while making b² fit in U512
    let shift = coeff_b.bit_len().saturating_sub(254);
    let a_shifted = u1024_to_u512(coeff_a >> shift)?;
    let b_shifted = u1024_to_u512(coeff_b >> shift)?;
    let c_shifted = u1024_to_u512(coeff_c >> shift)?;

    // b² ≤ 2^508 and 4ac ≤ b² by the shift choice, so neither product can overflow U512
    let b_squared = b_shifted * b_shifted;
    let four_a_c = U512::from(4u8) * a_shifted * c_shifted;
    // Truncation in the shared shift can push 4ac marginally above b²; the guard loop
    // in the caller corrects the resulting off-by-a-few root.
    let sqrt_discriminant = sqrt_u512(b_squared.saturating_sub(four_a_c));

    let two_a = a_shifted * U512::from(2u8);
    let root = if two_a == U512::ZERO {
        // Liquidity is negligible relative to the accumulated amounts: the quadratic
        // degenerates to the linear equation b·s = c
        c_shifted / b_shifted
    } else if zero_for_one {
        // Smaller root, rounded up: stopping earlier can only improve the trade price
        let numerator = b_shifted - sqrt_discriminant;
        (numerator + two_a - U512::ONE) / two_a
    } else {
        // Larger root, rounded down by the division for the same reason
        (b_shifted + sqrt_discriminant) / two_a
    };
    Ok(root)
}

fn trade_price_overflow() -> SimulationError {
    SimulationError::FatalError("Overflow in trade price computation".to_string())
}

fn u1024_to_u512(value: U1024) -> Result<U512, SimulationError> {
    let limbs = value.as_limbs();
    if limbs[8..].iter().any(|limb| *limb != 0) {
        return Err(trade_price_overflow());
    }
    let mut lower = [0u64; 8];
    lower.copy_from_slice(&limbs[..8]);
    Ok(U512::from_limbs(lower))
}

#[cfg(test)]
mod tests {
    use std::{ops::Neg, str::FromStr};

    use super::*;

    struct TestCase {
        price: U256,
        target: U256,
        liquidity: u128,
        remaining: I256,
        fee: u32,
        exp: (U256, U256, U256, U256),
    }

    #[test]
    fn test_compute_swap_step() {
        let cases = vec![
            TestCase {
                price: U256::from_str("1917240610156820439288675683655550").unwrap(),
                target: U256::from_str("1919023616462402511535565081385034").unwrap(),
                liquidity: 23130341825817804069u128,
                remaining: I256::exp10(18),
                fee: 500,
                exp: (
                    U256::from_str("1917244033735642980420262835667387").unwrap(),
                    U256::from_str("999500000000000000").unwrap(),
                    U256::from_str("1706820897").unwrap(),
                    U256::from_str("500000000000000").unwrap(),
                ),
            },
            TestCase {
                price: U256::from_str("1917240610156820439288675683655550").unwrap(),
                target: U256::from_str("1919023616462402511535565081385034").unwrap(),
                liquidity: 23130341825817804069u128,
                remaining: I256::exp10(18).neg(),
                fee: 500,
                exp: (
                    U256::from_str("1919023616462402511535565081385034").unwrap(),
                    U256::from_str("520541484453545253034").unwrap(),
                    U256::from_str("888091216672").unwrap(),
                    U256::from_str("260400942698121688").unwrap(),
                ),
            },
            TestCase {
                price: U256::from_str("1917240610156820439288675683655550").unwrap(),
                target: U256::from_str("1908498483466244238266951834509291").unwrap(),
                liquidity: 23130341825817804069u128,
                remaining: I256::exp10(18).neg(),
                fee: 500,
                exp: (
                    U256::from_str("1917237184865352164019453920762266").unwrap(),
                    U256::from_str("1707680836").unwrap(),
                    U256::from_str("1000000000000000000").unwrap(),
                    U256::from_str("854268").unwrap(),
                ),
            },
            TestCase {
                price: U256::from_str("1917240610156820439288675683655550").unwrap(),
                target: U256::from_str("1908498483466244238266951834509291").unwrap(),
                liquidity: 23130341825817804069u128,
                remaining: I256::exp10(18),
                fee: 500,
                exp: (
                    U256::from_str("1908498483466244238266951834509291").unwrap(),
                    U256::from_str("4378348149175").unwrap(),
                    U256::from_str("2552228553845698906796").unwrap(),
                    U256::from_str("2190269210").unwrap(),
                ),
            },
            TestCase {
                price: U256::from_str("1917240610156820439288675683655550").unwrap(),
                target: U256::from_str("1908498483466244238266951834509291").unwrap(),
                liquidity: 0u128,
                remaining: I256::exp10(18),
                fee: 500,
                exp: (
                    U256::from_str("1908498483466244238266951834509291").unwrap(),
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                ),
            },
        ];

        for case in cases {
            let res = compute_swap_step(
                case.price,
                case.target,
                case.liquidity,
                case.remaining,
                case.fee,
            )
            .unwrap();

            assert_eq!(res, case.exp);
        }
    }

    /// Asserts the cumulative trade price at `(amount_in, amount_out)` satisfies the limit.
    fn assert_limit_satisfied(
        total_in: U256,
        total_out: U256,
        amount_in: U256,
        amount_out: U256,
        limit_num: U256,
        limit_den: U256,
    ) {
        let achieved = (U512::from(total_out) + U512::from(amount_out)) * U512::from(limit_den);
        let required = (U512::from(total_in) + U512::from(amount_in)) * U512::from(limit_num);
        assert!(achieved >= required, "Cumulative trade price below the limit");
    }

    #[test]
    fn test_compute_swap_step_to_trade_price_zero_for_one() {
        let sqrt_price = U256::from_str("1917240610156820439288675683655550").unwrap();
        let liquidity = 23130341825817804069u128;
        let fee = 500u32;

        // Limit 0.1% below the effective spot trade price g·s0²/(F·Q192)
        let limit_num = sqrt_price * sqrt_price * U256::from(1_000_000 - fee) * U256::from(999u32);
        let limit_den = (U256::from(1u8) << 192) * U256::from(1_000_000u32) * U256::from(1000u32);

        let (sqrt_price_next, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            sqrt_price / U256::from(2u8),
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();

        assert!(sqrt_price_next < sqrt_price, "Price should move down for zero_for_one");
        assert!(sqrt_price_next > sqrt_price / U256::from(2u8), "Root should be inside the range");
        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
        assert_limit_satisfied(U256::ZERO, U256::ZERO, amount_in, amount_out, limit_num, limit_den);

        // Maximality: stopping noticeably deeper in the range must violate the limit
        let gap = (sqrt_price - sqrt_price_next) / U256::from(100u8) + U256::from(16u8);
        let (deeper_in, deeper_out) =
            trade_price_range_amounts(sqrt_price, sqrt_price_next - gap, liquidity, fee, true)
                .unwrap();
        let achieved = U512::from(deeper_out) * U512::from(limit_den);
        let required = U512::from(deeper_in) * U512::from(limit_num);
        assert!(achieved < required, "A deeper stop should violate the limit");
    }

    #[test]
    fn test_compute_swap_step_to_trade_price_one_for_zero() {
        let sqrt_price = U256::from_str("1917240610156820439288675683655550").unwrap();
        let liquidity = 23130341825817804069u128;
        let fee = 500u32;

        // Limit 0.1% below the effective spot trade price g·Q192/(F·s0²)
        let limit_num = (U256::from(1u8) << 192) * U256::from(1_000_000 - fee) * U256::from(999u32);
        let limit_den = sqrt_price * sqrt_price * U256::from(1_000_000u32) * U256::from(1000u32);

        let (sqrt_price_next, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            sqrt_price * U256::from(2u8),
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();

        assert!(sqrt_price_next > sqrt_price, "Price should move up for one_for_zero");
        assert!(sqrt_price_next < sqrt_price * U256::from(2u8), "Root should be inside the range");
        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
        assert_limit_satisfied(U256::ZERO, U256::ZERO, amount_in, amount_out, limit_num, limit_den);

        let gap = (sqrt_price_next - sqrt_price) / U256::from(100u8) + U256::from(16u8);
        let (deeper_in, deeper_out) =
            trade_price_range_amounts(sqrt_price, sqrt_price_next + gap, liquidity, fee, false)
                .unwrap();
        let achieved = U512::from(deeper_out) * U512::from(limit_den);
        let required = U512::from(deeper_in) * U512::from(limit_num);
        assert!(achieved < required, "A deeper stop should violate the limit");
    }

    #[test]
    fn test_compute_swap_step_to_trade_price_with_accumulated_amounts() {
        let sqrt_price = U256::from_str("1917240610156820439288675683655550").unwrap();
        let liquidity = 23130341825817804069u128;
        let fee = 500u32;

        let limit_num = sqrt_price * sqrt_price * U256::from(1_000_000 - fee) * U256::from(995u32);
        let limit_den = (U256::from(1u8) << 192) * U256::from(1_000_000u32) * U256::from(1000u32);

        // Accumulated amounts whose running trade price is 0.2% better than the limit
        let total_in = U256::from(10u8).pow(U256::from(18u8));
        let total_out = mul_div(total_in, limit_num, limit_den).unwrap() * U256::from(1002u32) /
            U256::from(1000u32);

        let (sqrt_price_next, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            sqrt_price / U256::from(2u8),
            liquidity,
            total_in,
            total_out,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();

        assert!(sqrt_price_next < sqrt_price);
        assert!(amount_in > U256::ZERO, "Better-than-limit history should allow a further swap");
        assert_limit_satisfied(total_in, total_out, amount_in, amount_out, limit_num, limit_den);
    }

    #[test]
    fn test_compute_swap_step_to_trade_price_zero_liquidity() {
        let sqrt_price = U256::from_str("1917240610156820439288675683655550").unwrap();

        let (sqrt_price_next, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            sqrt_price / U256::from(2u8),
            0,
            U256::ZERO,
            U256::ZERO,
            U256::from(1u8),
            U256::from(1u8),
            500,
        )
        .unwrap();

        assert_eq!(sqrt_price_next, sqrt_price);
        assert_eq!(amount_in, U256::ZERO);
        assert_eq!(amount_out, U256::ZERO);
    }
    // ── Tests ported from tycho-simulation PR 494 ────────────────────────────────────────
    // The PR's solver takes a fee-adjusted "formula" price and returns net amounts with the
    // fee separate; this solver takes the user price (gross out/in) plus fee_pips and
    // returns gross input, so the ports pass the PR's user-level targets directly. Three
    // contract divergences are noted inline: zero liquidity yields a zero step instead of
    // an error, entering a range with a cumulative price already below the limit is a
    // broken invariant here (the walk stops before that can happen) instead of a solvable
    // scenario, and conservative rounding means the achieved price never drops BELOW the
    // limit (the PR rounds to nearest and asserts the opposite direction).

    fn pr494_assert_limit_satisfied(
        total_in: U256,
        total_out: U256,
        amount_in: U256,
        amount_out: U256,
        limit_num: U256,
        limit_den: U256,
    ) {
        let achieved = (U512::from(total_out) + U512::from(amount_out)) * U512::from(limit_den);
        let required = (U512::from(total_in) + U512::from(amount_in)) * U512::from(limit_num);
        assert!(achieved >= required, "Cumulative trade price must not drop below the limit");
    }

    fn pr494_sqrt_price() -> U256 {
        U256::from_str("112045541949572287496682733568").unwrap()
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_basic() {
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        let fee = 3000u32;
        // User target 1.9, worse than spot ~2.0
        let limit_num = U256::from(19u64);
        let limit_den = U256::from(10u64);
        let boundary = U256::from_str("79228162514264337593543950336").unwrap();

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();

        assert!(sqrt_price_new < sqrt_price, "Price should decrease for zero_for_one");
        assert!(sqrt_price_new > boundary, "Should not hit the boundary");
        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
        pr494_assert_limit_satisfied(
            U256::ZERO,
            U256::ZERO,
            amount_in,
            amount_out,
            limit_num,
            limit_den,
        );

        // Cross-validate with compute_swap_step as an independent oracle: its net input
        // plus fee must equal this solver's gross input
        let large_amount = I256::checked_from_sign_and_abs(
            alloy::primitives::Sign::Positive,
            U256::from(u128::MAX),
        )
        .unwrap();
        let (step_sqrt, step_in, step_out, step_fee) =
            compute_swap_step(sqrt_price, sqrt_price_new, liquidity, large_amount, fee).unwrap();
        assert_eq!(step_sqrt, sqrt_price_new, "compute_swap_step should arrive at the same price");
        assert_eq!(step_in + step_fee, amount_in, "gross amount_in should match");
        assert_eq!(step_out, amount_out, "amount_out should match");
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_hits_tick_boundary() {
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        // Target 0.5, much worse than spot: requires crossing the close boundary
        let limit_num = U256::from(5u64);
        let limit_den = U256::from(10u64);
        let boundary = U256::from_str("111000000000000000000000000000").unwrap();

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            3000,
        )
        .unwrap();

        assert_eq!(sqrt_price_new, boundary, "Should stop at the tick boundary");
        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_target_already_achieved() {
        let sqrt_price = pr494_sqrt_price();
        // Target 2.5, better than spot ~2.0: only the zero-size trade satisfies it
        let limit_num = U256::from(25u64);
        let limit_den = U256::from(10u64);
        let boundary = U256::from_str("79228162514264337593543950336").unwrap();

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            1_000_000_000_000_000_000u128,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            3000,
        )
        .unwrap();

        assert_eq!(sqrt_price_new, sqrt_price, "Price should not change");
        assert_eq!(amount_in, U256::ZERO, "No input needed");
        assert_eq!(amount_out, U256::ZERO, "No output");
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_one_for_zero() {
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        let fee = 3000u32;
        // Target 0.485, slightly worse than the effective spot ~0.497
        let limit_num = U256::from(485u64);
        let limit_den = U256::from(1000u64);
        let boundary = U256::from_str("150000000000000000000000000000").unwrap();

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();

        assert!(sqrt_price_new > sqrt_price, "Price should increase for one_for_zero");
        assert!(sqrt_price_new < boundary, "Should not hit the boundary");
        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
        pr494_assert_limit_satisfied(
            U256::ZERO,
            U256::ZERO,
            amount_in,
            amount_out,
            limit_num,
            limit_den,
        );

        let large_amount = I256::checked_from_sign_and_abs(
            alloy::primitives::Sign::Positive,
            U256::from(u128::MAX),
        )
        .unwrap();
        let (step_sqrt, step_in, step_out, step_fee) =
            compute_swap_step(sqrt_price, sqrt_price_new, liquidity, large_amount, fee).unwrap();
        assert_eq!(step_sqrt, sqrt_price_new, "compute_swap_step should arrive at the same price");
        assert_eq!(step_in + step_fee, amount_in, "gross amount_in should match");
        assert_eq!(step_out, amount_out, "amount_out should match");
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_zero_liquidity() {
        // Divergence from PR 494 (which errors here): zero liquidity contributes a zero
        // step so the caller's walk can continue into later ranges
        let sqrt_price = pr494_sqrt_price();
        let boundary = U256::from_str("79228162514264337593543950336").unwrap();

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            0,
            U256::ZERO,
            U256::ZERO,
            U256::from(19u64),
            U256::from(10u64),
            3000,
        )
        .unwrap();

        assert_eq!(sqrt_price_new, sqrt_price);
        assert_eq!(amount_in, U256::ZERO);
        assert_eq!(amount_out, U256::ZERO);
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_at_tick_boundary() {
        let sqrt_price = pr494_sqrt_price();
        let boundary = U256::from_str("97000000000000000000000000000").unwrap();
        // Target 1.5: lands near the boundary
        let limit_num = U256::from(15u64);
        let limit_den = U256::from(10u64);

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            1_000_000_000_000_000_000u128,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            3000,
        )
        .unwrap();

        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
        assert!(sqrt_price_new < sqrt_price, "Price should decrease for zero_for_one");
        assert!(sqrt_price_new >= boundary, "New price should be at or above the boundary");
    }

    #[test]
    fn pr494_compute_swap_to_trade_price_target_equals_boundary() {
        let sqrt_price = pr494_sqrt_price();
        let boundary = U256::from_str("101400000000000000000000000000").unwrap();
        // Target 1.8: solution lands close to this boundary
        let limit_num = U256::from(18u64);
        let limit_den = U256::from(10u64);

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            1_000_000_000_000_000_000u128,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            3000,
        )
        .unwrap();

        assert!(amount_in > U256::ZERO, "Should have swapped some amount");
        assert!(amount_out > U256::ZERO, "Should have output");
        assert!(sqrt_price_new >= boundary, "New price should be at or above the boundary");
    }

    #[test]
    fn pr494_accumulated_zero_gives_same_result() {
        let sqrt_price = pr494_sqrt_price();
        let boundary = U256::from_str("79228162514264337593543950336").unwrap();

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            1_000_000_000_000_000_000u128,
            U256::ZERO,
            U256::ZERO,
            U256::from(19u64),
            U256::from(10u64),
            3000,
        )
        .unwrap();

        assert!(sqrt_price_new > boundary, "Should not hit the boundary");
        assert!(amount_in > U256::ZERO, "amount_in should be positive");
        assert!(amount_out > U256::ZERO, "amount_out should be positive");
    }

    #[test]
    fn pr494_accumulated_zero_for_one_basic() {
        // Divergence from PR 494: its solver targets price EQUALITY and can pull a
        // cumulative ratio of 1.9 UP toward a 1.95 target. Here the limit is a floor the
        // walk never crosses, so entering a range with the cumulative price already below
        // the limit is a broken invariant and must fail.
        let result = compute_swap_step_to_trade_price(
            pr494_sqrt_price(),
            U256::from_str("79228162514264337593543950336").unwrap(),
            1_000_000_000_000_000_000u128,
            U256::from(100_000u128),
            U256::from(190_000u128),
            U256::from(195u64),
            U256::from(100u64),
            3000,
        );

        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn pr494_accumulated_unreachable_target_falls_back_to_boundary() {
        // Divergence from PR 494: cumulative ratio 1.7 is below the 1.9 limit, which this
        // solver's caller invariant forbids (see pr494_accumulated_zero_for_one_basic)
        let result = compute_swap_step_to_trade_price(
            pr494_sqrt_price(),
            U256::from_str("100000000000000000000000000000").unwrap(),
            1_000_000_000_000_000_000u128,
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_700_000_000_000_000_000u128),
            U256::from(19u64),
            U256::from(10u64),
            3000,
        );

        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn pr494_accumulated_one_for_zero_basic() {
        // Divergence from PR 494: cumulative ratio 0.48 is below the 0.49 limit, which
        // this solver's caller invariant forbids (see pr494_accumulated_zero_for_one_basic)
        let result = compute_swap_step_to_trade_price(
            pr494_sqrt_price(),
            U256::from_str("150000000000000000000000000000").unwrap(),
            1_000_000_000_000_000_000u128,
            U256::from(100_000u128),
            U256::from(48_000u128),
            U256::from(49u64),
            U256::from(100u64),
            3000,
        );

        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn pr494_accumulated_hits_tick_boundary() {
        let sqrt_price = pr494_sqrt_price();
        let boundary = U256::from_str("111000000000000000000000000000").unwrap();
        // Cumulative ratio 1.8 with an aggressive 0.5 limit: the boundary is hit first
        let limit_num = U256::from(5u64);
        let limit_den = U256::from(10u64);

        let (sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            1_000_000_000_000_000_000u128,
            U256::from(500_000_000_000_000_000u128),
            U256::from(900_000_000_000_000_000u128),
            limit_num,
            limit_den,
            3000,
        )
        .unwrap();

        assert_eq!(sqrt_price_new, boundary, "Should stop at the tick boundary");
        assert!(amount_in > U256::ZERO);
        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn pr494_accumulated_target_unreachable_due_to_worse_cumulative() {
        // Divergence from PR 494: cumulative ratio 1.3 is below the 1.5 limit, which this
        // solver's caller invariant forbids (see pr494_accumulated_zero_for_one_basic)
        let result = compute_swap_step_to_trade_price(
            pr494_sqrt_price(),
            U256::from_str("79228162514264337593543950336").unwrap(),
            1_000_000_000_000_000_000u128,
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_300_000_000_000_000_000u128),
            U256::from(15u64),
            U256::from(10u64),
            3000,
        );

        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn pr494_negative_residual_accumulated_out_exceeds_target() {
        let sqrt_price = pr494_sqrt_price();
        let boundary = U256::from_str("79228162514264337593543950336").unwrap();
        // Cumulative ratio 2.1 is above the 1.95 limit: more can be swapped
        let accumulated_in = U256::from(100_000u128);
        let accumulated_out = U256::from(210_000u128);
        let limit_num = U256::from(195u64);
        let limit_den = U256::from(100u64);

        let (_sqrt_price_new, amount_in, amount_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            1_000_000_000_000_000_000u128,
            accumulated_in,
            accumulated_out,
            limit_num,
            limit_den,
            3000,
        )
        .unwrap();

        assert!(amount_in > U256::ZERO, "Should swap some amount");
        assert!(amount_out > U256::ZERO, "Should produce output");
        pr494_assert_limit_satisfied(
            accumulated_in,
            accumulated_out,
            amount_in,
            amount_out,
            limit_num,
            limit_den,
        );

        // The cumulative ratio should land close to the limit (their 0.5% window)
        let total_in = 100_000u128 + amount_in.to::<u128>();
        let total_out = 210_000u128 + amount_out.to::<u128>();
        let cumulative_ratio = total_out as f64 / total_in as f64;
        let relative_diff = (cumulative_ratio - 1.95).abs() / 1.95;
        assert!(
            relative_diff < 0.005,
            "Cumulative ratio {cumulative_ratio:.6} should be close to 1.95, \
             diff: {relative_diff:.6}"
        );
    }

    #[test]
    fn pr494_rounding_direction_is_conservative() {
        // Assertion direction flipped vs PR 494: there "conservative" means the achieved
        // price never EXCEEDS the target (it may undershoot the user's floor); here the
        // limit is a floor, so the achieved price must never drop BELOW it.
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        let fee = 3000u32;

        let limit_num_z = U256::from(19u64);
        let limit_den_z = U256::from(10u64);
        let (_sp, amount_in_z, amount_out_z) = compute_swap_step_to_trade_price(
            sqrt_price,
            U256::from_str("79228162514264337593543950336").unwrap(),
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num_z,
            limit_den_z,
            fee,
        )
        .unwrap();
        pr494_assert_limit_satisfied(
            U256::ZERO,
            U256::ZERO,
            amount_in_z,
            amount_out_z,
            limit_num_z,
            limit_den_z,
        );

        let limit_num_o = U256::from(485u64);
        let limit_den_o = U256::from(1000u64);
        let (_sp, amount_in_o, amount_out_o) = compute_swap_step_to_trade_price(
            sqrt_price,
            U256::from_str("150000000000000000000000000000").unwrap(),
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num_o,
            limit_den_o,
            fee,
        )
        .unwrap();
        pr494_assert_limit_satisfied(
            U256::ZERO,
            U256::ZERO,
            amount_in_o,
            amount_out_o,
            limit_num_o,
            limit_den_o,
        );
    }

    #[test]
    fn pr494_accumulated_multi_tick_simulation() {
        // Two-stage walk: consume a range up to a boundary, then continue with the
        // accumulated amounts; the final cumulative price must satisfy the limit and land
        // close to it
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        let fee = 3000u32;
        let limit_num = U256::from(185u64);
        let limit_den = U256::from(100u64);

        let intermediate_boundary = U256::from_str("111500000000000000000000000000").unwrap();
        let (stage1_sqrt, stage1_in, stage1_out) = compute_swap_step_to_trade_price(
            sqrt_price,
            intermediate_boundary,
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();
        assert_eq!(stage1_sqrt, intermediate_boundary, "Stage 1 should stop at the boundary");

        let far_boundary = U256::from_str("79228162514264337593543950336").unwrap();
        let (stage2_sqrt, stage2_in, stage2_out) = compute_swap_step_to_trade_price(
            stage1_sqrt,
            far_boundary,
            liquidity,
            stage1_in,
            stage1_out,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();
        assert!(stage2_sqrt > far_boundary, "Stage 2 should solve inside the range");

        let total_in = stage1_in + stage2_in;
        let total_out = stage1_out + stage2_out;
        pr494_assert_limit_satisfied(
            U256::ZERO,
            U256::ZERO,
            total_in,
            total_out,
            limit_num,
            limit_den,
        );

        let cumulative = total_out.to::<u128>() as f64 / total_in.to::<u128>() as f64;
        let relative_diff = (cumulative - 1.85).abs() / 1.85;
        assert!(
            relative_diff < 0.005,
            "Cumulative trade price {cumulative:.6} should be close to 1.85, \
             diff: {relative_diff:.6}"
        );
    }

    #[test]
    fn pr494_zero_accumulated_shortcut_matches_full_solver_z4o() {
        // The zero-accumulated shortcut and the full quadratic must agree: tiny
        // accumulated amounts at exactly the limit ratio leave the solution unchanged
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        let fee = 3000u32;
        let limit_num = U256::from(19u64);
        let limit_den = U256::from(10u64);
        let boundary = U256::from_str("79228162514264337593543950336").unwrap();

        let (shortcut_sqrt, _, _) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();
        let (full_sqrt, _, _) = compute_swap_step_to_trade_price(
            sqrt_price, boundary, liquidity, limit_den, limit_num, limit_num, limit_den, fee,
        )
        .unwrap();

        let diff = if shortcut_sqrt > full_sqrt {
            shortcut_sqrt - full_sqrt
        } else {
            full_sqrt - shortcut_sqrt
        };
        assert!(
            diff <= shortcut_sqrt / U256::from(1_000_000_000_000u64),
            "Shortcut root {shortcut_sqrt} should match the full solver root {full_sqrt}"
        );
    }

    #[test]
    fn pr494_zero_accumulated_shortcut_matches_full_solver_o4z() {
        let sqrt_price = pr494_sqrt_price();
        let liquidity = 1_000_000_000_000_000_000u128;
        let fee = 3000u32;
        let limit_num = U256::from(485u64);
        let limit_den = U256::from(1000u64);
        let boundary = U256::from_str("150000000000000000000000000000").unwrap();

        let (shortcut_sqrt, _, _) = compute_swap_step_to_trade_price(
            sqrt_price,
            boundary,
            liquidity,
            U256::ZERO,
            U256::ZERO,
            limit_num,
            limit_den,
            fee,
        )
        .unwrap();
        let (full_sqrt, _, _) = compute_swap_step_to_trade_price(
            sqrt_price, boundary, liquidity, limit_den, limit_num, limit_num, limit_den, fee,
        )
        .unwrap();

        let diff = if shortcut_sqrt > full_sqrt {
            shortcut_sqrt - full_sqrt
        } else {
            full_sqrt - shortcut_sqrt
        };
        assert!(
            diff <= shortcut_sqrt / U256::from(1_000_000_000_000u64),
            "Shortcut root {shortcut_sqrt} should match the full solver root {full_sqrt}"
        );
    }
}
