use alloy::primitives::U256;
use num_bigint::BigUint;
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::u256_to_biguint,
};

pub fn get_amount_out(
    amount_in: U256,
    zero2one: bool,
    reserve0: U256,
    reserve1: U256,
    fee_bps: u32,
    decimals0: u8,
    decimals1: u8,
) -> Result<U256, SimulationError> {
    if amount_in.is_zero() {
        return Err(SimulationError::InvalidInput("Amount in cannot be zero".to_string(), None));
    }

    if reserve0.is_zero() || reserve1.is_zero() {
        return Err(SimulationError::RecoverableError("No liquidity".to_string()));
    }

    let xy = _k(reserve0, reserve1, decimals0, decimals1)?;
    let e18 = U256::from(10u128.pow(18));
    let decimals0_scale = U256::from(10u128.pow(decimals0 as u32));
    let decimals1_scale = U256::from(10u128.pow(decimals1 as u32));

    let reserve0_normalized = safe_div_u256(safe_mul_u256(reserve0, e18)?, decimals0_scale)?;
    let reserve1_normalized = safe_div_u256(safe_mul_u256(reserve1, e18)?, decimals1_scale)?;

    let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero2one {
        (reserve0_normalized, reserve1_normalized, decimals0, decimals1)
    } else {
        (reserve1_normalized, reserve0_normalized, decimals1, decimals0)
    };

    let fee_amount =
        safe_div_u256(safe_mul_u256(amount_in, U256::from(fee_bps))?, U256::from(10000))?;
    let amount_in_with_fee = safe_sub_u256(amount_in, fee_amount)?;

    let decimals_in_scale = U256::from(10u128.pow(decimals_in as u32));
    let amount_in_normalized =
        safe_div_u256(safe_mul_u256(amount_in_with_fee, e18)?, decimals_in_scale)?;

    let x0 = safe_add_u256(amount_in_normalized, reserve_in)?;
    let y_new = _get_y(x0, xy, reserve_out)?;
    let y_diff = safe_sub_u256(reserve_out, y_new)?;
    let decimals_out_scale = U256::from(10u128.pow(decimals_out as u32));
    let amount_out = safe_div_u256(safe_mul_u256(y_diff, decimals_out_scale)?, e18)?;

    Ok(amount_out)
}

fn _f(x0: U256, y: U256) -> Result<U256, SimulationError> {
    let e18 = U256::from(10u128.pow(18));
    let a = safe_div_u256(safe_mul_u256(x0, y)?, e18)?;
    let x0_squared = safe_div_u256(safe_mul_u256(x0, x0)?, e18)?;
    let y_squared = safe_div_u256(safe_mul_u256(y, y)?, e18)?;
    let b = safe_add_u256(x0_squared, y_squared)?;
    safe_div_u256(safe_mul_u256(a, b)?, e18)
}

fn _d(x0: U256, y: U256) -> Result<U256, SimulationError> {
    let e18 = U256::from(10u128.pow(18));
    let y_squared = safe_div_u256(safe_mul_u256(y, y)?, e18)?;
    let term1 = safe_div_u256(safe_mul_u256(safe_mul_u256(U256::from(3), x0)?, y_squared)?, e18)?;
    let x0_squared = safe_div_u256(safe_mul_u256(x0, x0)?, e18)?;
    let term2 = safe_div_u256(safe_mul_u256(x0_squared, x0)?, e18)?;
    safe_add_u256(term1, term2)
}

fn _k(x: U256, y: U256, decimals0: u8, decimals1: u8) -> Result<U256, SimulationError> {
    let e18 = U256::from(10u128.pow(18));
    let decimals0_scale = U256::from(10u128.pow(decimals0 as u32));
    let decimals1_scale = U256::from(10u128.pow(decimals1 as u32));

    let x = safe_div_u256(safe_mul_u256(x, e18)?, decimals0_scale)?;
    let y = safe_div_u256(safe_mul_u256(y, e18)?, decimals1_scale)?;
    let a = safe_div_u256(safe_mul_u256(x, y)?, e18)?;
    let b = safe_add_u256(
        safe_div_u256(safe_mul_u256(x, x)?, e18)?,
        safe_div_u256(safe_mul_u256(y, y)?, e18)?,
    )?;
    safe_div_u256(safe_mul_u256(a, b)?, e18)
}

