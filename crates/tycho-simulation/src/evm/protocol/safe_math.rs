//! Safe Math
//!
//! This module contains basic functions to perform arithmetic operations on
//! numerical types of the alloy crate and preventing them from overflowing.
//! Should an operation cause an overflow a result containing TradeSimulationError
//! will be returned.
//! Functions for the types I256, U256, U512 are available.
use alloy::primitives::{I256, U256, U512};
use tycho_common::simulation::errors::SimulationError;

pub fn safe_mul_u256(a: U256, b: U256) -> Result<U256, SimulationError> {
    let res = a.checked_mul(b);
    _construc_result_u256(res)
}

pub fn safe_div_u256(a: U256, b: U256) -> Result<U256, SimulationError> {
    if b.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    let res = a.checked_div(b);
    _construc_result_u256(res)
}

pub fn safe_add_u256(a: U256, b: U256) -> Result<U256, SimulationError> {
    let res = a.checked_add(b);
    _construc_result_u256(res)
}

pub fn safe_sub_u256(a: U256, b: U256) -> Result<U256, SimulationError> {
    let res = a.checked_sub(b);
    _construc_result_u256(res)
}

pub fn div_mod_u256(a: U256, b: U256) -> Result<(U256, U256), SimulationError> {
    if b.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    let result = a / b;
    let rest = a % b;
    Ok((result, rest))
}

pub fn _construc_result_u256(res: Option<U256>) -> Result<U256, SimulationError> {
    match res {
        None => Err(SimulationError::FatalError("U256 arithmetic overflow".to_string())),
        Some(value) => Ok(value),
    }
}

pub fn safe_mul_u512(a: U512, b: U512) -> Result<U512, SimulationError> {
    let res = a.checked_mul(b);
    _construc_result_u512(res)
}

pub fn safe_div_u512(a: U512, b: U512) -> Result<U512, SimulationError> {
    if b.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    let res = a.checked_div(b);
    _construc_result_u512(res)
}

pub fn safe_add_u512(a: U512, b: U512) -> Result<U512, SimulationError> {
    let res = a.checked_add(b);
    _construc_result_u512(res)
}

pub fn safe_sub_u512(a: U512, b: U512) -> Result<U512, SimulationError> {
    let res = a.checked_sub(b);
    _construc_result_u512(res)
}

pub fn div_mod_u512(a: U512, b: U512) -> Result<(U512, U512), SimulationError> {
    if b.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    let result = a / b;
    let rest = a % b;
    Ok((result, rest))
}

pub fn _construc_result_u512(res: Option<U512>) -> Result<U512, SimulationError> {
    match res {
        None => Err(SimulationError::FatalError("U512 arithmetic overflow".to_string())),
        Some(value) => Ok(value),
    }
}

pub fn safe_mul_i256(a: I256, b: I256) -> Result<I256, SimulationError> {
    let res = a.checked_mul(b);
    _construc_result_i256(res)
}

pub fn safe_div_i256(a: I256, b: I256) -> Result<I256, SimulationError> {
    if b.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    let res = a.checked_div(b);
    _construc_result_i256(res)
}

pub fn safe_add_i256(a: I256, b: I256) -> Result<I256, SimulationError> {
    let res = a.checked_add(b);
    _construc_result_i256(res)
}

pub fn safe_sub_i256(a: I256, b: I256) -> Result<I256, SimulationError> {
    let res = a.checked_sub(b);
    _construc_result_i256(res)
}

pub fn _construc_result_i256(res: Option<I256>) -> Result<I256, SimulationError> {
    match res {
        None => Err(SimulationError::FatalError("I256 arithmetic overflow".to_string())),
        Some(value) => Ok(value),
    }
}

/// Computes the integer square root of a U512 value using Newton's method.
///
/// Returns the floor of the square root.
///
/// # Algorithm
///
/// Uses Newton's method iteration:
/// - Start with initial guess based on bit length
/// - Iterate: x_new = (x + n/x) / 2
/// - Stop when convergence is reached or value stops decreasing
pub fn sqrt_u512(value: U512) -> U512 {
    // Handle zero case
    if value == U512::ZERO {
        return U512::ZERO;
    }

    // Handle small values
    if value == U512::from(1u32) {
        return U512::from(1u32);
    }

    // Initial guess: use bit length to get approximate starting point
    // For square root, start with 2^(bits/2)
    let bits = 512 - value.leading_zeros();
    let mut result = U512::from(1u32) << (bits / 2);

    // Newton's method iteration for square root
    // x_new = (x + n/x) / 2
    let mut decreasing = false;
    loop {
        // Calculate: (value / result + result) / 2
        let division = value / result;
        let iter = (division + result) / U512::from(2u32);

        // Check convergence
        if iter == result {
            // Hit fixed point, we're done
            break;
        }

        if iter > result {
            if decreasing {
                // Was decreasing, now increasing - we've converged
                break;
            }
            // Limit increase to prevent slow convergence
            result =
                if iter > result * U512::from(2u32) { result * U512::from(2u32) } else { iter };
        } else {
            // Converging downwards
            decreasing = true;
            result = iter;
        }
    }

    result
}

