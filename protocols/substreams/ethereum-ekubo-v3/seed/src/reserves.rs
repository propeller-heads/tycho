//! Token reserves implied by a pool's liquidity distribution.
//!
//! Ekubo's core keeps one balance per token for all pools, so per-pool reserves are derived: each
//! interval between consecutive initialized ticks holds the liquidity active there, and the
//! amounts of token0 above and token1 below the current price sum to the reserves.

use alloy::primitives::U256;
use anyhow::{anyhow, Context, Result};
use ekubo_sdk::{
    chain::evm::{Evm, EVM_MIN_SQRT_RATIO},
    math::{
        delta::{amount0_delta, amount1_delta},
        tick::to_sqrt_ratio,
    },
};

/// Reserves of token0 and token1 for `ticks`, the initialized ticks with their net liquidity
/// deltas sorted by index and summing to zero. Amounts round down like the contract does when it
/// pays out.
pub fn reserves(sqrt_ratio: U256, ticks: &[(i32, i128)]) -> Result<(u128, u128)> {
    let mut reserve0 = 0u128;
    let mut reserve1 = 0u128;
    let mut liquidity = 0i128;
    let mut lower = EVM_MIN_SQRT_RATIO;

    for &(tick, delta) in ticks {
        let upper =
            to_sqrt_ratio::<Evm>(tick).ok_or_else(|| anyhow!("tick {tick} out of range"))?;
        let active: u128 = liquidity
            .try_into()
            .with_context(|| format!("negative liquidity {liquidity} below tick {tick}"))?;

        if active != 0 {
            let amount1_upper = sqrt_ratio.min(upper);
            if lower < amount1_upper {
                reserve1 = reserve1
                    .checked_add(amount1_delta(lower, amount1_upper, active, false)?)
                    .context("token1 reserves overflow")?;
            }

            let amount0_lower = sqrt_ratio.max(lower);
            if amount0_lower < upper {
                reserve0 = reserve0
                    .checked_add(amount0_delta(amount0_lower, upper, active, false)?)
                    .context("token0 reserves overflow")?;
            }
        }

        lower = upper;
        liquidity = liquidity
            .checked_add(delta)
            .context("liquidity overflow")?;
    }

    if liquidity != 0 {
        return Err(anyhow!("tick liquidity deltas sum to {liquidity} instead of zero"));
    }

    Ok((reserve0, reserve1))
}

#[cfg(test)]
mod tests {
    use ekubo_sdk::math::tick::to_sqrt_ratio;

    use super::*;

    const L: i128 = 1_000_000_000_000;

    #[test]
    fn empty_pool_has_no_reserves() {
        assert_eq!(reserves(to_sqrt_ratio::<Evm>(0).unwrap(), &[]).unwrap(), (0, 0));
    }

    #[test]
    fn single_position_around_the_price_matches_the_delta_math() {
        let price = to_sqrt_ratio::<Evm>(0).unwrap();
        let lower = to_sqrt_ratio::<Evm>(-1000).unwrap();
        let upper = to_sqrt_ratio::<Evm>(1000).unwrap();

        let (reserve0, reserve1) = reserves(price, &[(-1000, L), (1000, -L)]).unwrap();

        assert_eq!(reserve0, amount0_delta(price, upper, L as u128, false).unwrap());
        assert_eq!(reserve1, amount1_delta(lower, price, L as u128, false).unwrap());
        assert!(reserve0 > 0 && reserve1 > 0);
    }

    #[test]
    fn position_entirely_above_the_price_holds_only_token0() {
        let price = to_sqrt_ratio::<Evm>(-5000).unwrap();

        let (reserve0, reserve1) = reserves(price, &[(-1000, L), (1000, -L)]).unwrap();

        assert!(reserve0 > 0);
        assert_eq!(reserve1, 0);
    }

    #[test]
    fn stacked_positions_add_up() {
        let price = to_sqrt_ratio::<Evm>(0).unwrap();
        let ticks = [(-2000, L), (-1000, L), (1000, -L), (2000, -L)];

        let (reserve0, reserve1) = reserves(price, &ticks).unwrap();
        let (inner0, inner1) = reserves(price, &[(-1000, L), (1000, -L)]).unwrap();
        let (outer0, outer1) = reserves(price, &[(-2000, L), (2000, -L)]).unwrap();

        assert_eq!(reserve0, inner0 + outer0);
        assert_eq!(reserve1, inner1 + outer1);
    }

    #[test]
    fn unbalanced_deltas_are_rejected() {
        assert!(reserves(to_sqrt_ratio::<Evm>(0).unwrap(), &[(-1000, L)]).is_err());
    }
}