fn _get_y(x0: U256, xy: U256, mut y: U256) -> Result<U256, SimulationError> {
    let e18 = U256::from(10u128.pow(18));

    for _ in 0..255 {
        let k = _f(x0, y)?;

        if k < xy {
            let d = _d(x0, y)?;
            if d.is_zero() {
                return Err(SimulationError::FatalError("Division by zero in _get_y".to_string()));
            }

            let diff = safe_sub_u256(xy, k)?;
            let mut dy = safe_div_u256(safe_mul_u256(diff, e18)?, d)?;

            if dy.is_zero() {
                if k == xy {
                    return Ok(y);
                }

                let y_plus_1 = safe_add_u256(y, U256::from(1))?;
                if _f(x0, y_plus_1)? > xy {
                    return Ok(y_plus_1);
                }

                dy = U256::from(1);
            }
            y = safe_add_u256(y, dy)?;
        } else {
            let d = _d(x0, y)?;
            if d.is_zero() {
                return Err(SimulationError::FatalError("Division by zero in _get_y".to_string()));
            }

            let diff = safe_sub_u256(k, xy)?;
            let mut dy = safe_div_u256(safe_mul_u256(diff, e18)?, d)?;

            if dy.is_zero() {
                if k == xy {
                    return Ok(y);
                }
                let y_minus_1 = safe_sub_u256(y, U256::from(1))?;
                if _f(x0, y_minus_1)? < xy {
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

pub fn get_limits(
    sell_token: Bytes,
    buy_token: Bytes,
    reserve0: U256,
    reserve1: U256,
    decimals0: u8,
    decimals1: u8,
) -> Result<(BigUint, BigUint), SimulationError> {
    let (amount_in, amount_out) =
        get_limits_u256(sell_token < buy_token, reserve0, reserve1, decimals0, decimals1)?;
    Ok((u256_to_biguint(amount_in), u256_to_biguint(amount_out)))
}

/// `U256`-native core of [`get_limits`]. Avoids the `BigUint` round-trip for the `SwapQuoter` path.
pub fn get_limits_u256(
    zero_for_one: bool,
    reserve0: U256,
    reserve1: U256,
    decimals0: u8,
    decimals1: u8,
) -> Result<(U256, U256), SimulationError> {
    if reserve0.is_zero() || reserve1.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }

    let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero_for_one {
        (reserve0, reserve1, decimals0, decimals1)
    } else {
        (reserve1, reserve0, decimals1, decimals0)
    };

    let xy = _k(reserve0, reserve1, decimals0, decimals1)?;
    let e18 = U256::from(10u128.pow(18));
    let decimals_in_scale = U256::from(10u128.pow(decimals_in as u32));
    let decimals_out_scale = U256::from(10u128.pow(decimals_out as u32));

    let reserve_in_normalized = safe_div_u256(safe_mul_u256(reserve_in, e18)?, decimals_in_scale)?;
    let reserve_out_normalized =
        safe_div_u256(safe_mul_u256(reserve_out, e18)?, decimals_out_scale)?;

    let amount_in_estimate =
        safe_div_u256(safe_mul_u256(reserve_in, U256::from(300))?, U256::from(100))?;
    let amount_in_normalized =
        safe_div_u256(safe_mul_u256(amount_in_estimate, e18)?, decimals_in_scale)?;

    let x0 = safe_add_u256(reserve_in_normalized, amount_in_normalized)?;
    let y_new = _get_y(x0, xy, reserve_out_normalized)?;
    let amount_out_normalized = safe_sub_u256(reserve_out_normalized, y_new)?;
    let amount_out = safe_div_u256(safe_mul_u256(amount_out_normalized, decimals_out_scale)?, e18)?;

    Ok((amount_in_estimate, amount_out))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use tycho_common::simulation::errors::SimulationError;

    use super::*;

    #[test]
    fn test_get_amount_out() {
        assert_eq!(
            get_amount_out(
                U256::from_str("2000000000000000000").unwrap(),
                true,
                U256::from_str("2642455102346776307825").unwrap(),
                U256::from_str("3320301880379841502303").unwrap(),
                5,
                18,
                18,
            )
            .unwrap(),
            U256::from_str("2004830151166915124").unwrap()
        )
    }

    #[test]
    fn test_get_amount_out_zero_input_rejected() {
        let err = get_amount_out(
            U256::ZERO,
            true,
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            5,
            18,
            18,
        )
        .expect_err("zero input should fail");

        assert!(matches!(err, SimulationError::InvalidInput(_, _)));
    }

    #[test]
    fn test_get_amount_out_no_liquidity_rejected() {
        let err =
            get_amount_out(U256::from(1u64), true, U256::ZERO, U256::from(1_000_000u64), 5, 18, 18)
                .expect_err("zero reserve should fail");

        assert!(matches!(err, SimulationError::RecoverableError(_)));
    }

    #[test]
    fn test_get_amount_out_higher_fee_reduces_output() {
        let reserve0 = U256::from_str("2642455102346776307825").unwrap();
        let reserve1 = U256::from_str("3320301880379841502303").unwrap();
        let amount_in = U256::from_str("2000000000000000000").unwrap();

        let low_fee_out = get_amount_out(amount_in, true, reserve0, reserve1, 5, 18, 18).unwrap();
        let high_fee_out =
            get_amount_out(amount_in, true, reserve0, reserve1, 100, 18, 18).unwrap();

        assert!(high_fee_out < low_fee_out);
    }

    #[test]
    fn test_get_amount_out_reverse_direction() {
        let reserve0 = U256::from_str("2642455102346776307825").unwrap();
        let reserve1 = U256::from_str("3320301880379841502303").unwrap();
        let amount_in = U256::from_str("2000000000000000000").unwrap();

        let out = get_amount_out(amount_in, false, reserve0, reserve1, 5, 18, 18).unwrap();

        assert!(out > U256::ZERO);
        assert!(out < reserve0);
    }

    #[test]
    fn test_get_amount_out_with_different_decimals() {
        let reserve0 = U256::from_str("1000000000000000000000000").unwrap();
        let reserve1 = U256::from(1_000_000_000_000u64);
        let amount_in = U256::from_str("1000000000000000000").unwrap();

        let out = get_amount_out(amount_in, true, reserve0, reserve1, 5, 18, 6).unwrap();

        assert!(out > U256::ZERO);
        assert!(out < reserve1);
    }

    #[test]
    fn test_get_limits_zero_liquidity_returns_zeroes() {
        let sell = Bytes::from([0_u8; 20]);
        let mut buy_addr = [0_u8; 20];
        buy_addr[19] = 1;
        let buy = Bytes::from(buy_addr);

        let (amount_in, amount_out) =
            get_limits(sell, buy, U256::ZERO, U256::from(1_000_000u64), 18, 18)
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

        let (amount_in, amount_out) =
            get_limits(sell, buy, reserve0, reserve1, 18, 18).expect("limits should succeed");

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

        let (zero_to_one_in, zero_to_one_out) =
            get_limits(sell0.clone(), sell1.clone(), reserve0, reserve1, 18, 18).unwrap();
        let (one_to_zero_in, one_to_zero_out) =
            get_limits(sell1, sell0, reserve0, reserve1, 18, 18).unwrap();

        assert!(zero_to_one_in > BigUint::ZERO);
        assert!(zero_to_one_out > BigUint::ZERO);
        assert!(one_to_zero_in > BigUint::ZERO);
        assert!(one_to_zero_out > BigUint::ZERO);
        assert!(zero_to_one_in != one_to_zero_in || zero_to_one_out != one_to_zero_out);
    }
}
