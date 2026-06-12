use alloy::primitives::U256;
use tycho_common::{
    simulation::{errors::SimulationError, protocol_sim::Price},
    Bytes,
};

use crate::evm::protocol::{
    safe_math::{
        div_mod_u256, safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256, sqrt_u256,
    },
    u256_num::{biguint_to_u256, u256_to_f64},
    utils::solidity_math::{mul_div, mul_div_rounding_up, mul_div_rounding_up_384},
};

const Q96: U256 = U256::from_limbs([0, 4294967296, 0, 0]);
const Q192: U256 = U256::from_limbs([0, 0, 0, 1]); // 2^192
const RESOLUTION: U256 = U256::from_limbs([96, 0, 0, 0]);
const U160_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 4294967295, 0]);

fn maybe_flip_ratios(a: U256, b: U256) -> (U256, U256) {
    if a > b {
        (b, a)
    } else {
        (a, b)
    }
}

fn div_rounding_up(a: U256, b: U256) -> Result<U256, SimulationError> {
    let (result, rest) = div_mod_u256(a, b)?;
    if rest > U256::from(0u64) {
        // SAFETY: rest > 0 implies result * b = a - rest <= a - 1, and b >= 1 (zero divisor
        // already errored), so result <= a - 1 < U256::MAX and the increment cannot wrap.
        debug_assert!(result < U256::MAX, "rounding increment would wrap");
        Ok(result.wrapping_add(U256::from(1u64)))
    } else {
        Ok(result)
    }
}

pub(crate) fn get_amount0_delta(
    a: U256,
    b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let (sqrt_ratio_a, sqrt_ratio_b) = maybe_flip_ratios(a, b);

    if sqrt_ratio_a == U256::ZERO {
        return Err(SimulationError::FatalError(
            "sqrt_ratio_a must be greater than zero".to_string(),
        ));
    }

    let numerator1 = U256::from(liquidity) << RESOLUTION;
    let numerator2 = sqrt_ratio_b - sqrt_ratio_a;

    if round_up {
        div_rounding_up(
            mul_div_rounding_up_384(numerator1, numerator2, sqrt_ratio_b)?,
            sqrt_ratio_a,
        )
    } else {
        safe_div_u256(mul_div_rounding_up_384(numerator1, numerator2, sqrt_ratio_b)?, sqrt_ratio_a)
    }
}

pub(crate) fn get_amount1_delta(
    a: U256,
    b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let (sqrt_ratio_a, sqrt_ratio_b) = maybe_flip_ratios(a, b);
    if round_up {
        mul_div_rounding_up_384(U256::from(liquidity), sqrt_ratio_b - sqrt_ratio_a, Q96)
    } else {
        safe_div_u256(
            safe_mul_u256(U256::from(liquidity), safe_sub_u256(sqrt_ratio_b, sqrt_ratio_a)?)?,
            Q96,
        )
    }
}

pub(super) fn get_next_sqrt_price_from_input(
    sqrt_price: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256, SimulationError> {
    if sqrt_price == U256::ZERO {
        return Err(SimulationError::FatalError("sqrt_price must be greater than zero".to_string()));
    }

    if zero_for_one {
        Ok(get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_in, true)?)
    } else {
        Ok(get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_in, true)?)
    }
}

pub(super) fn get_next_sqrt_price_from_output(
    sqrt_price: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256, SimulationError> {
    if sqrt_price == U256::ZERO {
        return Err(SimulationError::FatalError("sqrt_price must be greater than zero".to_string()));
    }
    if liquidity == 0 {
        return Err(SimulationError::FatalError("liquidity must be greater than zero".to_string()));
    }

    if zero_for_one {
        Ok(get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_in, false)?)
    } else {
        Ok(get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_in, false)?)
    }
}

fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_price: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, SimulationError> {
    if amount == U256::from(0u64) {
        return Ok(sqrt_price);
    }
    let numerator1 = U256::from(liquidity) << RESOLUTION;

    if add {
        // The wrapped product is intentional: the quotient check below detects wrap-around
        // (Solidity-port idiom), so the double-width overflow flag of `overflowing_mul` is
        // redundant work.
        let product = amount.wrapping_mul(sqrt_price);
        if product / amount == sqrt_price {
            // No overflow case: liquidity * sqrtPX96 / (liquidity +- amount * sqrtPX96)
            let denominator = safe_add_u256(numerator1, product)?;
            if denominator >= numerator1 {
                return mul_div_rounding_up_384(numerator1, sqrt_price, denominator);
            }
        }
        // Overflow: liquidity / (liquidity / sqrtPX96 +- amount)
        div_rounding_up(numerator1, safe_add_u256(safe_div_u256(numerator1, sqrt_price)?, amount)?)
    } else {
        // Same wrapped-product idiom as the `add` branch: the quotient check detects wrap.
        let product = amount.wrapping_mul(sqrt_price);
        if safe_div_u256(product, amount)? != sqrt_price || numerator1 <= product {
            return Err(SimulationError::FatalError(
                "sqrt_price_math: overflow in get_next_sqrt_price_from_amount0".to_string(),
            ));
        }
        let denominator = safe_sub_u256(numerator1, product)?;
        // No overflow case: liquidity * sqrtPX96 / (liquidity +- amount * sqrtPX96)
        mul_div_rounding_up_384(numerator1, sqrt_price, denominator)
    }
}

fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_price: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, SimulationError> {
    if add {
        let quotient = if amount <= U160_MAX {
            safe_div_u256(amount << RESOLUTION, U256::from(liquidity))
        } else {
            mul_div(amount, Q96, U256::from(liquidity))
        };

        safe_add_u256(sqrt_price, quotient?)
    } else {
        let quotient = if amount <= U160_MAX {
            div_rounding_up(amount << RESOLUTION, U256::from(liquidity))?
        } else {
            mul_div_rounding_up(amount, Q96, U256::from(liquidity))?
        };

        if sqrt_price <= quotient {
            return Err(SimulationError::FatalError(
                "sqrt_price_math: sqrt_price underflow in get_next_sqrt_price_from_amount1"
                    .to_string(),
            ));
        }
        safe_sub_u256(sqrt_price, quotient)
    }
}

/// Converts a sqrt price in Q96 representation to its approximate f64 representation
///
/// # Panics
/// Will panic if the `x` is bigger than U160.
pub(crate) fn sqrt_price_q96_to_f64(
    x: U256,
    token_0_decimals: u32,
    token_1_decimals: u32,
) -> Result<f64, SimulationError> {
    if x >= U160_MAX {
        return Err(SimulationError::FatalError(format!(
            "sqrt_price_q96_to_f64: x value {x} exceeds U160 max"
        )));
    }
    let token_correction = 10f64.powi(token_0_decimals as i32 - token_1_decimals as i32);

    let price = u256_to_f64(x)? / 2.0f64.powi(96);
    Ok(price.powi(2) * token_correction)
}

/// Computes sqrt(price_0/price_1) * 2^96
///
/// This function calculates the square root price in Q96 format from a price ratio.
/// The computation is: sqrt(price_0/price_1) * 2^96 = sqrt(price_0 * 2^192 / price_1)
///
/// # Arguments
/// * `price_0` - Numerator of the price ratio
/// * `price_1` - Denominator of the price ratio
///
/// # Returns
/// The sqrt price in Q96 format as U256
pub(crate) fn get_sqrt_price_q96(price_0: U256, price_1: U256) -> Result<U256, SimulationError> {
    // sqrt(price_0/price_1) * 2^96 = sqrt(price_0 * 2^192 / price_1)
    // We need to compute this carefully to avoid overflow

    let ratio = mul_div(price_0, Q192, price_1)?;

    // Compute integer square root
    sqrt_u256(ratio)
}

