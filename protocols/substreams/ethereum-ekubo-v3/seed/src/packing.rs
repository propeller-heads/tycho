//! Bit layouts of the packed storage words in `evm-contracts/src/types/*.sol`.

use alloy::primitives::{B256, U256};

const MASK_112: u128 = (1 << 112) - 1;

/// `poolState.sol`: sqrt ratio (96-bit float) | tick (int32) | liquidity (uint128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub sqrt_ratio_float: [u8; 12],
    pub tick: i32,
    pub liquidity: u128,
}

pub fn parse_pool_state(word: B256) -> PoolState {
    PoolState {
        sqrt_ratio_float: word[0..12].try_into().unwrap(),
        tick: i32::from_be_bytes(word[12..16].try_into().unwrap()),
        liquidity: u128::from_be_bytes(word[16..32].try_into().unwrap()),
    }
}

/// `tickInfo.sol`: liquidity referencing the tick (uint128) | net liquidity change when crossing
/// it upwards (int128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInfo {
    pub liquidity_delta: i128,
    pub liquidity_net: u128,
}

pub fn parse_tick_info(word: B256) -> TickInfo {
    TickInfo {
        liquidity_net: u128::from_be_bytes(word[0..16].try_into().unwrap()),
        liquidity_delta: i128::from_be_bytes(word[16..32].try_into().unwrap()),
    }
}

/// `twammPoolState.sol`: sale rate of token1 (uint112) | sale rate of token0 (uint112) | last
/// virtual order execution time (uint32). BoostedFees stores its donate rates and last donate time
/// in the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedPoolState {
    pub last_time_u32: u32,
    pub rate0: u128,
    pub rate1: u128,
}

pub fn parse_timed_pool_state(word: B256) -> TimedPoolState {
    let word = U256::from_be_bytes(word.0);

    TimedPoolState {
        last_time_u32: (word & U256::from(u32::MAX)).to::<u32>(),
        rate0: ((word >> 32usize) & U256::from(MASK_112)).to::<u128>(),
        rate1: (word >> 144usize).to::<u128>(),
    }
}

/// `timeInfo.sol` (TWAMM): number of orders (uint32) | sale rate delta of token0 (int112) | sale
/// rate delta of token1 (int112).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeInfo {
    pub num_orders: u32,
    pub delta0: i128,
    pub delta1: i128,
}

pub fn parse_time_info(word: B256) -> TimeInfo {
    let word = U256::from_be_bytes(word.0);

    TimeInfo {
        num_orders: (word >> 224usize).to::<u32>(),
        delta0: sign_extend_112(((word >> 112usize) & U256::from(MASK_112)).to::<u128>()),
        delta1: sign_extend_112((word & U256::from(MASK_112)).to::<u128>()),
    }
}

/// `poolBalanceUpdate.sol` (BoostedFees time infos): delta0 (int128) | delta1 (int128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolBalanceUpdate {
    pub delta0: i128,
    pub delta1: i128,
}

pub fn parse_pool_balance_update(word: B256) -> PoolBalanceUpdate {
    PoolBalanceUpdate {
        delta0: i128::from_be_bytes(word[0..16].try_into().unwrap()),
        delta1: i128::from_be_bytes(word[16..32].try_into().unwrap()),
    }
}

/// Recovers the full timestamp from the truncated `uint32` the extensions store, the way
/// `twammPoolState.sol::realLastVirtualOrderExecutionTime` does against `block.timestamp`.
pub fn real_last_time(block_timestamp: u64, stored: u32) -> u64 {
    block_timestamp - (block_timestamp as u32).wrapping_sub(stored) as u64
}

fn sign_extend_112(value: u128) -> i128 {
    if value & (1 << 111) != 0 {
        (value | !MASK_112) as i128
    } else {
        value as i128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(parts: &[(u32, u128)]) -> B256 {
        // Assemble `value << shift` terms into one 256-bit word.
        let mut acc = U256::ZERO;
        for (shift, value) in parts {
            acc |= U256::from(*value) << *shift;
        }

        acc.into()
    }

    #[test]
    fn pool_state_fields() {
        let sqrt = 0x3ffffffffffffffff9ba1f6d_u128;
        let tick = -12345_i32;
        let liquidity = 0xdead_beef_u128 << 64;
        let word = word(&[(160, sqrt), (128, tick as u32 as u128), (0, liquidity)]);

        let state = parse_pool_state(word);

        assert_eq!(state.sqrt_ratio_float, sqrt.to_be_bytes()[4..]);
        assert_eq!(state.tick, tick);
        assert_eq!(state.liquidity, liquidity);
    }

    #[test]
    fn tick_info_fields() {
        let delta = -7_i128;
        let net = 7_u128;
        let word = word(&[(128, net), (0, delta as u128)]);

        assert_eq!(parse_tick_info(word), TickInfo { liquidity_delta: delta, liquidity_net: net });
    }

    #[test]
    fn timed_pool_state_fields() {
        let word = word(&[(144, 5), (32, 3), (0, 0xffff_fff0)]);

        assert_eq!(
            parse_timed_pool_state(word),
            TimedPoolState { last_time_u32: 0xffff_fff0, rate0: 3, rate1: 5 }
        );
    }

    #[test]
    fn time_info_fields_sign_extend() {
        let delta0 = -3_i128;
        let delta1 = 9_i128;
        let word = word(&[(224, 2), (112, delta0 as u128 & MASK_112), (0, delta1 as u128)]);

        assert_eq!(parse_time_info(word), TimeInfo { num_orders: 2, delta0, delta1 });
    }

    #[test]
    fn pool_balance_update_fields() {
        let word = word(&[(128, (-1_i128) as u128), (0, 4)]);

        assert_eq!(parse_pool_balance_update(word), PoolBalanceUpdate { delta0: -1, delta1: 4 });
    }

    #[test]
    fn real_last_time_survives_a_u32_wrap() {
        assert_eq!(real_last_time(1_800_000_000, 1_799_999_000), 1_799_999_000);

        let block_ts = (1_u64 << 32) + 100;
        let stored = u32::MAX - 50;
        assert_eq!(real_last_time(block_ts, stored), (1_u64 << 32) - 51);
    }
}
