use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::Zero;
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
    let decimals0_scale = U256::from(10u128.pow(decimals0 as u32));
    let decimals1_scale = U256::from(10u128.pow(decimals1 as u32));

    let reserve0_normalized = safe_div_u256(safe_mul_u256(reserve0, E18)?, decimals0_scale)?;
    let reserve1_normalized = safe_div_u256(safe_mul_u256(reserve1, E18)?, decimals1_scale)?;

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
        safe_div_u256(safe_mul_u256(amount_in_with_fee, E18)?, decimals_in_scale)?;

    let x0 = safe_add_u256(amount_in_normalized, reserve_in)?;
    let y_new = _get_y(x0, xy, reserve_out)?;
    let y_diff = safe_sub_u256(reserve_out, y_new)?;
    let decimals_out_scale = U256::from(10u128.pow(decimals_out as u32));
    let amount_out = safe_div_u256(safe_mul_u256(y_diff, decimals_out_scale)?, E18)?;

    Ok(amount_out)
}

/// 1e18, the fixed-point scale of the Solidly invariant.
const E18: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

/// The terms of `_f` and `_d` that depend only on `x0`, which `_get_y` keeps fixed.
struct Invariants {
    /// `x0^2 / 1e18`
    x0_squared: U256,
    /// `x0^3 / 1e18^2`, which is `_d`'s second term in full
    x0_cubed: U256,
    /// `3 * x0`, the constant factor of `_d`'s first term
    three_x0: U256,
}

impl Invariants {
    fn new(x0: U256) -> Result<Self, SimulationError> {
        let x0_squared = safe_div_u256(safe_mul_u256(x0, x0)?, E18)?;
        let x0_cubed = safe_div_u256(safe_mul_u256(x0_squared, x0)?, E18)?;
        let three_x0 = safe_mul_u256(U256::from(3), x0)?;
        Ok(Self { x0_squared, x0_cubed, three_x0 })
    }
}

/// `f(x0, y) = x0*y * (x0^2 + y^2)`, all in 1e18 fixed point.
fn _f_with(x0: U256, y: U256, inv: &Invariants, y_squared: U256) -> Result<U256, SimulationError> {
    let a = safe_div_u256(safe_mul_u256(x0, y)?, E18)?;
    let b = safe_add_u256(inv.x0_squared, y_squared)?;
    safe_div_u256(safe_mul_u256(a, b)?, E18)
}

/// `d(x0, y) = 3*x0*y^2 + x0^3`, the derivative of `_f` with respect to `y`.
fn _d_with(inv: &Invariants, y_squared: U256) -> Result<U256, SimulationError> {
    let term1 = safe_div_u256(safe_mul_u256(inv.three_x0, y_squared)?, E18)?;
    safe_add_u256(term1, inv.x0_cubed)
}

fn y_squared_of(y: U256) -> Result<U256, SimulationError> {
    safe_div_u256(safe_mul_u256(y, y)?, E18)
}

fn _k(x: U256, y: U256, decimals0: u8, decimals1: u8) -> Result<U256, SimulationError> {
    let decimals0_scale = U256::from(10u128.pow(decimals0 as u32));
    let decimals1_scale = U256::from(10u128.pow(decimals1 as u32));

    let x = safe_div_u256(safe_mul_u256(x, E18)?, decimals0_scale)?;
    let y = safe_div_u256(safe_mul_u256(y, E18)?, decimals1_scale)?;
    let a = safe_div_u256(safe_mul_u256(x, y)?, E18)?;
    let b = safe_add_u256(
        safe_div_u256(safe_mul_u256(x, x)?, E18)?,
        safe_div_u256(safe_mul_u256(y, y)?, E18)?,
    )?;
    safe_div_u256(safe_mul_u256(a, b)?, E18)
}

