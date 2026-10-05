use alloy::primitives::{U256, U512};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::{div_mod_u256, div_mod_u512, safe_div_u256, safe_div_u512};

pub(crate) fn mul_div_rounding_up(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let product: U512 = a.widening_mul(b);
    if let Some(product) = to_u256_if_fits(&product) {
        let (result, rest) = div_mod_u256(product, denom)?;
        // A remainder needs `denom >= 2`, so `result <= U256::MAX / 2` and adding one fits.
        return Ok(if rest.is_zero() { result } else { result + U256::from(1u64) });
    }
    let (mut result, rest) = div_mod_u512(product, U512::from(denom))?;
    if !rest.is_zero() {
        result = result
            .checked_add(U512::from(1u64))
            .ok_or_else(|| SimulationError::FatalError("Overflow when rounding up".to_string()))?;
    }
    truncate_to_u256(result)
}

pub(crate) fn mul_div(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let product: U512 = a.widening_mul(b);
    if let Some(product) = to_u256_if_fits(&product) {
        return safe_div_u256(product, denom);
    }
    let result = safe_div_u512(product, U512::from(denom))?;
    truncate_to_u256(result)
}

/// `ceil(a * b / 2^96)`: [`mul_div_rounding_up`] with `Q96` as the denominator, where a shift
/// does the division.
pub(crate) fn mul_div_q96_rounding_up(a: U256, b: U256) -> Result<U256, SimulationError> {
    let product: U512 = a.widening_mul(b);
    let limbs = product.as_limbs();
    let has_remainder = limbs[0] != 0 || limbs[1] & u64::from(u32::MAX) != 0;
    let mut result = product >> 96;
    if has_remainder {
        result += U512::from(1u64);
    }
    truncate_to_u256(result)
}

/// The value as a U256, when its upper 256 bits are zero.
fn to_u256_if_fits(value: &U512) -> Option<U256> {
    let limbs = value.as_limbs();
    (limbs[4] | limbs[5] | limbs[6] | limbs[7] == 0)
        .then(|| U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

fn truncate_to_u256(value: U512) -> Result<U256, SimulationError> {
    to_u256_if_fits(&value)
        .ok_or_else(|| SimulationError::FatalError("Overflow: Value exceeds 256 bits".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mul_div` and `mul_div_rounding_up` computed with 512-bit arithmetic only.
    fn reference_mul_div(
        a: U256,
        b: U256,
        denom: U256,
        round_up: bool,
    ) -> Result<U256, SimulationError> {
        if denom.is_zero() {
            return Err(SimulationError::FatalError("Division by zero".to_string()));
        }
        let (mut result, rest) = (U512::from(a) * U512::from(b)).div_rem(U512::from(denom));
        if round_up && !rest.is_zero() {
            result += U512::from(1u64);
        }
        truncate_to_u256(result)
    }

    #[test]
    fn test_mul_div_matches_the_512_bit_reference() {
        let values = [
            U256::ZERO,
            U256::from(1u64),
            U256::from(2u64),
            U256::from(3u64),
            U256::from(1_000_000u64),
            U256::from(u128::MAX),
            U256::from(1u64) << 128,
            (U256::from(1u64) << 160) - U256::from(1u64),
            U256::MAX / U256::from(2u64),
            U256::MAX - U256::from(1u64),
            U256::MAX,
        ];
        for a in values {
            for b in values {
                for denom in values {
                    assert_eq!(
                        format!("{:?}", mul_div(a, b, denom)),
                        format!("{:?}", reference_mul_div(a, b, denom, false)),
                        "mul_div({a}, {b}, {denom})"
                    );
                    assert_eq!(
                        format!("{:?}", mul_div_rounding_up(a, b, denom)),
                        format!("{:?}", reference_mul_div(a, b, denom, true)),
                        "mul_div_rounding_up({a}, {b}, {denom})"
                    );
                }
            }
        }
    }

    #[test]
    fn test_mul_div_rounding_up() {
        let a = U256::from(23);
        let b = U256::from(10);
        let denom = U256::from(50);
        let res = mul_div_rounding_up(a, b, denom).unwrap();

        assert_eq!(res, U256::from(5));
    }

    #[test]
    fn test_mul_div_rounding_up_overflow_u256() {
        let (a, b) = (U256::MAX, U256::MAX);
        let denom = U256::from(1);

        let result = mul_div_rounding_up(a, b, denom);

        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn test_mul_div_q96_rounding_up_matches_mul_div_rounding_up() {
        let q96 = U256::from(1u64) << 96;
        let values = [
            U256::ZERO,
            U256::from(1u64),
            U256::from(23u64),
            q96 - U256::from(1u64),
            q96,
            q96 + U256::from(1u64),
            U256::from(u128::MAX),
            (U256::from(1u64) << 160) - U256::from(1u64),
            U256::from(1u64) << 200,
            U256::MAX,
        ];
        for a in values {
            for b in values {
                assert_eq!(
                    format!("{:?}", mul_div_q96_rounding_up(a, b)),
                    format!("{:?}", mul_div_rounding_up(a, b, q96)),
                    "a = {a}, b = {b}"
                );
            }
        }
    }

    #[test]
    fn test_mul_div() {
        let a = U256::from(23);
        let b = U256::from(10);
        let denom = U256::from(50);
        let res = mul_div(a, b, denom).unwrap();

        assert_eq!(res, U256::from(4));
    }

    #[test]
    fn test_mul_div_overflow_u256() {
        let (a, b) = (U256::MAX, U256::MAX);
        let denom = U256::from(1);

        let result = mul_div(a, b, denom);

        assert!(matches!(result, Err(SimulationError::FatalError(_))));
    }
}
