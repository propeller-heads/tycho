use alloy::primitives::{I256, U256};
use pool_tick::PoolTick;
use swap_step_cache::CachedSwapProgress;
use tycho_common::Bytes;

pub(crate) mod liquidity_math;
pub(crate) mod lp_fee;
pub(crate) mod pool_tick;
pub(crate) mod sqrt_price_math;
pub(crate) mod swap_math;
pub(crate) mod swap_step_cache;
pub mod tick_list;
pub(crate) mod tick_math;

/// Uniswap fees are in pips. One pip is a hundredth of a basis point, so 1,000,000 pips is 100%.
pub(crate) const FEE_PIPS_DENOMINATOR: u32 = 1_000_000;

#[derive(Debug)]
pub(crate) struct SwapState {
    pub(crate) amount_remaining: I256,
    pub(crate) amount_calculated: I256,
    pub(crate) sqrt_price: U256,
    pub(crate) tick: PoolTick,
    pub(crate) liquidity: u128,
}

impl SwapState {
    /// Moves a swap that has not taken a step yet to the end of `progress`, as if the swap loop had
    /// taken its steps.
    ///
    /// `amount_remaining` holds the swap's exact input, positive or negative, and moves towards
    /// zero either way.
    pub(crate) fn apply_cached_progress(&mut self, progress: &CachedSwapProgress) {
        let spent = I256::from_raw(progress.amount_in);
        if self.amount_remaining.is_negative() {
            self.amount_remaining += spent;
        } else {
            self.amount_remaining -= spent;
        }
        self.amount_calculated = -I256::from_raw(progress.amount_out);
        self.sqrt_price = progress.sqrt_price;
        self.tick = PoolTick::from(progress.tick);
        self.liquidity = progress.liquidity;
    }
}

#[derive(Debug)]
pub(crate) struct StepComputation {
    pub(crate) sqrt_price_start: U256,
    pub(crate) tick_next: i32,
    pub(crate) sqrt_price_next: U256,
    pub(crate) amount_in_with_fee: U256,
    pub(crate) amount_out: U256,
}

#[derive(Debug, Default)]
pub(crate) struct SwapResults {
    pub(crate) amount_calculated: I256,
    pub(crate) amount_specified: I256,
    pub(crate) amount_remaining: I256,
    pub(crate) sqrt_price: U256,
    pub(crate) liquidity: u128,
    pub(crate) tick: PoolTick,
    pub(crate) gas_used: U256,
}

/// Converts a slice of bytes representing a big-endian 24-bit signed integer
/// to a 32-bit signed integer.
///
/// # Arguments
/// * `val` - A reference to a `Bytes` type, which should contain at most three bytes.
///
/// # Returns
/// * The 32-bit signed integer representation of the input bytes.
pub(crate) fn i24_be_bytes_to_i32(val: &Bytes) -> i32 {
    let bytes_slice = val.as_ref();
    let bytes_len = bytes_slice.len();
    let mut result = 0i32;

    for (i, &byte) in bytes_slice.iter().enumerate() {
        result |= (byte as i32) << (8 * (bytes_len - 1 - i));
    }

    // If the first byte (most significant byte) has its most significant bit set (0x80),
    // perform sign extension for negative numbers.
    if bytes_len > 0 && bytes_slice[0] & 0x80 != 0 {
        result |= -1i32 << (8 * bytes_len);
    }
    result
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use alloy::primitives::{I256, U256};
    use tycho_common::Bytes;

    use crate::evm::protocol::utils::uniswap::{
        i24_be_bytes_to_i32, pool_tick::PoolTick, swap_step_cache::CachedSwapProgress, SwapState,
    };

    #[test]
    fn test_i24_be_bytes_to_i32() {
        let val = Bytes::from_str("0xfeafc6").unwrap();
        let converted = i24_be_bytes_to_i32(&val);
        assert_eq!(converted, -86074);
        let val = Bytes::from_str("0x02dd").unwrap();
        let converted = i24_be_bytes_to_i32(&val);
        assert_eq!(converted, 733);
        let val = Bytes::from_str("0xe2bb").unwrap();
        let converted = i24_be_bytes_to_i32(&val);
        assert_eq!(converted, -7493);
    }

    #[rstest::rstest]
    #[case::v3_positive_input(5_000, 4_000)]
    #[case::v4_negative_input(-5_000, -4_000)]
    fn test_apply_cached_progress(#[case] amount_specified: i64, #[case] amount_remaining: i64) {
        let progress = CachedSwapProgress {
            steps_taken: 1,
            min_input: U256::from(1_000u64),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_994u64),
            sqrt_price: U256::from(900u64),
            tick: 9,
            liquidity: 4_000,
            gas: U256::from(150u64),
        };
        let mut state = SwapState {
            amount_remaining: I256::try_from(amount_specified).unwrap(),
            amount_calculated: I256::ZERO,
            sqrt_price: U256::from(1_000u64),
            tick: PoolTick::from(10),
            liquidity: 5_000,
        };

        state.apply_cached_progress(&progress);

        assert_eq!(state.amount_remaining, I256::try_from(amount_remaining).unwrap());
        assert_eq!(state.amount_calculated, I256::try_from(-1_994).unwrap());
        assert_eq!(state.sqrt_price, U256::from(900u64));
        assert_eq!(state.tick.value(), 9);
        assert_eq!(state.liquidity, 4_000);
    }
}
