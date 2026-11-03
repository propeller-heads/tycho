//! Fluid DEX v2 D3/D4 exact-input swap math, ported from `CommonSwapModuleInternals._swapIn`
//! and the helpers it calls (`_computeSwapStepForSwapInWithoutFee`,
//! `_nextInitializedTickWithinOneWord`, `_verifySqrtPriceX96ChangeLimits`).
//!
//! Amounts here are raw-adjusted: token amounts scaled to 9 decimals and divided by the token's
//! Liquidity exchange price. Only constant-LP-fee swaps are ported: fee version 1 and fees fetched
//! from a controller are rejected before a swap reaches this module.
use std::collections::BTreeMap;

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    utils::{
        solidity_math::{mul_div, mul_div_rounding_up},
        uniswap::{
            sqrt_price_math::{get_amount1_delta, get_next_sqrt_price_from_input},
            tick_math::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio},
        },
    },
};

/// Fluid narrows Uniswap's tick range to ±524287.
pub(super) const MIN_TICK: i32 = -524_287;
pub(super) const MAX_TICK: i32 = 524_287;

const SIX_DECIMALS: u64 = 1_000_000;
const TWO_DECIMALS: u64 = 100;
const FOUR_DECIMALS: u64 = 10_000;
const TEN_DECIMALS: u64 = 10_000_000_000;
/// 20% in 10 decimals: one swap may move the sqrt price at most this much.
const MAX_SQRT_PRICE_CHANGE: u64 = 2_000_000_000;
/// 5e-10 in 10 decimals: one swap must move the sqrt price at least this much.
const MIN_SQRT_PRICE_CHANGE: u64 = 5;
const MAX_STEP_AMOUNT_OUT_BITS: usize = 86;

/// Pool state a swap reads, oriented for one swap direction.
#[derive(Debug, Clone)]
pub(super) struct SwapPool<'a> {
    /// Initialized ticks and their net liquidity.
    pub ticks: &'a BTreeMap<i32, i128>,
    pub tick_spacing: i32,
    pub tick: i32,
    /// The stored sqrt price, decoded with [`decode_sqrt_price`] for the swap direction.
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    /// Protocol fee of the swap direction, in pips.
    pub protocol_fee: u64,
    /// Share of the LP fee that goes to the protocol, in percent.
    pub protocol_cut_fee: u64,
    /// Constant LP fee, in pips.
    pub lp_fee: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SwapOutcome {
    pub amount_in_raw: U256,
    pub amount_out_raw: U256,
    pub protocol_fee_raw: U256,
    pub lp_fee_raw: U256,
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
    pub initialized_ticks_crossed: u64,
}

