//! Default `query_pool_swap` implementation for EVM protocols.
//!
//!
//! # Supported Constraints
//!
//! - [`SwapConstraint::PoolTargetPrice`]: Find amount to reach a target **spot price**
//! - [`SwapConstraint::TradeLimitPrice`]: Find max trade where **execution price** ≥ limit
//!
//! # Usage
//!
//! ```ignore
//! use tycho_simulation::evm::query_pool_swap::query_pool_swap;
//!
//! let result = query_pool_swap(&pool_state, &params)?;
//! let amount_in = result.amount_in();
//! let new_state = result.new_state();
//! ```

use num_bigint::BigUint;
use num_traits::{FromPrimitive, One, ToPrimitive, Zero};
use tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{
            GetAmountOutResult, PoolSwap, Price, PricePoint, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
};

const MAX_ITERATIONS: u32 = 30;
const IQI_THRESHOLD: f64 = 0.01;

/// Find the swap amount to reach a target price.
///
/// Uses a history-based Brent's method adapted for AMM price curves.
/// Combines inverse quadratic interpolation (IQI) and secant methods
/// with bisection fallback for robust convergence. The criteria for
/// choosing IQI over secant has been changed to a heuristic that performs
/// best for the VM protocols based on internal benchmark testing.
///
/// See [`ProtocolSim::query_pool_swap`] for the trait method this implements.
///
/// # Constraints
///
/// - `PoolTargetPrice`: Returns amount needed to move **spot price** to target. The resulting spot
///   price will be within `tolerance` of target.
///
/// - `TradeLimitPrice`: Returns max amount where **execution price** ≥ limit. Execution price =
///   amount_out / amount_in (decimal-adjusted).
///
/// # Returns
///
/// A [`PoolSwap`] containing:
/// - `amount_in`: Input token amount
/// - `amount_out`: Output token amount
/// - `new_state`: Pool state after the swap
/// - `price_points`: Search history for debugging (boundary points and probe measurements)
///
/// # Errors
///
/// Returns `SimulationError::InvalidInput` if the target is above spot, below the price at
/// `max_in`, or — for a trade-limit target — not reached at any probed size.
pub fn query_pool_swap(
    state: &dyn ProtocolSim,
    params: &QueryPoolSwapParams,
) -> Result<PoolSwap, SimulationError> {
    let token_in = params.token_in();
    let token_out = params.token_out();

    match params.swap_constraint() {
        SwapConstraint::TradeLimitPrice { limit, tolerance, .. } => {
            let scaled_trade_limit_price =
                price_to_f64_with_decimals(limit, token_in.decimals, token_out.decimals)?;
            search(state, scaled_trade_limit_price, *tolerance, token_in, token_out, true)
        }

        SwapConstraint::PoolTargetPrice { target, tolerance, .. } => {
            let scaled_pool_target_price =
                price_to_f64_with_decimals(target, token_in.decimals, token_out.decimals)?;
            search(state, scaled_pool_target_price, *tolerance, token_in, token_out, false)
        }
    }
}

/// Converts a Price to f64 with decimal adjustment for token pairs.
///
/// Applies decimal scaling in integer arithmetic before converting to f64 to preserve
/// precision for large numerator/denominator values.
///
/// # Arguments
/// * `price` - The price as a fraction (numerator/denominator)
/// * `decimals_in` - Decimals of the input token
/// * `decimals_out` - Decimals of the output token
///
/// # Returns
/// The price as f64, adjusted for decimal differences: `(num/den) * 10^(decimals_in -
/// decimals_out)`
fn price_to_f64_with_decimals(
    price: &Price,
    decimals_in: u32,
    decimals_out: u32,
) -> Result<f64, SimulationError> {
    let (scaled_num, scaled_den) = if decimals_in >= decimals_out {
        let scale = BigUint::from(10u64).pow(decimals_in - decimals_out);
        (&price.numerator * scale, price.denominator.clone())
    } else {
        let scale = BigUint::from(10u64).pow(decimals_out - decimals_in);
        (price.numerator.clone(), &price.denominator * scale)
    };

    let num_f64 = scaled_num.to_f64().ok_or_else(|| {
        SimulationError::InvalidInput("Price numerator too large for f64".into(), None)
    })?;
    let den_f64 = scaled_den.to_f64().ok_or_else(|| {
        SimulationError::InvalidInput("Price denominator too large for f64".into(), None)
    })?;

    if den_f64 == 0.0 {
        return Err(SimulationError::InvalidInput("Price denominator is zero".into(), None));
    }

    Ok(num_f64 / den_f64)
}

