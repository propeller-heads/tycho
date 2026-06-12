//! Frozen pre-elision copies of the swap-math primitives, used as differential references.
//!
//! The production functions in `solidity_math` and `sqrt_price_math` replaced provably-dead
//! checked multiplies/adds with wrapping ops (see the SAFETY comments at each site). The
//! `*_reference` functions below are verbatim copies of the originals (checked ops, `U512`
//! fallbacks) so randomized differential tests can assert bit-equality of `Ok` values and
//! equality of `Err`/`Ok`-ness over the documented operand domains.

use alloy::primitives::{I256, U256, U512};
use ruint::aliases::U384;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::{
    div_mod_u256, div_mod_u512, safe_add_u256, safe_div_u256, safe_div_u512, safe_mul_u256,
    safe_mul_u512, safe_sub_u256,
};

const Q96: U256 = U256::from_limbs([0, 4294967296, 0, 0]);
const RESOLUTION: U256 = U256::from_limbs([96, 0, 0, 0]);
const U160_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 4294967295, 0]);

pub(crate) fn mul_div_rounding_up_reference(
    a: U256,
    b: U256,
    denom: U256,
) -> Result<U256, SimulationError> {
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

pub(crate) fn mul_div_reference(a: U256, b: U256, denom: U256) -> Result<U256, SimulationError> {
    let a_big = U512::from(a);
    let b_big = U512::from(b);
    let product = safe_mul_u512(a_big, b_big)?;
    let result = safe_div_u512(product, U512::from(denom))?;
    truncate_to_u256(result)
}

pub(crate) fn mul_div_384_reference(
    a: U256,
    b: U256,
    denom: U256,
) -> Result<U256, SimulationError> {
    let Some(product) = U384::from(a).checked_mul(U384::from(b)) else {
        return mul_div_reference(a, b, denom);
    };
    let denom = U384::from(denom);
    if denom.is_zero() {
        return Err(SimulationError::FatalError("Division by zero".to_string()));
    }
    truncate_u384_to_u256(product / denom)
}

pub(crate) fn mul_div_rounding_up_384_reference(
    a: U256,
    b: U256,
    denom: U256,
) -> Result<U256, SimulationError> {
    let Some(product) = U384::from(a).checked_mul(U384::from(b)) else {
        return mul_div_rounding_up_reference(a, b, denom);
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
    let limbs = value.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return Err(SimulationError::FatalError("Overflow: Value exceeds 256 bits".to_string()));
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

fn maybe_flip_ratios(a: U256, b: U256) -> (U256, U256) {
    if a > b {
        (b, a)
    } else {
        (a, b)
    }
}

fn div_rounding_up_reference(a: U256, b: U256) -> Result<U256, SimulationError> {
    let (result, rest) = div_mod_u256(a, b)?;
    if rest > U256::from(0u64) {
        let res = safe_add_u256(result, U256::from(1u64))?;
        Ok(res)
    } else {
        Ok(result)
    }
}

pub(crate) fn get_amount0_delta_reference(
    a: U256,
    b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let (sqrt_ratio_a, sqrt_ratio_b) = maybe_flip_ratios(a, b);

    if sqrt_ratio_a == U256::ZERO {
        return Err(SimulationError::FatalError(
            "sqrt_ratio_a must be greater than zero".to_string(),
        ));
    }

    let numerator1 = U256::from(liquidity) << RESOLUTION;
    let numerator2 = sqrt_ratio_b - sqrt_ratio_a;

    if round_up {
        div_rounding_up_reference(
            mul_div_rounding_up_384_reference(numerator1, numerator2, sqrt_ratio_b)?,
            sqrt_ratio_a,
        )
    } else {
        safe_div_u256(
            mul_div_rounding_up_384_reference(numerator1, numerator2, sqrt_ratio_b)?,
            sqrt_ratio_a,
        )
    }
}

pub(crate) fn get_amount1_delta_reference(
    a: U256,
    b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let (sqrt_ratio_a, sqrt_ratio_b) = maybe_flip_ratios(a, b);
    if round_up {
        mul_div_rounding_up_384_reference(U256::from(liquidity), sqrt_ratio_b - sqrt_ratio_a, Q96)
    } else {
        safe_div_u256(
            safe_mul_u256(U256::from(liquidity), safe_sub_u256(sqrt_ratio_b, sqrt_ratio_a)?)?,
            Q96,
        )
    }
}

pub(crate) fn get_next_sqrt_price_from_input_reference(
    sqrt_price: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256, SimulationError> {
    if sqrt_price == U256::ZERO {
        return Err(SimulationError::FatalError("sqrt_price must be greater than zero".to_string()));
    }

    if zero_for_one {
        Ok(get_next_sqrt_price_from_amount0_rounding_up_reference(
            sqrt_price, liquidity, amount_in, true,
        )?)
    } else {
        Ok(get_next_sqrt_price_from_amount1_rounding_down_reference(
            sqrt_price, liquidity, amount_in, true,
        )?)
    }
}

pub(crate) fn get_next_sqrt_price_from_output_reference(
    sqrt_price: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256, SimulationError> {
    if sqrt_price == U256::ZERO {
        return Err(SimulationError::FatalError("sqrt_price must be greater than zero".to_string()));
    }
    if liquidity == 0 {
        return Err(SimulationError::FatalError("liquidity must be greater than zero".to_string()));
    }

    if zero_for_one {
        Ok(get_next_sqrt_price_from_amount1_rounding_down_reference(
            sqrt_price, liquidity, amount_in, false,
        )?)
    } else {
        Ok(get_next_sqrt_price_from_amount0_rounding_up_reference(
            sqrt_price, liquidity, amount_in, false,
        )?)
    }
}

fn get_next_sqrt_price_from_amount0_rounding_up_reference(
    sqrt_price: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, SimulationError> {
    if amount == U256::from(0u64) {
        return Ok(sqrt_price);
    }
    let numerator1 = U256::from(liquidity) << RESOLUTION;

    if add {
        let (product, _) = amount.overflowing_mul(sqrt_price);
        if product / amount == sqrt_price {
            let denominator = safe_add_u256(numerator1, product)?;
            if denominator >= numerator1 {
                return mul_div_rounding_up_384_reference(numerator1, sqrt_price, denominator);
            }
        }
        div_rounding_up_reference(
            numerator1,
            safe_add_u256(safe_div_u256(numerator1, sqrt_price)?, amount)?,
        )
    } else {
        let (product, _) = amount.overflowing_mul(sqrt_price);
        if safe_div_u256(product, amount)? != sqrt_price || numerator1 <= product {
            return Err(SimulationError::FatalError(
                "sqrt_price_math: overflow in get_next_sqrt_price_from_amount0".to_string(),
            ));
        }
        let denominator = safe_sub_u256(numerator1, product)?;
        mul_div_rounding_up_384_reference(numerator1, sqrt_price, denominator)
    }
}

fn get_next_sqrt_price_from_amount1_rounding_down_reference(
    sqrt_price: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, SimulationError> {
    if add {
        let quotient = if amount <= U160_MAX {
            safe_div_u256(amount << RESOLUTION, U256::from(liquidity))
        } else {
            mul_div_reference(amount, Q96, U256::from(liquidity))
        };

        safe_add_u256(sqrt_price, quotient?)
    } else {
        let quotient = if amount <= U160_MAX {
            div_rounding_up_reference(amount << RESOLUTION, U256::from(liquidity))?
        } else {
            mul_div_rounding_up_reference(amount, Q96, U256::from(liquidity))?
        };

        if sqrt_price <= quotient {
            return Err(SimulationError::FatalError(
                "sqrt_price_math: sqrt_price underflow in get_next_sqrt_price_from_amount1"
                    .to_string(),
            ));
        }
        safe_sub_u256(sqrt_price, quotient)
    }
}

pub(crate) fn compute_swap_step_reference(
    sqrt_ratio_current: U256,
    sqrt_ratio_target: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> Result<(U256, U256, U256, U256), SimulationError> {
    let zero_for_one = sqrt_ratio_current >= sqrt_ratio_target;
    let exact_in = amount_remaining >= I256::from_raw(U256::from(0u64));
    let sqrt_ratio_next: U256;
    let mut amount_in = U256::from(0u64);
    let mut amount_out = U256::from(0u64);

    if exact_in {
        let amount_remaining_less_fee = mul_div_384_reference(
            amount_remaining.into_raw(),
            U256::from(1_000_000 - fee_pips),
            U256::from(1_000_000),
        )?;
        amount_in = if zero_for_one {
            get_amount0_delta_reference(sqrt_ratio_target, sqrt_ratio_current, liquidity, true)?
        } else {
            get_amount1_delta_reference(sqrt_ratio_current, sqrt_ratio_target, liquidity, true)?
        };
        if amount_remaining_less_fee >= amount_in {
            sqrt_ratio_next = sqrt_ratio_target
        } else {
            sqrt_ratio_next = get_next_sqrt_price_from_input_reference(
                sqrt_ratio_current,
                liquidity,
                amount_remaining_less_fee,
                zero_for_one,
            )?
        }
    } else {
        amount_out = if zero_for_one {
            get_amount1_delta_reference(sqrt_ratio_target, sqrt_ratio_current, liquidity, false)?
        } else {
            get_amount0_delta_reference(sqrt_ratio_current, sqrt_ratio_target, liquidity, false)?
        };
        if amount_remaining.abs().into_raw() > amount_out {
            sqrt_ratio_next = sqrt_ratio_target;
        } else {
            sqrt_ratio_next = get_next_sqrt_price_from_output_reference(
                sqrt_ratio_current,
                liquidity,
                amount_remaining.abs().into_raw(),
                zero_for_one,
            )?;
        }
    }

    let max = sqrt_ratio_target == sqrt_ratio_next;

    if zero_for_one {
        amount_in = if max && exact_in {
            amount_in
        } else {
            get_amount0_delta_reference(sqrt_ratio_next, sqrt_ratio_current, liquidity, true)?
        };
        amount_out = if max && !exact_in {
            amount_out
        } else {
            get_amount1_delta_reference(sqrt_ratio_next, sqrt_ratio_current, liquidity, false)?
        }
    } else {
        amount_in = if max && exact_in {
            amount_in
        } else {
            get_amount1_delta_reference(sqrt_ratio_current, sqrt_ratio_next, liquidity, true)?
        };
        amount_out = if max && !exact_in {
            amount_out
        } else {
            get_amount0_delta_reference(sqrt_ratio_current, sqrt_ratio_next, liquidity, false)?
        };
    }

    if !exact_in && amount_out > amount_remaining.abs().into_raw() {
        amount_out = amount_remaining.abs().into_raw();
    }

    let fee_amount = if exact_in && sqrt_ratio_next != sqrt_ratio_target {
        safe_sub_u256(amount_remaining.abs().into_raw(), amount_in)?
    } else {
        mul_div_rounding_up_384_reference(
            amount_in,
            U256::from(fee_pips),
            U256::from(1_000_000 - fee_pips),
        )?
    };
    Ok((sqrt_ratio_next, amount_in, amount_out, fee_amount))
}

/// xorshift64* — deterministic, dependency-free PRNG (same scheme as `clmm_capture`).
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// Uniformly random bit width in `[0, max_bits]`, then a uniform value of that width.
    /// Width-stratified sampling covers magnitudes evenly instead of clustering near 2^256.
    pub(crate) fn u256_up_to_bits(&mut self, max_bits: usize) -> U256 {
        let bits = self.below(max_bits as u64 + 1) as usize;
        self.u256_with_bits(bits)
    }

    pub(crate) fn u256_with_bits(&mut self, bits: usize) -> U256 {
        if bits == 0 {
            return U256::ZERO;
        }
        let limbs = [self.next(), self.next(), self.next(), self.next()];
        U256::from_limbs(limbs) >> (256 - bits)
    }

    pub(crate) fn u128_any(&mut self) -> u128 {
        let bits = self.below(129) as usize;
        if bits == 0 {
            return 0;
        }
        let value = ((self.next() as u128) << 64) | self.next() as u128;
        value >> (128 - bits)
    }
}

/// Asserts bit-equality of `Ok` values and equality of `Err`/`Ok`-ness (including the error
/// message) between the elided implementation and its frozen reference.
pub(crate) fn assert_result_eq<T: PartialEq + std::fmt::Debug>(
    actual: &Result<T, SimulationError>,
    reference: &Result<T, SimulationError>,
    context: &dyn std::fmt::Debug,
) {
    match (actual, reference) {
        (Ok(a), Ok(r)) => assert_eq!(a, r, "Ok value diverged for {context:?}"),
        (Err(a), Err(r)) => {
            assert_eq!(format!("{a:?}"), format!("{r:?}"), "Err diverged for {context:?}")
        }
        _ => panic!("Ok/Err-ness diverged for {context:?}: actual={actual:?} ref={reference:?}"),
    }
}

#[cfg(test)]
mod differential_tests {
    use super::*;
    use crate::evm::protocol::utils::{
        solidity_math::{mul_div, mul_div_384, mul_div_rounding_up, mul_div_rounding_up_384},
        uniswap::{
            sqrt_price_math::{
                get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
                get_next_sqrt_price_from_output,
            },
            swap_math::compute_swap_step,
            tick_math::{MAX_SQRT_RATIO, MIN_SQRT_RATIO},
        },
    };

    const CASES: usize = 100_000;

    fn edge_u256(rng: &mut Rng) -> U256 {
        let u112_max = (U256::from(1u8) << 112) - U256::from(1u8);
        let u160_max = (U256::from(1u8) << 160) - U256::from(1u8);
        let edges = [
            U256::ZERO,
            U256::from(1u8),
            U256::MAX,
            u112_max,
            u112_max + U256::from(1u8),
            u160_max,
            u160_max + U256::from(1u8),
            MIN_SQRT_RATIO,
            MAX_SQRT_RATIO,
            Q96,
        ];
        edges[rng.below(edges.len() as u64) as usize]
    }

    /// Full-range operands: the elided U512 widening check is unconditionally dead, so the
    /// whole U256^3 input space is in-domain.
    #[test]
    fn mul_div_and_rounding_up_match_reference_full_range() {
        let mut rng = Rng(0xD1FF_0001);
        for case in 0..CASES {
            let pick = |rng: &mut Rng, case: usize| {
                if case.is_multiple_of(16) {
                    edge_u256(rng)
                } else {
                    rng.u256_up_to_bits(256)
                }
            };
            let a = pick(&mut rng, case);
            let b = pick(&mut rng, case / 2);
            let denom = pick(&mut rng, case / 3);
            assert_result_eq(
                &mul_div(a, b, denom),
                &mul_div_reference(a, b, denom),
                &(a, b, denom),
            );
            assert_result_eq(
                &mul_div_rounding_up(a, b, denom),
                &mul_div_rounding_up_reference(a, b, denom),
                &(a, b, denom),
            );
        }
    }

    /// In-domain operands for the 384-bit fast path: widths chosen so the product stays below
    /// 2^384 (the proven swap-math bound), including pairs sitting exactly on the
    /// width boundary (e.g. 224 x 160 bits).
    #[test]
    fn mul_div_384_matches_reference_in_domain() {
        let mut rng = Rng(0xD1FF_0002);
        let tight_pairs = [
            // The canonical tightest swap-math site: (liquidity << 96) x sqrt-price delta.
            ((U256::from(u128::MAX) << 96), (U256::from(1u8) << 160) - U256::from(1u8)),
            (U256::MAX >> 32, U256::from(u128::MAX)),
            (U256::ZERO, U256::MAX),
            (U256::MAX, U256::ZERO),
            (U256::from(1u8), U256::MAX),
        ];
        for case in 0..CASES {
            let (a, b) = if case % 16 == 0 {
                tight_pairs[rng.below(tight_pairs.len() as u64) as usize]
            } else {
                let a_bits = rng.below(257) as usize;
                let b_bits = rng.below((384 - a_bits).min(256) as u64 + 1) as usize;
                (rng.u256_with_bits(a_bits), rng.u256_with_bits(b_bits))
            };
            let denom = if case % 8 == 0 { edge_u256(&mut rng) } else { rng.u256_up_to_bits(256) };
            assert_result_eq(
                &mul_div_384(a, b, denom),
                &mul_div_384_reference(a, b, denom),
                &(a, b, denom),
            );
            assert_result_eq(
                &mul_div_rounding_up_384(a, b, denom),
                &mul_div_rounding_up_384_reference(a, b, denom),
                &(a, b, denom),
            );
        }
    }

    /// Out-of-domain products (>= 2^384) must scream in debug builds instead of silently
    /// wrapping: the proof in `mul_div_384` would be invalidated.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "proven < 2^384 product domain")]
    fn mul_div_384_out_of_domain_screams() {
        let _ = mul_div_384(U256::MAX, U256::MAX, U256::from(1u8));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "proven < 2^384 product domain")]
    fn mul_div_rounding_up_384_out_of_domain_screams() {
        let _ = mul_div_rounding_up_384(U256::MAX, U256::MAX, U256::from(1u8));
    }

    /// Sqrt ratios live in 160 bits (Q64.96 capped at `MAX_SQRT_RATIO`); liquidity is u128.
    /// Zero ratios are included to pin the error-path parity.
    #[test]
    fn get_amount_deltas_match_reference() {
        let mut rng = Rng(0xD1FF_0003);
        for case in 0..CASES {
            let ratio = |rng: &mut Rng, case: usize| match case % 8 {
                0 => MIN_SQRT_RATIO,
                1 => MAX_SQRT_RATIO,
                _ => rng.u256_up_to_bits(160),
            };
            let a = ratio(&mut rng, case);
            let b = ratio(&mut rng, case / 2);
            let liquidity = match case % 16 {
                0 => 0u128,
                1 => u128::MAX,
                _ => rng.u128_any(),
            };
            let round_up = rng.below(2) == 0;
            assert_result_eq(
                &get_amount0_delta(a, b, liquidity, round_up),
                &get_amount0_delta_reference(a, b, liquidity, round_up),
                &(a, b, liquidity, round_up),
            );
            assert_result_eq(
                &get_amount1_delta(a, b, liquidity, round_up),
                &get_amount1_delta_reference(a, b, liquidity, round_up),
                &(a, b, liquidity, round_up),
            );
        }
    }

    /// Amounts span the full U256 range (exercising both the shift and `mul_div` quotient
    /// paths plus the wrapped-product checks); sqrt prices stay in their 160-bit domain.
    #[test]
    fn get_next_sqrt_prices_match_reference() {
        let mut rng = Rng(0xD1FF_0004);
        for case in 0..CASES {
            let sqrt_price = match case % 8 {
                0 => U256::ZERO,
                1 => MIN_SQRT_RATIO,
                2 => MAX_SQRT_RATIO,
                _ => rng.u256_up_to_bits(160),
            };
            let liquidity = match case % 16 {
                0 => 0u128,
                1 => u128::MAX,
                _ => rng.u128_any(),
            };
            let amount =
                if case % 16 == 2 { edge_u256(&mut rng) } else { rng.u256_up_to_bits(256) };
            let zero_for_one = rng.below(2) == 0;
            assert_result_eq(
                &get_next_sqrt_price_from_input(sqrt_price, liquidity, amount, zero_for_one),
                &get_next_sqrt_price_from_input_reference(
                    sqrt_price,
                    liquidity,
                    amount,
                    zero_for_one,
                ),
                &(sqrt_price, liquidity, amount, zero_for_one),
            );
            assert_result_eq(
                &get_next_sqrt_price_from_output(sqrt_price, liquidity, amount, zero_for_one),
                &get_next_sqrt_price_from_output_reference(
                    sqrt_price,
                    liquidity,
                    amount,
                    zero_for_one,
                ),
                &(sqrt_price, liquidity, amount, zero_for_one),
            );
        }
    }

    /// End-to-end step parity over in-range sqrt ratios, full liquidity/amount ranges and the
    /// real fee tiers plus boundary fees (0, 999_999 and the div-by-zero 1_000_000 case).
    #[test]
    fn compute_swap_step_matches_reference() {
        let mut rng = Rng(0xD1FF_0005);
        let fees = [0u32, 1, 100, 500, 3000, 10_000, 100_000, 999_999, 1_000_000];
        for case in 0..CASES {
            let ratio = |rng: &mut Rng, case: usize| match case % 8 {
                0 => MIN_SQRT_RATIO,
                1 => MAX_SQRT_RATIO,
                _ => rng.u256_up_to_bits(160),
            };
            let current = ratio(&mut rng, case);
            let target = ratio(&mut rng, case / 2);
            let liquidity = match case % 16 {
                0 => 0u128,
                1 => u128::MAX,
                _ => rng.u128_any(),
            };
            let magnitude = rng.u256_up_to_bits(255);
            let amount_remaining = if rng.below(2) == 0 {
                I256::from_raw(magnitude)
            } else {
                -I256::from_raw(magnitude)
            };
            let fee_pips = if case % 4 == 0 {
                fees[rng.below(fees.len() as u64) as usize]
            } else {
                rng.below(1_000_000) as u32
            };
            assert_result_eq(
                &compute_swap_step(current, target, liquidity, amount_remaining, fee_pips),
                &compute_swap_step_reference(
                    current,
                    target,
                    liquidity,
                    amount_remaining,
                    fee_pips,
                ),
                &(current, target, liquidity, amount_remaining, fee_pips),
            );
        }
    }
}
