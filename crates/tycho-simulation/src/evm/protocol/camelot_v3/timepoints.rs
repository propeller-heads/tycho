//! Port of Algebra V1.9's `DataStorage` library: the oracle ring of a pool's
//! `DataStorageOperator`, from which the adaptive fee reads its 1-day averages.
//!
//! The on-chain ring has 65,536 fixed slots. The indexer only keeps the slots that
//! `getAverages` can still reach — the newest timepoint at or before `now - WINDOW` and
//! everything after it, plus the one before the last — so [`Timepoints`] holds a contiguous
//! stretch of the ring ordered by time, and every lookup that Solidity does by ring index is
//! done here by position in that stretch.
//!
//! Solidity 0.7.6 arithmetic is unchecked, and the cumulative fields are *meant* to wrap
//! (`volatilityCumulative` "overflow after ~34800 years is desired"). Every operation below
//! therefore wraps at the field's own width: int56, uint88, uint144, uint160, int24, uint32.

use std::cmp::Reverse;

use alloy::primitives::{I256, U256};
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use super::attributes::sign_extend;

/// `DataStorage.WINDOW`: the averaging window of the adaptive fee, one day.
pub const WINDOW: u32 = 86_400;

const INT56_MASK: i128 = (1 << 56) - 1;
const UINT88_MASK: u128 = (1 << 88) - 1;

/// `DataStorage.Timepoint`, with its ring index.
///
/// Field widths follow the contract: `tickCumulative` is an int56, `volatilityCumulative` a
/// uint88, `secondsPerLiquidityCumulative` a uint160 and `volumePerLiquidityCumulative` a
/// uint144; the wider Rust types never hold values outside those ranges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timepoint {
    pub index: u16,
    pub initialized: bool,
    pub block_timestamp: u32,
    pub tick_cumulative: i64,
    pub seconds_per_liquidity_cumulative: U256,
    pub volatility_cumulative: u128,
    pub average_tick: i32,
    pub volume_per_liquidity_cumulative: U256,
}

impl Timepoint {
    /// Decodes the `timepoints/{index}` attribute: the two storage words of the struct, slot
    /// `2 * index` followed by slot `2 * index + 1`, exactly as the operator stores them.
    pub fn from_attribute(index: u16, bytes: &[u8]) -> Result<Self, SimulationError> {
        if bytes.len() != 64 {
            return Err(SimulationError::FatalError(format!(
                "timepoint {index} attribute of {} bytes is not two storage words",
                bytes.len()
            )));
        }
        let (first, second) = bytes.split_at(32);
        // First word, from the low end: initialized (1), blockTimestamp (4), tickCumulative
        // (7), secondsPerLiquidityCumulative (20).
        let initialized = first[31] != 0;
        let block_timestamp = u32::from_be_bytes(
            first[27..31]
                .try_into()
                .expect("4 bytes"),
        );
        let tick_cumulative = sign_extend(&first[20..27]) as i64;
        let seconds_per_liquidity_cumulative = U256::from_be_slice(&first[0..20]);
        // Second word, from the low end: volatilityCumulative (11), averageTick (3),
        // volumePerLiquidityCumulative (18).
        let volatility_cumulative = U256::from_be_slice(&second[21..32]).to::<u128>();
        let average_tick = sign_extend(&second[18..21]) as i32;
        let volume_per_liquidity_cumulative = U256::from_be_slice(&second[0..18]);
        Ok(Self {
            index,
            initialized,
            block_timestamp,
            tick_cumulative,
            seconds_per_liquidity_cumulative,
            volatility_cumulative,
            average_tick,
            volume_per_liquidity_cumulative,
        })
    }

    /// The inverse of [`Self::from_attribute`].
    pub fn to_attribute(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[31] = u8::from(self.initialized);
        out[27..31].copy_from_slice(&self.block_timestamp.to_be_bytes());
        out[20..27].copy_from_slice(&self.tick_cumulative.to_be_bytes()[1..8]);
        out[0..20].copy_from_slice(
            &self
                .seconds_per_liquidity_cumulative
                .to_be_bytes::<32>()[12..32],
        );
        out[53..64].copy_from_slice(&self.volatility_cumulative.to_be_bytes()[5..16]);
        out[50..53].copy_from_slice(&self.average_tick.to_be_bytes()[1..4]);
        out[32..50].copy_from_slice(
            &self
                .volume_per_liquidity_cumulative
                .to_be_bytes::<32>()[14..32],
        );
        out
    }
}