/// Core search using Brent's method adapted for AMM price curves.
///
/// Maintains a bracket [low, high] where price(low) > target > price(high).
/// Each iteration narrows the bracket using [`next_amount`] until convergence
/// or the bracket width reaches 1 (integer precision limit).
///
/// Targeting an execution price, the low end of the bracket comes from
/// [`probe_execution_price`]: a positive low end halves the ratio `high / low` each step, where
/// `low` at zero degrades [`geometric_mean`] to arithmetic halving.
///
/// Based on the van Wijngaarden-Dekker-Brent method for root finding.
/// See: Brent, R.P. (1973). "Algorithms for Minimization without Derivatives"
fn search(
    state: &dyn ProtocolSim,
    target_price: f64,
    tolerance: f64,
    token_in: &Token,
    token_out: &Token,
    use_trade_price: bool,
) -> Result<PoolSwap, SimulationError> {
    let spot = state.spot_price(token_in, token_out)?;

    if target_price > spot {
        return Err(SimulationError::InvalidInput(
            format!("Target {} > spot {}", target_price, spot),
            None,
        ));
    }

    let (max_in, _) = state.get_limits(token_in.address.clone(), token_out.address.clone())?;
    let limit_result = state.get_amount_out(max_in.clone(), token_in, token_out)?;

    let limit_price = if use_trade_price {
        calculate_trade_price(
            max_in.to_f64().unwrap_or(f64::MAX),
            limit_result
                .amount
                .to_f64()
                .unwrap_or(0.0),
            token_in.decimals,
            token_out.decimals,
        )
    } else {
        limit_result
            .new_state
            .spot_price(token_in, token_out)?
    };

    if target_price < limit_price {
        return Err(SimulationError::InvalidInput(
            format!("Target {} < limit {}", target_price, limit_price),
            None,
        ));
    }

    let mut low = BigUint::zero();
    let mut high = max_in.clone();

    // In trade mode zero is not on the curve: execution starts at the fee-adjusted rate.
    let mut price_points: Vec<PricePoint> = if use_trade_price {
        vec![PricePoint::new(max_in.clone(), limit_result.amount.clone(), limit_price)]
    } else {
        vec![
            PricePoint::new(BigUint::zero(), BigUint::zero(), spot),
            PricePoint::new(max_in.clone(), limit_result.amount.clone(), limit_price),
        ]
    };

    // A zero-amount seed answers a spot target the pool already meets; in trade mode the seed
    // must come from a probe that was actually priced, or a miss would report a zero depth.
    let (mut best, mut best_error) = if use_trade_price {
        let probed = probe_execution_price(
            state,
            spot,
            target_price,
            token_in,
            token_out,
            &max_in,
            &mut price_points,
        )?;

        // `max_in` is only a candidate when it prices exactly at the target: the caller already
        // rejected anything below it, and anything above it a probe would have found.
        let probe = match probed {
            Some(probe) => Some(probe),
            None if limit_price >= target_price => Some((max_in, limit_result, limit_price)),
            None => None,
        };

        // A target no probe reaches is out of reach at any size.
        let Some((probe_amount, probe_result, probe_price)) = probe else {
            let best_measured = price_points
                .iter()
                .map(|point| point.price)
                .fold(f64::NEG_INFINITY, f64::max);
            return Err(SimulationError::InvalidInput(
                format!(
                    "no trade size reaches target price {target_price}: the best execution price \
                     measured was {best_measured} (spot {spot})"
                ),
                None,
            ));
        };

        // The probe seeds the bracket and the fallback answer but skips the tolerance
        // early-exit: quantized dust prices land inside the tolerance band routinely.
        let probe_swap = PoolSwap::new(
            probe_amount.clone(),
            probe_result.amount.clone(),
            probe_result.new_state.clone(),
            Some(price_points.clone()),
        );
        low = probe_amount;
        (probe_swap, (probe_price - target_price) / target_price)
    } else {
        (
            PoolSwap::new(
                BigUint::zero(),
                BigUint::zero(),
                state.clone_box(),
                Some(price_points.clone()),
            ),
            (spot - target_price) / target_price,
        )
    };

    for _ in 0..MAX_ITERATIONS {
        let amount = next_amount(&price_points, &low, &high, target_price);

        // f64 cannot resolve the gap; re-probing a measured point cannot move the bracket.
        if amount <= low || amount >= high {
            break;
        }

        let result = state.get_amount_out(amount.clone(), token_in, token_out)?;

        let price = if use_trade_price {
            calculate_trade_price(
                amount.to_f64().unwrap_or(0.0),
                result.amount.to_f64().unwrap_or(0.0),
                token_in.decimals,
                token_out.decimals,
            )
        } else {
            result
                .new_state
                .spot_price(token_in, token_out)?
        };

        price_points.push(PricePoint::new(amount.clone(), result.amount.clone(), price));

        if price >= target_price {
            let error = (price - target_price) / target_price;
            if error < best_error {
                best_error = error;
                best = PoolSwap::new(
                    amount.clone(),
                    result.amount.clone(),
                    result.new_state.clone(),
                    Some(price_points.clone()),
                );
            }
        }

        if is_within_tolerance(price, target_price, tolerance) {
            return Ok(PoolSwap::new(amount, result.amount, result.new_state, Some(price_points)));
        }

        if price > target_price {
            low = amount;
        } else {
            high = amount;
        }

        if &high - &low <= BigUint::one() {
            break;
        }
    }

    Ok(best)
}

/// Probes power-of-ten inputs from [`first_nonzero_output_exponent`] up to `max_in`, returning
/// the smallest one that executes at or above `target_price` with its swap result and price.
///
/// Dust trades understate the execution rate (outputs floor, integer fees round up, VM adapters
/// carry fixed-point error), so measured prices rise with size until the true, decreasing curve
/// takes over. A probe pricing below its predecessor is past that peak, so no larger amount can
/// clear the target and `None` is returned.
///
/// # Errors
///
/// Propagates a probe's error once an earlier probe has priced, and the last error seen when
/// no probe could be priced at all.
fn probe_execution_price(
    state: &dyn ProtocolSim,
    spot: f64,
    target_price: f64,
    token_in: &Token,
    token_out: &Token,
    max_in: &BigUint,
    price_points: &mut Vec<PricePoint>,
) -> Result<Option<(BigUint, GetAmountOutResult, f64)>, SimulationError> {
    // `10^bits(max_in)` exceeds `max_in`, so the clamp bounds the allocation for garbage
    // decimals without changing which probes run.
    let max_exponent = u32::try_from(max_in.bits()).unwrap_or(u32::MAX);
    let first_exponent = first_nonzero_output_exponent(spot, token_in.decimals, token_out.decimals)
        .min(max_exponent);
    let mut amount = BigUint::from(10u64).pow(first_exponent);
    let mut previous_price = 0.0;
    let mut priced_any_probe = false;
    let mut last_probe_error = None;

    while &amount <= max_in {
        // A rejection below an adapter's minimum describes the probe, not the pool: step up.
        let result = match state.get_amount_out(amount.clone(), token_in, token_out) {
            Ok(result) => result,
            Err(error) if priced_any_probe => return Err(error),
            Err(error) => {
                last_probe_error = Some(error);
                amount *= 10u32;
                continue;
            }
        };
        priced_any_probe = true;

        let price = calculate_trade_price(
            amount.to_f64().unwrap_or(0.0),
            result.amount.to_f64().unwrap_or(0.0),
            token_in.decimals,
            token_out.decimals,
        );
        price_points.push(PricePoint::new(amount.clone(), result.amount.clone(), price));

        if price >= target_price {
            return Ok(Some((amount, result, price)));
        }
        if price < previous_price {
            break;
        }
        previous_price = price;
        amount *= 10u32;
    }

    match last_probe_error {
        Some(error) if !priced_any_probe => Err(error),
        _ => Ok(None),
    }
}

