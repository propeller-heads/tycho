use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::safe_div_u256;

pub(crate) const MIN_TICK: i32 = -887272;
pub(crate) const MAX_TICK: i32 = 887272;

// MIN_SQRT_RATIO: 4295128739
pub(crate) const MIN_SQRT_RATIO: U256 = U256::from_limbs([4295128739u64, 0, 0, 0]);

// MAX_SQRT_RATIO: 1461446703485210103287273052203988822378723970342
pub(crate) const MAX_SQRT_RATIO: U256 =
    U256::from_limbs([6743328256752651558u64, 17280870778742802505u64, 4294805859u64, 0]);

/// `(ratio * factor) >> 128` for the tick-ratio ladder.
///
/// `ratio <= 2^128` and every `factor < 2^128`, so the product fits in 256 bits.
fn mul_shift_128(ratio: U256, factor: U256) -> U256 {
    debug_assert!(ratio <= U256::from(1u64) << 128 && factor < U256::from(1u64) << 128);
    ratio.wrapping_mul(factor) >> 128
}

pub(crate) fn get_sqrt_ratio_at_tick(tick: i32) -> Result<U256, SimulationError> {
    if tick.abs() > MAX_TICK {
        return Err(SimulationError::FatalError(format!(
            "Tick {} is outside valid range [{}, {}]",
            tick, -MAX_TICK, MAX_TICK
        )));
    }
    let abs_tick = U256::from(tick.unsigned_abs());
    let mut ratio = if abs_tick.bit(0) {
        U256::from_limbs([12262481743371124737u64, 18445821805675392311u64, 0, 0])
    } else {
        U256::from_limbs([0, 0, 1u64, 0])
    };
    // This section is generated with the code below
    if abs_tick.bit(1) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([6459403834229662010u64, 18444899583751176498u64, 0, 0]),
        )
    }
    if abs_tick.bit(2) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([17226890335427755468u64, 18443055278223354162u64, 0, 0]),
        )
    }
    if abs_tick.bit(3) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([2032852871939366096u64, 18439367220385604838u64, 0, 0]),
        )
    }
    if abs_tick.bit(4) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([14545316742740207172u64, 18431993317065449817u64, 0, 0]),
        )
    }
    if abs_tick.bit(5) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([5129152022828963008u64, 18417254355718160513u64, 0, 0]),
        )
    }
    if abs_tick.bit(6) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([4894419605888772193u64, 18387811781193591352u64, 0, 0]),
        )
    }
    if abs_tick.bit(7) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([1280255884321894483u64, 18329067761203520168u64, 0, 0]),
        )
    }
    if abs_tick.bit(8) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([15924666964335305636u64, 18212142134806087854u64, 0, 0]),
        )
    }
    if abs_tick.bit(9) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([8010504389359918676u64, 17980523815641551639u64, 0, 0]),
        )
    }
    if abs_tick.bit(10) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([10668036004952895731u64, 17526086738831147013u64, 0, 0]),
        )
    }
    if abs_tick.bit(11) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([4878133418470705625u64, 16651378430235024244u64, 0, 0]),
        )
    }
    if abs_tick.bit(12) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([9537173718739605541u64, 15030750278693429944u64, 0, 0]),
        )
    }
    if abs_tick.bit(13) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([9972618978014552549u64, 12247334978882834399u64, 0, 0]),
        )
    }
    if abs_tick.bit(14) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([10428997489610666743u64, 8131365268884726200u64, 0, 0]),
        )
    }
    if abs_tick.bit(15) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([9305304367709015974u64, 3584323654723342297u64, 0, 0]),
        )
    }
    if abs_tick.bit(16) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([14301143598189091785u64, 696457651847595233u64, 0, 0]),
        )
    }
    if abs_tick.bit(17) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([7393154844743099908u64, 26294789957452057u64, 0, 0]),
        )
    }
    if abs_tick.bit(18) {
        ratio = mul_shift_128(
            ratio,
            U256::from_limbs([2209338891292245656u64, 37481735321082u64, 0, 0]),
        )
    }
    if abs_tick.bit(19) {
        ratio = mul_shift_128(ratio, U256::from_limbs([10518117631919034274u64, 76158723u64, 0, 0]))
    }

    if tick > 0 {
        ratio = safe_div_u256(U256::MAX, ratio)?;
    }

    let rest = ratio & U256::from(u32::MAX);
    Ok((ratio >> 32) + if rest.is_zero() { U256::ZERO } else { U256::from(1u64) })
}

/// `x * x` as its high and low 128 bits.
fn square_u128(x: u128) -> (u128, u128) {
    let (high_half, low_half) = (x >> 64, x & u128::from(u64::MAX));
    let cross = high_half * low_half;
    let (low, carry) = (low_half * low_half).overflowing_add(cross << 65);
    (high_half * high_half + (cross >> 63) + u128::from(carry), low)
}

