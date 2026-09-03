//! Storage layout of the Ekubo v3 core and timed extension contracts.
//!
//! Both contracts address most state arithmetically from the pool id instead of hashing
//! (`CoreStorageLayout.sol`, `TWAMMStorageLayout.sol`): a slot is `poolId + OFFSET + key`, where
//! the key is a tick, a bitmap word, a timestamp or a small index. Given the set of pool ids, a
//! dumped slot can therefore be attributed to a pool and a key by subtracting the offset and
//! finding the pool id closest below. Pool ids are 256-bit hashes, so two pools whose ranges
//! overlap would need ids within 2^66 of each other, which does not happen in practice;
//! `classify_*` still reports it instead of guessing.

use alloy::primitives::{uint, B256, U256};
use anyhow::{bail, Result};

pub const MIN_TICK: i32 = -88722835;
pub const MAX_TICK: i32 = 88722835;

/// Added to `tick / tickSpacing` so all ticks map to a contiguous range of unsigned bitmap
/// positions with tick 0 centered in one word (`tickBitmap.sol`).
pub const TICK_BITMAP_STORAGE_OFFSET: u64 = 89421695;

/// Ticks span `2 * MAX_TICK + 1` positions per pool, and the bitmap of a tick-spacing-1 pool spans
/// `(2 * MAX_TICK + TICK_BITMAP_STORAGE_OFFSET) >> 8` words, both far below 2^32.
const TICK_RANGE: u64 = 2 * MAX_TICK as u64 + 1;
const BITMAP_WORDS: u64 = 1 << 32;

// `cast keccak "CoreStorageLayout#..."`
const FPL_OFFSET: U256 =
    uint!(0xb09b03866d96933565a9435bfb511c8ac5b2be454285ca331201452704799f72_U256);
const TICKS_OFFSET: U256 =
    uint!(0x435a5eb89a296820174331cf5a3902d9fca683928d56726d8e7acd6efb28c568_U256);
const FPL_OUTSIDE_OFFSET_VALUE0: U256 =
    uint!(0x5695060fdb9cfea656f872ae4887221aff7dbfefc45eaf753e4e70cdfb5cd19c_U256);
const FPL_OUTSIDE_OFFSET_VALUE1: U256 =
    uint!(0x7a2a03fc08af3dae7869678617dc8abe8f15a3b719b37ba108dba879571f8b02_U256);
const BITMAPS_OFFSET: U256 =
    uint!(0x3def450d0010a2fef515ce5eba4b363b5a0f42fdd4c53e1c737975db05a2e3a5_U256);

// `cast keccak "TWAMMStorageLayout#..."`
const REWARD_RATES_OFFSET: U256 =
    uint!(0x6536a49ed1752ddb42ba94b6b00660382279a8d99d650d701d5d127e7a3bbd95_U256);
const TIME_BITMAPS_OFFSET: U256 =
    uint!(0x07f3f693b68a1a1b1b3315d4b74217931d60e9dc7f1af4989f50e7ab31c8820e_U256);
const TIME_INFOS_OFFSET: U256 =
    uint!(0x70db18ef1c685b7aa06d1ac5ea2d101c7261974df22a15951f768f92187043fb_U256);
const REWARD_RATES_BEFORE_OFFSET: U256 =
    uint!(0x6a7cb7181a18ced052a38531ee9ccb088f76cd0fb0c4475d55c480aebfae7b2b_U256);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreSlot {
    PoolState {
        pool: B256,
    },
    Tick {
        pool: B256,
        tick: i32,
    },
    Bitmap {
        pool: B256,
        word: u64,
    },
    FeesPerLiquidity {
        pool: B256,
        index: u8,
    },
    FeesPerLiquidityOutside0 {
        pool: B256,
        tick: i32,
    },
    FeesPerLiquidityOutside1 {
        pool: B256,
        tick: i32,
    },
    /// Keccak-addressed state: positions, saved balances, extension registrations.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimedSlot {
    PoolState {
        pool: B256,
    },
    RewardRates {
        pool: B256,
        index: u8,
    },
    TimeBitmap {
        pool: B256,
        word: u64,
    },
    TimeInfo {
        pool: B256,
        time: u64,
    },
    RewardRatesBefore {
        pool: B256,
        time: u64,
        index: u8,
    },
    /// Keccak-addressed state: orders.
    Other,
}

/// The pool ids a dump is attributed against, sorted for binary search.
pub struct PoolIds(Vec<U256>);

impl PoolIds {
    pub fn new(ids: impl IntoIterator<Item = B256>) -> Self {
        let mut ids: Vec<U256> = ids
            .into_iter()
            .map(|id| id.into())
            .collect();
        ids.sort_unstable();
        ids.dedup();

        Self(ids)
    }

    pub fn contains(&self, pool: B256) -> bool {
        self.0
            .binary_search(&pool.into())
            .is_ok()
    }

