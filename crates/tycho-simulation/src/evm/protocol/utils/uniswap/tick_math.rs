use std::{cell::RefCell, ops::BitOr};

use alloy::primitives::{Sign, I256, U256};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::safe_div_u256;

pub(crate) const MIN_TICK: i32 = -887272;
pub(crate) const MAX_TICK: i32 = 887272;

// MIN_SQRT_RATIO: 4295128739
pub(crate) const MIN_SQRT_RATIO: U256 = U256::from_limbs([4295128739u64, 0, 0, 0]);

// MAX_SQRT_RATIO: 1461446703485210103287273052203988822378723970342
pub(crate) const MAX_SQRT_RATIO: U256 =
    U256::from_limbs([6743328256752651558u64, 17280870778742802505u64, 4294805859u64, 0]);

/// Slots in each thread's cache of [`get_sqrt_ratio_at_tick_cached`]. Must be a power of two.
const SQRT_RATIO_CACHE_SLOTS: usize = 1024;

/// 2^32 divided by the golden ratio. Multiplying by it spreads nearby ticks over the slots
/// (Fibonacci hashing).
const FIBONACCI_HASH_MULTIPLIER: u32 = 0x9E37_79B1;

thread_local! {
    /// Tick and sqrt ratio, in a slot picked by the tick.
    static SQRT_RATIO_CACHE: RefCell<Vec<Option<(i32, U256)>>> =
        RefCell::new(vec![None; SQRT_RATIO_CACHE_SLOTS]);
}

/// [`get_sqrt_ratio_at_tick`], with the results kept in a cache for each thread.
///
/// A slot holds one tick, and a new tick replaces the old one.
pub(crate) fn get_sqrt_ratio_at_tick_cached(tick: i32) -> Result<U256, SimulationError> {
    // The top bits of the product, as many as the slot count needs.
    let shift = u32::BITS - SQRT_RATIO_CACHE_SLOTS.trailing_zeros();
    let slot = ((tick as u32).wrapping_mul(FIBONACCI_HASH_MULTIPLIER) >> shift) as usize;
    let cached = SQRT_RATIO_CACHE.with_borrow(|cache| match cache[slot] {
        Some((cached_tick, ratio)) if cached_tick == tick => Some(ratio),
        _ => None,
    });
    if let Some(ratio) = cached {
        return Ok(ratio);
    }
    let ratio = get_sqrt_ratio_at_tick(tick)?;
    SQRT_RATIO_CACHE.with_borrow_mut(|cache| cache[slot] = Some((tick, ratio)));
    Ok(ratio)
}

/// `(ratio * factor) >> 128` for the tick-ratio ladder.
///
/// `ratio <= 2^128` and every `factor < 2^128`, so the product fits in 256 bits.
fn mul_shift_128(ratio: U256, factor: U256) -> U256 {
    debug_assert!(ratio <= U256::from(1u64) << 128 && factor < U256::from(1u64) << 128);
    ratio.wrapping_mul(factor) >> 128
}

pub(crate) fn get_sqrt_ratio_at_tick(tick: i32) -> Result<U256, SimulationError> {
    if tick.unsigned_abs() > MAX_TICK.unsigned_abs() {
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

    let has_remainder = ratio.as_limbs()[0] & u64::from(u32::MAX) != 0;
    Ok((ratio >> 32) + if has_remainder { U256::from(1u64) } else { U256::ZERO })
}

fn most_significant_bit(x: U256) -> Result<usize, SimulationError> {
    if x == U256::ZERO {
        return Err(SimulationError::FatalError(
            "most_significant_bit requires non-zero value".to_string(),
        ));
    }
    Ok(x.bit_len() - 1)
}

pub(crate) fn get_tick_at_sqrt_ratio(sqrt_price: U256) -> Result<i32, SimulationError> {
    if sqrt_price < MIN_SQRT_RATIO || sqrt_price >= MAX_SQRT_RATIO {
        return Err(SimulationError::FatalError(format!(
            "sqrt_price {} is outside valid range [{}, {})",
            sqrt_price, MIN_SQRT_RATIO, MAX_SQRT_RATIO
        )));
    }
    let ratio_x128 = sqrt_price << 32;
    let msb = most_significant_bit(ratio_x128)?;
    let msb_diff = (msb as i32) - 128;
    // Convert msb_diff to I256
    let mut log_2: I256 = if msb_diff >= 0 {
        I256::from_raw(U256::from(msb_diff as u64)) << 64
    } else {
        -I256::from_raw(U256::from((-msb_diff) as u64)) << 64
    };

    let mut r = if msb >= 128 { ratio_x128 >> (msb - 127) } else { ratio_x128 << (127 - msb) };

    for i in 0..14 {
        r = (r * r) >> 127;
        let f = r >> 128;
        let shift_value = I256::checked_from_sign_and_abs(Sign::Positive, f << (63 - i))
            .ok_or_else(|| {
                SimulationError::FatalError("Failed to convert shifted value to I256".to_string())
            })?;
        log_2 = log_2.bitor(shift_value);
        r >>= f;
    }

    let log_sqrt10001 =
        log_2 * I256::from_raw(U256::from_limbs([11745905768312294533u64, 13863u64, 0, 0]));

    let tmp1 =
        I256::from_raw(U256::from_limbs([6552757943157144234u64, 184476617836266586u64, 0, 0]));

    let tick_low: I256 = (log_sqrt10001 - tmp1).asr(128);
    let tick_high: I256 = (log_sqrt10001 +
        I256::from_raw(U256::from_limbs([
            4998474450511881007u64,
            15793544031827761793u64,
            0,
            0,
        ])))
    .asr(128);

    if tick_low == tick_high {
        Ok(tick_low.as_i32())
    } else if get_sqrt_ratio_at_tick(tick_high.as_i32())? <= sqrt_price {
        Ok(tick_high.as_i32())
    } else {
        Ok(tick_low.as_i32())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_get_sqrt_ratio_at_tick_i32_min() {
        assert!(get_sqrt_ratio_at_tick(i32::MIN).is_err());
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_cached_matches_uncached() {
        // Every tick a few times over, so slots fill, collide and get replaced.
        let ticks: Vec<i32> = (-3000..3000)
            .step_by(7)
            .chain([MIN_TICK, MAX_TICK, 0])
            .collect();
        for _ in 0..3 {
            for &tick in &ticks {
                assert_eq!(
                    get_sqrt_ratio_at_tick_cached(tick).unwrap(),
                    get_sqrt_ratio_at_tick(tick).unwrap(),
                    "tick {tick}"
                );
            }
        }
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_cached_out_of_range() {
        for tick in [MIN_TICK - 1, MAX_TICK + 1] {
            assert!(get_sqrt_ratio_at_tick_cached(tick).is_err(), "tick {tick}");
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
