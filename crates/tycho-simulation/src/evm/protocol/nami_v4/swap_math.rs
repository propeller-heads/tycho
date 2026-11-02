//! Algebra Integral swap-step math (`PriceMovementMath` and `TokenDeltaMath`).
//!
//! The shared `utils::uniswap` math is Uniswap V3's. Algebra differs in rounding and branch
//! conditions in ways that change results by a few wei (exact-output uses `>=` to decide whether
//! the target is reached, the rounded-down token0 delta is a floor `mulDiv`, and overflowing
//! products fall back to a second formula instead of reverting), so this module ports the Algebra
//! libraries line by line.
use alloy::primitives::{I256, U256};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::utils::solidity_math::{mul_div, mul_div_rounding_up};

/// Fee denominator: fees are expressed in hundredths of a bip (1e-6).
pub(super) const FEE_DENOMINATOR: u32 = 1_000_000;
const RESOLUTION: usize = 96;

fn q96() -> U256 {
    U256::ONE << RESOLUTION
}

fn max_u160() -> U256 {
    (U256::ONE << 160) - U256::ONE
}

fn revert(reason: &str) -> SimulationError {
    SimulationError::RecoverableError(format!("Algebra math revert: {reason}"))
}

fn div_rounding_up(x: U256, y: U256) -> Result<U256, SimulationError> {
    if y.is_zero() {
        return Err(revert("division by zero"));
    }
    let (quotient, remainder) = x.div_rem(y);
    Ok(if remainder.is_zero() { quotient } else { quotient + U256::ONE })
}

fn to_u160(value: U256) -> Result<U256, SimulationError> {
    if value > max_u160() {
        return Err(revert("uint160 overflow"));
    }
    Ok(value)
}

/// Amount of token0 between two sqrt prices (`TokenDeltaMath.getToken0Delta`).
///
/// Errors when `price_lower` is zero or above `price_upper`.
pub(super) fn get_token0_delta(
    price_lower: U256,
    price_upper: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let price_delta = price_upper.wrapping_sub(price_lower);
    if price_delta >= price_upper {
        return Err(revert("token0 delta: invalid price range"));
    }
    let liquidity_shifted = U256::from(liquidity) << RESOLUTION;

    if round_up {
        div_rounding_up(
            mul_div_rounding_up(price_delta, liquidity_shifted, price_upper)?,
            price_lower,
        )
    } else {
        Ok(mul_div(price_delta, liquidity_shifted, price_upper)? / price_lower)
    }
}

/// Amount of token1 between two sqrt prices (`TokenDeltaMath.getToken1Delta`).
///
/// Errors when `price_upper` is below `price_lower`.
pub(super) fn get_token1_delta(
    price_lower: U256,
    price_upper: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    if price_upper < price_lower {
        return Err(revert("token1 delta: invalid price range"));
    }
    let price_delta = price_upper - price_lower;
    if round_up {
        mul_div_rounding_up(price_delta, U256::from(liquidity), q96())
    } else {
        mul_div(price_delta, U256::from(liquidity), q96())
    }
}