fn wrap_i56(value: i128) -> i64 {
    let bits = value & INT56_MASK;
    if bits & (1 << 55) != 0 {
        (bits - (1 << 56)) as i64
    } else {
        bits as i64
    }
}

fn wrap_i24(value: i64) -> i32 {
    let bits = value & 0xff_ffff;
    if bits & (1 << 23) != 0 {
        (bits - (1 << 24)) as i32
    } else {
        bits as i32
    }
}

fn mask(value: U256, bits: usize) -> U256 {
    value & ((U256::from(1) << bits) - U256::from(1))
}

/// `lteConsideringOverflow`: whether `a` is chronologically at or before `b`, for 32-bit
/// timestamps that may have wrapped relative to `current_time`.
fn lte(a: u32, b: u32, current_time: u32) -> bool {
    let mut res = a > current_time;
    if res == (b > current_time) {
        res = a <= b;
    }
    res
}

/// `_volatilityOnRange`: the sum of squared tick-to-average distances over `dt` seconds, with
/// both the tick and the average tick interpolated linearly between the two timepoints.
///
/// Terms reach 2^149, so the arithmetic is done in `I256`. The result is non-negative and
/// returned as the contract's `uint256` cast of it.
fn volatility_on_range(dt: u32, tick0: i32, tick1: i32, avg_tick0: i32, avg_tick1: i32) -> U256 {
    let dt = I256::unchecked_from(i128::from(dt));
    let (tick0, tick1) =
        (I256::unchecked_from(i128::from(tick0)), I256::unchecked_from(i128::from(tick1)));
    let (avg0, avg1) =
        (I256::unchecked_from(i128::from(avg_tick0)), I256::unchecked_from(i128::from(avg_tick1)));
    let six = I256::unchecked_from(6i128);
    let k = (tick1 - tick0) - (avg1 - avg0);
    let b = (tick0 - avg0) * dt;
    let sum_of_squares = dt * (dt + I256::ONE) * (I256::unchecked_from(2i128) * dt + I256::ONE);
    let sum_of_sequence = dt * (dt + I256::ONE);
    let volatility = (k * k * sum_of_squares + six * b * k * sum_of_sequence + six * dt * b * b) /
        (six * dt * dt);
    volatility.into_raw()
}

/// `createNewTimepoint`: the timepoint written at `block_timestamp` given the previous one.
///
/// `tick` is the pool tick at the write, `prev_tick` the tick at the previous timepoint,
/// `liquidity` the in-range liquidity and `volume_per_liquidity` the pool's accumulated
/// `volumePerLiquidityInBlock`.
fn create_new_timepoint(
    last: &Timepoint,
    block_timestamp: u32,
    tick: i32,
    prev_tick: i32,
    liquidity: u128,
    average_tick: i32,
    volume_per_liquidity: u128,
) -> Timepoint {
    let delta = block_timestamp.wrapping_sub(last.block_timestamp);
    let tick_cumulative =
        wrap_i56(i128::from(last.tick_cumulative) + i128::from(tick) * i128::from(delta));
    let seconds_per_liquidity_cumulative = mask(
        last.seconds_per_liquidity_cumulative +
            ((U256::from(delta) << 128) / U256::from(liquidity.max(1))),
        160,
    );
    // `uint88(_volatilityOnRange(...))` then a wrapping add.
    let volatility =
        mask(volatility_on_range(delta, prev_tick, tick, last.average_tick, average_tick), 88)
            .to::<u128>();
    let volatility_cumulative = last
        .volatility_cumulative
        .wrapping_add(volatility) &
        UINT88_MASK;
    let volume_per_liquidity_cumulative =
        mask(last.volume_per_liquidity_cumulative + U256::from(volume_per_liquidity), 144);
    Timepoint {
        index: last.index,
        initialized: true,
        block_timestamp,
        tick_cumulative,
        seconds_per_liquidity_cumulative,
        volatility_cumulative,
        average_tick,
        volume_per_liquidity_cumulative,
    }
}

/// The kept stretch of a pool's oracle ring, ordered by time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timepoints {
    entries: Vec<Timepoint>,
}