/// Swaps `amount_in_raw` through the pool, as `_swapIn` does.
///
/// With `sqrt_price_limit`, the swap stops at that price instead of consuming the whole input,
/// and `amount_in_raw` of the outcome is what reaching it took; the contract has no such limit,
/// it serves [`max_sqrt_price_move`].
///
/// # Errors
/// - `InvalidInput` when the input reaches past the outermost tick (the dex fills nothing
///   partially) or moves the price outside the per-swap bounds.
/// - `FatalError` on arithmetic the contract would not reach with consistent state.
pub(super) fn swap_in(
    pool: &SwapPool,
    swap0_to1: bool,
    amount_in_raw: U256,
    sqrt_price_limit: Option<U256>,
) -> Result<SwapOutcome, SimulationError> {
    let mut sqrt_price = pool.sqrt_price_x96;
    let mut tick = pool.tick;
    let mut liquidity = pool.liquidity;
    let mut amount_remaining = amount_in_raw;
    let mut outcome = SwapOutcome {
        amount_in_raw: U256::ZERO,
        amount_out_raw: U256::ZERO,
        protocol_fee_raw: U256::ZERO,
        lp_fee_raw: U256::ZERO,
        sqrt_price_x96: sqrt_price,
        tick,
        liquidity,
        initialized_ticks_crossed: 0,
    };
    // A swap starting without liquidity measures its price move from where it finds some.
    let mut sqrt_price_start = (liquidity > 0).then_some(sqrt_price);

    while amount_remaining > U256::ZERO {
        if sqrt_price_limit == Some(sqrt_price) {
            break;
        }
        let step_start = sqrt_price;
        let (next_tick, initialized) =
            next_initialized_tick_within_one_word(pool.ticks, tick, pool.tick_spacing, swap0_to1);
        if !(MIN_TICK..=MAX_TICK).contains(&next_tick) {
            return Err(SimulationError::InvalidInput(
                "Fluid v2 pool lacks the liquidity to fill the whole input".to_string(),
                None,
            ));
        }
        let sqrt_price_next = get_sqrt_ratio_at_tick(next_tick)?;
        sqrt_price = align_rounded_sqrt_price(sqrt_price, sqrt_price_next, swap0_to1)?;
        let target = match sqrt_price_limit {
            Some(limit) if swap0_to1 == (limit > sqrt_price_next) => limit,
            _ => sqrt_price_next,
        };

        let step = compute_swap_step(sqrt_price, target, liquidity, amount_remaining)?;
        let fees = apply_fees(step.amount_out, pool)?;
        amount_remaining = safe_sub_u256(amount_remaining, step.amount_in)?;
        outcome.amount_in_raw = safe_add_u256(outcome.amount_in_raw, step.amount_in)?;
        outcome.amount_out_raw = safe_add_u256(outcome.amount_out_raw, fees.amount_out)?;
        outcome.protocol_fee_raw = safe_add_u256(outcome.protocol_fee_raw, fees.protocol_fee)?;
        outcome.lp_fee_raw = safe_add_u256(outcome.lp_fee_raw, fees.lp_fee)?;
        sqrt_price = step.sqrt_price_next;

        if sqrt_price == sqrt_price_next {
            if initialized {
                let net = pool
                    .ticks
                    .get(&next_tick)
                    .copied()
                    .unwrap_or_default();
                liquidity = cross_tick(liquidity, net, swap0_to1)?;
                outcome.initialized_ticks_crossed += 1;
                if sqrt_price_start.is_none() && liquidity > 0 {
                    sqrt_price_start = Some(sqrt_price);
                }
            }
            tick = if swap0_to1 { next_tick - 1 } else { next_tick };
        } else if sqrt_price != step_start {
            tick = get_tick_at_sqrt_ratio(sqrt_price)?;
        }

        // Prices only move one way, so once past the bound the contract reverts at the end.
        if let Some(start) = sqrt_price_start {
            if sqrt_price_change(start, sqrt_price)? > U256::from(MAX_SQRT_PRICE_CHANGE) {
                return Err(SimulationError::InvalidInput(
                    "Fluid v2 swap moves the price more than 20%".to_string(),
                    None,
                ));
            }
        }
    }

    verify_sqrt_price_change_limits(sqrt_price_start.unwrap_or(sqrt_price), sqrt_price)?;
    outcome.sqrt_price_x96 = sqrt_price;
    outcome.tick = tick;
    outcome.liquidity = liquidity;
    Ok(outcome)
}

/// The furthest sqrt price one swap may reach from `sqrt_price_x96`.
pub(super) fn max_sqrt_price_move(
    sqrt_price_x96: U256,
    swap0_to1: bool,
) -> Result<U256, SimulationError> {
    let max_change =
        mul_div(sqrt_price_x96, U256::from(MAX_SQRT_PRICE_CHANGE), U256::from(TEN_DECIMALS))?;
    if swap0_to1 {
        safe_sub_u256(sqrt_price_x96, max_change)
    } else {
        safe_add_u256(sqrt_price_x96, max_change)
    }
}

/// The next initialized tick within the bitmap word of `tick`, or the word's edge, mirroring
/// `_nextInitializedTickWithinOneWord` over the initialized ticks in `ticks`.
pub(super) fn next_initialized_tick_within_one_word(
    ticks: &BTreeMap<i32, i128>,
    tick: i32,
    tick_spacing: i32,
    swap0_to1: bool,
) -> (i32, bool) {
    let mut compressed = tick.div_euclid(tick_spacing);
    if swap0_to1 {
        let word_start = (compressed >> 8) << 8;
        let found = ticks
            .range(word_start * tick_spacing..=compressed * tick_spacing)
            .next_back();
        match found {
            Some((&next, _)) => (next, true),
            None => (word_start * tick_spacing, false),
        }
    } else {
        compressed += 1;
        let word_end = ((compressed >> 8) << 8) + 255;
        let found = ticks
            .range(compressed * tick_spacing..=word_end * tick_spacing)
            .next();
        match found {
            Some((&next, _)) => (next, true),
            None => (word_end * tick_spacing, false),
        }
    }
}

