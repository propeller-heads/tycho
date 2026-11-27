use alloy::primitives::{Sign, I256, U256};
use num_bigint::BigUint;
use tycho_common::{
    simulation::{errors::SimulationError, protocol_sim::Price},
    Bytes,
};

use crate::evm::protocol::{
    u256_num::{biguint_to_u256, u256_to_biguint},
    utils::uniswap::{
        sqrt_price_math::get_reachable_sqrt_price_limit_u256,
        SwapResults,
    },
};

// U160_MAX = 2^160 - 1, used for "infinite" swap amounts in swap_to_price
const U160_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, u64::MAX >> 32, 0]); // 2^160 - 1

/// Abstracted swap_to_price implementation for Concentrated Liquidity Market Makers (CLMM).
///
/// This function encapsulates the common logic for swap_to_price across UniswapV3 and UniswapV4,
/// handling differences through the provided closure.
///
/// # Arguments
/// * `sqrt_price` - Current sqrt price of the pool in Q96 format
/// * `token_in` - Token being sold
/// * `token_out` - Token being bought
/// * `target_price` - Target price as token_out/token_in (tycho convention)
/// * `fee_pips` - Total fee in pips (1/1_000_000)
/// * `amount_sign` - Sign for the amount_specified (Positive for V3, Negative for V4)
/// * `swap_fn` - Closure that performs the actual swap operation
///
/// # Returns
/// A tuple containing (amount_in, amount_out, SwapResults)
pub fn clmm_swap_to_price<F>(
    sqrt_price: U256,
    token_in: &Bytes,
    token_out: &Bytes,
    target_price: &Price,
    fee_pips: u32,
    amount_sign: Sign,
    swap_fn: F,
) -> Result<(BigUint, BigUint, SwapResults), SimulationError>
where
    F: FnOnce(bool, I256, U256) -> Result<SwapResults, SimulationError>,
{
    let zero_for_one = token_in < token_out;

    let Some(sqrt_price_limit) = get_reachable_sqrt_price_limit_u256(
        zero_for_one,
        sqrt_price,
        biguint_to_u256(&target_price.numerator),
        biguint_to_u256(&target_price.denominator),
        U256::from(fee_pips),
    )?
    else {
        return Err(SimulationError::InvalidInput(
            "Target price is unreachable (already below current spot price)".to_string(),
            None,
        ));
    };

    // Use U160_MAX as "infinite" amount to find maximum available liquidity
    let amount_specified =
        I256::checked_from_sign_and_abs(amount_sign, U160_MAX).ok_or_else(|| {
            SimulationError::InvalidInput("I256 overflow: U160_MAX".to_string(), None)
        })?;

    // Call the provided swap function
    // The swap function should already ensure that the sqrt price result is on the correct side of
    // the sqrt_price_limit
    let result = swap_fn(zero_for_one, amount_specified, sqrt_price_limit)?;

    // Calculate amount_in from amount consumed: amount_in = amount_specified - amount_remaining
    let amount_in = (result.amount_specified - result.amount_remaining)
        .abs()
        .into_raw();

    if amount_in == U256::ZERO {
        return Ok((BigUint::ZERO, BigUint::ZERO, SwapResults::default()));
    }

    // Use the accumulated amount_calculated for output
    let amount_out = result
        .amount_calculated
        .abs()
        .into_raw();

    Ok((u256_to_biguint(amount_in), u256_to_biguint(amount_out), result))
}

/// Zero-allocation variant of [`clmm_swap_to_price`] operating directly on `U256` target price
/// (`target_num / target_den` = `token_out / token_in`). Returns `Ok(None)` when the target price
/// is unreachable or yields zero input.
#[expect(
    clippy::too_many_arguments,
    reason = "matches clmm_swap_to_price with split U256 target price limbs"
)]
pub fn clmm_swap_to_price_u256<F>(
    sqrt_price: U256,
    token_in: &Bytes,
    token_out: &Bytes,
    target_num: U256,
    target_den: U256,
    fee_pips: u32,
    amount_sign: Sign,
    swap_fn: F,
) -> Result<Option<(U256, U256)>, SimulationError>
where
    F: FnOnce(bool, I256, U256) -> Result<SwapResults, SimulationError>,
{
    let zero_for_one = token_in < token_out;

    let Some(sqrt_price_limit) = get_reachable_sqrt_price_limit_u256(
        zero_for_one,
        sqrt_price,
        target_num,
        target_den,
        U256::from(fee_pips),
    )?
    else {
        return Ok(None);
    };

    let amount_specified =
        I256::checked_from_sign_and_abs(amount_sign, U160_MAX).ok_or_else(|| {
            SimulationError::InvalidInput("I256 overflow: U160_MAX".to_string(), None)
        })?;

    let result = swap_fn(zero_for_one, amount_specified, sqrt_price_limit)?;

    let amount_in = (result.amount_specified - result.amount_remaining)
        .abs()
        .into_raw();

    if amount_in == U256::ZERO {
        return Ok(None);
    }

    let amount_out = result
        .amount_calculated
        .abs()
        .into_raw();

    Ok(Some((amount_in, amount_out)))
}
