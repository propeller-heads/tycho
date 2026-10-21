//! Bin fee math. Fees are pips (`1e6`); the helpers scale to `1e18` internally as
//! `fee_pips * 1e12`, so pass pips. Pre-scaling double-scales silently.
//!
//! [`FeeHelper.sol`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/FeeHelper.sol),
//! [`ProtocolFeeLibrary.sol`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/ProtocolFeeLibrary.sol),
//! [`PackedUint128Math.sol`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/PackedUint128Math.sol).

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use super::constants::{PIPS_DENOMINATOR, PRECISION};
use crate::evm::protocol::safe_math::{safe_add_u256, safe_mul_u256};

/// Fee already contained in an amount, rounded up. Charged when the input is smaller than the bin
/// can absorb.
///
/// [`FeeHelper.sol#L13-L19`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/FeeHelper.sol#L13-L19)
pub fn get_fee_amount_from(amount_with_fee: u128, fee_pips: u32) -> Result<u128, SimulationError> {
    let total_fee = U256::from(fee_pips) * U256::from(1_000_000_000_000u64);
    let numerator = safe_mul_u256(U256::from(amount_with_fee), total_fee)?;
    let fee = safe_add_u256(numerator, PRECISION - U256::from(1))? / PRECISION;
    u128::try_from(fee)
        .map_err(|_| SimulationError::InvalidInput(format!("fee overflows u128: {fee}"), None))
}

/// Fee to add on top of a net amount, rounded up. Charged on `max_amount_in`, which comes from a
/// net reserve. Denominator is `1e18 - total_fee`, so this is not [`get_fee_amount_from`]
/// rearranged.
///
/// Rejects fees at or above `1e6` pips: the denominator would underflow, and `fee_pips` arrives
/// from an indexer attribute. On-chain cap is `100_000` (10%).
///
/// [`FeeHelper.sol#L25-L32`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/FeeHelper.sol#L25-L32)
pub fn get_fee_amount(amount: u128, fee_pips: u32) -> Result<u128, SimulationError> {
    if fee_pips >= 1_000_000 {
        return Err(SimulationError::InvalidInput(
            format!("fee out of range: {fee_pips} pips"),
            None,
        ));
    }
    let total_fee = U256::from(fee_pips) * U256::from(1_000_000_000_000u64);
    let denominator = PRECISION - total_fee;
    let numerator = safe_mul_u256(U256::from(amount), total_fee)?;
    let fee = safe_add_u256(numerator, denominator - U256::from(1))? / denominator;
    u128::try_from(fee)
        .map_err(|_| SimulationError::InvalidInput(format!("fee overflows u128: {fee}"), None))
}

/// Swap fee in pips: protocol fee first, LP fee on the remainder. `protocol_fee` is the 12-bit
/// directional half, not the packed word.
///
/// The product goes through `u64`: masked inputs multiply to more than `u32::MAX`.
///
/// [`ProtocolFeeLibrary.sol#L51-L59`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/ProtocolFeeLibrary.sol#L51-L59)
pub fn calculate_swap_fee(protocol_fee: u16, lp_fee: u32) -> u32 {
    let protocol_fee = u32::from(protocol_fee) & 0xfff;
    let lp_fee = lp_fee & 0xff_ffff;
    let numerator = u64::from(protocol_fee) * u64::from(lp_fee);
    protocol_fee + lp_fee - (numerator / u64::from(PIPS_DENOMINATOR)) as u32
}

/// Protocol's cut of a bin's fee, rounded down. Callers pass the input side's fee: `zero2one` when
/// selling token0, `one2zero` otherwise.
///
/// This leaves the pool, so `swap` subtracts it before crediting the bin. Skipping it overstates
/// every reserve after a swap.
///
/// [`PackedUint128Math.sol#L219-L247`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/PackedUint128Math.sol#L219-L247)
pub fn protocol_fee_amount(
    fee_amount: u128,
    protocol_fee: u16,
    swap_fee: u32,
) -> Result<u128, SimulationError> {
    // Masked as `getZeroForOneFee` does: an unmasked half would exceed the swap fee it is a share
    // of, and the caller would subtract more than it charged.
    let protocol_fee = protocol_fee & 0xfff;
    if protocol_fee == 0 || swap_fee == 0 {
        return Ok(0);
    }
    if u32::from(protocol_fee) == swap_fee {
        return Ok(fee_amount);
    }
    let share =
        safe_mul_u256(U256::from(fee_amount), U256::from(protocol_fee))? / U256::from(swap_fee);
    u128::try_from(share).map_err(|_| {
        SimulationError::InvalidInput(format!("protocol fee overflows u128: {share}"), None)
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// `1e18` at 0.01% (100 pips) gives `1e14`, the fee already inside the amount.
    #[test]
    fn test_fee_amount_from_known_value() {
        assert_eq!(
            get_fee_amount_from(1_000_000_000_000_000_000, 100).unwrap(),
            100_000_000_000_000
        );
    }

    /// `ProtocolFeeLibrary.calculateSwapFee`: 1000 + 3000 - 1000 * 3000 / 1e6.
    #[test]
    fn test_swap_fee_charges_protocol_fee_first() {
        assert_eq!(calculate_swap_fee(1_000, 3_000), 3_997);
    }

    /// The pool the harness indexes charges 3 pips per side on top of a 7-pip LP fee, so the swap
    /// fee is 10 pips and the protocol takes 3 of every 10 units of fee. Values from
    /// `ProtocolFeeLibrary.calculateSwapFee` and `PackedUint128Math.getProtocolFeeAmt`.
    #[test]
    fn test_protocol_takes_its_share_of_the_swap_fee() {
        let swap_fee = calculate_swap_fee(3, 7);

        assert_eq!(swap_fee, 10, "protocol fee first, LP fee on the remainder");
        assert_eq!(protocol_fee_amount(100, 3, swap_fee).unwrap(), 30);
    }

    /// Both short circuits, and the mask that keeps an out-of-range half from claiming more than
    /// the whole fee.
    #[rstest]
    #[case::no_protocol_fee(100, 0, 10, 0)]
    #[case::no_swap_fee(100, 3, 0, 0)]
    #[case::protocol_takes_all(100, 10, 10, 100)]
    #[case::masked_to_twelve_bits(100, 0x1000, 10, 0)]
    fn test_protocol_fee_amount(
        #[case] fee_amount: u128,
        #[case] protocol_fee: u16,
        #[case] swap_fee: u32,
        #[case] expected: u128,
    ) {
        assert_eq!(protocol_fee_amount(fee_amount, protocol_fee, swap_fee).unwrap(), expected);
    }
}
