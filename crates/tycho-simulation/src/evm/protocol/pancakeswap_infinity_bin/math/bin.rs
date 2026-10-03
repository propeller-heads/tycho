//! Bin prices and per-bin swap math.
//!
//! [`PriceHelper.sol`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/PriceHelper.sol),
//! [`BinHelper.sol`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/BinHelper.sol).

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use super::{
    constants::{BASIS_POINT_MAX, REAL_ID_SHIFT, SCALE, SCALE_OFFSET},
    fee::{get_fee_amount, get_fee_amount_from},
    uint128x128::pow,
    uint256x256::{
        mul_shift_round_down, mul_shift_round_up, shift_div_round_down, shift_div_round_up,
    },
};

/// Price ratio between neighbouring bins, `1 + bin_step / 10_000` in 128.128.
///
/// [`PriceHelper.sol#L45-L49`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/PriceHelper.sol#L45-L49)
pub fn get_base(bin_step: u16) -> U256 {
    SCALE + (U256::from(bin_step) << SCALE_OFFSET) / U256::from(BASIS_POINT_MAX)
}

/// Price of bin `id` in 128.128, y per x. Exponent is signed: bins below `2^23` price x under 1 y.
///
/// [`PriceHelper.sol#L22-L27`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/PriceHelper.sol#L22-L27)
pub fn get_price_from_id(id: u32, bin_step: u16) -> Result<U256, SimulationError> {
    pow(get_base(bin_step), id as i32 - REAL_ID_SHIFT as i32)
}

/// One bin's contribution to a swap step.
///
/// The Solidity returns packed `bytes32` pairs; one direction only touches one side of each, so
/// scalars carry the same information. Input token for `amount_in_with_fee` and `fee_amount`,
/// output token for `amount_out`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinAmounts {
    /// Input consumed by this bin, fee included. Subtract from the amount left to swap.
    pub amount_in_with_fee: u128,
    /// Output taken out of the bin. Never exceeds the bin's output reserve.
    pub amount_out: u128,
    /// Total fee inside `amount_in_with_fee`, split between LPs and the protocol by the caller.
    pub fee_amount: u128,
}

/// One exact-input swap step against a single bin.
///
/// `bin_reserve_out` is the bin's reserve of the output token, `swap_fee_pips` the combined fee
/// from [`super::fee::calculate_swap_fee`]. `amount_in_with_fee < amount_in_left` on return means
/// the bin is drained and the caller must step on.
///
/// Rounding is asymmetric between the two branches, and the multiply/divide roles swap with
/// direction. Narrowing happens before the clamp: a raw output above `u128` reverts on chain
/// rather than clamping, and a high-priced bin reaches that.
///
/// [`BinHelper.sol#L267-L311`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/BinHelper.sol#L267-L311)
pub fn get_amounts_out(
    bin_reserve_out: u128,
    swap_fee_pips: u32,
    bin_step: u16,
    swap_for_y: bool,
    active_id: u32,
    amount_in_left: u128,
) -> Result<BinAmounts, SimulationError> {
    let price = get_price_from_id(active_id, bin_step)?;
    let reserve_out = U256::from(bin_reserve_out);

    let max_amount_in = if swap_for_y {
        shift_div_round_up(reserve_out, SCALE_OFFSET, price)?
    } else {
        mul_shift_round_up(reserve_out, price, SCALE_OFFSET)?
    };
    let max_amount_in = to_u128(max_amount_in, "max amount in")?;
    let max_fee = get_fee_amount(max_amount_in, swap_fee_pips)?;
    let max_amount_in = max_amount_in
        .checked_add(max_fee)
        .ok_or_else(|| {
            SimulationError::InvalidInput("max amount in overflows u128".into(), None)
        })?;

    if amount_in_left >= max_amount_in {
        // Bin is drained. The fee is the one computed from max_amount_in, not recomputed.
        return Ok(BinAmounts {
            amount_in_with_fee: max_amount_in,
            amount_out: bin_reserve_out,
            fee_amount: max_fee,
        });
    }

    let fee_amount = get_fee_amount_from(amount_in_left, swap_fee_pips)?;
    let net_amount_in = U256::from(amount_in_left - fee_amount);
    let amount_out = if swap_for_y {
        mul_shift_round_down(net_amount_in, price, SCALE_OFFSET)?
    } else {
        shift_div_round_down(net_amount_in, SCALE_OFFSET, price)?
    };
    let amount_out = to_u128(amount_out, "amount out")?;

    Ok(BinAmounts {
        amount_in_with_fee: amount_in_left,
        amount_out: amount_out.min(bin_reserve_out),
        fee_amount,
    })
}