/// Exponent of the smallest power-of-ten input expected to return a nonzero output:
/// `amount_in = 10^(decimals_in - decimals_out) / spot` yields one raw unit of the output token.
/// Never below one wei.
fn first_nonzero_output_exponent(spot: f64, decimals_in: u32, decimals_out: u32) -> u32 {
    if spot <= 0.0 || !spot.is_finite() {
        return 0;
    }
    let exponent = (decimals_in as f64 - decimals_out as f64 - spot.log10()).ceil();
    exponent.max(0.0) as u32
}

/// Calculates the trade price as `amount_out / amount_in`, adjusted for decimal differences.
#[inline]
fn calculate_trade_price(
    amount_in: f64,
    amount_out: f64,
    decimals_in: u32,
    decimals_out: u32,
) -> f64 {
    if amount_in <= 0.0 {
        return f64::MAX;
    }
    (amount_out / amount_in) * 10_f64.powi(decimals_in as i32 - decimals_out as i32)
}

/// Returns true if `actual` is within `[target, target * (1 + tolerance)]`.
#[inline]
pub(crate) fn is_within_tolerance(actual: f64, target: f64, tolerance: f64) -> bool {
    actual >= target && actual <= target * (1.0 + tolerance)
}

/// Select next amount to evaluate using heuristics.
///
/// Tries methods in order of convergence speed:
/// 1. IQI (if 3+ points and estimate improves bracket by >1%)
/// 2. Secant (if 2+ points and estimate is within bracket)
/// 3. Geometric mean bisection (fallback)
fn next_amount(
    history: &[PricePoint],
    low: &BigUint,
    high: &BigUint,
    target_price: f64,
) -> BigUint {
    let low_f64 = low.to_f64().unwrap_or(0.0);
    let high_f64 = high.to_f64().unwrap_or(f64::MAX);
    let bracket = high_f64 - low_f64;
    let len = history.len();

    if len >= 3 {
        if let Some(est) = iqi(&history[len - 3..], target_price) {
            if est > low_f64 && est < high_f64 {
                let improvement = (est - low_f64).min(high_f64 - est);
                if improvement > bracket * IQI_THRESHOLD {
                    if let Some(amt) = BigUint::from_f64(est) {
                        if &amt > low && &amt < high {
                            return amt;
                        }
                    }
                }
            }
        }
    }

    if len >= 2 {
        if let Some(est) = secant(&history[len - 2..], target_price) {
            if est > low_f64 && est < high_f64 {
                if let Some(amt) = BigUint::from_f64(est) {
                    if &amt > low && &amt < high {
                        return amt;
                    }
                }
            }
        }
    }

    geometric_mean(low, high)
}

/// Inverse Quadratic Interpolation (IQI) - estimates amount for target price.
///
/// Fits a quadratic through 3 points in (price, amount) space and solves for
/// the amount where price equals target. This is the "inverse" of standard
/// quadratic interpolation which would estimate price from amount.
///
/// Part of Brent's method. See: <https://en.wikipedia.org/wiki/Brent%27s_method>
fn iqi(p: &[PricePoint], target_price: f64) -> Option<f64> {
    if p.len() != 3 {
        return None;
    }
    let (a0, a1, a2) = (
        p[0].amount_in.to_f64().unwrap_or(0.0),
        p[1].amount_in.to_f64().unwrap_or(0.0),
        p[2].amount_in.to_f64().unwrap_or(0.0),
    );
    let (pr0, pr1, pr2) = (p[0].price, p[1].price, p[2].price);

    let d1 = (pr0 - pr1) * (pr0 - pr2);
    let d2 = (pr1 - pr0) * (pr1 - pr2);
    let d3 = (pr2 - pr0) * (pr2 - pr1);

    let result = a0 * (target_price - pr1) * (target_price - pr2) / d1 +
        a1 * (target_price - pr0) * (target_price - pr2) / d2 +
        a2 * (target_price - pr0) * (target_price - pr1) / d3;

    (result.is_finite() && result > 0.0).then_some(result)
}

/// Secant method - linear interpolation to estimate amount for target price.
///
/// Uses 2 points to draw a line and find where it crosses the target price.
fn secant(p: &[PricePoint], target_price: f64) -> Option<f64> {
    if p.len() != 2 {
        return None;
    }
    let (a0, a1) = (p[0].amount_in.to_f64().unwrap_or(0.0), p[1].amount_in.to_f64().unwrap_or(0.0));
    let (pr0, pr1) = (p[0].price, p[1].price);
    let dp = pr1 - pr0;
    let result = a1 - (pr1 - target_price) * (a1 - a0) / dp;
    (result.is_finite() && result > 0.0).then_some(result)
}