    /// The pool whose region contains `slot`, given the region's offset and size in keys, and the
    /// key within the region.
    fn locate(&self, slot: U256, offset: U256, size: u64) -> Option<(B256, u64)> {
        let base = slot.wrapping_sub(offset);
        let candidate = self
            .0
            .partition_point(|id| *id <= base)
            .checked_sub(1)?;
        let pool = self.0[candidate];
        let key = base - pool;

        (key < U256::from(size)).then(|| (pool.into(), key.to::<u64>()))
    }

    /// Like [`Self::locate`] for regions keyed by a tick, where the key runs from `MIN_TICK` to
    /// `MAX_TICK` around the pool id.
    fn locate_tick(&self, slot: U256, offset: U256) -> Option<(B256, i32)> {
        let (pool, key) =
            self.locate(slot.wrapping_add(U256::from(MAX_TICK as u64)), offset, TICK_RANGE)?;

        Some((pool, (key as i64 + MIN_TICK as i64) as i32))
    }
}

/// Attributes a core storage slot to a pool and a key.
pub fn classify_core_slot(pool_ids: &PoolIds, slot: B256) -> Result<CoreSlot> {
    let slot_u256: U256 = slot.into();
    let mut matches = Vec::with_capacity(1);

    if pool_ids.contains(slot) {
        matches.push(CoreSlot::PoolState { pool: slot });
    }
    if let Some((pool, tick)) = pool_ids.locate_tick(slot_u256, TICKS_OFFSET) {
        matches.push(CoreSlot::Tick { pool, tick });
    }
    if let Some((pool, word)) = pool_ids.locate(slot_u256, BITMAPS_OFFSET, BITMAP_WORDS) {
        matches.push(CoreSlot::Bitmap { pool, word });
    }
    if let Some((pool, index)) = pool_ids.locate(slot_u256, FPL_OFFSET, 2) {
        matches.push(CoreSlot::FeesPerLiquidity { pool, index: index as u8 });
    }
    if let Some((pool, tick)) = pool_ids.locate_tick(slot_u256, FPL_OUTSIDE_OFFSET_VALUE0) {
        matches.push(CoreSlot::FeesPerLiquidityOutside0 { pool, tick });
    }
    if let Some((pool, tick)) = pool_ids
        .locate_tick(slot_u256, FPL_OUTSIDE_OFFSET_VALUE0.wrapping_add(FPL_OUTSIDE_OFFSET_VALUE1))
    {
        matches.push(CoreSlot::FeesPerLiquidityOutside1 { pool, tick });
    }

    single_match(matches, slot, CoreSlot::Other)
}

/// Attributes a TWAMM or BoostedFees storage slot to a pool and a key.
pub fn classify_timed_slot(pool_ids: &PoolIds, slot: B256) -> Result<TimedSlot> {
    let slot_u256: U256 = slot.into();
    let mut matches = Vec::with_capacity(1);

    if pool_ids.contains(slot) {
        matches.push(TimedSlot::PoolState { pool: slot });
    }
    if let Some((pool, index)) = pool_ids.locate(slot_u256, REWARD_RATES_OFFSET, 2) {
        matches.push(TimedSlot::RewardRates { pool, index: index as u8 });
    }
    if let Some((pool, word)) = pool_ids.locate(slot_u256, TIME_BITMAPS_OFFSET, 1 << 48) {
        matches.push(TimedSlot::TimeBitmap { pool, word });
    }
    if let Some((pool, time)) = pool_ids.locate(slot_u256, TIME_INFOS_OFFSET, u64::MAX) {
        matches.push(TimedSlot::TimeInfo { pool, time });
    }
    if let Some((pool, key)) = pool_ids.locate(slot_u256, REWARD_RATES_BEFORE_OFFSET, u64::MAX) {
        matches.push(TimedSlot::RewardRatesBefore { pool, time: key / 2, index: (key % 2) as u8 });
    }

    single_match(matches, slot, TimedSlot::Other)
}

fn single_match<T: std::fmt::Debug>(matches: Vec<T>, slot: B256, other: T) -> Result<T> {
    let mut matches = matches.into_iter();
    let Some(first) = matches.next() else {
        return Ok(other);
    };
    let rest: Vec<_> = matches.collect();
    if !rest.is_empty() {
        bail!("slot {slot} matches more than one storage region: {first:?} and {rest:?}");
    }

    Ok(first)
}

/// Bitmap word and bit holding the initialized flag of `tick`, rounding the tick down to a multiple
/// of `tick_spacing` (`tickBitmap.sol::tickToBitmapWordAndIndex`).
pub fn tick_to_bitmap_word_and_index(tick: i32, tick_spacing: u32) -> (u64, u8) {
    let raw_index = tick.div_euclid(tick_spacing as i32) as i64 + TICK_BITMAP_STORAGE_OFFSET as i64;

    ((raw_index >> 8) as u64, (raw_index & 0xff) as u8)
}

