use tycho_common::simulation::errors::SimulationError;

// Solidity spec: function addDelta(uint128 x, int128 y) internal pure returns (uint128 z) {
pub fn add_liquidity_delta(x: u128, y: i128) -> Result<u128, SimulationError> {
    if y < 0 {
        x.checked_sub((-y) as u128)
            .ok_or_else(|| SimulationError::FatalError("Underflow: Result is negative".to_string()))
    } else {
        Ok(x + (y as u128))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_liquidity_delta_y_neg() {
        let x = 10000;
        let y = -1000;

        let res = add_liquidity_delta(x, y).unwrap();

        assert_eq!(res, 9000);
    }

    #[test]
    fn test_add_liquidity_delta_y_pos() {
        let x = 10000;
        let y = 1000;

        let res = add_liquidity_delta(x, y).unwrap();

        assert_eq!(res, 11000);
    }

    #[test]
    fn test_add_liquidity_delta_underflow() {
        let x = 5;
        let y = -10;

        let res = add_liquidity_delta(x, y);
        assert!(matches!(res, Err(SimulationError::FatalError(_))));
    }
}