/// The stored sqrt price is rounded down, so its tick's price can sit just past it in the swap
/// direction; the contract snaps to the tick price when the gap is at most 0.01%.
fn align_rounded_sqrt_price(
    sqrt_price: U256,
    sqrt_price_next: U256,
    swap0_to1: bool,
) -> Result<U256, SimulationError> {
    let crossed =
        if swap0_to1 { sqrt_price_next > sqrt_price } else { sqrt_price_next < sqrt_price };
    if !crossed {
        return Ok(sqrt_price);
    }
    let diff = sqrt_price_next.abs_diff(sqrt_price);
    if safe_mul_u256(diff, U256::from(FOUR_DECIMALS))? > sqrt_price {
        return Err(SimulationError::FatalError(
            "Fluid v2 sqrt price deviates more than 0.01% from its tick".to_string(),
        ));
    }
    Ok(sqrt_price_next)
}

struct SwapStep {
    sqrt_price_next: U256,
    amount_in: U256,
    amount_out: U256,
}

/// `_computeSwapStepForSwapInWithoutFee`: when the input does not reach the target, the step
/// consumes all of it.
fn compute_swap_step(
    sqrt_price: U256,
    sqrt_price_target: U256,
    liquidity: u128,
    amount_remaining: U256,
) -> Result<SwapStep, SimulationError> {
    let swap0_to1 = sqrt_price > sqrt_price_target;
    let amount_in_available = if swap0_to1 {
        amount0_delta(sqrt_price, sqrt_price_target, liquidity, true)?
    } else {
        get_amount1_delta(sqrt_price, sqrt_price_target, liquidity, true)?
    };
    let (sqrt_price_next, amount_in) = if amount_remaining >= amount_in_available {
        (sqrt_price_target, amount_in_available)
    } else {
        (
            get_next_sqrt_price_from_input(sqrt_price, liquidity, amount_remaining, swap0_to1)?,
            amount_remaining,
        )
    };
    let amount_out = if swap0_to1 {
        get_amount1_delta(sqrt_price, sqrt_price_next, liquidity, false)?
    } else {
        amount0_delta(sqrt_price, sqrt_price_next, liquidity, false)?
    };
    if amount_out >> MAX_STEP_AMOUNT_OUT_BITS != U256::ZERO {
        return Err(SimulationError::FatalError("Fluid v2 step amount out overflows".to_string()));
    }
    Ok(SwapStep { sqrt_price_next, amount_in, amount_out })
}

/// `SqrtPriceMath.getAmount0Delta`. The shared Uniswap helper rounds its intermediate product up
/// even when rounding the result down; that adds a unit when the floor of the product lands one
/// below a multiple of the lower price.
fn amount0_delta(
    sqrt_price_a: U256,
    sqrt_price_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let (lower, upper) = if sqrt_price_a > sqrt_price_b {
        (sqrt_price_b, sqrt_price_a)
    } else {
        (sqrt_price_a, sqrt_price_b)
    };
    if lower.is_zero() {
        return Err(SimulationError::FatalError("Fluid v2 sqrt price is zero".to_string()));
    }
    let numerator1 = U256::from(liquidity) << 96;
    let numerator2 = upper - lower;
    if round_up {
        let product = mul_div_rounding_up(numerator1, numerator2, upper)?;
        Ok(product.div_ceil(lower))
    } else {
        safe_div_u256(mul_div(numerator1, numerator2, upper)?, lower)
    }
}

struct StepFees {
    amount_out: U256,
    protocol_fee: U256,
    lp_fee: U256,
}

/// Takes the protocol fee, then the LP fee, from the step output, then moves the protocol's cut
/// of the LP fee to the protocol fee. Every fee rounds up by one unit.
fn apply_fees(amount_out: U256, pool: &SwapPool) -> Result<StepFees, SimulationError> {
    let mut amount_out = amount_out;
    let protocol_fee = fee_of(amount_out, pool.protocol_fee, SIX_DECIMALS)?;
    amount_out = amount_out.saturating_sub(protocol_fee);
    let lp_fee = fee_of(amount_out, pool.lp_fee, SIX_DECIMALS)?;
    amount_out = amount_out.saturating_sub(lp_fee);

    let cut = fee_of(lp_fee, pool.protocol_cut_fee, TWO_DECIMALS)?;
    let (protocol_fee, lp_fee) = if cut.is_zero() {
        (protocol_fee, lp_fee)
    } else if lp_fee > cut {
        (safe_add_u256(protocol_fee, cut)?, lp_fee - cut)
    } else {
        (safe_add_u256(protocol_fee, lp_fee)?, U256::ZERO)
    };
    Ok(StepFees { amount_out, protocol_fee, lp_fee })
}