fn most_significant_bit(x: U256) -> Result<usize, SimulationError> {
    if x == U256::ZERO {
        return Err(SimulationError::FatalError(
            "most_significant_bit requires non-zero value".to_string(),
        ));
    }
    Ok(x.bit_len() - 1)
}

/// Fails exactly when `get_tick_at_sqrt_ratio` fails.
pub(crate) fn check_sqrt_price_in_range(sqrt_price: U256) -> Result<(), SimulationError> {
    if sqrt_price < MIN_SQRT_RATIO || sqrt_price >= MAX_SQRT_RATIO {
        return Err(SimulationError::FatalError(format!(
            "sqrt_price {} is outside valid range [{}, {})",
            sqrt_price, MIN_SQRT_RATIO, MAX_SQRT_RATIO
        )));
    }
    Ok(())
}

pub(crate) fn get_tick_at_sqrt_ratio(sqrt_price: U256) -> Result<i32, SimulationError> {
    check_sqrt_price_in_range(sqrt_price)?;
    let ratio_x128 = sqrt_price << 32;
    let msb = most_significant_bit(ratio_x128)?;

    // Normalised to [2^127, 2^128): the mantissa whose repeated squaring yields the fraction bits.
    let mut r: u128 =
        if msb >= 128 { ratio_x128 >> (msb - 127) } else { ratio_x128 << (127 - msb) }.to();
    let mut fraction = 0u64;
    for i in 0..14 {
        let (high, low) = square_u128(r);
        let f = high >> 127;
        fraction |= (f as u64) << (63 - i);
        r = if f == 1 { high } else { (high << 1) | (low >> 127) };
    }
    // The integer part occupies bits 64 and up, the fraction bits 50..=63, so they never overlap.
    let log_2 = ((msb as i128 - 128) << 64) | fraction as i128;

    // |log_2| < 2^70 and the factor is below 2^78, so the product fits the two 128-bit halves and
    // the ticks below are the high halves, as Uniswap's arithmetic shift by 128 takes them.
    let log_sqrt10001 = mul_i128_by_u128(log_2, LOG_SQRT10001_FACTOR);
    let tick_low = high_half_minus(log_sqrt10001, TICK_LOW_OFFSET) as i32;
    let tick_high = high_half_plus(log_sqrt10001, TICK_HIGH_OFFSET) as i32;

    if tick_low == tick_high {
        Ok(tick_low)
    } else if get_sqrt_ratio_at_tick(tick_high)? <= sqrt_price {
        Ok(tick_high)
    } else {
        Ok(tick_low)
    }
}

const LOG_SQRT10001_FACTOR: u128 = (13863u128 << 64) | 11745905768312294533u128;
const TICK_LOW_OFFSET: u128 = (184476617836266586u128 << 64) | 6552757943157144234u128;
const TICK_HIGH_OFFSET: u128 = (15793544031827761793u128 << 64) | 4998474450511881007u128;

/// `x * y` as a 256-bit two's-complement number: its signed high half and its low half.
fn mul_i128_by_u128(x: i128, y: u128) -> (i128, u128) {
    let (high, low) = widening_mul_u128(x.unsigned_abs(), y);
    if x >= 0 {
        return (high as i128, low);
    }
    let (negated_low, borrow) = 0u128.overflowing_sub(low);
    ((!high).wrapping_add(u128::from(!borrow)) as i128, negated_low)
}

/// `a * b` as its high and low 128 bits.
fn widening_mul_u128(a: u128, b: u128) -> (u128, u128) {
    let mask = u128::from(u64::MAX);
    let (a_high, a_low, b_high, b_low) = (a >> 64, a & mask, b >> 64, b & mask);
    let low_low = a_low * b_low;
    let low_high = a_low * b_high;
    let high_low = a_high * b_low;
    let middle = (low_low >> 64) + (low_high & mask) + (high_low & mask);
    let low = (low_low & mask) | (middle << 64);
    let high = a_high * b_high + (low_high >> 64) + (high_low >> 64) + (middle >> 64);
    (high, low)
}

fn high_half_plus((high, low): (i128, u128), addend: u128) -> i128 {
    let (_, carry) = low.overflowing_add(addend);
    high + i128::from(carry)
}

fn high_half_minus((high, low): (i128, u128), subtrahend: u128) -> i128 {
    let (_, borrow) = low.overflowing_sub(subtrahend);
    high - i128::from(borrow)
}

#[cfg(test)]
mod tests {
    use std::{ops::BitOr, str::FromStr};

    use alloy::primitives::{Sign, I256};

    use super::*;

