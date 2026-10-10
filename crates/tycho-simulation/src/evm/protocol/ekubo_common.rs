//! Target-price swap flow shared by the Ekubo v1 and v3 states.

use num_bigint::BigUint;
use num_traits::One;
use tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{PoolSwap, Price, ProtocolSim, QueryPoolSwapParams},
    },
};

use crate::evm::query_pool_swap::{price_to_f64_with_decimals, query_pool_swap};

/// The pool operations that [`swap_to_target_price`] needs, in the SDK types of each version.
pub(crate) trait EkuboSwapToPrice: ProtocolSim + Sized {
    type SqrtRatio: Copy + Ord;

    /// Converts a Q128.128 sqrt ratio, or returns `None` when it is outside the valid range.
    fn sqrt_ratio_in_range(sqrt_ratio: &BigUint) -> Option<Self::SqrtRatio>;

    fn current_sqrt_ratio(&self) -> Self::SqrtRatio;

    /// Quotes a sale of `amount` of `token_in` that stops at `sqrt_ratio_limit` when it is set.
    /// Returns `(consumed_amount, calculated_amount, new_state)`.
    fn quote_to_limit(
        &self,
        token_in: &Token,
        amount: i128,
        sqrt_ratio_limit: Option<Self::SqrtRatio>,
    ) -> Result<(i128, u128, Self), SimulationError>;
}

/// Swaps `token_in` until the pool's spot price reaches `target`. `spot_price_fee` is the fee, in
/// 0.64 fixed point, that `spot_price` marks up.
///
/// Executes virtual orders before checking the target. Returns their advanced state for a zero
/// user swap when the target equals the resulting price. Uses numerical search for out-of-range
/// limits, an exhausted native input allowance, or a quote that stops before the limit.
///
/// # Errors
///
/// Returns [`SimulationError::InvalidInput`] when the target is above the spot price.
pub(crate) fn swap_to_target_price<S: EkuboSwapToPrice>(
    state: &S,
    params: &QueryPoolSwapParams,
    target: &Price,
    spot_price_fee: u64,
) -> Result<PoolSwap, SimulationError> {
    let (token_in, token_out) = (params.token_in(), params.token_out());
    let zero_for_one = token_in.address < token_out.address;

    // Capture virtual execution once; all target decisions and quotes use this state.
    let (_, _, start_state) = state.quote_to_limit(token_in, 0, None)?;
    let Some(limit) = target_sqrt_ratio(target, zero_for_one, spot_price_fee)
        .and_then(|ratio| quantize_sqrt_ratio(&ratio, zero_for_one))
        .and_then(|sqrt_ratio| S::sqrt_ratio_in_range(&sqrt_ratio))
    else {
        return query_pool_swap(&start_state, params);
    };
    let current = start_state.current_sqrt_ratio();

    if limit == current {
        return Ok(zero_swap(&start_state));
    }
    if !lies_ahead(limit, current, zero_for_one) {
        let target = price_to_f64_with_decimals(target, token_in.decimals, token_out.decimals)?;
        let spot = start_state.spot_price(token_in, token_out)?;
        return Err(SimulationError::InvalidInput(
            format!("Target price {target} is above spot price {spot}"),
            None,
        ));
    }

    let (consumed, calculated, new_state) =
        start_state.quote_to_limit(token_in, i128::MAX, Some(limit))?;
    // Unused input does not prove that the quote reached the limit: a pool with no liquidity
    // consumes nothing and stays at its price.
    if consumed == i128::MAX || new_state.current_sqrt_ratio() != limit {
        return query_pool_swap(&start_state, params);
    }

    Ok(PoolSwap::new(
        BigUint::from(consumed.unsigned_abs()),
        BigUint::from(calculated),
        Box::new(new_state),
        None,
    ))
}

/// Round-trip through Core's 96-bit SqrtRatio encoding (94-bit mantissa, four regions).
/// The expanded result remains Q128.128 for the SDK. Round toward the legal side of the target.
fn quantize_sqrt_ratio(ratio: &BigUint, round_up: bool) -> Option<BigUint> {
    for (bits, shift) in [(96usize, 2usize), (128, 34), (160, 66), (192, 98)] {
        let step = BigUint::one() << shift;
        let adjusted = if round_up { ratio + &step - 1u8 } else { ratio.clone() };
        if adjusted < BigUint::one() << bits {
            return Some((adjusted >> shift) << shift);
        }
    }
    None
}