/// `((amount * rate + 1) / precision) + 1`, or zero when either factor is zero.
fn fee_of(amount: U256, rate: u64, precision: u64) -> Result<U256, SimulationError> {
    if amount.is_zero() || rate == 0 {
        return Ok(U256::ZERO);
    }
    let numerator = safe_add_u256(safe_mul_u256(amount, U256::from(rate))?, U256::from(1u64))?;
    safe_add_u256(numerator / U256::from(precision), U256::from(1u64))
}

fn cross_tick(liquidity: u128, net: i128, swap0_to1: bool) -> Result<u128, SimulationError> {
    let delta = if swap0_to1 { -net } else { net };
    liquidity
        .checked_add_signed(delta)
        .ok_or_else(|| {
            SimulationError::FatalError("Fluid v2 active liquidity underflows".to_string())
        })
}

fn sqrt_price_change(start: U256, end: U256) -> Result<U256, SimulationError> {
    safe_div_u256(safe_mul_u256(start.abs_diff(end), U256::from(TEN_DECIMALS))?, start)
}

fn verify_sqrt_price_change_limits(start: U256, end: U256) -> Result<(), SimulationError> {
    let change = sqrt_price_change(start, end)?;
    if change > U256::from(MAX_SQRT_PRICE_CHANGE) || change < U256::from(MIN_SQRT_PRICE_CHANGE) {
        return Err(SimulationError::InvalidInput(
            "Fluid v2 swap moves the price outside [5e-10, 20%]".to_string(),
            None,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(entries: &[(i32, i128)]) -> BTreeMap<i32, i128> {
        entries.iter().copied().collect()
    }

    #[test]
    fn next_tick_downwards_finds_tick_at_or_below() {
        let ticks = ticks(&[(-20, 1), (10, 1), (30, -2)]);

        assert_eq!(next_initialized_tick_within_one_word(&ticks, 10, 10, true), (10, true));
        // Tick -20 sits in the previous bitmap word, so the search stops at this word's edge.
        assert_eq!(next_initialized_tick_within_one_word(&ticks, 9, 10, true), (0, false));
        assert_eq!(next_initialized_tick_within_one_word(&ticks, -1, 10, true), (-20, true));
        assert_eq!(next_initialized_tick_within_one_word(&ticks, -25, 10, true), (-2560, false));
    }

    #[test]
    fn next_tick_upwards_skips_current_tick() {
        let ticks = ticks(&[(-20, 1), (10, 1), (30, -2)]);

        assert_eq!(next_initialized_tick_within_one_word(&ticks, 10, 10, false), (30, true));
        assert_eq!(next_initialized_tick_within_one_word(&ticks, -21, 10, false), (-20, true));
        assert_eq!(next_initialized_tick_within_one_word(&ticks, 30, 10, false), (2550, false));
    }

    #[test]
    fn next_tick_stays_within_word_for_negative_ticks() {
        let ticks = ticks(&[(-300, 1)]);

        // compressed -1 lies in word -1, which spans compressed [-256, -1].
        assert_eq!(next_initialized_tick_within_one_word(&ticks, -1, 1, true), (-256, false));
        assert_eq!(next_initialized_tick_within_one_word(&ticks, -257, 1, true), (-300, true));
    }

    #[test]
    fn fees_round_up_and_cut_moves_to_protocol() {
        let ticks = BTreeMap::new();
        let pool = SwapPool {
            ticks: &ticks,
            tick_spacing: 1,
            tick: 0,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            protocol_fee: 0,
            protocol_cut_fee: 10,
            lp_fee: 100,
        };

        let fees = apply_fees(U256::from(1_000_000u64), &pool).unwrap();

        // lp fee ((1e6 * 100 + 1) / 1e6) + 1 = 101; cut ((101 * 10 + 1) / 100) + 1 = 11.
        assert_eq!(fees.amount_out, U256::from(999_899u64));
        assert_eq!(fees.lp_fee, U256::from(90u64));
        assert_eq!(fees.protocol_fee, U256::from(11u64));
    }

    #[test]
    fn max_sqrt_price_move_is_twenty_percent() {
        let start = U256::from(1_000_000_000_000u64);

        assert_eq!(max_sqrt_price_move(start, true).unwrap(), U256::from(800_000_000_000u64));
        assert_eq!(max_sqrt_price_move(start, false).unwrap(), U256::from(1_200_000_000_000u64));
    }
}