/// The tick whose initialized flag lives at `word` / `index` for `tick_spacing`
/// (`tickBitmap.sol::bitmapWordAndIndexToTick`).
pub fn bitmap_word_and_index_to_tick(word: u64, index: u8, tick_spacing: u32) -> i32 {
    let raw_index = ((word << 8) | index as u64) as i64 - TICK_BITMAP_STORAGE_OFFSET as i64;

    (raw_index * tick_spacing as i64) as i32
}

/// Bitmap word and bit holding the initialized flag of `time`
/// (`timeBitmap.sol::timeToBitmapWordAndIndex`).
pub fn time_to_bitmap_word_and_index(time: u64) -> (u64, u8) {
    (time >> 16, ((time >> 8) & 0xff) as u8)
}

pub fn bit_is_set(word: U256, index: u8) -> bool {
    word.bit(index as usize)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::keccak256;

    use super::*;

    fn pool(seed: u8) -> B256 {
        keccak256([seed])
    }

    /// `poolId + offset + key` for a signed key.
    fn slot(pool: B256, offset: U256, key: i64) -> B256 {
        let base = U256::from_be_bytes(pool.0).wrapping_add(offset);
        if key >= 0 {
            base.wrapping_add(U256::from(key as u64))
                .into()
        } else {
            base.wrapping_sub(U256::from(key.unsigned_abs()))
                .into()
        }
    }

    fn tick_slot(pool: B256, tick: i32) -> B256 {
        slot(pool, TICKS_OFFSET, tick as i64)
    }

    fn bitmap_slot(pool: B256, word: u64) -> B256 {
        slot(pool, BITMAPS_OFFSET, word as i64)
    }

    fn time_info_slot(pool: B256, time: u64) -> B256 {
        slot(pool, TIME_INFOS_OFFSET, time as i64)
    }

    fn time_bitmap_slot(pool: B256, word: u64) -> B256 {
        slot(pool, TIME_BITMAPS_OFFSET, word as i64)
    }

    #[test]
    fn classifies_arithmetic_regions_and_leaves_hashes_alone() {
        let ids = PoolIds::new([pool(1), pool(2), pool(3)]);

        assert_eq!(
            classify_core_slot(&ids, pool(2)).unwrap(),
            CoreSlot::PoolState { pool: pool(2) }
        );
        for tick in [MIN_TICK, -1, 0, 1, 1000, MAX_TICK] {
            assert_eq!(
                classify_core_slot(&ids, tick_slot(pool(3), tick)).unwrap(),
                CoreSlot::Tick { pool: pool(3), tick },
                "tick {tick}"
            );
        }
        assert_eq!(
            classify_core_slot(&ids, bitmap_slot(pool(1), 695_877)).unwrap(),
            CoreSlot::Bitmap { pool: pool(1), word: 695_877 }
        );
        assert_eq!(classify_core_slot(&ids, keccak256(b"position")).unwrap(), CoreSlot::Other);
        assert_eq!(
            classify_timed_slot(&ids, time_info_slot(pool(2), 1_800_000_000)).unwrap(),
            TimedSlot::TimeInfo { pool: pool(2), time: 1_800_000_000 }
        );
        assert_eq!(
            classify_timed_slot(&ids, time_bitmap_slot(pool(2), 1_800_000_000 >> 16)).unwrap(),
            TimedSlot::TimeBitmap { pool: pool(2), word: 1_800_000_000 >> 16 }
        );
    }

    #[test]
    fn ticks_of_unknown_pools_are_other() {
        let ids = PoolIds::new([pool(1)]);

        assert_eq!(classify_core_slot(&ids, tick_slot(pool(9), 5)).unwrap(), CoreSlot::Other);
    }

    #[test]
    fn tick_bitmap_positions_round_toward_negative_infinity() {
        assert_eq!(
            tick_to_bitmap_word_and_index(0, 10),
            (TICK_BITMAP_STORAGE_OFFSET >> 8, (TICK_BITMAP_STORAGE_OFFSET & 0xff) as u8)
        );
        assert_eq!(tick_to_bitmap_word_and_index(-1, 10), tick_to_bitmap_word_and_index(-10, 10));
        assert_ne!(tick_to_bitmap_word_and_index(-1, 10), tick_to_bitmap_word_and_index(0, 10));
        for tick in
            [MIN_TICK + MIN_TICK.rem_euclid(10), -10, 0, 10, 12340, MAX_TICK - MAX_TICK % 10]
        {
            let (word, index) = tick_to_bitmap_word_and_index(tick, 10);
            assert_eq!(bitmap_word_and_index_to_tick(word, index, 10), tick);
        }
        assert_eq!(time_to_bitmap_word_and_index(0x1_2345_67ff), (0x1_2345, 0x67));
    }
}
