use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::Zero;
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::u256_to_biguint,
    utils::solidity_math::mul_div,
};

/// How the curve evaluates `a * b / c` products.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StableMath {
    /// Checked 256-bit multiplication: overflow is an error, as in Solidity 0.8 `a * b / c`
    /// (Aerodrome V1).
    Checked,
    /// 512-bit intermediate, as in OpenZeppelin `Math.mulDiv` (AERO basic pools).
    FullPrecision,
}

impl StableMath {
    fn mul_div(self, a: U256, b: U256, denominator: U256) -> Result<U256, SimulationError> {
        match self {
            StableMath::Checked => safe_div_u256(safe_mul_u256(a, b)?, denominator),
            StableMath::FullPrecision => mul_div(a, b, denominator),
        }
    }
}

/// A Solidly stable pool (`x^3 * y + y^3 * x = k`): raw reserves and token decimals.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StablePool {
    pub reserve0: U256,
    pub reserve1: U256,
    pub decimals0: u8,
    pub decimals1: u8,
    pub math: StableMath,
}

fn e18() -> U256 {
    U256::from(10u128.pow(18))
}

fn scale(decimals: u8) -> U256 {
    U256::from(10u128.pow(decimals as u32))
}

impl StablePool {
    /// Output of selling `amount_in` after taking `fee_bps` from the input, as the pool's
    /// `getAmountOut`.
    pub fn get_amount_out(
        &self,
        amount_in: U256,
        zero2one: bool,
        fee_bps: u32,
    ) -> Result<U256, SimulationError> {
        if amount_in.is_zero() {
            return Err(SimulationError::InvalidInput("Amount in cannot be zero".to_string(), None));
        }

        if self.reserve0.is_zero() || self.reserve1.is_zero() {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }

        let xy = self.k()?;
        let reserve0_normalized =
            safe_div_u256(safe_mul_u256(self.reserve0, e18())?, scale(self.decimals0))?;
        let reserve1_normalized =
            safe_div_u256(safe_mul_u256(self.reserve1, e18())?, scale(self.decimals1))?;

        let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero2one {
            (reserve0_normalized, reserve1_normalized, self.decimals0, self.decimals1)
        } else {
            (reserve1_normalized, reserve0_normalized, self.decimals1, self.decimals0)
        };

        let fee_amount =
            safe_div_u256(safe_mul_u256(amount_in, U256::from(fee_bps))?, U256::from(10000))?;
        let amount_in_with_fee = safe_sub_u256(amount_in, fee_amount)?;
        let amount_in_normalized =
            safe_div_u256(safe_mul_u256(amount_in_with_fee, e18())?, scale(decimals_in))?;

        let x0 = safe_add_u256(amount_in_normalized, reserve_in)?;
        let y_new = self.get_y(x0, xy, reserve_out)?;
        let y_diff = safe_sub_u256(reserve_out, y_new)?;
        safe_div_u256(safe_mul_u256(y_diff, scale(decimals_out))?, e18())
    }

    pub fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if self.reserve0.is_zero() || self.reserve1.is_zero() {
            return Ok((BigUint::zero(), BigUint::zero()));
        }

        let zero_for_one = sell_token < buy_token;
        let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero_for_one {
            (self.reserve0, self.reserve1, self.decimals0, self.decimals1)
        } else {
            (self.reserve1, self.reserve0, self.decimals1, self.decimals0)
        };

        let xy = self.k()?;
        let reserve_in_normalized =
            safe_div_u256(safe_mul_u256(reserve_in, e18())?, scale(decimals_in))?;
        let reserve_out_normalized =
            safe_div_u256(safe_mul_u256(reserve_out, e18())?, scale(decimals_out))?;

        let amount_in_estimate =
            safe_div_u256(safe_mul_u256(reserve_in, U256::from(300))?, U256::from(100))?;
        let amount_in_normalized =
            safe_div_u256(safe_mul_u256(amount_in_estimate, e18())?, scale(decimals_in))?;

        let x0 = safe_add_u256(reserve_in_normalized, amount_in_normalized)?;
        let y_new = self.get_y(x0, xy, reserve_out_normalized)?;
        let amount_out_normalized = safe_sub_u256(reserve_out_normalized, y_new)?;
        let amount_out =
            safe_div_u256(safe_mul_u256(amount_out_normalized, scale(decimals_out))?, e18())?;