/// Sqrt price after adding `amount` of input or removing `amount` of output
/// (`PriceMovementMath.getNewPrice`).
fn get_new_price(
    price: U256,
    liquidity: u128,
    amount: U256,
    zero_to_one: bool,
    from_input: bool,
) -> Result<U256, SimulationError> {
    if price.is_zero() || liquidity == 0 {
        return Err(revert("new price: zero price or liquidity"));
    }
    if amount.is_zero() {
        return Ok(price);
    }

    if zero_to_one == from_input {
        let liquidity_shifted = U256::from(liquidity) << RESOLUTION;
        if from_input {
            if let Some(product) = amount.checked_mul(price) {
                if let Some(denominator) = liquidity_shifted.checked_add(product) {
                    return mul_div_rounding_up(liquidity_shifted, price, denominator);
                }
            }
            let denominator = (liquidity_shifted / price)
                .checked_add(amount)
                .ok_or_else(|| revert("new price: denominator overflow"))?;
            // Solidity casts with `uint160(...)`; the quotient is below `price`, so it always fits.
            div_rounding_up(liquidity_shifted, denominator)
        } else {
            let product = amount
                .checked_mul(price)
                .ok_or_else(|| revert("new price: product overflow"))?;
            if liquidity_shifted <= product {
                return Err(revert("new price: denominator underflow"));
            }
            to_u160(mul_div_rounding_up(liquidity_shifted, price, liquidity_shifted - product)?)
        }
    } else if from_input {
        let quotient = if amount <= max_u160() {
            (amount << RESOLUTION) / U256::from(liquidity)
        } else {
            mul_div(amount, q96(), U256::from(liquidity))?
        };
        to_u160(
            price
                .checked_add(quotient)
                .ok_or_else(|| revert("new price: price overflow"))?,
        )
    } else {
        let quotient = if amount <= max_u160() {
            div_rounding_up(amount << RESOLUTION, U256::from(liquidity))?
        } else {
            mul_div_rounding_up(amount, q96(), U256::from(liquidity))?
        };
        if price <= quotient {
            return Err(revert("new price: price underflow"));
        }
        Ok(price - quotient)
    }
}

fn input_token_delta(
    zero_to_one: bool,
    to: U256,
    from: U256,
    liquidity: u128,
) -> Result<U256, SimulationError> {
    if zero_to_one {
        get_token0_delta(to, from, liquidity, true)
    } else {
        get_token1_delta(from, to, liquidity, true)
    }
}

fn output_token_delta(
    zero_to_one: bool,
    to: U256,
    from: U256,
    liquidity: u128,
) -> Result<U256, SimulationError> {
    if zero_to_one {
        get_token1_delta(to, from, liquidity, false)
    } else {
        get_token0_delta(from, to, liquidity, false)
    }
}

/// Result of one swap step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PriceMovement {
    /// Sqrt price reached by the step.
    pub(super) result_price: U256,
    /// Input consumed, excluding the fee.
    pub(super) input: U256,
    /// Output produced.
    pub(super) output: U256,
    /// Fee charged on the input.
    pub(super) fee_amount: U256,
}

/// Moves the price from `current_price` towards `target_price` with at most `amount_available`
/// (positive: exact input, negative: exact output), charging `fee` in hundredths of a bip
/// (`PriceMovementMath.movePriceTowardsTarget`).
pub(super) fn move_price_towards_target(
    zero_to_one: bool,
    current_price: U256,
    target_price: U256,
    liquidity: u128,
    amount_available: I256,
    fee: u32,
) -> Result<PriceMovement, SimulationError> {
    let fee_u256 = U256::from(fee);
    let fee_complement = U256::from(FEE_DENOMINATOR - fee);

    if !amount_available.is_negative() {
        let amount_available = amount_available.into_raw();
        let amount_after_fee =
            mul_div(amount_available, fee_complement, U256::from(FEE_DENOMINATOR))?;
        let mut input = input_token_delta(zero_to_one, target_price, current_price, liquidity)?;
        let result_price;
        let fee_amount;
        if amount_after_fee >= input {
            result_price = target_price;
            fee_amount = mul_div_rounding_up(input, fee_u256, fee_complement)?;
        } else {
            result_price =
                get_new_price(current_price, liquidity, amount_after_fee, zero_to_one, true)?;
            if result_price == target_price {
                return Err(revert("price movement: target reached with partial input"));
            }
            input = input_token_delta(zero_to_one, result_price, current_price, liquidity)?;
            // The target was not reached, so the rest of the available input is the fee.
            fee_amount = amount_available - input;
        }
        let output = output_token_delta(zero_to_one, result_price, current_price, liquidity)?;
        return Ok(PriceMovement { result_price, input, output, fee_amount });
    }

    if amount_available == I256::MIN {
        return Err(revert("price movement: invalid amount required"));
    }
    let amount_requested = amount_available.unsigned_abs();
    let mut output = output_token_delta(zero_to_one, target_price, current_price, liquidity)?;
    let result_price = if amount_requested >= output {
        target_price
    } else {
        let price = get_new_price(current_price, liquidity, amount_requested, zero_to_one, false)?;
        if target_price != price {
            output = output_token_delta(zero_to_one, price, current_price, liquidity)?;
        }
        output = output.min(amount_requested);
        price
    };
    let input = input_token_delta(zero_to_one, result_price, current_price, liquidity)?;
    let fee_amount = mul_div_rounding_up(input, fee_u256, fee_complement)?;
    Ok(PriceMovement { result_price, input, output, fee_amount })
}

