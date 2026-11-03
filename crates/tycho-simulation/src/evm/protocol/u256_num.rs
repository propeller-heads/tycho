//! Numeric methods for the U256 type
use std::collections::HashMap;

use alloy::primitives::{bytes::Bytes, U256};
use num_bigint::BigUint;
use tycho_common::simulation::errors::SimulationError;

/// Converts a U256 integer into it's closest floating point representation
///
/// Rounds to "nearest even" if the number has to be truncated (number uses more than 53 bits).
///
/// ## Rounding rules
/// This function converts a `U256` value to a `f64` value by applying a rounding
/// rule to the least significant bits of the `U256` value. The general rule when
/// rounding binary fractions to the n-th place prescribes to check the digit
/// following the n-th place in the number (round_bit). If it’s 0, then the number
/// should always be rounded down. If, instead, the digit is 1 and any of the
/// following digits (sticky_bits) are also 1, then the number should be rounded up.
/// If, however, all of the following digits are 0’s, then a tie breaking rule is
/// applied using the least significant bit (lsb) and usually it’s the ‘ties to even’.
/// This rule says that we should round to the number that has 0 at the n-th place.
/// If after rounding, the significand uses more than 53 bits, the significand is
/// shifted to the right and the exponent is decreased by 1.
///
/// ## Additional Reading
/// - [Double-precision floating-point format](https://en.wikipedia.org/wiki/Double-precision_floating-point_format)
/// - [Converting uint to float bitwise on SO](https://stackoverflow.com/a/20308114/8648259 )
/// - [Int to Float rounding on SO](https://stackoverflow.com/a/42032175/8648259)
/// - [How to round binary numbers](https://indepth.dev/posts/1017/how-to-round-binary-numbers)
/// - [Paper: "What Every Computer Scientist Should Know About Floating Point Arithmetic"](http://www.validlab.com/goldberg/paper.pdf)
pub fn u256_to_f64(x: U256) -> Result<f64, SimulationError> {
    if x == U256::from(0u64) {
        return Ok(0.0);
    }

    let x_bits = x.bit_len();
    let n_shifts = 53i32 - x_bits as i32;
    let mut exponent = (1023 + 52 - n_shifts) as u64;

    let mut significant = if n_shifts >= 0 {
        let shift = n_shifts as u32;
        let shifted = x << shift;
        u64::try_from(shifted).map_err(|_| {
            SimulationError::FatalError(format!(
                "Unable to convert shifted U256 value `{shifted}` into u64 when converting {x}"
            ))
        })?
    } else {
        /*
        shift right if neg, dropping LSBs, round to nearest even

        The general rule when rounding binary fractions to the n-th place prescribes to check
        the digit following the n-th place in the number (round_bit). If it’s 0, then the
        number should always be rounded down. If, instead, the digit is 1 and any of the
        following digits (sticky_bits) are also 1, then the number should be rounded up.
        If, however, all of the following digits are 0’s, then a tie breaking rule must
        be applied and usually it’s the ‘ties to even’. This rule says that we should
        round to the number that has 0 at the n-th place.
        */
        // least significant bit is be used as tiebreaker
        let shift = n_shifts.unsigned_abs();

        // least significant bit is used as tiebreaker
        let lsb = (x >> shift) & U256::from(1u64);
        let round_bit = (x >> (shift - 1)) & U256::from(1u64);

        // build mask for sticky bit, handle case when no data for sticky bit is available
        let sticky_bit = if shift < 2 {
            // no bits after round bit, so sticky bit is 0
            U256::from(0u64)
        } else {
            x & ((U256::from(1u64) << (shift - 2)) - U256::from(1u64))
        };

        let rounded_towards_zero_u256 = x >> shift;
        let rounded_towards_zero: u64 = rounded_towards_zero_u256.try_into().map_err(|_| {
            SimulationError::FatalError(format!(
                "Unable to convert rounded U256 value `{rounded_towards_zero_u256}` into u64 when converting {x}"
            ))
        })?;

        if round_bit == U256::from(1u64) {
            if sticky_bit == U256::from(0u64) {
                // tiebreaker: round up if lsb is 1 and down if lsb is 0
                if lsb == U256::from(0u64) {
                    rounded_towards_zero
                } else {
                    rounded_towards_zero + 1
                }
            } else {
                rounded_towards_zero + 1
            }
        } else {
            rounded_towards_zero
        }
    };

    // due to rounding rules significand might be using 54 bits instead of 53 if
    // this is the case we shift to the right once more and decrease the exponent.
    if significant & (1 << 53) > 0 {
        significant >>= 1;
        exponent += 1;
    }

    let merged = (exponent << 52) | (significant & 0xFFFFFFFFFFFFFu64);
    Ok(f64::from_bits(merged))
}

