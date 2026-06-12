use alloy::primitives::{Sign, I256, U256, U512};
use num_bigint::BigUint;
use num_traits::Zero;
use tycho_common::{
    simulation::{errors::SimulationError, protocol_sim::Price},
    Bytes,
};

use crate::evm::protocol::{
    safe_math::safe_add_u256,
    u256_num::{biguint_to_u256, u256_to_biguint},
    utils::uniswap::{
        liquidity_math,
        sqrt_price_math::get_sqrt_price_limit,
        swap_math::{compute_swap_step_to_trade_price, trade_price_range_amounts},
        tick_list::TickList,
        tick_math::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_TICK, MIN_TICK},
        SwapResults, SwapToTradePriceResult,
    },
};

// U160_MAX = 2^160 - 1, used for "infinite" swap amounts in swap_to_price
const U160_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, u64::MAX >> 32, 0]); // 2^160 - 1

// Mirrors UniswapV3State::get_limits: bounds the tick walk to what a single swap could
// afford gas-wise ((16.7M gas budget - 70k base) / 24k per initialized tick crossing).
const MAX_TICKS_CROSSED: u64 = (16_700_000 - 70_000) / 24_000;

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
#[allow(clippy::too_many_arguments)]
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

    let sqrt_price_limit =
        get_sqrt_price_limit(token_in, token_out, target_price, U256::from(fee_pips))?;

    // Validate price limit is compatible with swap direction
    if zero_for_one && sqrt_price_limit >= sqrt_price {
        return Err(SimulationError::InvalidInput(
            "Target price is unreachable (already below current spot price)".to_string(),
            None,
        ));
    }
    if !zero_for_one && sqrt_price_limit <= sqrt_price {
        return Err(SimulationError::InvalidInput(
            "Target price is unreachable (already below current spot price)".to_string(),
            None,
        ));
    }

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

