use alloy::primitives::{U256, U512};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::{div_mod_u512, safe_div_u512};

pub(crate) fn mul_div_rounding_up(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let product: U512 = a.widening_mul(b);
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
    let result = safe_div_u512(product, U512::from(denom))?;
    truncate_to_u256(result)
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
}