fn geometric_mean(a: &BigUint, b: &BigUint) -> BigUint {
    let a_f64 = a.to_f64().unwrap_or(0.0);
    let b_f64 = b.to_f64().unwrap_or(f64::MAX);
    if a_f64 <= 0.0 || b_f64 <= 0.0 {
        return (a + b) / 2u32;
    }
    BigUint::from_f64((a_f64 * b_f64).sqrt()).unwrap_or_else(|| (a + b) / 2u32)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_common::{hex_bytes::Bytes, models::Chain, simulation::protocol_sim::Price};

    use super::*;
    use crate::evm::protocol::{
        uniswap_v2::state::UniswapV2State,
        uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
        utils::uniswap::tick_list::TickInfo,
    };

    fn create_token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn token_0() -> Token {
        create_token("0x0000000000000000000000000000000000000000", "T0", 18)
    }

    fn token_1() -> Token {
        create_token("0x0000000000000000000000000000000000000001", "T1", 18)
    }

    // =========================================================================
    // Tests for within_tolerance
    // =========================================================================

    #[test]
    fn test_within_tolerance_exact() {
        assert!(is_within_tolerance(1.0, 1.0, 0.001));
        assert!(is_within_tolerance(1000.0, 1000.0, 0.001));
        assert!(is_within_tolerance(0.001, 0.001, 0.001));
    }

    #[test]
    fn test_within_tolerance_above_within_range() {
        let tolerance = 0.001;
        assert!(is_within_tolerance(1.0005, 1.0, tolerance));
        assert!(is_within_tolerance(1.001, 1.0, tolerance));
    }

    #[test]
    fn test_within_tolerance_above_out_of_range() {
        let tolerance = 0.001;
        assert!(!is_within_tolerance(1.002, 1.0, tolerance));
        assert!(!is_within_tolerance(1.01, 1.0, tolerance));
    }

    #[test]
    fn test_within_tolerance_below_target() {
        let tolerance = 0.001;
        assert!(!is_within_tolerance(0.999, 1.0, tolerance));
        assert!(!is_within_tolerance(0.9999, 1.0, tolerance));
        assert!(!is_within_tolerance(0.0, 1.0, tolerance));
    }

    #[test]
    fn test_within_tolerance_zero_tolerance() {
        assert!(is_within_tolerance(1.0, 1.0, 0.0));
        assert!(!is_within_tolerance(1.0001, 1.0, 0.0));
    }

    // =========================================================================
    // Tests for geometric_mean
    // =========================================================================

    #[test]
    fn test_geometric_mean_basic() {
        let a = BigUint::from(100u32);
        let b = BigUint::from(400u32);
        assert_eq!(geometric_mean(&a, &b), BigUint::from(200u32));
    }

    #[test]
    fn test_geometric_mean_same_values() {
        let a = BigUint::from(100u32);
        assert_eq!(geometric_mean(&a, &a), BigUint::from(100u32));
    }

    #[test]
    fn test_geometric_mean_one_and_large() {
        let a = BigUint::one();
        let b = BigUint::from(1000000u32);
        assert_eq!(geometric_mean(&a, &b), BigUint::from(1000u32));
    }

    #[test]
    fn test_geometric_mean_with_zero() {
        let a = BigUint::from(0u32);
        let b = BigUint::from(100u32);
        assert_eq!(geometric_mean(&a, &b), BigUint::from(50u32));
    }

    #[test]
    fn test_geometric_mean_adjacent() {
        let a = BigUint::from(10u32);
        let b = BigUint::from(11u32);
        let result = geometric_mean(&a, &b);
        assert!(result == BigUint::from(10u32) || result == BigUint::from(11u32));
    }

    // =========================================================================
    // Tests for trade_price
    // =========================================================================

    #[test]
    fn test_trade_price_basic() {
        let price = calculate_trade_price(50.0, 100.0, 18, 18);
        assert!((price - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_trade_price_decimal_adjustment() {
        let price = calculate_trade_price(50.0, 100.0, 6, 18);
        assert!((price - 2e-12).abs() < 1e-18);
    }

    #[test]
    fn test_trade_price_reverse_decimal_adjustment() {
        let price = calculate_trade_price(50.0, 100.0, 18, 6);
        assert!((price - 2e12).abs() < 1.0);
    }

    #[test]
    fn test_trade_price_zero_input() {
        assert_eq!(calculate_trade_price(0.0, 100.0, 18, 18), f64::MAX);
    }

    #[test]
    fn test_trade_price_negative_input() {
        assert_eq!(calculate_trade_price(-1.0, 100.0, 18, 18), f64::MAX);
    }

    // =========================================================================
    // Tests for iqi (Inverse Quadratic Interpolation)
    // =========================================================================

    #[test]
    fn test_iqi_linear_data() {
        let points = vec![
            PricePoint::new(BigUint::from(1u32), BigUint::zero(), 2.0),
            PricePoint::new(BigUint::from(2u32), BigUint::zero(), 4.0),
            PricePoint::new(BigUint::from(3u32), BigUint::zero(), 6.0),
        ];
        let result = iqi(&points, 3.5);
        assert!(result.is_some());
        assert!((result.unwrap() - 1.75).abs() < 0.1);
    }

    #[test]
    fn test_iqi_same_prices() {
        let points = vec![
            PricePoint::new(BigUint::from(1u32), BigUint::zero(), 5.0),
            PricePoint::new(BigUint::from(2u32), BigUint::zero(), 5.0),
            PricePoint::new(BigUint::from(3u32), BigUint::zero(), 5.0),
        ];
        assert!(iqi(&points, 5.0).is_none());
    }

    #[test]
    fn test_iqi_two_points() {
        let points = vec![
            PricePoint::new(BigUint::from(1u32), BigUint::zero(), 2.0),
            PricePoint::new(BigUint::from(2u32), BigUint::zero(), 4.0),
        ];
        assert!(iqi(&points, 3.0).is_none());
    }

    #[test]
    fn test_iqi_quadratic_data() {
        let points = vec![
            PricePoint::new(BigUint::from(100u32), BigUint::zero(), 1.0),
            PricePoint::new(BigUint::from(200u32), BigUint::zero(), 0.5),
            PricePoint::new(BigUint::from(400u32), BigUint::zero(), 0.25),
        ];
        let result = iqi(&points, 0.4);
        assert!(result.is_some());
        let est = result.unwrap();
        assert!(est > 200.0 && est < 400.0);
    }

    // =========================================================================
    // Tests for secant
    // =========================================================================

    #[test]
    fn test_secant_basic() {
        let points = vec![
            PricePoint::new(BigUint::from(1u32), BigUint::zero(), 2.0),
            PricePoint::new(BigUint::from(3u32), BigUint::zero(), 6.0),
        ];
        let result = secant(&points, 4.0);
        assert!(result.is_some());
        assert!((result.unwrap() - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_secant_same_prices() {
        let points = vec![
            PricePoint::new(BigUint::from(1u32), BigUint::zero(), 5.0),
            PricePoint::new(BigUint::from(2u32), BigUint::zero(), 5.0),
        ];
        let result = secant(&points, 5.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_secant_one_point() {
        let points = vec![PricePoint::new(BigUint::from(1u32), BigUint::zero(), 2.0)];
        assert!(secant(&points, 3.0).is_none());
    }

    #[test]
    fn test_secant_negative_result() {
        let points = vec![
            PricePoint::new(BigUint::from(1u32), BigUint::zero(), 2.0),
            PricePoint::new(BigUint::from(2u32), BigUint::zero(), 1.0),
        ];
        let result = secant(&points, 5.0);
        assert!(result.is_none());
    }

    // =========================================================================
    // Tests for next_amount
    // =========================================================================

    #[test]
    fn test_next_amount_fallback_to_geometric_mean() {
        let history = vec![PricePoint::new(BigUint::from(100u32), BigUint::zero(), 1.0)];
        let low = BigUint::from(100u32);
        let high = BigUint::from(400u32);
        let result = next_amount(&history, &low, &high, 0.5);
        assert_eq!(result, BigUint::from(200u32));
    }

    #[test]
    fn test_next_amount_uses_secant() {
        let history = vec![
            PricePoint::new(BigUint::from(100u32), BigUint::zero(), 1.0),
            PricePoint::new(BigUint::from(400u32), BigUint::zero(), 0.25),
        ];
        let low = BigUint::from(100u32);
        let high = BigUint::from(400u32);
        let result = next_amount(&history, &low, &high, 0.5);
        assert!(result > low && result < high);
    }

    // =========================================================================
    // Integration tests - Pool setup helpers
    // =========================================================================

    mod pool_setup {
        use std::collections::HashMap;

        use revm::{
            primitives::KECCAK_EMPTY,
            state::{AccountInfo, Bytecode},
        };
        use serde_json::Value;
        use tycho_client::feed::BlockHeader;

        use super::*;
        use crate::evm::{
            engine_db::{
                create_engine, engine_db_interface::EngineDatabaseInterface, tycho_db::PreCachedDB,
                SHARED_TYCHO_DB,
            },
            protocol::{
                uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
                utils::{bytes_to_address, uniswap::tick_list::TickInfo},
                vm::{
                    constants::{BALANCER_V2, ERC20_PROXY_BYTECODE},
                    state::EVMPoolState,
                    state_builder::EVMPoolStateBuilder,
                },
            },
            simulation::SimulationEngine,
            tycho_models::AccountUpdate,
        };

        pub async fn v2_same_decimals() -> (Box<dyn ProtocolSim>, Token, Token) {
            let state = UniswapV2State::new(
                U256::from(1000u64) * U256::from(10u64).pow(U256::from(18u64)),
                U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(18u64)),
            );
            let token_in = create_token("0x0000000000000000000000000000000000000001", "DAI", 18);
            let token_out = create_token("0x0000000000000000000000000000000000000000", "WETH", 18);
            (Box::new(state), token_in, token_out)
        }

        pub async fn v2_different_decimals() -> (Box<dyn ProtocolSim>, Token, Token) {
            let state = UniswapV2State::new(
                U256::from(1000u64) * U256::from(10u64).pow(U256::from(18u64)),
                U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(6u64)),
            );
            let token_in = create_token("0x0000000000000000000000000000000000000001", "USDC", 6);
            let token_out = create_token("0x0000000000000000000000000000000000000000", "WETH", 18);
            (Box::new(state), token_in, token_out)
        }

        pub async fn v3_different_decimals() -> (Box<dyn ProtocolSim>, Token, Token) {
            let state = UniswapV3State::new(
                377952820878029838,
                U256::from_str("28437325270877025820973479874632004").unwrap(),
                FeeAmount::Low,
                255830,
                vec![
                    TickInfo::new(255760, 1759015528199933i128).unwrap(),
                    TickInfo::new(255770, 6393138051835308i128).unwrap(),
                    TickInfo::new(255780, 228206673808681i128).unwrap(),
                    TickInfo::new(255820, 1319490609195820i128).unwrap(),
                    TickInfo::new(255830, 678916926147901i128).unwrap(),
                    TickInfo::new(255840, 12208947683433103i128).unwrap(),
                    TickInfo::new(255850, 1177970713095301i128).unwrap(),
                    TickInfo::new(255860, 8752304680520407i128).unwrap(),
                    TickInfo::new(255880, 1486478248067104i128).unwrap(),
                    TickInfo::new(255890, 1878744276123248i128).unwrap(),
                    TickInfo::new(255900, 77340284046725227i128).unwrap(),
                ],
            )
            .unwrap();
            let token_in = create_token("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599", "WBTC", 8);
            let token_out = create_token("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", "WETH", 18);
            (Box::new(state), token_in, token_out)
        }

        fn balancer_dai() -> Token {
            create_token("0x6b175474e89094c44da98b954eedeac495271d0f", "DAI", 18)
        }

        fn balancer_bal() -> Token {
            create_token("0xba100000625a3754423978a60c9317c58a424e3d", "BAL", 18)
        }

        async fn setup_balancer_pool() -> EVMPoolState<PreCachedDB> {
            SHARED_TYCHO_DB
                .clear()
                .expect("Failed to clear SHARED TX");

            let data_str =
                include_str!("protocol/vm/assets/balancer_contract_storage_block_20463609.json");
            let data: Value = serde_json::from_str(data_str).expect("Failed to parse JSON");

            let accounts: Vec<AccountUpdate> = serde_json::from_value(data["accounts"].clone())
                .expect("Expected accounts to match AccountUpdate structure");

            let db = SHARED_TYCHO_DB.clone();
            let engine: SimulationEngine<_> = create_engine(db.clone(), false).unwrap();

            let block = BlockHeader {
                number: 20463609,
                hash: Bytes::from_str(
                    "0x4315fd1afc25cc2ebc72029c543293f9fd833eeb305e2e30159459c827733b1b",
                )
                .unwrap(),
                timestamp: 1722875891,
                ..Default::default()
            };

            for account in accounts.clone() {
                engine
                    .state
                    .init_account(
                        account.address,
                        AccountInfo {
                            balance: account.balance.unwrap_or_default(),
                            nonce: 0u64,
                            code_hash: KECCAK_EMPTY,
                            code: account
                                .code
                                .clone()
                                .map(|arg0: Vec<u8>| Bytecode::new_raw(arg0.into())),
                        },
                        None,
                        false,
                    )
                    .expect("Failed to initialize account");
            }
            db.update(accounts, Some(block))
                .unwrap();

            let tokens = vec![balancer_dai().address, balancer_bal().address];
            for token in &tokens {
                engine
                    .state
                    .init_account(
                        bytes_to_address(token).unwrap(),
                        AccountInfo {
                            balance: U256::from(0),
                            nonce: 0,
                            code_hash: KECCAK_EMPTY,
                            code: Some(Bytecode::new_raw(ERC20_PROXY_BYTECODE.into())),
                        },
                        None,
                        true,
                    )
                    .expect("Failed to initialize account");
            }

            let block = BlockHeader {
                number: 18485417,
                hash: Bytes::from_str(
                    "0x28d41d40f2ac275a4f5f621a636b9016b527d11d37d610a45ac3a821346ebf8c",
                )
                .expect("Invalid block hash"),
                timestamp: 0,
                ..Default::default()
            };
            db.update(vec![], Some(block.clone()))
                .unwrap();

            let pool_id: String =
                "0x4626d81b3a1711beb79f4cecff2413886d461677000200000000000000000011".into();

            let stateless_contracts = HashMap::from([(
                String::from("0x3de27efa2f1aa663ae5d458857e731c129069f29"),
                Some(Vec::new()),
            )]);

            let dai_addr = bytes_to_address(&balancer_dai().address).unwrap();
            let bal_addr = bytes_to_address(&balancer_bal().address).unwrap();
            let balances = HashMap::from([
                (dai_addr, U256::from_str("178754012737301807104").unwrap()),
                (bal_addr, U256::from_str("91082987763369885696").unwrap()),
            ]);
            let adapter_address =
                alloy::primitives::Address::from_str("0xA2C5C98A892fD6656a7F39A2f63228C0Bc846270")
                    .unwrap();

            #[allow(deprecated)]
            EVMPoolStateBuilder::new(pool_id, tokens, adapter_address)
                .balances(balances)
                .balance_owner(
                    alloy::primitives::Address::from_str(
                        "0xBA12222222228d8Ba445958a75a0704d566BF2C8",
                    )
                    .unwrap(),
                )
                .adapter_contract_bytecode(Bytecode::new_raw(BALANCER_V2.into()))
                .stateless_contracts(stateless_contracts)
                .build(SHARED_TYCHO_DB.clone())
                .await
                .expect("Failed to build pool state")
        }

        pub async fn balancer_same_decimals() -> (Box<dyn ProtocolSim>, Token, Token) {
            let token_in = balancer_dai();
            let token_out = balancer_bal();
            let state = setup_balancer_pool().await;

            let prime_result = state
                .get_amount_out(BigUint::from(1_000_000_000_000_000_000u128), &token_in, &token_out)
                .expect("Failed to prime spot prices");
            let primed_state = prime_result
                .new_state
                .as_any()
                .downcast_ref::<EVMPoolState<PreCachedDB>>()
                .unwrap()
                .clone();

            (Box::new(primed_state), token_in, token_out)
        }
    }

    fn to_price(price_f64: f64, token_in: &Token, token_out: &Token) -> Price {
        let decimal_adj = 10_f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
        let price_no_decimals = price_f64 / decimal_adj;
        Price::new(BigUint::from((price_no_decimals * 1e18) as u128), BigUint::from(10u128.pow(18)))
    }

    // =========================================================================
    // Integration tests - PoolTargetPrice (all pool types)
    // =========================================================================

    #[rstest]
    #[case::v2_same_decimals(pool_setup::v2_same_decimals(), 0.99)]
    #[case::v2_different_decimals(pool_setup::v2_different_decimals(), 0.95)]
    #[case::v3_different_decimals(pool_setup::v3_different_decimals(), 0.998)]
    #[case::balancer_same_decimals(pool_setup::balancer_same_decimals(), 0.98)]
    #[tokio::test]
    async fn test_query_pool_swap_pool_target_price(
        #[case]
        #[future]
        pool: (Box<dyn ProtocolSim>, Token, Token),
        #[case] price_multiplier: f64,
    ) {
        let (state, token_in, token_out) = pool.await;
        let tolerance_bps = 10f64;

        let spot_price = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target_price_f64 = spot_price * price_multiplier;
        let target_price = to_price(target_price_f64, &token_in, &token_out);

        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: tolerance_bps / 10000.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(state.as_ref(), &params);
        assert!(result.is_ok(), "query_pool_swap failed: {:?}", result.err());

        let pool_swap = result.unwrap();
        assert!(pool_swap.amount_in() > &BigUint::zero(), "amount_in should be > 0");

        let new_spot = pool_swap
            .new_state()
            .spot_price(&token_in, &token_out)
            .unwrap();
        let error_bps = ((new_spot - target_price_f64) / target_price_f64).abs() * 10000.0;
        assert!(
            error_bps < tolerance_bps,
            "New spot {} should be within {}bps of target {}, got {}bps",
            new_spot,
            tolerance_bps,
            target_price_f64,
            error_bps
        );
    }

    // =========================================================================
    // Integration tests - TradeLimitPrice (all pool types)
    // =========================================================================

    #[rstest]
    #[case::v2_same_decimals(pool_setup::v2_same_decimals(), 0.99)]
    #[case::v2_different_decimals(pool_setup::v2_different_decimals(), 0.95)]
    #[case::v3_different_decimals(pool_setup::v3_different_decimals(), 0.998)]
    #[case::balancer_same_decimals(pool_setup::balancer_same_decimals(), 0.98)]
    #[tokio::test]
    async fn test_query_pool_swap_trade_limit_price(
        #[case]
        #[future]
        pool: (Box<dyn ProtocolSim>, Token, Token),
        #[case] price_multiplier: f64,
    ) {
        let (state, token_in, token_out) = pool.await;

        let spot_price = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let limit_price_f64 = spot_price * price_multiplier;
        let limit_price = to_price(limit_price_f64, &token_in, &token_out);

        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: limit_price,
                tolerance: 0.001,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(state.as_ref(), &params);
        assert!(result.is_ok(), "query_pool_swap failed: {:?}", result.err());

        let pool_swap = result.unwrap();
        assert!(pool_swap.amount_in() > &BigUint::zero(), "amount_in should be > 0");
        assert!(pool_swap.amount_out() > &BigUint::zero(), "amount_out should be > 0");

        let actual_trade_price = calculate_trade_price(
            pool_swap.amount_in().to_f64().unwrap(),
            pool_swap.amount_out().to_f64().unwrap(),
            token_in.decimals,
            token_out.decimals,
        );
        assert!(
            actual_trade_price >= limit_price_f64,
            "Trade price {} should be >= limit {}",
            actual_trade_price,
            limit_price_f64
        );
    }

    // =========================================================================
    // Edge cases and algorithm tests (V2 only - not pool-specific)
    // =========================================================================

    #[test]
    fn test_query_pool_swap_pool_target_price_unreachable() {
        let state = UniswapV2State::new(U256::from(2_000_000u32), U256::from(1_000_000u32));
        let token_in = token_0();
        let token_out = token_1();

        let target_price = Price::new(BigUint::from(1u32), BigUint::from(1u32));

        let params = QueryPoolSwapParams::new(
            token_in,
            token_out,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(&state, &params);
        assert!(result.is_err(), "Should return error for unreachable price");
    }

    #[test]
    fn test_query_pool_swap_trade_limit_price_maximizes_trade() {
        let state = UniswapV2State::new(
            U256::from(1000u64) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(18u64)),
        );
        let token_in = create_token("0x0000000000000000000000000000000000000001", "DAI", 18);
        let token_out = create_token("0x0000000000000000000000000000000000000000", "WETH", 18);
        let spot_price = state
            .spot_price(&token_in, &token_out)
            .unwrap();

        let mut prev_amount = BigUint::zero();
        for multiplier in [0.99, 0.95, 0.90, 0.80] {
            let limit_price_f64 = spot_price * multiplier;
            let limit_price = to_price(limit_price_f64, &token_in, &token_out);

            let params = QueryPoolSwapParams::new(
                token_in.clone(),
                token_out.clone(),
                SwapConstraint::TradeLimitPrice {
                    limit: limit_price,
                    tolerance: 0.001,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            );

            let result = query_pool_swap(&state, &params);
            assert!(result.is_ok(), "query_pool_swap failed at multiplier {}", multiplier);

            let amount = result.unwrap().amount_in().clone();
            assert!(
                amount >= prev_amount,
                "Lower price limit should allow larger trade: {} vs {}",
                amount,
                prev_amount
            );
            prev_amount = amount;
        }
    }

    /// An unreachable trade-limit target must surface as an error, not a zero-amount swap that
    /// callers would store as a measured depth. A limit within `(1 - fee)^2` of spot qualifies.
    #[test]
    fn test_trade_limit_price_unreachable_target_errors() {
        let state = UniswapV2State::new(U256::from(1_000_000_000u64), U256::from(1_000_000_000u64));
        let token_in = token_0();
        let token_out = token_1();

        let spot = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        // UniswapV2 charges 0.3% and spot carries the reciprocal markup, so no amount executes
        // within 0.1% of spot.
        let target_price = to_price(spot * 0.999, &token_in, &token_out);

        let params = QueryPoolSwapParams::new(
            token_in,
            token_out,
            SwapConstraint::TradeLimitPrice {
                limit: target_price,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        match query_pool_swap(&state, &params) {
            Err(SimulationError::InvalidInput(msg, _)) => {
                // The best measured price must be the fee-implied ceiling `spot * (1 - fee)^2`,
                // not an artefact of giving up early.
                assert!(
                    msg.contains("no trade size reaches target price"),
                    "expected an unreachable-target error, got: {msg}"
                );
                let measured: f64 = msg
                    .split("measured was ")
                    .nth(1)
                    .and_then(|rest| rest.split(' ').next())
                    .expect("error should report the best measured price")
                    .parse()
                    .expect("measured price should parse");
                let ceiling = spot * 0.997 * 0.997;
                assert!(
                    (measured - ceiling).abs() / ceiling < 1e-3,
                    "probing should reach the fee-implied ceiling {ceiling}, measured {measured}"
                );
            }
            Err(other) => panic!("expected InvalidInput, got: {other:?}"),
            Ok(swap) => panic!(
                "expected an error, got Ok(amount_in = {}) — a zero here would be stored as a \
                 measured depth",
                swap.amount_in()
            ),
        }
    }

    // =========================================================================
    // Tests for probe_execution_price
    // =========================================================================

    #[test]
    fn test_first_nonzero_output_exponent_matches_the_rate() {
        // 1e9 wei of an 18-decimal token at 3000 buys 3 raw units of a 6-decimal token.
        assert_eq!(first_nonzero_output_exponent(3000.0, 18, 6), 9);
        assert_eq!(first_nonzero_output_exponent(1.0 / 3000.0, 6, 18), 0);
        assert_eq!(first_nonzero_output_exponent(1.0, 18, 18), 0);
        // A rate the pool cannot report starts the probing at one wei.
        assert_eq!(first_nonzero_output_exponent(0.0, 18, 6), 0);
        assert_eq!(first_nonzero_output_exponent(f64::NAN, 18, 6), 0);
    }

    /// The 18→6 decimal pair makes dust outputs floor hard (1e9 wei prices at half the true
    /// rate), so the first probes measurably undershoot and the smallest clearing amount is > 1.
    #[test]
    fn test_probe_execution_price_returns_the_smallest_clearing_amount() {
        let state = UniswapV2State::new(
            U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(6u64)),
            U256::from(1000u64) * U256::from(10u64).pow(U256::from(18u64)),
        );
        let token_in = create_token("0x0000000000000000000000000000000000000001", "DAI", 18);
        let token_out = create_token("0x0000000000000000000000000000000000000000", "USDC", 6);

        let spot = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target = spot * 0.9;
        let (max_in, _) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();

        let mut price_points = Vec::new();
        let (amount, result, price) = probe_execution_price(
            &state,
            spot,
            target,
            &token_in,
            &token_out,
            &max_in,
            &mut price_points,
        )
        .unwrap()
        .expect("a pool priced 10% above the target must clear it at some probe amount");

        assert!(
            price >= target,
            "returned amount {amount} prices at {price}, below target {target}"
        );
        assert!(result.amount > BigUint::zero(), "the clearing amount must move output");
        assert_eq!(
            price_points.last().unwrap().amount_in,
            amount,
            "the returned amount should be the last one measured"
        );

        let previous_amount = &amount / 10u32;
        assert!(
            previous_amount > BigUint::zero(),
            "fixture must not clear at the first probe or the smallest-amount check is vacuous"
        );
        let previous = state
            .get_amount_out(previous_amount.clone(), &token_in, &token_out)
            .unwrap();
        let previous_price = calculate_trade_price(
            previous_amount.to_f64().unwrap(),
            previous.amount.to_f64().unwrap(),
            token_in.decimals,
            token_out.decimals,
        );
        assert!(
            previous_price < target,
            "amount {previous_amount} already clears the target at {previous_price}, so \
             {amount} is not the smallest"
        );
    }

    /// Liquidity thin at the current tick and enormous far below puts the answer more halvings
    /// away from `max_in` than `MAX_ITERATIONS` covers when bisecting down from `[0, max_in]`.
    #[test]
    fn test_trade_limit_price_converges_across_a_bracket_wider_than_the_budget() {
        let state = UniswapV3State::new(
            1_000_000_000_000_000_000u128,
            U256::from_str("79228162514264337593543950336").unwrap(),
            FeeAmount::Low,
            0,
            vec![
                TickInfo::new(-100_000, 625_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
                TickInfo::new(-60_000, 625_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
                TickInfo::new(-20_000, 1_250_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
                TickInfo::new(-6_000, 1_250_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
                TickInfo::new(-2_000, 2_500_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
                TickInfo::new(-800, 5_000_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
                TickInfo::new(-100, -10_000_000_000_000_000_000_000_000_000_000_000i128).unwrap(),
            ],
        )
        .unwrap();
        let token_in = token_0();
        let token_out = token_1();

        let spot = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let limit_price_f64 = spot * 0.99;
        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: to_price(limit_price_f64, &token_in, &token_out),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let pool_swap = query_pool_swap(&state, &params).expect("search should converge");
        let found = pool_swap.amount_in().clone();
        assert!(found > BigUint::zero(), "the search must return a real trade");

        let (max_in, _) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();
        let halvings = (max_in.to_f64().unwrap() / found.to_f64().unwrap()).log2();
        assert!(
            halvings > MAX_ITERATIONS as f64,
            "fixture must sit beyond the halving budget, sits at {halvings}"
        );

        let executed = calculate_trade_price(
            found.to_f64().unwrap(),
            pool_swap.amount_out().to_f64().unwrap(),
            token_in.decimals,
            token_out.decimals,
        );
        assert!(
            executed >= limit_price_f64,
            "returned trade executes at {executed}, below the limit {limit_price_f64}"
        );

        // The answer is the largest such trade, not merely a feasible one: twice as much must miss.
        let doubled = &found * 2u32;
        let at_doubled = state
            .get_amount_out(doubled.clone(), &token_in, &token_out)
            .unwrap();
        let doubled_price = calculate_trade_price(
            doubled.to_f64().unwrap(),
            at_doubled.amount.to_f64().unwrap(),
            token_in.decimals,
            token_out.decimals,
        );
        assert!(
            doubled_price < limit_price_f64,
            "twice the returned amount still executes at {doubled_price}, at or above the limit \
             {limit_price_f64}, so {found} is not the largest feasible trade"
        );
    }

    #[test]
    fn test_query_pool_swap_at_spot_price() {
        let state = UniswapV2State::new(U256::from(2_000_000u32), U256::from(1_000_000u32));
        let token_in = token_0();
        let token_out = token_1();

        let spot = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target_price = to_price(spot, &token_in, &token_out);

        let params = QueryPoolSwapParams::new(
            token_in,
            token_out,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0.001,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(&state, &params);
        assert!(result.is_ok());
        let pool_swap = result.unwrap();
        assert!(
            pool_swap.amount_in() <= &BigUint::from(1u32) ||
                pool_swap.amount_in() == &BigUint::zero()
        );
    }

    #[test]
    fn test_query_pool_swap_returns_price_points() {
        let state = UniswapV2State::new(
            U256::from(1000u64) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(18u64)),
        );
        let token_in = create_token("0x0000000000000000000000000000000000000001", "DAI", 18);
        let token_out = create_token("0x0000000000000000000000000000000000000000", "WETH", 18);

        let spot_price = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target_price = to_price(spot_price * 0.95, &token_in, &token_out);

        let params = QueryPoolSwapParams::new(
            token_in,
            token_out,
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0.001,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(&state, &params).unwrap();
        let price_points = result.price_points();
        assert!(price_points.is_some(), "should have price_points");
        let pp = price_points.as_ref().unwrap();
        assert!(pp.len() >= 2, "Should have at least 2 boundary points");
    }

    #[test]
    fn test_query_pool_swap_large_reserves() {
        let state = UniswapV2State::new(
            U256::from_str("1000000000000000000000000").unwrap(),
            U256::from_str("2000000000000000000000000000").unwrap(),
        );
        let token_in = create_token("0x0000000000000000000000000000000000000001", "DAI", 18);
        let token_out = create_token("0x0000000000000000000000000000000000000000", "WETH", 18);

        let spot_price = state
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target_price = to_price(spot_price * 0.90, &token_in, &token_out);

        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target: target_price,
                tolerance: 0.001,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(&state, &params);
        assert!(result.is_ok(), "Should handle large reserves");

        let pool_swap = result.unwrap();
        let new_spot = pool_swap
            .new_state()
            .spot_price(&token_in, &token_out)
            .unwrap();
        let target_f64 = spot_price * 0.90;
        let error_bps = ((new_spot - target_f64) / target_f64).abs() * 10000.0;
        assert!(error_bps < 10.0, "Error should be <10bps for large reserves");
    }
}