/// Computes the maximum swap on a CLMM pool whose trade price (`amount_out / amount_in`)
/// stays at or above a limit price.
///
/// This function encapsulates the common logic for the `TradeLimitPrice` constraint across
/// UniswapV3 and UniswapV4. Unlike [`clmm_swap_to_price`] it never simulates a swap: it
/// only reads pool data, so the caller forwards its state fields as plain arguments and no
/// closure is needed.
///
/// # Algorithm
///
/// The trade price is strictly decreasing in the input amount; its supremum at
/// infinitesimal size is the fee-adjusted spot price, which bounds reachability. The pool
/// is walked range by range (same skeleton as `UniswapV3State::get_limits`): if consuming
/// an entire range keeps the cumulative trade price at or above the limit, the range is
/// taken whole and the walk continues across the tick; otherwise the exact stopping point
/// inside the range is solved analytically by
/// [`compute_swap_step_to_trade_price`](crate::evm::protocol::utils::uniswap::swap_math::compute_swap_step_to_trade_price).
/// Running out of ticks or liquidity is not an error — everything accumulated is returned,
/// as the limit admits all of it.
///
/// The limit is first normalized to at most 128 bits per side (numerator rounded up,
/// denominator rounded down) so that all comparisons fit in U512; the directed rounding
/// can only tighten the limit, never loosen it.
///
/// # Arguments
/// * `sqrt_price` - Current sqrt price of the pool in Q96 format.
/// * `tick` - Current tick of the pool.
/// * `liquidity` - Currently active liquidity.
/// * `ticks` - The pool's tick list.
/// * `fee_pips` - Total swap fee in pips (1/1_000_000).
/// * `token_in` - Address of the token being sold.
/// * `token_out` - Address of the token being bought.
/// * `limit` - The minimum acceptable trade price as `token_out/token_in`, in raw atomic units.
///
/// # Returns
/// A [`SwapToTradePriceResult`] with the total amounts and the post-swap pool state
/// fields. Amounts are zero when the limit equals the effective spot price.
///
/// # Errors
/// * `SimulationError::InvalidInput` - The limit is above the effective spot price (unreachable),
///   or the fee consumes the entire input.
/// * `SimulationError::FatalError` - Arithmetic overflow.
#[allow(clippy::too_many_arguments)]
pub(crate) fn clmm_swap_to_trade_price(
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
    ticks: &TickList,
    fee_pips: u32,
    token_in: &Bytes,
    token_out: &Bytes,
    limit: &Price,
) -> Result<SwapToTradePriceResult, SimulationError> {
    if fee_pips >= 1_000_000 {
        return Err(SimulationError::InvalidInput(
            "Fee consumes the entire input".to_string(),
            None,
        ));
    }
    let zero_for_one = token_in < token_out;
    let (limit_num, limit_den) = normalize_limit_price(limit)?;

    // Reachability: the limit must not exceed the fee-adjusted spot price, which for
    // zero_for_one is g·sqrt_price²/(F·Q192) and for one_for_zero its reciprocal in Q192.
    // With the limit normalized to ≤129 bits every product below stays under 2^470.
    let fee_complement = U512::from(1_000_000 - fee_pips);
    let fee_precision = U512::from(1_000_000u32);
    let q192 = U512::ONE << 192;
    let sqrt_price_squared = U512::from(sqrt_price) * U512::from(sqrt_price);
    let (limit_side, spot_side) = if zero_for_one {
        (
            U512::from(limit_num) * fee_precision * q192,
            U512::from(limit_den) * fee_complement * sqrt_price_squared,
        )
    } else {
        (
            U512::from(limit_num) * fee_precision * sqrt_price_squared,
            U512::from(limit_den) * fee_complement * q192,
        )
    };
    if limit_side > spot_side {
        return Err(SimulationError::InvalidInput(
            "Limit trade price is unreachable (above effective spot price)".to_string(),
            None,
        ));
    }
    if limit_side == spot_side {
        return Ok(SwapToTradePriceResult {
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            sqrt_price,
            liquidity,
            tick,
        });
    }

    let mut current_tick = tick;
    let mut current_sqrt_price = sqrt_price;
    let mut current_liquidity = liquidity;
    let mut total_amount_in = U256::ZERO;
    let mut total_amount_out = U256::ZERO;
    let mut ticks_crossed: u64 = 0;

    while let Ok((next_tick, initialized)) =
        ticks.next_initialized_tick_within_one_word(current_tick, zero_for_one)
    {
        if ticks_crossed >= MAX_TICKS_CROSSED {
            break;
        }
        ticks_crossed += 1;

        let next_tick = next_tick.clamp(MIN_TICK, MAX_TICK);
        let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;

        let (range_amount_in, range_amount_out) = trade_price_range_amounts(
            current_sqrt_price,
            sqrt_price_next,
            current_liquidity,
            fee_pips,
            zero_for_one,
        )?;

        // Whole-range test: does consuming the entire range keep the cumulative trade
        // price at or above the limit?
        let full_out = U512::from(total_amount_out) + U512::from(range_amount_out);
        let full_in = U512::from(total_amount_in) + U512::from(range_amount_in);
        if full_out * U512::from(limit_den) < full_in * U512::from(limit_num) {
            // The limit is hit inside this range: solve for the exact stopping point
            let (sqrt_price_final, partial_in, partial_out) = compute_swap_step_to_trade_price(
                current_sqrt_price,
                sqrt_price_next,
                current_liquidity,
                total_amount_in,
                total_amount_out,
                limit_num,
                limit_den,
                fee_pips,
            )?;
            total_amount_in = safe_add_u256(total_amount_in, partial_in)?;
            total_amount_out = safe_add_u256(total_amount_out, partial_out)?;
            return Ok(SwapToTradePriceResult {
                amount_in: total_amount_in,
                amount_out: total_amount_out,
                sqrt_price: sqrt_price_final,
                liquidity: current_liquidity,
                tick: get_tick_at_sqrt_ratio(sqrt_price_final)?,
            });
        }

        total_amount_in = safe_add_u256(total_amount_in, range_amount_in)?;
        total_amount_out = safe_add_u256(total_amount_out, range_amount_out)?;

        if initialized {
            let liquidity_raw = ticks
                .get_tick(next_tick)
                .unwrap()
                .net_liquidity;
            let liquidity_net = if zero_for_one { -liquidity_raw } else { liquidity_raw };
            match liquidity_math::add_liquidity_delta(current_liquidity, liquidity_net) {
                Ok(new_liquidity) => current_liquidity = new_liquidity,
                // Liquidity would underflow: everything up to here is the maximum usable
                Err(_) => break,
            }
        }
        current_tick = if zero_for_one { next_tick - 1 } else { next_tick };
        current_sqrt_price = sqrt_price_next;
    }

    // The limit is worse than everything the pool offers: return all available liquidity
    Ok(SwapToTradePriceResult {
        amount_in: total_amount_in,
        amount_out: total_amount_out,
        sqrt_price: current_sqrt_price,
        liquidity: current_liquidity,
        tick: current_tick,
    })
}

/// Normalizes a limit price fraction to at most 128 bits per side.
///
/// The numerator rounds up and the denominator rounds down, so the normalized limit is
/// never below the original: any trade satisfying it also satisfies the caller's limit.
///
/// # Errors
/// * `SimulationError::InvalidInput` - The denominator vanishes after normalization, which means
///   the limit exceeds any representable pool price.
fn normalize_limit_price(limit: &Price) -> Result<(U256, U256), SimulationError> {
    let bits = limit
        .numerator
        .bits()
        .max(limit.denominator.bits());
    let shift = bits.saturating_sub(128);
    if shift == 0 {
        return Ok((biguint_to_u256(&limit.numerator), biguint_to_u256(&limit.denominator)));
    }
    let rounding = (BigUint::from(1u8) << shift) - BigUint::from(1u8);
    let numerator = (&limit.numerator + rounding) >> shift;
    let denominator = &limit.denominator >> shift;
    if denominator.is_zero() {
        return Err(SimulationError::InvalidInput(
            "Limit trade price is unreachable (above any pool price)".to_string(),
            None,
        ));
    }
    Ok((biguint_to_u256(&numerator), biguint_to_u256(&denominator)))
}