fn lies_ahead<R: Ord>(to: R, from: R, zero_for_one: bool) -> bool {
    if zero_for_one {
        to < from
    } else {
        to > from
    }
}

fn zero_swap(state: &dyn ProtocolSim) -> PoolSwap {
    PoolSwap::new(BigUint::ZERO, BigUint::ZERO, state.clone_box(), None)
}

/// Returns the Q128.128 sqrt ratio at which the pool's spot price equals `target`, rounded so
/// that the spot price stays at or above the target. Returns `None` for a zero price term.
fn target_sqrt_ratio(target: &Price, zero_for_one: bool, spot_price_fee: u64) -> Option<BigUint> {
    let fee_factor = (BigUint::one() << 64usize) - spot_price_fee;
    let (price1, price0) = if zero_for_one {
        (&target.numerator * fee_factor, &target.denominator << 64usize)
    } else {
        (&target.denominator << 64usize, &target.numerator * fee_factor)
    };
    if price0 == BigUint::ZERO {
        return None;
    }

    // sqrt(price1 / price0) * 2^128 == sqrt(price1 * 2^256 / price0)
    let numerator = price1 << 256usize;
    let squared =
        if zero_for_one { (numerator + &price0 - 1u32) / &price0 } else { numerator / &price0 };
    let root = squared.sqrt();
    Some(if zero_for_one && &root * &root < squared { root + 1u32 } else { root })
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use num_bigint::BigUint;
    use tycho_common::{
        models::token::Token,
        simulation::{
            errors::SimulationError,
            protocol_sim::{Price, ProtocolSim},
        },
    };

    use crate::evm::query_pool_swap::{
        self, price_to_f64_with_decimals,
        test_helpers::{target_price_params, to_price},
    };

    /// Returns the spot price after a 1 wei swap, which executes any TWAMM virtual orders.
    fn probe_spot(state: &dyn ProtocolSim, token_in: &Token, token_out: &Token) -> f64 {
        state
            .get_amount_out(BigUint::from(1u8), token_in, token_out)
            .expect("probing spot price")
            .new_state
            .spot_price(token_in, token_out)
            .expect("computing spot price")
    }

    /// Checks in both directions that a target of `multiplier` times the spot price lands the
    /// spot price in `[target, target * (1 + tolerance)]`.
    pub(crate) fn assert_lands_in_band(
        state: &dyn ProtocolSim,
        token0: &Token,
        token1: &Token,
        multiplier: f64,
    ) {
        for (token_in, token_out) in [(token0, token1), (token1, token0)] {
            let target =
                to_price(probe_spot(state, token_in, token_out) * multiplier, token_in, token_out);
            let target_f64 =
                price_to_f64_with_decimals(&target, token_in.decimals, token_out.decimals).unwrap();
            let tolerance = (1.0 - multiplier) / 1e3;
            let params = target_price_params(token_in, token_out, target, tolerance);

            let swap = state
                .query_pool_swap(&params)
                .expect("native query_pool_swap");

            assert!(swap.amount_in() > &BigUint::ZERO);
            let spot = swap
                .new_state()
                .spot_price(token_in, token_out)
                .unwrap();
            // `spot_price` works in f64, so it can land a few ulps below the exact target.
            assert!(
                spot >= target_f64 * (1.0 - 1e-12) && spot <= target_f64 * (1.0 + tolerance),
                "spot {spot} is outside [{target_f64}, {target_f64} * (1 + {tolerance})]"
            );
        }
    }

    pub(crate) fn assert_target_above_spot_rejected(
        state: &dyn ProtocolSim,
        token0: &Token,
        token1: &Token,
    ) {
        for (token_in, token_out) in [(token0, token1), (token1, token0)] {
            let spot = state
                .spot_price(token_in, token_out)
                .unwrap();
            let target = to_price(spot * 1.01, token_in, token_out);
            let params = target_price_params(token_in, token_out, target, 1e-4);

            let res = state.query_pool_swap(&params);

            let Err(SimulationError::InvalidInput(..)) = res else {
                panic!("target above spot must be rejected, got {res:?}");
            };
        }
    }

    /// Checks a zero swap and an unchanged state for a pool whose spot price is exactly 1.
    pub(crate) fn assert_target_at_spot_gives_zero_swap(
        state: &dyn ProtocolSim,
        token0: &Token,
        token1: &Token,
    ) {
        for (token_in, token_out) in [(token0, token1), (token1, token0)] {
            let target = Price::new(BigUint::from(1u8), BigUint::from(1u8));
            let params = target_price_params(token_in, token_out, target, 1e-4);

            let swap = state
                .query_pool_swap(&params)
                .expect("native query_pool_swap");

            assert_eq!(swap.amount_in(), &BigUint::ZERO);
            assert_eq!(swap.amount_out(), &BigUint::ZERO);
            assert!(ProtocolSim::eq(swap.new_state(), state), "the state must not change");
        }
    }

    /// Checks that a target outside the sqrt ratio range gives the numerical search result.
    pub(crate) fn assert_out_of_range_falls_back(
        state: &dyn ProtocolSim,
        token0: &Token,
        token1: &Token,
    ) {
        for (token_in, token_out) in [(token0, token1), (token1, token0)] {
            let target = Price::new(BigUint::from(1u8), BigUint::from(10u8).pow(60));
            let params = target_price_params(token_in, token_out, target, 1e-4);

            let native = state.query_pool_swap(&params);
            let numerical = query_pool_swap::query_pool_swap(state, &params);

            assert_eq!(format!("{native:?}"), format!("{numerical:?}"));
        }
    }

    /// Checks that a pool with no liquidity, whose quote stops before the limit, gives the
    /// numerical search result and not a swap that leaves the price unchanged.
    pub(crate) fn assert_missed_limit_falls_back(
        state: &dyn ProtocolSim,
        token0: &Token,
        token1: &Token,
    ) {
        for (token_in, token_out) in [(token0, token1), (token1, token0)] {
            let spot = state
                .spot_price(token_in, token_out)
                .unwrap();
            let target = to_price(spot * 0.99, token_in, token_out);
            let params = target_price_params(token_in, token_out, target, 1e-4);

            let native = state.query_pool_swap(&params);
            let numerical = query_pool_swap::query_pool_swap(state, &params);

            assert!(native.is_err(), "an empty pool cannot reach the target, got {native:?}");
            assert_eq!(format!("{native:?}"), format!("{numerical:?}"));
        }
    }

    /// Checks rejection when virtual orders move the price below the target before the user swap.
    /// `token_in` must be the token that the virtual orders sell.
    pub(crate) fn assert_virtual_orders_applied_before_direction_check(
        state: &dyn ProtocolSim,
        token_in: &Token,
        token_out: &Token,
    ) {
        let pre_spot = state
            .spot_price(token_in, token_out)
            .unwrap();
        let post_spot = probe_spot(state, token_in, token_out);
        assert!(post_spot < pre_spot, "the virtual orders must move the price down");
        let target = to_price((pre_spot * post_spot).sqrt(), token_in, token_out);
        let params = target_price_params(token_in, token_out, target, 1e-4);

        assert!(matches!(state.query_pool_swap(&params), Err(SimulationError::InvalidInput(..))));

        // The opposite direction becomes reachable only after virtual execution. The old
        // pre-state direction check rejected this target before executing virtual orders.
        let reverse_pre = state
            .spot_price(token_out, token_in)
            .unwrap();
        let reverse_post = probe_spot(state, token_out, token_in);
        assert!(reverse_post > reverse_pre);
        let target = to_price((reverse_pre * reverse_post).sqrt(), token_out, token_in);
        let target_f =
            price_to_f64_with_decimals(&target, token_out.decimals, token_in.decimals).unwrap();
        let tolerance = (reverse_post / reverse_pre - 1.0) / 100.0;
        let params = target_price_params(token_out, token_in, target, tolerance);
        let swap = state
            .query_pool_swap(&params)
            .expect("reachable after virtual execution");
        let price = swap
            .new_state()
            .spot_price(token_out, token_in)
            .unwrap();
        assert!(price >= target_f * (1.0 - 1e-12) && price <= target_f * (1.0 + tolerance));
    }
}

#[cfg(test)]
mod rounding_tests {
    use super::*;

    #[test]
    fn compact_ratio_rounding_at_region_boundaries() {
        for (bits, shift) in [(96usize, 2usize), (128, 34), (160, 66), (192, 98)] {
            let boundary = BigUint::one() << bits;
            let below = &boundary - 1u8;
            assert_eq!(
                quantize_sqrt_ratio(&below, false),
                Some(&boundary - (BigUint::one() << shift))
            );
            assert_eq!(quantize_sqrt_ratio(&below, true), (bits < 192).then_some(boundary));
        }
    }
}