        Ok((u256_to_biguint(amount_in_estimate), u256_to_biguint(amount_out)))
    }

    fn f(&self, x0: U256, y: U256) -> Result<U256, SimulationError> {
        let a = self.math.mul_div(x0, y, e18())?;
        let b = safe_add_u256(self.math.mul_div(x0, x0, e18())?, self.math.mul_div(y, y, e18())?)?;
        self.math.mul_div(a, b, e18())
    }

    fn d(&self, x0: U256, y: U256) -> Result<U256, SimulationError> {
        let y_squared = self.math.mul_div(y, y, e18())?;
        let term1 = self
            .math
            .mul_div(safe_mul_u256(U256::from(3), x0)?, y_squared, e18())?;
        let x0_squared = self.math.mul_div(x0, x0, e18())?;
        let term2 = self
            .math
            .mul_div(x0_squared, x0, e18())?;
        safe_add_u256(term1, term2)
    }

    fn k(&self) -> Result<U256, SimulationError> {
        let x = safe_div_u256(safe_mul_u256(self.reserve0, e18())?, scale(self.decimals0))?;
        let y = safe_div_u256(safe_mul_u256(self.reserve1, e18())?, scale(self.decimals1))?;
        let a = self.math.mul_div(x, y, e18())?;
        let b = safe_add_u256(self.math.mul_div(x, x, e18())?, self.math.mul_div(y, y, e18())?)?;
        self.math.mul_div(a, b, e18())
    }

    fn get_y(&self, x0: U256, xy: U256, mut y: U256) -> Result<U256, SimulationError> {
        for _ in 0..255 {
            let k = self.f(x0, y)?;

            if k < xy {
                let d = self.d(x0, y)?;
                if d.is_zero() {
                    return Err(SimulationError::FatalError(
                        "Division by zero in _get_y".to_string(),
                    ));
                }

                let diff = safe_sub_u256(xy, k)?;
                let mut dy = self.math.mul_div(diff, e18(), d)?;

                if dy.is_zero() {
                    let y_plus_1 = safe_add_u256(y, U256::from(1))?;
                    if self.f(x0, y_plus_1)? > xy {
                        return Ok(y_plus_1);
                    }

                    dy = U256::from(1);
                }
                y = safe_add_u256(y, dy)?;
            } else {
                let d = self.d(x0, y)?;
                if d.is_zero() {
                    return Err(SimulationError::FatalError(
                        "Division by zero in _get_y".to_string(),
                    ));
                }

                let diff = safe_sub_u256(k, xy)?;
                let mut dy = self.math.mul_div(diff, e18(), d)?;

                if dy.is_zero() {
                    if k == xy {
                        return Ok(y);
                    }
                    let y_minus_1 = safe_sub_u256(y, U256::from(1))?;
                    if self.f(x0, y_minus_1)? < xy {
                        return Ok(y);
                    }
                    dy = U256::from(1);
                }
                y = safe_sub_u256(y, dy)?;
            }
        }

        Err(SimulationError::FatalError(
            "Failed to converge in _get_y after 255 iterations".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use tycho_common::simulation::errors::SimulationError;

    use super::*;

    fn pool(reserve0: U256, reserve1: U256, decimals0: u8, decimals1: u8) -> StablePool {
        StablePool { reserve0, reserve1, decimals0, decimals1, math: StableMath::Checked }
    }

    #[test]
    fn test_get_amount_out() {
        assert_eq!(
            pool(
                U256::from_str("2642455102346776307825").unwrap(),
                U256::from_str("3320301880379841502303").unwrap(),
                18,
                18
            )
            .get_amount_out(U256::from_str("2000000000000000000").unwrap(), true, 5)
            .unwrap(),
            U256::from_str("2004830151166915124").unwrap()
        )
    }

    #[test]
    fn test_get_amount_out_zero_input_rejected() {
        let err = pool(U256::from(1_000_000u64), U256::from(1_000_000u64), 18, 18)
            .get_amount_out(U256::ZERO, true, 5)
            .expect_err("zero input should fail");

        assert!(matches!(err, SimulationError::InvalidInput(_, _)));
    }

    #[test]
    fn test_get_amount_out_no_liquidity_rejected() {
        let err = pool(U256::ZERO, U256::from(1_000_000u64), 18, 18)
            .get_amount_out(U256::from(1u64), true, 5)
            .expect_err("zero reserve should fail");

        assert!(matches!(err, SimulationError::RecoverableError(_)));
    }

    #[test]
    fn test_get_amount_out_higher_fee_reduces_output() {
        let reserve0 = U256::from_str("2642455102346776307825").unwrap();
        let reserve1 = U256::from_str("3320301880379841502303").unwrap();
        let amount_in = U256::from_str("2000000000000000000").unwrap();

        let low_fee_out = pool(reserve0, reserve1, 18, 18)
            .get_amount_out(amount_in, true, 5)
            .unwrap();
        let high_fee_out = pool(reserve0, reserve1, 18, 18)
            .get_amount_out(amount_in, true, 100)
            .unwrap();

        assert!(high_fee_out < low_fee_out);
    }

    #[test]
    fn test_get_amount_out_reverse_direction() {
        let reserve0 = U256::from_str("2642455102346776307825").unwrap();
        let reserve1 = U256::from_str("3320301880379841502303").unwrap();
        let amount_in = U256::from_str("2000000000000000000").unwrap();

        let out = pool(reserve0, reserve1, 18, 18)
            .get_amount_out(amount_in, false, 5)
            .unwrap();

        assert!(out > U256::ZERO);
        assert!(out < reserve0);
    }

    #[test]
    fn test_get_amount_out_with_different_decimals() {
        let reserve0 = U256::from_str("1000000000000000000000000").unwrap();
        let reserve1 = U256::from(1_000_000_000_000u64);
        let amount_in = U256::from_str("1000000000000000000").unwrap();

        let out = pool(reserve0, reserve1, 18, 6)
            .get_amount_out(amount_in, true, 5)
            .unwrap();

        assert!(out > U256::ZERO);
        assert!(out < reserve1);
    }

    #[test]
    fn test_get_limits_zero_liquidity_returns_zeroes() {
        let sell = Bytes::from([0_u8; 20]);
        let mut buy_addr = [0_u8; 20];
        buy_addr[19] = 1;
        let buy = Bytes::from(buy_addr);

        let (amount_in, amount_out) = pool(U256::ZERO, U256::from(1_000_000u64), 18, 18)
            .get_limits(sell, buy)
            .expect("zero-liquidity limits should succeed");

        assert_eq!(amount_in, BigUint::ZERO);
        assert_eq!(amount_out, BigUint::ZERO);
    }

    #[test]
    fn test_get_limits_returns_non_zero_values() {
        let sell = Bytes::from([0_u8; 20]);
        let mut buy_addr = [0_u8; 20];
        buy_addr[19] = 1;
        let buy = Bytes::from(buy_addr);

        let reserve0 = U256::from_str("2642455102346776307825").unwrap();
        let reserve1 = U256::from_str("3320301880379841502303").unwrap();

        let (amount_in, amount_out) = pool(reserve0, reserve1, 18, 18)
            .get_limits(sell, buy)
            .expect("limits should succeed");

        assert!(amount_in > BigUint::ZERO);
        assert!(amount_out > BigUint::ZERO);
    }

    #[test]
    fn test_get_limits_changes_with_direction() {
        let sell0 = Bytes::from([0_u8; 20]);
        let mut sell1_addr = [0_u8; 20];
        sell1_addr[19] = 1;
        let sell1 = Bytes::from(sell1_addr);

        let reserve0 = U256::from_str("2642455102346776307825").unwrap();
        let reserve1 = U256::from_str("3320301880379841502303").unwrap();

        let (zero_to_one_in, zero_to_one_out) = pool(reserve0, reserve1, 18, 18)
            .get_limits(sell0.clone(), sell1.clone())
            .unwrap();
        let (one_to_zero_in, one_to_zero_out) = pool(reserve0, reserve1, 18, 18)
            .get_limits(sell1, sell0)
            .unwrap();

        assert!(zero_to_one_in > BigUint::ZERO);
        assert!(zero_to_one_out > BigUint::ZERO);
        assert!(one_to_zero_in > BigUint::ZERO);
        assert!(one_to_zero_out > BigUint::ZERO);
        assert!(zero_to_one_in != one_to_zero_in || zero_to_one_out != one_to_zero_out);
    }
}