/// Liquidity a bin holds, `price * x + (y << 128)`. Both terms count: the active bin has both
/// reserves.
///
/// Errors where the Solidity reverts `BinHelper__LiquidityOverflow`. `swap` needs it for the
/// `MAX_LIQUIDITY_PER_BIN` check after crediting a bin.
///
/// [`BinHelper.sol#L124-L156`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/BinHelper.sol#L124-L156)
pub fn get_liquidity(x: u128, y: u128, price: U256) -> Result<U256, SimulationError> {
    let mut liquidity = U256::ZERO;
    if x > 0 {
        liquidity = price
            .checked_mul(U256::from(x))
            .ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!("bin liquidity overflow: price {price} * x {x}"),
                    None,
                )
            })?;
    }
    if y > 0 {
        // Cannot overflow: y < 2^128, so the shifted value fits U256.
        let shifted_y = U256::from(y) << SCALE_OFFSET;
        liquidity = liquidity
            .checked_add(shifted_y)
            .ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!("bin liquidity overflow: {liquidity} + y {y} << 128"),
                    None,
                )
            })?;
    }
    Ok(liquidity)
}

/// Narrows a 256-bit intermediate, erroring where the Solidity's `safe128` reverts.
fn to_u128(value: U256, what: &str) -> Result<u128, SimulationError> {
    u128::try_from(value)
        .map_err(|_| SimulationError::InvalidInput(format!("{what} exceeds u128: {value}"), None))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{super::uint128x128::pow, *};
    use crate::evm::protocol::pancakeswap_infinity_bin::math::constants::SCALE;

    /// Expected values come from calling the pinned Solidity itself (solc 0.8.26, forge script),
    /// never from this port. `bin_step` is 10, as on the USDC/USDT pool the harness indexes.
    #[rstest]
    // 1 USDC into 1000 USDT at price 1.0, fee 0.01%.
    #[case::partial_swap_for_y(
        1_000_000_000, 100, true, REAL_ID_SHIFT, 1_000_000,
        BinAmounts { amount_in_with_fee: 1_000_000, amount_out: 999_900, fee_amount: 100 }
    )]
    // Input above max_amount_in: bin drained, fee derived from the max, not the capped input.
    #[case::drains_bin(
        1_000_000_000, 100, true, REAL_ID_SHIFT, 10_000_000_000,
        BinAmounts {
            amount_in_with_fee: 1_000_100_011,
            amount_out: 1_000_000_000,
            fee_amount: 100_011,
        }
    )]
    // One-for-zero, five bins up: price above 1, less output per input.
    #[case::partial_one_for_zero(
        1_000_000_000, 100, false, REAL_ID_SHIFT + 5, 1_000_000,
        BinAmounts { amount_in_with_fee: 1_000_000, amount_out: 994_915, fee_amount: 100 }
    )]
    // 10% fee, on-chain max.
    #[case::max_fee(
        1_000_000_000, 100_000, true, REAL_ID_SHIFT, 1_000_000,
        BinAmounts { amount_in_with_fee: 1_000_000, amount_out: 900_000, fee_amount: 100_000 }
    )]
    // Eight bins down: pow's negative-exponent path, through a quote.
    #[case::below_shift(
        1_000_000_000, 100, true, REAL_ID_SHIFT - 8, 1_000_000,
        BinAmounts { amount_in_with_fee: 1_000_000, amount_out: 991_936, fee_amount: 100 }
    )]
    fn test_get_amounts_out(
        #[case] bin_reserve_out: u128,
        #[case] swap_fee_pips: u32,
        #[case] swap_for_y: bool,
        #[case] active_id: u32,
        #[case] amount_in_left: u128,
        #[case] expected: BinAmounts,
    ) {
        let amounts = get_amounts_out(
            bin_reserve_out,
            swap_fee_pips,
            10,
            swap_for_y,
            active_id,
            amount_in_left,
        )
        .unwrap();

        assert_eq!(amounts, expected);
    }

    /// Bin `2^23` is price 1.0: the exponent is zero, and `pow` short-circuits to `SCALE`.
    #[test]
    fn test_price_at_id_shift_is_one() {
        assert_eq!(get_price_from_id(REAL_ID_SHIFT, 25).unwrap(), SCALE);
        assert_eq!(pow(get_base(25), 0).unwrap(), SCALE);
    }

    /// One bin above the shift is exactly the base. Every base exceeds `2^128`, so `pow` runs the
    /// inverting branch: this also pins `U256::MAX / (U256::MAX / base) == base`, which only holds
    /// with the step-by-step truncation.
    #[test]
    fn test_price_one_bin_above_shift_is_base() {
        assert_eq!(get_price_from_id(REAL_ID_SHIFT + 1, 25).unwrap(), get_base(25));
        assert_eq!(get_price_from_id(REAL_ID_SHIFT + 1, 10).unwrap(), get_base(10));
    }

    /// One bin below the shift, i.e. the negative-exponent path where the two inversions cancel.
    /// Values transliterated from the pinned assembly, not from this port.
    #[rstest]
    #[case::step_10(10, "ffbe878b6170458ffbe878b617045900")]
    #[case::step_25(25, "ff5c918e5d34fcced7c7d208f00a36e7")]
    fn test_price_one_bin_below_shift(#[case] bin_step: u16, #[case] expected: &str) {
        assert_eq!(
            get_price_from_id(REAL_ID_SHIFT - 1, bin_step).unwrap(),
            U256::from_str_radix(expected, 16).unwrap()
        );
    }

    /// Exponents of `2^20` and beyond never enter the loop, so the result stays zero and the
    /// Solidity reverts `Uint128x128Math__PowUnderflow`. Bin ids can be that far from the shift.
    #[test]
    fn test_pow_errors_outside_the_loop_range() {
        assert!(
            pow(get_base(10), 1 << 20).is_err(),
            "exponents past the loop range underflow to zero on chain"
        );
    }

    /// Both reserves count. At price 1.0 the terms are plain shifts, so the sum is `(x + y) << 128`
    /// and a `get_liquidity` keeping only one side fails the first case. Literals are the pinned
    /// Solidity's.
    #[rstest]
    #[case::both_sides_at_price_one(7, 11, SCALE, "6125082604576892342340742933771827806208")]
    #[case::x_only_at_base_price(
        1_000_000,
        0,
        get_base(10),
        "340622649287859401926837982039199979667000000"
    )]
    fn test_get_liquidity(
        #[case] x: u128,
        #[case] y: u128,
        #[case] price: U256,
        #[case] expected: &str,
    ) {
        assert_eq!(
            get_liquidity(x, y, price).unwrap(),
            U256::from_str_radix(expected, 10).unwrap()
        );
    }

    /// `BinHelper__LiquidityOverflow` on chain, an error here.
    #[test]
    fn test_liquidity_errors_on_overflow() {
        assert!(
            get_liquidity(2, 0, U256::MAX).is_err(),
            "price * x overflowing must error, not wrap"
        );
    }
}
