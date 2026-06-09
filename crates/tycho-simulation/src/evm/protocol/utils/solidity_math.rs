use alloy::primitives::{U256, U512};
use ruint::aliases::U384;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::{div_mod_u512, safe_div_u512, safe_mul_u512};

pub(crate) fn mul_div_rounding_up(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let a_big = U512::from(a);
    let b_big = U512::from(b);
    let product = safe_mul_u512(a_big, b_big)?;
    let (mut result, rest) = div_mod_u512(product, U512::from(denom))?;
    if !rest.is_zero() {
        result = result
            .checked_add(U512::from(1u64))
            .ok_or_else(|| SimulationError::FatalError("Overflow when rounding up".to_string()))?;
    }
    truncate_to_u256(result)
}

pub(crate) fn mul_div(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let a_big = U512::from(a);
    let b_big = U512::from(b);
    let product = safe_mul_u512(a_big, b_big)?;
    let result = safe_div_u512(product, U512::from(denom))?;
    truncate_to_u256(result)
}

/// `floor(a*b/denom)`, bit-exact with [`mul_div`], on a 384-bit (6-limb) intermediate rather
/// than the 512-bit (8-limb) path. Every Uniswap swap-math product is provably < 2^384 (the
/// tightest site, `(liquidity << 96) * delta_sqrt`, is 2^224 * 2^160 = 2^384, strictly less), so
/// `checked_mul` on `U384` succeeds for in-domain inputs and divides ~25% fewer limbs. Any product
/// outside that domain makes `checked_mul` return `None`, and we defer to the exact [`mul_div`] —
/// so the result is always exact and never silently wraps (the fallback is never taken in
/// practice but is free insurance that survives release builds, unlike a `debug_assert`).
pub(crate) fn mul_div_384(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let Some(product) = U384::from(a).checked_mul(U384::from(b)) else {
        return mul_div(a, b, denom);
    };
    let denom = U384::from(denom);
    if denom.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    truncate_u384_to_u256(product / denom)
}

/// `ceil(a*b/denom)`, bit-exact with [`mul_div_rounding_up`], on a 384-bit intermediate. See
/// [`mul_div_384`] for the width bound and the exact-`U512` fallback.
pub(crate) fn mul_div_rounding_up_384(
    a: U256,
    b: U256,
    denom: U256,
) -> Result<U256, SimulationError> {
    let Some(product) = U384::from(a).checked_mul(U384::from(b)) else {
        return mul_div_rounding_up(a, b, denom);
    };
    let denom = U384::from(denom);
    if denom.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    let quotient = product / denom;
    let rest = product % denom;
    let result = if rest.is_zero() { quotient } else { quotient + U384::from(1u64) };
    truncate_u384_to_u256(result)
}

fn truncate_u384_to_u256(value: U384) -> Result<U256, SimulationError> {
    let limbs = value.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 {
        return Err(SimulationError::FatalError("Overflow: Value exceeds 256 bits".to_string()));
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

fn truncate_to_u256(value: U512) -> Result<U256, SimulationError> {
    // Access the limbs of the U512 value
    let limbs = value.as_limbs();

    // Check if the upper 256 bits are non-zero
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return Err(SimulationError::FatalError("Overflow: Value exceeds 256 bits".to_string()));
    }

    // Extract the lower 256 bits
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn mul_div_384_matches_u512_reference() {
        use std::str::FromStr;

        // Bit-exact with the U512 path across the in-domain width envelope, including operands
        // past U160_MAX that exercise the wide 384-bit intermediate.
        let u160_max = (U256::from(1u8) << 160) - U256::from(1u8);
        let cases = [
            (U256::from(23u64), U256::from(10u64), U256::from(50u64)),
            (
                U256::from_str("79224201403219477170569942574").unwrap(),
                U256::from_str("170506771201").unwrap(),
                U256::from_str("79394708140106462983274643745").unwrap(),
            ),
            ((U256::from(u128::MAX) << 96), u160_max, u160_max),
        ];
        for (a, b, denom) in cases {
            assert_eq!(mul_div_384(a, b, denom).unwrap(), mul_div(a, b, denom).unwrap());
            assert_eq!(
                mul_div_rounding_up_384(a, b, denom).unwrap(),
                mul_div_rounding_up(a, b, denom).unwrap(),
            );
        }
    }
}
