//! [`Uint128x128Math.sol`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint128x128Math.sol).

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use super::constants::SCALE;
use crate::evm::protocol::safe_math::safe_mul_u256;

/// `x^y`, `x` a 128.128 number, `y` a plain exponent.
///
/// Errors on underflow to zero, matching `Uint128x128Math__PowUnderflow`. Exponents of `2^20` or
/// more skip the loop entirely and land on that same error, and bin ids reach `2^23` from the
/// shift, so it is reachable (the natspec bound of `2^21` disagrees with the code).
///
/// Two details decide the result. Every bin base exceeds `2^128`, so the `U256::MAX / x`
/// inversion always fires: positive exponents end inverted, negative ones cancel. And the `>> 128`
/// happens per step, not once at the end.
///
/// [`Uint128x128Math.sol#L87-L156`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint128x128Math.sol#L87-L156)
pub fn pow(x: U256, y: i32) -> Result<U256, SimulationError> {
    if y == 0 {
        return Ok(SCALE);
    }
    let mut invert = y < 0;
    let mut abs_y = y.unsigned_abs(); // u32; i32::MIN is fine, lands out of range below
    if abs_y >= 0x100000 {
        return Err(SimulationError::InvalidInput(format!("pow exponent out of range: {y}"), None));
    }
    let mut squared = x;
    if squared > U256::from(u128::MAX) {
        squared = U256::MAX / squared;
        invert = !invert;
    }
    let mut result = SCALE;
    while abs_y != 0 {
        if abs_y & 1 == 1 {
            result = safe_mul_u256(result, squared)? >> 128;
        }
        abs_y >>= 1;
        if abs_y != 0 {
            squared = safe_mul_u256(squared, squared)? >> 128;
        }
    }
    if result.is_zero() {
        return Err(SimulationError::InvalidInput(format!("pow underflow: x={x}, y={y}"), None));
    }
    Ok(if invert { U256::MAX / result } else { result })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::evm::protocol::pancakeswap_infinity_bin::math::bin::get_base;

    /// Exponents with several bits set, where the per-step `>> 128` compounds. `bin.rs` reaches
    /// only 0 and ±1, which run at most one round, so a shift in the wrong place or a loop bound
    /// off by one survives those. Expected values come from running the pinned Solidity assembly
    /// under forge, never from this port.
    #[rstest]
    // The ETH/USDC pool the native execution test swaps: active id 8369064, so 8369064 - 2^23.
    #[case::six_bits_inverted(10, -19544, "e1aaf4b436474f204bf29e5e7")]
    // One bit: nineteen squarings, a single multiply.
    #[case::one_bit(10, 8192, "e0d2f39dbd7102d6e371ff85575d8e919c3")]
    #[case::six_bits(25, 1337, "1c2c08a6aa5a13200be37ffb2df20b3b12")]
    fn test_pow_truncates_per_step(
        #[case] bin_step: u16,
        #[case] exponent: i32,
        #[case] expected: &str,
    ) {
        assert_eq!(
            pow(get_base(bin_step), exponent).unwrap(),
            U256::from_str_radix(expected, 16).unwrap(),
            "pow(base({bin_step}), {exponent})"
        );
    }
}