    /// The 256-bit log2 loop the native one must match bit for bit.
    fn tick_at_sqrt_ratio_reference(sqrt_price: U256) -> i32 {
        let ratio_x128 = sqrt_price << 32;
        let msb = most_significant_bit(ratio_x128).unwrap();
        let msb_diff = (msb as i32) - 128;
        let mut log_2: I256 = if msb_diff >= 0 {
            I256::from_raw(U256::from(msb_diff as u64)) << 64
        } else {
            -I256::from_raw(U256::from((-msb_diff) as u64)) << 64
        };
        let mut r = if msb >= 128 { ratio_x128 >> (msb - 127) } else { ratio_x128 << (127 - msb) };
        for i in 0..14 {
            r = r.wrapping_mul(r) >> 127;
            let f = r >> 128;
            log_2 = log_2
                .bitor(I256::checked_from_sign_and_abs(Sign::Positive, f << (63 - i)).unwrap());
            r >>= f;
        }
        let log_sqrt10001 =
            log_2 * I256::from_raw(U256::from_limbs([11745905768312294533u64, 13863u64, 0, 0]));
        let tick_low = (log_sqrt10001 -
            I256::from_raw(U256::from_limbs([
                6552757943157144234u64,
                184476617836266586u64,
                0,
                0,
            ])))
        .asr(128);
        let tick_high = (log_sqrt10001 +
            I256::from_raw(U256::from_limbs([
                4998474450511881007u64,
                15793544031827761793u64,
                0,
                0,
            ])))
        .asr(128);
        if tick_low == tick_high {
            tick_low.as_i32()
        } else if get_sqrt_ratio_at_tick(tick_high.as_i32()).unwrap() <= sqrt_price {
            tick_high.as_i32()
        } else {
            tick_low.as_i32()
        }
    }

    #[test]
    fn test_get_tick_at_sqrt_ratio_matches_reference() {
        let mut prices = Vec::new();
        let dense = (MIN_TICK..MIN_TICK + 2048)
            .chain(-2048..2048)
            .chain(MAX_TICK - 2048..=MAX_TICK);
        for tick in dense.chain((MIN_TICK..=MAX_TICK).step_by(101)) {
            let boundary = get_sqrt_ratio_at_tick(tick).unwrap();
            prices.extend([boundary - U256::from(1u64), boundary, boundary + U256::from(1u64)]);
        }
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let span = MAX_SQRT_RATIO - MIN_SQRT_RATIO;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bits = 32 + (state % 128) as usize;
            let raw = U256::from_limbs([state, state.rotate_left(17), state.rotate_left(41), 0]);
            prices.push(MIN_SQRT_RATIO + (raw >> (192 - bits)) % span);
        }
        for price in prices {
            if price < MIN_SQRT_RATIO || price >= MAX_SQRT_RATIO {
                continue;
            }
            assert_eq!(
                get_tick_at_sqrt_ratio(price).unwrap(),
                tick_at_sqrt_ratio_reference(price),
                "{price}"
            );
        }
    }

    struct TestCase {
        tick: i32,
        ratio: U256,
    }

    #[test]
    fn test_most_significant_bit() {
        assert_eq!(most_significant_bit(U256::from(1)).unwrap(), 0);
        assert_eq!(most_significant_bit(U256::from(3)).unwrap(), 1);
        assert_eq!(most_significant_bit(U256::from(8)).unwrap(), 3);
        assert_eq!(most_significant_bit(U256::from(256)).unwrap(), 8);
        assert_eq!(most_significant_bit(U256::from(511)).unwrap(), 8);
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick() {
        let cases = vec![
            TestCase { tick: 0, ratio: U256::from_str("79228162514264337593543950336").unwrap() },
            TestCase { tick: 1, ratio: U256::from_str("79232123823359799118286999568").unwrap() },
            TestCase { tick: -1, ratio: U256::from_str("79224201403219477170569942574").unwrap() },
            TestCase { tick: 42, ratio: U256::from_str("79394708140106462983274643745").unwrap() },
            TestCase { tick: -42, ratio: U256::from_str("79061966249810860392253787324").unwrap() },
            TestCase { tick: MIN_TICK, ratio: U256::from_str("4295128739").unwrap() },
            TestCase {
                tick: MAX_TICK,
                ratio: U256::from_str("1461446703485210103287273052203988822378723970342").unwrap(),
            },
        ];
        for case in cases {
            assert_eq!(get_sqrt_ratio_at_tick(case.tick).unwrap(), case.ratio);
        }
    }

    #[test]
    fn test_get_tick_at_sqrt_ratio() {
        let cases = vec![
            TestCase { tick: 0, ratio: U256::from_str("79228162514264337593543950336").unwrap() },
            TestCase { tick: 1, ratio: U256::from_str("79232123823359799118286999568").unwrap() },
            TestCase { tick: -1, ratio: U256::from_str("79224201403219477170569942574").unwrap() },
            TestCase { tick: 42, ratio: U256::from_str("79394708140106462983274643745").unwrap() },
            TestCase { tick: -42, ratio: U256::from_str("79061966249810860392253787324").unwrap() },
            TestCase { tick: MIN_TICK, ratio: U256::from_str("4295128739").unwrap() },
            TestCase {
                tick: MAX_TICK - 1,
                ratio: U256::from_str("1461446703485210103287273052203988822378723970341").unwrap(),
            },
        ];
        for case in cases {
            assert_eq!(get_tick_at_sqrt_ratio(case.ratio).unwrap(), case.tick);
        }
    }
}