/// Integer square root for U256, returning U256
pub fn sqrt_u256(value: U256) -> Result<U256, SimulationError> {
    if value == U256::ZERO {
        return Ok(U256::ZERO);
    }

    let bits = 256 - value.leading_zeros();
    let mut remainder = U256::ZERO;
    let mut temp = U256::ZERO;
    let result = compute_karatsuba_sqrt(value, &mut remainder, &mut temp, bits);

    // Extract lower 256 bits
    let limbs = result.as_limbs();
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

/// Recursive Karatsuba square root implementation
/// Computes sqrt(x) and stores remainder in r
/// Uses temp variable t for intermediate calculations
/// Ref: https://hal.inria.fr/file/index/docid/72854/filename/RR-3805.pdf
fn compute_karatsuba_sqrt(x: U256, r: &mut U256, t: &mut U256, bits: usize) -> U256 {
    // Base case: exact integer floor sqrt once the value fits in one 64-bit limb.
    // `u64::isqrt` returns floor(sqrt) directly; the root is at most u32::MAX, so
    // `result * result` cannot overflow u64.
    if bits <= 64 {
        let x_small = x.as_limbs()[0];
        let result = x_small.isqrt();
        *r = x - U256::from(result * result);
        return U256::from(result);
    }

    // Divide-and-conquer approach
    // Split into quarters: process b bits at a time where b = bits/4
    let b = bits / 4;

    // q = x >> (2*b)  -- extract upper bits
    let mut q = x >> (b * 2);

    // Recursively compute sqrt of upper portion
    let mut s = compute_karatsuba_sqrt(q, r, t, bits - b * 2);

    // Build mask for extracting bits: (1 << (2*b)) - 1
    *t = (U256::from(1u32) << (b * 2)) - U256::from(1u32);

    // Extract middle bits and combine with remainder from recursive call
    *r = (*r << b) | ((x & *t) >> b);

    // Divide: t = r / (2*s), with quotient q and remainder r
    s <<= 1;
    q = *r / s;
    *r -= q * s;

    // Build s = (s << (b-1)) + q
    s = (s << (b - 1)) + q;

    // Extract lower b bits
    *t = (U256::from(1u32) << b) - U256::from(1u32);
    *r = (*r << b) | (x & *t);

    // Compute q^2
    let q_squared = q * q;

    // Adjust if remainder is too small
    if *r < q_squared {
        *t = (s << 1) - U256::from(1u32);
        *r += *t;
        s -= U256::from(1u32);
    }

    *r -= q_squared;
    s
}

#[cfg(test)]
mod safe_math_tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;

    const U256_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, u64::MAX, u64::MAX]);
    const U512_MAX: U512 = U512::from_limbs([
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
    ]);
    /// I256 maximum value: 2^255 - 1
    const I256_MAX: I256 = I256::from_raw(U256::from_limbs([
        u64::MAX,
        u64::MAX,
        u64::MAX,
        9223372036854775807u64, // 2^63 - 1 in the highest limb
    ]));

    /// I256 minimum value: -2^255
    const I256_MIN: I256 = I256::from_raw(U256::from_limbs([
        0,
        0,
        0,
        9223372036854775808u64, // 2^63 in the highest limb
    ]));

    fn u256(s: &str) -> U256 {
        U256::from_str(s).unwrap()
    }

    #[rstest]
    #[case(U256_MAX, u256("2"), true, false, u256("0"))]
    #[case(u256("3"), u256("2"), false, true, u256("6"))]
    fn test_safe_mul_u256(
        #[case] a: U256,
        #[case] b: U256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U256,
    ) {
        let res = safe_mul_u256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(U256_MAX, u256("2"), true, false, u256("0"))]
    #[case(u256("3"), u256("2"), false, true, u256("5"))]
    fn test_safe_add_u256(
        #[case] a: U256,
        #[case] b: U256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U256,
    ) {
        let res = safe_add_u256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(u256("0"), u256("2"), true, false, u256("0"))]
    #[case(u256("10"), u256("2"), false, true, u256("8"))]
    fn test_safe_sub_u256(
        #[case] a: U256,
        #[case] b: U256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U256,
    ) {
        let res = safe_sub_u256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(u256("1"), u256("0"), true, false, u256("0"))]
    #[case(u256("10"), u256("2"), false, true, u256("5"))]
    fn test_safe_div_u256(
        #[case] a: U256,
        #[case] b: U256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U256,
    ) {
        let res = safe_div_u256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    fn u512(s: &str) -> U512 {
        U512::from_str(s).unwrap()
    }

    #[rstest]
    #[case(U512_MAX, u512("2"), true, false, u512("0"))]
    #[case(u512("3"), u512("2"), false, true, u512("6"))]
    fn test_safe_mul_u512(
        #[case] a: U512,
        #[case] b: U512,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U512,
    ) {
        let res = safe_mul_u512(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(U512_MAX, u512("2"), true, false, u512("0"))]
    #[case(u512("3"), u512("2"), false, true, u512("5"))]
    fn test_safe_add_u512(
        #[case] a: U512,
        #[case] b: U512,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U512,
    ) {
        let res = safe_add_u512(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(u512("0"), u512("2"), true, false, u512("0"))]
    #[case(u512("10"), u512("2"), false, true, u512("8"))]
    fn test_safe_sub_u512(
        #[case] a: U512,
        #[case] b: U512,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U512,
    ) {
        let res = safe_sub_u512(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(u512("1"), u512("0"), true, false, u512("0"))]
    #[case(u512("10"), u512("2"), false, true, u512("5"))]
    fn test_safe_div_u512(
        #[case] a: U512,
        #[case] b: U512,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: U512,
    ) {
        let res = safe_div_u512(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    fn i256(s: &str) -> I256 {
        I256::from_str(s).unwrap()
    }

    #[rstest]
    #[case(I256_MAX, i256("2"), true, false, i256("0"))]
    #[case(i256("3"), i256("2"), false, true, i256("6"))]
    fn test_safe_mul_i256(
        #[case] a: I256,
        #[case] b: I256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: I256,
    ) {
        let res = safe_mul_i256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(I256_MAX, i256("2"), true, false, i256("0"))]
    #[case(i256("3"), i256("2"), false, true, i256("5"))]
    fn test_safe_add_i256(
        #[case] a: I256,
        #[case] b: I256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: I256,
    ) {
        let res = safe_add_i256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(I256_MIN, i256("2"), true, false, i256("0"))]
    #[case(i256("10"), i256("2"), false, true, i256("8"))]
    fn test_safe_sub_i256(
        #[case] a: I256,
        #[case] b: I256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: I256,
    ) {
        let res = safe_sub_i256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[rstest]
    #[case(i256("1"), i256("0"), true, false, i256("0"))]
    #[case(i256("10"), i256("2"), false, true, i256("5"))]
    fn test_safe_div_i256(
        #[case] a: I256,
        #[case] b: I256,
        #[case] is_err: bool,
        #[case] is_ok: bool,
        #[case] expected: I256,
    ) {
        let res = safe_div_i256(a, b);
        assert_eq!(res.is_err(), is_err);
        assert_eq!(res.is_ok(), is_ok);

        if is_ok {
            assert_eq!(res.unwrap(), expected);
        }
    }

    #[test]
    fn test_sqrt_u512() {
        // Test edge cases
        assert_eq!(sqrt_u512(U512::ZERO), U512::ZERO);
        assert_eq!(sqrt_u512(U512::from(1u32)), U512::from(1u32));

        // Test small perfect squares
        assert_eq!(sqrt_u512(U512::from(4u32)), U512::from(2u32));
        assert_eq!(sqrt_u512(U512::from(100u32)), U512::from(10u32));
        assert_eq!(sqrt_u512(U512::from(10000u32)), U512::from(100u32));
        assert_eq!(sqrt_u512(U512::from(1000000u32)), U512::from(1000u32));

        // For non-perfect squares, should return floor of sqrt
        assert_eq!(sqrt_u512(U512::from(2u32)), U512::from(1u32)); // sqrt(2) ≈ 1.41
        assert_eq!(sqrt_u512(U512::from(3u32)), U512::from(1u32)); // sqrt(3) ≈ 1.73
        assert_eq!(sqrt_u512(U512::from(5u32)), U512::from(2u32)); // sqrt(5) ≈ 2.23
        assert_eq!(sqrt_u512(U512::from(8u32)), U512::from(2u32)); // sqrt(8) ≈ 2.82
        assert_eq!(sqrt_u512(U512::from(10u32)), U512::from(3u32)); // sqrt(10) ≈ 3.16
        assert_eq!(sqrt_u512(U512::from(15u32)), U512::from(3u32)); // sqrt(15) ≈ 3.87
        assert_eq!(sqrt_u512(U512::from(99u32)), U512::from(9u32)); // sqrt(99) ≈ 9.94

        // Test large values
        let large = U512::from_str("1000000000000000000000000000000000000").unwrap();
        let sqrt_large = sqrt_u512(large);
        // Verify that sqrt_large^2 <= large < (sqrt_large + 1)^2
        assert!(sqrt_large * sqrt_large <= large);
        assert!((sqrt_large + U512::from(1u32)) * (sqrt_large + U512::from(1u32)) > large);
    }

    // u64::MAX as f64 rounds up to 2^64, so the unclamped f64 seed would be 2^32 and
    // its square would overflow u64.
    #[test]
    fn test_sqrt_u256_u64_max() {
        let result = sqrt_u256(U256::from(u64::MAX)).unwrap();
        assert_eq!(result, U256::from(u32::MAX));
    }

    // 67108865^2 - 1: the f64 seed rounds up by 1; the correction step must restore the
    // floor invariant result² ≤ x < (result+1)².
    #[test]
    fn test_sqrt_u256_floor_near_perfect_square() {
        let x = U256::from(67108865u64 * 67108865u64 - 1);
        let result = sqrt_u256(x).unwrap();
        assert_eq!(result, U256::from(67108864u64));
    }

    // Whole-input-range sweep: boundary values and deterministic pseudo-random draws at
    // every bit length 1..=256. The floor invariant result² ≤ x < (result+1)² fully
    // characterizes integer sqrt, checked in U512 so x near U256::MAX cannot overflow.
    #[test]
    fn test_sqrt_u256_floor_invariant_full_range() {
        let mut rng_state = 0x9E3779B97F4A7C15u64;
        let mut next_rand = move || {
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            rng_state.wrapping_mul(0x2545F4914F6CDD1D)
        };

        let mut cases: Vec<U256> = vec![U256::ZERO, U256::from(1u64), U256::MAX];
        for bits in 1..=256u32 {
            let low = U256::from(1u64) << (bits - 1);
            let high =
                if bits == 256 { U256::MAX } else { (U256::from(1u64) << bits) - U256::from(1u64) };
            cases.push(low);
            cases.push(high);
            for _ in 0..4 {
                let mut draw = U256::ZERO;
                for limb in 0..4 {
                    draw |= U256::from(next_rand()) << (64 * limb);
                }
                cases.push(low + draw % (high - low + U256::from(1u64)));
            }
        }

        for x in cases {
            let result = sqrt_u256(x).unwrap();
            let wide = U512::from(result);
            let x_wide = U512::from(x);
            assert!(wide * wide <= x_wide, "floor violated for x={x}");
            let next = wide + U512::from(1u64);
            assert!(next * next > x_wide, "not the greatest root for x={x}");
        }
    }

    #[test]
    fn test_sqrt_u256_floor_invariant_in_base_case_range() {
        for x_small in [
            0u64,
            1,
            2,
            3,
            4,
            (1 << 26) - 1,
            1 << 26,
            (1 << 53) - 1,
            1 << 53,
            (1 << 53) + 1,
            67108864 * 67108864,
            u32::MAX as u64,
            (u32::MAX as u64).pow(2),
            (u32::MAX as u64).pow(2) - 1,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let x = U256::from(x_small);
            let result = sqrt_u256(x).unwrap();
            assert!(result * result <= x, "floor violated for {x_small}");
            let next = result + U256::from(1u64);
            assert!(next * next > x, "not the greatest root for {x_small}");
        }
    }

    // Adversarial perfect-square and k²±1 inputs at large bit-lengths, where the recursion
    // (not the base case) runs and the off-by-one correction in the combine step must hold.
    // Every root stays below 2^128, so k² fits U256 without overflow.
    #[test]
    fn test_sqrt_u256_recursive_perfect_square_boundaries() {
        let one = U256::from(1u64);
        let roots = [
            one << 33, // first root whose square (2^66) leaves the base case
            one << 64,
            (one << 96) - one,
            one << 100,
            (one << 120) + U256::from(12345u64),
            (one << 127) - one,
            (one << 128) - one, // floor(sqrt(U256::MAX))
        ];
        for k in roots {
            let square = k * k;
            assert_eq!(sqrt_u256(square).unwrap(), k, "sqrt(k²) for k={k}");
            assert_eq!(sqrt_u256(square - one).unwrap(), k - one, "sqrt(k²−1) for k={k}");
            // k² + 1 < (k+1)² for k >= 1, so the floor stays at k.
            assert_eq!(sqrt_u256(square + one).unwrap(), k, "sqrt(k²+1) for k={k}");
        }
    }
}