/// Converts a target price to sqrt_price_x96 format with fee adjustment
///
/// # Arguments
/// * `token_in` - The token being sold
/// * `token_out` - The token being bought
/// * `target_price` - The target price as token_out/token_in (tycho convention)
///
/// # Returns
/// The sqrt price limit in Q96 format
pub(crate) fn get_sqrt_price_limit(
    token_in: &Bytes,
    token_out: &Bytes,
    target_price: &Price,
    fee_tier: U256,
) -> Result<U256, SimulationError> {
    let zero_for_one = token_in < token_out;

    // Convert target pool price (token_out/token_in) to swap price (token_in/token_out)
    // by flipping numerator and denominator
    let swap_price_numerator = biguint_to_u256(&target_price.denominator);
    let swap_price_denominator = biguint_to_u256(&target_price.numerator);

    // Apply fee to the sell price (numerator) using integer division to match APEX behavior
    // For Uniswap V3: adjusted_price = price * (1 - fee)
    let fee_precision = U256::from_limbs([1_000_000, 0, 0, 0]);
    let sell_price_after_fee =
        safe_div_u256(swap_price_numerator * (fee_precision - fee_tier), fee_precision)?;
    let buy_price = swap_price_denominator;

    // Determine which price goes to which parameter based on swap direction
    // For Uniswap V3, sqrt_price_x96 = sqrt(token1/token0) * 2^96
    let (price_0, price_1) = if zero_for_one {
        (sell_price_after_fee, buy_price)
    } else {
        (buy_price, sell_price_after_fee)
    };

    // Convert to sqrt price: sqrt(price_1 / price_0) * 2^96
    get_sqrt_price_q96(price_1, price_0)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use approx::assert_ulps_eq;
    use rstest::rstest;

    use super::*;

    fn u256(s: &str) -> U256 {
        U256::from_str(s).unwrap()
    }

    #[test]
    fn test_maybe_flip() {
        let a = U256::from_str("646922711029656030980122427077").unwrap();
        let b = U256::from_str("78833030112140176575862854579").unwrap();
        let (a1, b1) = maybe_flip_ratios(a, b);

        assert_eq!(b, a1);
        assert_eq!(a, b1);
    }

    #[rstest]
    #[case(
        u256("646922711029656030980122427077"),
        u256("78833030112140176575862854579"),
        1000000000000u128,
        true,
        u256("882542983628")
    )]
    #[case(
        u256("646922711029656030980122427077"),
        u256("78833030112140176575862854579"),
        1000000000000u128,
        false,
        u256("882542983627")
    )]
    #[case(
        u256("79224201403219477170569942574"),
        u256("79394708140106462983274643745"),
        10000000u128,
        true,
        u256("21477")
    )]
    #[case(
        u256("79224201403219477170569942574"),
        u256("79394708140106462983274643745"),
        10000000u128,
        false,
        u256("21476")
    )]
    fn test_get_amount0_delta(
        #[case] a: U256,
        #[case] b: U256,
        #[case] liquidity: u128,
        #[case] round_up: bool,
        #[case] exp: U256,
    ) {
        let res = get_amount0_delta(a, b, liquidity, round_up).unwrap();
        assert_eq!(res, exp);
    }

    #[rstest]
    #[case(
        u256("79224201403219477170569942574"),
        u256("79394708140106462983274643745"),
        10000000u128,
        true,
        u256("21521")
    )]
    #[case(
        u256("79224201403219477170569942574"),
        u256("79394708140106462983274643745"),
        10000000u128,
        false,
        u256("21520")
    )]
    #[case(
        u256("646922711029656030980122427077"),
        u256("78833030112140176575862854579"),
        1000000000000u128,
        true,
        u256("7170299838965")
    )]
    #[case(
        u256("646922711029656030980122427077"),
        u256("78833030112140176575862854579"),
        1000000000000u128,
        false,
        u256("7170299838964")
    )]
    fn test_get_amount1_delta(
        #[case] a: U256,
        #[case] b: U256,
        #[case] liquidity: u128,
        #[case] round_up: bool,
        #[case] exp: U256,
    ) {
        let res = get_amount1_delta(a, b, liquidity, round_up).unwrap();
        assert_eq!(res, exp);
    }

    #[rstest]
    #[case(
        u256("79224201403219477170569942574"),
        1000000000000u128,
        u256("1000000"),
        true,
        u256("79224122183058203155816882540")
    )]
    #[case(
        u256("79224201403219477170569942574"),
        1000000000000u128,
        u256("1000000"),
        false,
        u256("79224280631381991434907536117")
    )]
    fn test_get_next_sqrt_price_from_input(
        #[case] sqrt_price: U256,
        #[case] liquidity: u128,
        #[case] amount_in: U256,
        #[case] zero_for_one: bool,
        #[case] exp: U256,
    ) {
        let res =
            get_next_sqrt_price_from_input(sqrt_price, liquidity, amount_in, zero_for_one).unwrap();
        assert_eq!(res, exp);
    }

    #[rstest]
    #[case(
        u256("79224201403219477170569942574"),
        1000000000000u128,
        u256("1000000"),
        true,
        u256("79224122175056962906232349030")
    )]
    #[case(
        u256("79224201403219477170569942574"),
        1000000000000u128,
        u256("1000000"),
        false,
        u256("79224280623539183744873644932")
    )]
    fn test_get_next_sqrt_price_from_output(
        #[case] sqrt_price: U256,
        #[case] liquidity: u128,
        #[case] amount_in: U256,
        #[case] zero_for_one: bool,
        #[case] exp: U256,
    ) {
        let res = get_next_sqrt_price_from_output(sqrt_price, liquidity, amount_in, zero_for_one)
            .unwrap();
        assert_eq!(res, exp);
    }

    #[rstest]
    #[case::usdc_eth(u256("2209221051636112667296733914466103"), 6, 18, 0.0007775336231174711f64)]
    #[case::wbtc_eth(u256("29654479368916176338227069900580738"), 8, 18, 14.00946143160293f64)]
    #[case::wdoge_eth(u256("672045190479078414067608947"), 18, 18, 7.195115788867147e-5)]
    #[case::shib_usdc(u256("231479673319799999440"), 18, 6, 8.536238764169166e-6)]
    #[case::min_price(u256("4295128740"), 18, 18, 2.9389568087743114e-39f64)]
    #[case::max_price(
        u256("1461446703485210103287273052203988822378723970341"),
        18,
        18,
        3.402_567_868_363_881e38_f64
    )]
    fn test_q96_to_f64(
        #[case] sqrt_price: U256,
        #[case] t0d: u32,
        #[case] t1d: u32,
        #[case] exp: f64,
    ) {
        let res = sqrt_price_q96_to_f64(sqrt_price, t0d, t1d).expect("convert sqrt price");

        assert_ulps_eq!(res, exp, epsilon = f64::EPSILON);
    }

    #[test]
    fn test_get_amount0_delta_zero_sqrt_ratio_returns_error() {
        let result = get_amount0_delta(U256::ZERO, U256::from(1u64), 1_000_000, true);
        assert!(
            matches!(result, Err(SimulationError::FatalError(ref msg)) if msg.contains("sqrt_ratio_a")),
            "expected FatalError about sqrt_ratio_a, got {result:?}"
        );
    }

    #[test]
    fn test_get_next_sqrt_price_from_input_zero_price_returns_error() {
        let result =
            get_next_sqrt_price_from_input(U256::ZERO, 1_000_000, U256::from(100u64), true);
        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn test_get_next_sqrt_price_from_output_zero_price_returns_error() {
        let result =
            get_next_sqrt_price_from_output(U256::ZERO, 1_000_000, U256::from(100u64), true);
        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn test_get_next_sqrt_price_from_output_zero_liquidity_returns_error() {
        let result = get_next_sqrt_price_from_output(
            u256("79224201403219477170569942574"),
            0,
            U256::from(100u64),
            true,
        );
        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn test_get_next_sqrt_price_from_amount0_overflow_returns_error() {
        let result = get_next_sqrt_price_from_output(
            u256("79224201403219477170569942574"),
            1,
            U256::MAX / U256::from(2u64),
            false,
        );
        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn test_get_next_sqrt_price_from_amount1_underflow_returns_error() {
        let result = get_next_sqrt_price_from_output(
            U256::from(1u64),
            1,
            U256::from(10u64).pow(U256::from(30u64)),
            true,
        );
        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }
}