#[inline]
pub fn u256_to_biguint(value: U256) -> BigUint {
    let digits = value
        .as_limbs()
        .map(|limb| [limb as u32, (limb >> 32) as u32]);
    BigUint::from_slice(digits.as_flattened())
}

pub fn biguint_to_u256(value: &BigUint) -> U256 {
    assert!(value.bits() <= 256, "BigUint value overflows U256");
    let mut limbs = [0u64; 4];
    for (i, digit) in value.iter_u64_digits().enumerate() {
        limbs[i] = digit;
    }
    U256::from_limbs(limbs)
}

pub fn bytes_to_u256(bytes: Bytes) -> U256 {
    // Ensure the input is exactly 32 bytes
    let mut padded_bytes = [0u8; 32];

    // Copy the input bytes into the padded array from the right
    let start = 32 - bytes.len().min(32);
    padded_bytes[start..].copy_from_slice(&bytes[bytes.len().saturating_sub(32)..]);

    // Convert the padded byte array into U256
    U256::from_be_slice(&padded_bytes)
}

pub fn map_slots_to_u256(
    slots: HashMap<tycho_common::hex_bytes::Bytes, tycho_common::hex_bytes::Bytes>,
) -> HashMap<U256, U256> {
    slots
        .into_iter()
        .map(|(k, v)| (bytes_to_u256(k.into()), bytes_to_u256(v.into())))
        .collect()
}

#[cfg(test)]
mod test {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::one(U256::from(1u64), 1.0f64)]
    #[case::two(U256::from(2), 2.0f64)]
    #[case::zero(U256::from(0u64), 0.0f64)]
    #[case::two_pow1024(U256::from(2).pow(U256::from(190)), 2.0f64.powi(190))]
    #[case::max32(U256::from_limbs([u32::MAX as u64, 0, 0, 0]), u32::MAX as f64)]
    #[case::max64(U256::from_limbs([u64::MAX, 0, 0, 0]), u64::MAX as f64)]
    #[case::edge_54bits_trailing_zeros(U256::from(2u64.pow(53)), 2u64.pow(53) as f64)]
    #[case::edge_54bits_trailing_ones(U256::from(2u64.pow(54) - 1), (2u64.pow(54) - 1) as f64)]
    #[case::edge_53bits_trailing_zeros(U256::from(2u64.pow(52)), 2u64.pow(52) as f64)]
    #[case::edge_53bits_trailing_ones(U256::from(2u64.pow(53) - 1), (2u64.pow(53) - 1) as f64)]
    fn test_convert(#[case] inp: U256, #[case] out: f64) {
        let res = u256_to_f64(inp).expect("convert U256 to f64");

        assert_eq!(res, out);
    }

    #[test]
    fn test_u256_biguint_round_trip() {
        let values = [
            U256::ZERO,
            U256::from(1u64),
            U256::from(u32::MAX),
            U256::from(u64::MAX),
            U256::from(u128::MAX),
            U256::from_limbs([0, 0, 0, 1]),
            U256::from_limbs([0x0123_4567_89ab_cdef, 0, 0xfedc_ba98_7654_3210, 0]),
            U256::MAX,
        ];

        for value in values {
            let converted = u256_to_biguint(value);

            assert_eq!(converted, BigUint::from_bytes_le(&value.to_le_bytes::<32>()));
            assert_eq!(biguint_to_u256(&converted), value);
        }
    }
}