impl Timepoints {
    /// Builds the ring from the indexed timepoints, in any order. A ring index given more than
    /// once keeps its newest timepoint, as a slot overwritten on chain would.
    pub fn new(mut timepoints: Vec<Timepoint>) -> Self {
        timepoints.retain(|t| t.initialized);
        timepoints.sort_by_key(|t| (t.index, Reverse(t.block_timestamp)));
        timepoints.dedup_by_key(|t| t.index);
        timepoints.sort_by_key(|t| (t.block_timestamp, t.index));
        Self { entries: timepoints }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: u16) -> Option<&Timepoint> {
        self.entries
            .iter()
            .find(|t| t.index == index)
    }

    /// Inserts or replaces the timepoint at `timepoint.index`, keeping time order.
    pub fn upsert(&mut self, timepoint: Timepoint) {
        self.remove(timepoint.index);
        if !timepoint.initialized {
            return;
        }
        let position = self
            .entries
            .partition_point(|t| t.block_timestamp <= timepoint.block_timestamp);
        self.entries.insert(position, timepoint);
    }

    pub fn remove(&mut self, index: u16) {
        self.entries
            .retain(|t| t.index != index);
    }

    /// `DataStorage.write`: the timepoint the pool writes at `block_timestamp`, or `None` when
    /// the last timepoint already carries that timestamp. The caller stores it with
    /// [`Self::upsert`] once the swap it belongs to is committed.
    pub fn write(
        &self,
        index: u16,
        block_timestamp: u32,
        tick: i32,
        liquidity: u128,
        volume_per_liquidity: u128,
    ) -> Result<Option<Timepoint>, SimulationError> {
        let ring = Ring::new(&self.entries, None);
        let last = ring.last(index)?;
        if last.block_timestamp == block_timestamp {
            return Ok(None);
        }
        let average_tick = wrap_i24(ring.average_tick(block_timestamp, tick, last)?);
        let prev_tick = ring.prev_tick(last, tick);
        let mut timepoint = create_new_timepoint(
            last,
            block_timestamp,
            tick,
            prev_tick,
            liquidity,
            average_tick,
            volume_per_liquidity,
        );
        timepoint.index = index.wrapping_add(1);
        Ok(Some(timepoint))
    }

    /// `DataStorage.getAverages`: the 1-day average volatility and volume per liquidity at
    /// `time`, as `DataStorageOperator.getFees` reads them right after a write.
    ///
    /// `index` is the last written timepoint, which may be `appended` — the timepoint a swap
    /// is about to write — rather than one already in the ring.
    pub fn averages(
        &self,
        appended: Option<&Timepoint>,
        time: u32,
        tick: i32,
        index: u16,
        liquidity: u128,
    ) -> Result<(u128, U256), SimulationError> {
        let ring = Ring::new(&self.entries, appended);
        let last = ring.last(index)?;
        let oldest = ring.oldest()?;
        let end_of_window = ring.single_timepoint(time, 0, tick, last, liquidity)?;
        if lte(oldest.block_timestamp, time.wrapping_sub(WINDOW), time) {
            let start_of_window = ring.single_timepoint(time, WINDOW, tick, last, liquidity)?;
            Ok((
                (end_of_window
                    .volatility_cumulative
                    .wrapping_sub(start_of_window.volatility_cumulative) &
                    UINT88_MASK) /
                    u128::from(WINDOW),
                mask(
                    end_of_window
                        .volume_per_liquidity_cumulative
                        .wrapping_sub(start_of_window.volume_per_liquidity_cumulative),
                    144,
                ) >> 57,
            ))
        } else if time != oldest.block_timestamp {
            Ok((
                (end_of_window
                    .volatility_cumulative
                    .wrapping_sub(oldest.volatility_cumulative) &
                    UINT88_MASK) /
                    u128::from(time.wrapping_sub(oldest.block_timestamp)),
                mask(
                    end_of_window
                        .volume_per_liquidity_cumulative
                        .wrapping_sub(oldest.volume_per_liquidity_cumulative),
                    144,
                ) >> 57,
            ))
        } else {
            Ok((0, U256::ZERO))
        }
    }
}

/// The ring as the contract sees it during one operation: the kept entries, optionally
/// followed by a timepoint that is being written.
struct Ring<'a> {
    entries: &'a [Timepoint],
    appended: Option<&'a Timepoint>,
}