fn _get_y(x0: U256, xy: U256, mut y: U256) -> Result<U256, SimulationError> {
    let inv = Invariants::new(x0)?;

    for _ in 0..255 {
        let y_squared = y_squared_of(y)?;
        let k = _f_with(x0, y, &inv, y_squared)?;
        let d = _d_with(&inv, y_squared)?;

        if d.is_zero() {
            return Err(SimulationError::FatalError("Division by zero in _get_y".to_string()));
        }

        if k < xy {
            let diff = safe_sub_u256(xy, k)?;
            let mut dy = safe_div_u256(safe_mul_u256(diff, E18)?, d)?;

            if dy.is_zero() {
                if k == xy {
                    return Ok(y);
                }

                let y_plus_1 = safe_add_u256(y, U256::from(1))?;
                if _f_with(x0, y_plus_1, &inv, y_squared_of(y_plus_1)?)? > xy {
                    return Ok(y_plus_1);
                }

                dy = U256::from(1);
            }
            y = safe_add_u256(y, dy)?;
        } else {
            let diff = safe_sub_u256(k, xy)?;
            let mut dy = safe_div_u256(safe_mul_u256(diff, E18)?, d)?;

            if dy.is_zero() {
                if k == xy {
                    return Ok(y);
                }
                let y_minus_1 = safe_sub_u256(y, U256::from(1))?;
                if _f_with(x0, y_minus_1, &inv, y_squared_of(y_minus_1)?)? < xy {
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
    if reserve0.is_zero() || reserve1.is_zero() {
        return Ok((BigUint::zero(), BigUint::zero()));
    }

    let zero_for_one = sell_token < buy_token;
    let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero_for_one {
        (reserve0, reserve1, decimals0, decimals1)
    } else {
        (reserve1, reserve0, decimals1, decimals0)
    };

    let xy = _k(reserve0, reserve1, decimals0, decimals1)?;
    let decimals_in_scale = U256::from(10u128.pow(decimals_in as u32));
    let decimals_out_scale = U256::from(10u128.pow(decimals_out as u32));

    let reserve_in_normalized = safe_div_u256(safe_mul_u256(reserve_in, E18)?, decimals_in_scale)?;
    let reserve_out_normalized =
        safe_div_u256(safe_mul_u256(reserve_out, E18)?, decimals_out_scale)?;

    let amount_in_estimate =
        safe_div_u256(safe_mul_u256(reserve_in, U256::from(300))?, U256::from(100))?;
    let amount_in_normalized =
        safe_div_u256(safe_mul_u256(amount_in_estimate, E18)?, decimals_in_scale)?;

    let x0 = safe_add_u256(reserve_in_normalized, amount_in_normalized)?;
    let y_new = _get_y(x0, xy, reserve_out_normalized)?;
    let amount_out_normalized = safe_sub_u256(reserve_out_normalized, y_new)?;
    let amount_out = safe_div_u256(safe_mul_u256(amount_out_normalized, decimals_out_scale)?, E18)?;

    Ok((u256_to_biguint(amount_in_estimate), u256_to_biguint(amount_out)))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use tycho_common::simulation::errors::SimulationError;

    use super::*;

    /// The arithmetic before the terms were hoisted, as the differential oracle.
    mod reference {
        use super::*;

        fn e18() -> U256 {
            U256::from(10u128.pow(18))
        }

        pub fn f(x0: U256, y: U256) -> Result<U256, SimulationError> {
            let e18 = e18();
            let a = safe_div_u256(safe_mul_u256(x0, y)?, e18)?;
            let x0_squared = safe_div_u256(safe_mul_u256(x0, x0)?, e18)?;
            let y_squared = safe_div_u256(safe_mul_u256(y, y)?, e18)?;
            let b = safe_add_u256(x0_squared, y_squared)?;
            safe_div_u256(safe_mul_u256(a, b)?, e18)
        }

        pub fn d(x0: U256, y: U256) -> Result<U256, SimulationError> {
            let e18 = e18();
            let y_squared = safe_div_u256(safe_mul_u256(y, y)?, e18)?;
            let term1 =
                safe_div_u256(safe_mul_u256(safe_mul_u256(U256::from(3), x0)?, y_squared)?, e18)?;
            let x0_squared = safe_div_u256(safe_mul_u256(x0, x0)?, e18)?;
            let term2 = safe_div_u256(safe_mul_u256(x0_squared, x0)?, e18)?;
            safe_add_u256(term1, term2)
        }

        pub fn get_y(x0: U256, xy: U256, mut y: U256) -> Result<U256, SimulationError> {
            let e18 = e18();
            for _ in 0..255 {
                let k = f(x0, y)?;
                if k < xy {
                    let dv = d(x0, y)?;
                    if dv.is_zero() {
                        return Err(SimulationError::FatalError(
                            "Division by zero in _get_y".to_string(),
                        ));
                    }
                    let diff = safe_sub_u256(xy, k)?;
                    let mut dy = safe_div_u256(safe_mul_u256(diff, e18)?, dv)?;
                    if dy.is_zero() {
                        if k == xy {
                            return Ok(y);
                        }
                        let y_plus_1 = safe_add_u256(y, U256::from(1))?;
                        if f(x0, y_plus_1)? > xy {
                            return Ok(y_plus_1);
                        }
                        dy = U256::from(1);
                    }
                    y = safe_add_u256(y, dy)?;
                } else {
                    let dv = d(x0, y)?;
                    if dv.is_zero() {
                        return Err(SimulationError::FatalError(
                            "Division by zero in _get_y".to_string(),
                        ));
                    }
                    let diff = safe_sub_u256(k, xy)?;
                    let mut dy = safe_div_u256(safe_mul_u256(diff, e18)?, dv)?;
                    if dy.is_zero() {
                        if k == xy {
                            return Ok(y);
                        }
                        let y_minus_1 = safe_sub_u256(y, U256::from(1))?;
                        if f(x0, y_minus_1)? < xy {
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

    #[test]
    fn differential_matches_unfused_arithmetic() {
        let reserve_pairs = [
            ("2642455102346776307825", "3320301880379841502303", 18u8, 18u8),
            ("1000000000000000000000000", "1000000000000", 18, 6),
            ("999999999999999999", "1000000000000000000000", 18, 18),
            ("50000000000", "49000000000", 6, 6),
            ("123456789012345678901", "987654321098765432109", 18, 18),
        ];
        let size_divisors = [1_000_000u64, 100_000, 10_000, 1_000, 100, 20, 5, 3, 2];

        let mut compared = 0usize;
        for (r0, r1, d0, d1) in reserve_pairs {
            let reserve0 = U256::from_str(r0).unwrap();
            let reserve1 = U256::from_str(r1).unwrap();

            for zero2one in [true, false] {
                let reserve_in = if zero2one { reserve0 } else { reserve1 };
                for divisor in size_divisors {
                    let amount_in = reserve_in / U256::from(divisor);
                    if amount_in.is_zero() {
                        continue;
                    }

                    let xy = _k(reserve0, reserve1, d0, d1).unwrap();
                    let scale_in = U256::from(10u128.pow(if zero2one { d0 } else { d1 } as u32));
                    let scale_out = U256::from(10u128.pow(if zero2one { d1 } else { d0 } as u32));
                    let reserve_in_norm =
                        safe_div_u256(safe_mul_u256(reserve_in, E18).unwrap(), scale_in).unwrap();
                    let reserve_out = if zero2one { reserve1 } else { reserve0 };
                    let reserve_out_norm =
                        safe_div_u256(safe_mul_u256(reserve_out, E18).unwrap(), scale_out).unwrap();
                    let amount_norm =
                        safe_div_u256(safe_mul_u256(amount_in, E18).unwrap(), scale_in).unwrap();
                    let x0 = safe_add_u256(amount_norm, reserve_in_norm).unwrap();

                    let fused = _get_y(x0, xy, reserve_out_norm);
                    let unfused = reference::get_y(x0, xy, reserve_out_norm);

                    match (fused, unfused) {
                        (Ok(a), Ok(b)) => assert_eq!(
                            a, b,
                            "fused _get_y diverged: reserves {r0}/{r1} decimals {d0}/{d1} \
                             zero2one {zero2one} divisor {divisor}"
                        ),
                        (Err(_), Err(_)) => {}
                        (a, b) => panic!(
                            "fused and unfused disagreed on success: {a:?} vs {b:?} \
                             (reserves {r0}/{r1}, divisor {divisor})"
                        ),
                    }
                    compared += 1;
                }
            }
        }

        assert!(compared >= 80, "differential sweep covered only {compared} cases");
    }

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