/// Converts a step amount to `I256`, erroring like Solidity's `SafeCast.toInt256`.
pub(super) fn amount_to_i256(value: U256) -> Result<I256, SimulationError> {
    I256::try_from(value).map_err(|_| revert("int256 overflow"))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    // sqrt(1) in Q64.96
    fn price_one() -> U256 {
        q96()
    }

    #[test]
    fn token_deltas_round_in_the_pools_favor() {
        let lower = price_one();
        let upper = U256::from_str("87150978765690771352898345369").unwrap(); // ~sqrt(1.21)
        let liquidity = 1_000_000_000_000_000_000u128;

        let token0_up = get_token0_delta(lower, upper, liquidity, true).unwrap();
        let token0_down = get_token0_delta(lower, upper, liquidity, false).unwrap();
        let token1_up = get_token1_delta(lower, upper, liquidity, true).unwrap();
        let token1_down = get_token1_delta(lower, upper, liquidity, false).unwrap();

        assert!(token0_up > token0_down && token0_up - token0_down <= U256::ONE);
        assert!(token1_up > token1_down && token1_up - token1_down <= U256::ONE);
    }

    #[test]
    fn token0_delta_rejects_zero_lower_price() {
        assert!(get_token0_delta(U256::ZERO, price_one(), 1, true).is_err());
    }

    #[test]
    fn exact_input_reaching_target_charges_fee_on_input() {
        let current = price_one();
        let target = current - (current >> 10);
        let step = move_price_towards_target(
            true,
            current,
            target,
            1_000_000_000_000_000_000u128,
            I256::try_from(10u128.pow(30)).unwrap(),
            3_000,
        )
        .unwrap();

        assert_eq!(step.result_price, target);
        let expected_fee =
            mul_div_rounding_up(step.input, U256::from(3_000), U256::from(997_000)).unwrap();
        assert_eq!(step.fee_amount, expected_fee);
    }

    #[test]
    fn exact_input_partial_step_keeps_the_remainder_as_fee() {
        let current = price_one();
        let target = current >> 1;
        let amount = U256::from(1_000_000u64);
        let step = move_price_towards_target(
            true,
            current,
            target,
            1_000_000_000_000_000_000u128,
            I256::from_raw(amount),
            3_000,
        )
        .unwrap();

        assert!(step.result_price < current && step.result_price > target);
        assert_eq!(step.input + step.fee_amount, amount);
    }

    #[test]
    fn exact_output_equal_to_step_output_reaches_the_target() {
        // Algebra treats `amount == output` as reaching the target (Uniswap V3 uses `>`).
        let current = price_one();
        let target = current - (current >> 20);
        let liquidity = 1_000_000_000_000_000_000u128;
        let output_to_target = output_token_delta(true, target, current, liquidity).unwrap();

        let step = move_price_towards_target(
            true,
            current,
            target,
            liquidity,
            -I256::from_raw(output_to_target),
            500,
        )
        .unwrap();

        assert_eq!(step.result_price, target);
        assert_eq!(step.output, output_to_target);
    }

    #[test]
    fn zero_liquidity_jumps_to_target_without_fee() {
        let current = price_one();
        let target = current >> 1;
        let step = move_price_towards_target(
            true,
            current,
            target,
            0,
            I256::from_raw(U256::from(5)),
            3_000,
        )
        .unwrap();

        assert_eq!(
            step,
            PriceMovement {
                result_price: target,
                input: U256::ZERO,
                output: U256::ZERO,
                fee_amount: U256::ZERO,
            }
        );
    }
}