impl<'a> Ring<'a> {
    /// The ring as the contract reads it while `appended` is being written: the slot the write
    /// takes over is the oldest one, and the indexer may still hold its previous content.
    fn new(entries: &'a [Timepoint], appended: Option<&'a Timepoint>) -> Self {
        let overwritten = appended.is_some_and(|written| {
            entries
                .first()
                .is_some_and(|oldest| oldest.index == written.index)
        });
        Self { entries: if overwritten { &entries[1..] } else { entries }, appended }
    }

    fn oldest(&self) -> Result<&'a Timepoint, SimulationError> {
        self.entries
            .first()
            .or(self.appended)
            .ok_or_else(|| SimulationError::FatalError("oracle ring is empty".to_string()))
    }

    /// The timepoint the pool's `timepointIndex` points at.
    fn last(&self, index: u16) -> Result<&'a Timepoint, SimulationError> {
        if let Some(appended) = self
            .appended
            .filter(|t| t.index == index)
        {
            return Ok(appended);
        }
        self.entries
            .iter()
            .rev()
            .find(|t| t.index == index)
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "oracle ring is missing the last written timepoint {index}"
                ))
            })
    }

    /// `self[index - 1]` for the timepoint before `last`, when `index != oldestIndex`.
    ///
    /// The indexer keeps that timepoint whenever the chain has one, so its absence means the
    /// ring holds a single timepoint and the contract would read an empty slot.
    fn before(&self, last: &Timepoint) -> Option<&'a Timepoint> {
        if self
            .appended
            .is_some_and(|t| t.index == last.index)
        {
            return self.entries.last();
        }
        // `last` is the newest entry, so the search starts from the end.
        let position = self
            .entries
            .iter()
            .rposition(|t| t.index == last.index)?;
        position
            .checked_sub(1)
            .map(|p| &self.entries[p])
    }

    /// The tick at the previous timepoint, derived from the cumulative ticks as the contract
    /// does, or the current tick when there is no previous timepoint.
    fn prev_tick(&self, last: &Timepoint, tick: i32) -> i32 {
        match self.before(last) {
            Some(prev) => wrap_i24(
                wrap_i56(i128::from(last.tick_cumulative) - i128::from(prev.tick_cumulative)) /
                    i64::from(
                        last.block_timestamp
                            .wrapping_sub(prev.block_timestamp),
                    ),
            ),
            None => tick,
        }
    }

    /// `_getAverageTick`: the average tick over the window ending at `time`, before the int24
    /// cast the callers apply.
    fn average_tick(&self, time: u32, tick: i32, last: &Timepoint) -> Result<i64, SimulationError> {
        let oldest = self.oldest()?;
        let window_start = time.wrapping_sub(WINDOW);
        if lte(oldest.block_timestamp, window_start, time) {
            if lte(last.block_timestamp, window_start, time) {
                // The last timepoint is older than the window: average over its own interval.
                return Ok(match self.before(last) {
                    Some(start) => {
                        wrap_i56(
                            i128::from(last.tick_cumulative) - i128::from(start.tick_cumulative),
                        ) / i64::from(
                            last.block_timestamp
                                .wrapping_sub(start.block_timestamp),
                        )
                    }
                    None => i64::from(tick),
                });
            }
            let start_of_window = self.single_timepoint(time, WINDOW, tick, last, 0)?;
            // `lastTimestamp - time + WINDOW` in uint32 arithmetic.
            let seconds = last
                .block_timestamp
                .wrapping_sub(time)
                .wrapping_add(WINDOW);
            return Ok(wrap_i56(
                i128::from(last.tick_cumulative) - i128::from(start_of_window.tick_cumulative),
            ) / i64::from(seconds));
        }
        if last.block_timestamp == oldest.block_timestamp {
            return Ok(i64::from(tick));
        }
        Ok(wrap_i56(i128::from(last.tick_cumulative) - i128::from(oldest.tick_cumulative)) /
            i64::from(
                last.block_timestamp
                    .wrapping_sub(oldest.block_timestamp),
            ))
    }

    /// `binarySearch`: the stored timepoints at or before and at or after `target`.
    fn binary_search(
        &self,
        time: u32,
        target: u32,
    ) -> Result<(Timepoint, Timepoint), SimulationError> {
        let position = self
            .entries
            .partition_point(|t| lte(t.block_timestamp, target, time));
        let Some(before_or_at) = position
            .checked_sub(1)
            .map(|p| self.entries[p])
        else {
            return Err(SimulationError::FatalError(
                "oracle target precedes every kept timepoint".to_string(),
            ));
        };
        let at_or_after = self
            .entries
            .get(position)
            .or(self.appended)
            .copied()
            .unwrap_or(before_or_at);
        Ok((before_or_at, at_or_after))
    }

    /// `getSingleTimepoint`: the timepoint as of `seconds_ago` before `time`, extrapolated
    /// from the last one when the target is newer than it, otherwise interpolated between the
    /// two stored neighbours.
    fn single_timepoint(
        &self,
        time: u32,
        seconds_ago: u32,
        tick: i32,
        last: &Timepoint,
        liquidity: u128,
    ) -> Result<Timepoint, SimulationError> {
        let target = time.wrapping_sub(seconds_ago);
        if seconds_ago == 0 || lte(last.block_timestamp, target, time) {
            if last.block_timestamp == target {
                return Ok(*last);
            }
            let average_tick = wrap_i24(self.average_tick(time, tick, last)?);
            let prev_tick = self.prev_tick(last, tick);
            return Ok(create_new_timepoint(
                last,
                target,
                tick,
                prev_tick,
                liquidity,
                average_tick,
                0,
            ));
        }
        let oldest = self.oldest()?;
        if !lte(oldest.block_timestamp, target, time) {
            return Err(SimulationError::FatalError(
                "OLD: oracle target precedes the oldest timepoint".to_string(),
            ));
        }
        let (mut before_or_at, at_or_after) = self.binary_search(time, target)?;
        if target == at_or_after.block_timestamp {
            return Ok(at_or_after);
        }
        if target != before_or_at.block_timestamp {
            let delta = at_or_after
                .block_timestamp
                .wrapping_sub(before_or_at.block_timestamp);
            let target_delta = target.wrapping_sub(before_or_at.block_timestamp);
            // Every step below is the contract's: a wrapping subtraction at the field width, a
            // truncating division by the time delta, a wrapping multiplication by the target
            // delta and a wrapping addition.
            let tick_step = wrap_i56(
                i128::from(at_or_after.tick_cumulative) - i128::from(before_or_at.tick_cumulative),
            ) / i64::from(delta);
            before_or_at.tick_cumulative = wrap_i56(
                i128::from(before_or_at.tick_cumulative) +
                    wrap_i56(i128::from(tick_step) * i128::from(target_delta)) as i128,
            );
            let liquidity_delta = mask(
                at_or_after
                    .seconds_per_liquidity_cumulative
                    .wrapping_sub(before_or_at.seconds_per_liquidity_cumulative),
                160,
            );
            before_or_at.seconds_per_liquidity_cumulative = mask(
                before_or_at.seconds_per_liquidity_cumulative +
                    mask(liquidity_delta * U256::from(target_delta) / U256::from(delta), 160),
                160,
            );
            let volatility_step = (at_or_after
                .volatility_cumulative
                .wrapping_sub(before_or_at.volatility_cumulative) &
                UINT88_MASK) /
                u128::from(delta);
            before_or_at.volatility_cumulative = before_or_at
                .volatility_cumulative
                .wrapping_add(volatility_step.wrapping_mul(u128::from(target_delta)) & UINT88_MASK) &
                UINT88_MASK;
            let volume_step = mask(
                at_or_after
                    .volume_per_liquidity_cumulative
                    .wrapping_sub(before_or_at.volume_per_liquidity_cumulative),
                144,
            ) / U256::from(delta);
            before_or_at.volume_per_liquidity_cumulative = mask(
                before_or_at.volume_per_liquidity_cumulative +
                    mask(volume_step * U256::from(target_delta), 144),
                144,
            );
        }
        Ok(before_or_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `timepoints(0)` of the WETH/USDC pool's operator `0xa5ebb8ff…ea23` at Arbitrum block
    /// 512282036: storage slots 0 and 1, checked against the getter.
    const TIMEPOINT_0: [u8; 64] = hex_literal::hex!(
        "000000000000000612fe4b4ffb93521573505bd9ffed77eba5c55b6ab5665e01"
        "0000000000000000056d5f282edec222eb4afcfcdf000000000003731a0a4938"
    );

    fn timepoint(index: u16, block_timestamp: u32, tick_cumulative: i64) -> Timepoint {
        Timepoint {
            index,
            initialized: true,
            block_timestamp,
            tick_cumulative,
            ..Default::default()
        }
    }

    #[test]
    fn decodes_the_two_storage_words_of_a_timepoint() {
        let decoded = Timepoint::from_attribute(0, &TIMEPOINT_0).unwrap();
        assert!(decoded.initialized);
        assert_eq!(decoded.block_timestamp, 1_790_273_118);
        assert_eq!(decoded.tick_cumulative, -20_375_666_309_797);
        assert_eq!(
            decoded.seconds_per_liquidity_cumulative,
            U256::from_str_radix("481247128073459272832309550041", 10).unwrap()
        );
        assert_eq!(decoded.volatility_cumulative, 3_792_893_004_088);
        assert_eq!(decoded.average_tick, -197_409);
        assert_eq!(
            decoded.volume_per_liquidity_cumulative,
            U256::from_str_radix("25629384300349513460554", 10).unwrap()
        );
        assert_eq!(decoded.to_attribute(), TIMEPOINT_0);
    }

    #[test]
    fn rejects_an_attribute_that_is_not_two_words() {
        assert!(Timepoint::from_attribute(0, &[0u8; 32]).is_err());
    }

    #[test]
    fn wraps_at_the_contract_widths() {
        assert_eq!(wrap_i56(1 << 55), -(1i64 << 55));
        assert_eq!(wrap_i56(-1), -1);
        assert_eq!(wrap_i24(1 << 23), -(1 << 23));
        assert_eq!(wrap_i24(-197_409), -197_409);
        assert_eq!(mask(U256::from(1) << 160, 160), U256::ZERO);
    }

    #[test]
    fn lte_handles_timestamps_around_a_wrap() {
        assert!(lte(10, 20, 100));
        assert!(!lte(20, 10, 100));
        // `a` wrapped past the current time, so it is chronologically earlier than `b`.
        assert!(lte(u32::MAX - 5, 10, 20));
        assert!(!lte(10, u32::MAX - 5, 20));
    }

    #[test]
    fn volatility_is_zero_when_tick_tracks_its_average() {
        assert_eq!(volatility_on_range(100, 5, 5, 5, 5), U256::ZERO);
    }

    #[test]
    fn volatility_of_a_constant_offset_is_dt_times_offset_squared() {
        // tick and average tick both constant, 3 apart: sum over dt seconds of 3^2.
        assert_eq!(volatility_on_range(7, 10, 10, 7, 7), U256::from(7 * 9));
    }

    #[test]
    fn write_is_a_no_op_at_the_last_timestamp() {
        let ring = Timepoints::new(vec![timepoint(0, 1_000, 0)]);
        assert!(ring
            .write(0, 1_000, 5, 1, 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn write_accumulates_from_the_last_timepoint() {
        let mut first = timepoint(0, 1_000, 0);
        first.average_tick = 5;
        let ring = Timepoints::new(vec![first]);
        let written = ring
            .write(0, 1_010, 5, 2, 7)
            .unwrap()
            .unwrap();
        assert_eq!(written.index, 1);
        assert_eq!(written.block_timestamp, 1_010);
        assert_eq!(written.tick_cumulative, 50);
        assert_eq!(
            written.seconds_per_liquidity_cumulative,
            (U256::from(10) << 128) / U256::from(2)
        );
        assert_eq!(written.volume_per_liquidity_cumulative, U256::from(7));
        // A ring with one timepoint averages to the current tick, and the previous tick
        // defaults to it too: tick and average never deviate, so no volatility accrues.
        assert_eq!(written.average_tick, 5);
        assert_eq!(written.volatility_cumulative, 0);
    }

    #[test]
    fn write_accrues_volatility_when_the_average_tick_moves() {
        // average tick 0 -> 5 while the tick stays at 5 over 10 seconds:
        // K = -5, B = 50; (25 * 2310 - 6 * 50 * 5 * 110 + 6 * 10 * 2500) / 600 = 71.
        let ring = Timepoints::new(vec![timepoint(0, 1_000, 0)]);
        let written = ring
            .write(0, 1_010, 5, 2, 0)
            .unwrap()
            .unwrap();
        assert_eq!(written.volatility_cumulative, 71);
    }

    #[test]
    fn write_wraps_the_ring_index() {
        let ring = Timepoints::new(vec![timepoint(u16::MAX, 1_000, 0)]);
        let written = ring
            .write(u16::MAX, 1_001, 0, 1, 0)
            .unwrap()
            .unwrap();
        assert_eq!(written.index, 0);
    }

    #[test]
    fn upsert_keeps_time_order_and_replaces_by_index() {
        let mut ring = Timepoints::new(vec![timepoint(1, 2_000, 0), timepoint(0, 1_000, 0)]);
        assert_eq!(ring.entries[0].index, 0);
        ring.upsert(timepoint(2, 3_000, 0));
        ring.upsert(timepoint(0, 500, 1));
        let order: Vec<_> = ring
            .entries
            .iter()
            .map(|t| (t.index, t.block_timestamp))
            .collect();
        assert_eq!(order, vec![(0, 500), (1, 2_000), (2, 3_000)]);
        ring.remove(1);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn averages_over_a_young_ring_use_the_oldest_timepoint() {
        // Two timepoints, both inside the last day: the contract averages from the oldest.
        let mut first = timepoint(0, 1_000, 0);
        first.average_tick = 0;
        let mut second = timepoint(1, 1_100, 0);
        second.volatility_cumulative = 400;
        second.volume_per_liquidity_cumulative = U256::from(1) << 60;
        let ring = Timepoints::new(vec![first, second]);
        let (volatility, volume) = ring
            .averages(None, 1_200, 0, 1, 1)
            .unwrap();
        // End of window extrapolates the last timepoint over 100 seconds with the tick on its
        // average, adding no volatility; 400 over 200 seconds since the oldest.
        assert_eq!(volatility, 2);
        assert_eq!(volume, U256::from(1) << 3);
    }

    #[test]
    fn averages_see_the_timepoint_being_written() {
        let last = timepoint(0, 1_000, 0);
        let ring = Timepoints::new(vec![last]);
        let appended = ring
            .write(0, 1_000 + WINDOW + 10, 0, 1, 1 << 57)
            .unwrap()
            .unwrap();
        // The oldest timepoint now precedes the window start, so the window branch runs; its
        // start interpolates between the two timepoints and the volume over the window is the
        // appended timepoint's share of the write.
        let (_, volume) = ring
            .averages(Some(&appended), 1_000 + WINDOW + 10, 0, 1, 1)
            .unwrap();
        assert!(volume <= U256::from(1));
    }

    #[test]
    fn missing_last_timepoint_is_an_error() {
        let ring = Timepoints::new(vec![timepoint(0, 1_000, 0)]);
        assert!(ring.write(7, 2_000, 0, 1, 0).is_err());
    }

    #[test]
    fn new_keeps_the_newest_write_of_a_ring_index() {
        let ring = Timepoints::new(vec![
            timepoint(0, 1_000, 0),
            timepoint(1, 2_000, 0),
            timepoint(0, 3_000, 7),
        ]);
        let order: Vec<_> = ring
            .entries
            .iter()
            .map(|t| (t.index, t.block_timestamp))
            .collect();
        assert_eq!(order, vec![(1, 2_000), (0, 3_000)]);
        assert_eq!(ring.get(0).unwrap().tick_cumulative, 7);
    }

    #[test]
    fn averages_ignore_the_slot_a_write_overwrites() {
        // A wrapped three-slot ring: slot 2 holds the oldest timepoint and is the next one
        // written. Once written, the contract's oldest timepoint is slot 0.
        let mut stale = timepoint(2, 1_000, 0);
        stale.volatility_cumulative = 100;
        let mut first = timepoint(0, 2_000, 0);
        first.volatility_cumulative = 1_000;
        first.volume_per_liquidity_cumulative = U256::from(1) << 60;
        let mut last = timepoint(1, 2_100, 0);
        last.volatility_cumulative = 1_500;
        last.volume_per_liquidity_cumulative = U256::from(3) << 60;
        let wrapped = Timepoints::new(vec![stale, first, last]);
        let after_write = Timepoints::new(vec![first, last]);
        // The window start falls between the overwritten slot and slot 0.
        let time = 1_500 + WINDOW;
        let written = wrapped
            .write(1, time, 0, 1, 0)
            .unwrap()
            .unwrap();
        assert_eq!(written.index, 2);

        assert_eq!(
            wrapped
                .averages(Some(&written), time, 0, 2, 1)
                .unwrap(),
            after_write
                .averages(Some(&written), time, 0, 2, 1)
                .unwrap()
        );
    }
}
