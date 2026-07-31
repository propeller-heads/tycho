use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256};

#[derive(Default, Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub(crate) block_timestamp: u32,
    pub(crate) tick_cumulative: i64,
    pub(crate) seconds_per_liquidity_cumulative_x128: U256,
    pub(crate) initialized: bool,
    pub(crate) index: i32,
}

impl Observation {
    pub fn from_attribute(index: i32, bytes: &[u8]) -> Result<Self, SimulationError> {
        if bytes.len() != 32 {
            return Err(SimulationError::FatalError(format!(
                "Invalid observation length: expected 32 bytes, got {}",
                bytes.len()
            )));
        }

        let value = U256::from_be_slice(bytes);

        // bits [0..32)     → blockTimestamp (uint32)
        // bits [32..88)    → tickCumulative (int56)
        // bits [88..248)   → secondsPerLiquidityCumulativeX128 (uint160)
        // bits [248..256)  → initialized (bool)

        // 1. blockTimestamp
        let block_timestamp: u32 = (value & U256::from((1u64 << 32) - 1)).to::<u32>();

        // 2. tickCumulative (signed 56-bit)
        let tick_bits: u64 = ((value >> 32u32) & U256::from((1u64 << 56) - 1)).to::<u64>();

        let tick_cumulative: i64 = if (tick_bits & (1 << 55)) != 0 {
            // 56-bit sign extend
            (tick_bits as i64) - (1i64 << 56)
        } else {
            tick_bits as i64
        };

        // 3. secondsPerLiquidityCumulativeX128 (160-bit)
        let seconds_per_liquidity_cumulative_x128 =
            (value >> 88) & ((U256::from(1) << 160) - U256::from(1));

        // 4. initialized (bool)
        let initialized = ((value >> 248) & U256::from(1)) == U256::from(1);

        Ok(Self {
            block_timestamp,
            tick_cumulative,
            seconds_per_liquidity_cumulative_x128,
            initialized,
            index,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observations {
    observations: Vec<Observation>,
}

impl Observations {
    pub fn new(observations: Vec<Observation>) -> Self {
        Self { observations }
    }

    fn observation_index_err(&self, idx: usize, index: u16, cardinality: u16) -> SimulationError {
        SimulationError::FatalError(format!(
            "Observation index {} out of bounds (len={}), index={} cardinality={}",
            idx,
            self.observations.len(),
            index,
            cardinality
        ))
    }

    pub fn upsert_observation(&mut self, index: i32, bytes: &[u8]) -> Result<(), SimulationError> {
        let idx = index as usize;
        if bytes.is_empty() {
            return if idx < self.observations.len() {
                self.observations.remove(idx);
                Ok(())
            } else {
                Err(SimulationError::FatalError(format!(
                    "Cannot delete: index {} out of bounds (len={})",
                    index,
                    self.observations.len()
                )))
            };
        }
        let mut obs = Observation::from_attribute(index, bytes)?;
        obs.index = index;
        if idx < self.observations.len() {
            self.observations[idx] = obs;
            return Ok(());
        }
        if idx >= self.observations.capacity() {
            self.observations
                .reserve(idx - self.observations.len() + 1);
        }

        while self.observations.len() < idx {
            self.observations
                .push(Observation::default());
        }

        self.observations.push(obs);
        Ok(())
    }

    pub fn timestamp_at(&self, index: u16, cardinality: u16) -> Result<u32, SimulationError> {
        let idx = index as usize;
        if idx >= self.observations.len() {
            return Err(self.observation_index_err(idx, index, cardinality));
        }

        Ok(self.observations[idx].block_timestamp)
    }

    pub fn observe(
        &self,
        time: u32,
        seconds_agos: &[u32],
        tick: i32,
        index: u16,
        liquidity: u128,
        cardinality: u16,
    ) -> Result<(Vec<i64>, Vec<U256>), SimulationError> {
        if cardinality == 0 {
            return Err(SimulationError::FatalError("Cardinality must be > 0".to_string()));
        }
        let mut tick_cumulatives = Vec::with_capacity(seconds_agos.len());
        let mut seconds_per_liquidity_cumulative_x128s = Vec::with_capacity(seconds_agos.len());

        for &seconds_ago in seconds_agos {
            let (tick_cum, sec_liq) =
                self.observe_single(time, seconds_ago, tick, index, liquidity, cardinality)?;
            tick_cumulatives.push(tick_cum);
            seconds_per_liquidity_cumulative_x128s.push(sec_liq);
        }

        Ok((tick_cumulatives, seconds_per_liquidity_cumulative_x128s))
    }

    pub fn binary_search(
        &self,
        time: u32,
        target: u32,
        index: u16,
        cardinality: u16,
    ) -> Result<(Observation, Observation), SimulationError> {
        if self.observations.is_empty() {
            return Err(SimulationError::FatalError("No observations available".to_string()));
        }
        let mut l = (index as usize + 1) % cardinality as usize;
        let mut r = l + cardinality as usize - 1;

        loop {
            let i = (l + r) / 2;
            let before_idx = i % cardinality as usize;
            if before_idx >= self.observations.len() {
                return Err(self.observation_index_err(before_idx, index, cardinality));
            }
            let before_or_at = self.observations[before_idx];

            if !before_or_at.initialized {
                l = i + 1;
                continue;
            }

            let after_idx = (i + 1) % cardinality as usize;
            if after_idx >= self.observations.len() {
                return Err(self.observation_index_err(after_idx, index, cardinality));
            }
            let at_or_after = self.observations[after_idx];
            let target_at_or_after = lte(time, before_or_at.block_timestamp, target);
            if target_at_or_after && lte(time, target, at_or_after.block_timestamp) {
                return Ok((before_or_at, at_or_after));
            }

            if !target_at_or_after {
                if i == 0 {
                    break;
                }
                r = i - 1;
            } else {
                l = i + 1;
            }
        }

        Err(SimulationError::FatalError(
            "Binary search failed — inconsistent oracle data".to_string(),
        ))
    }

    pub fn get_surrounding_observations(
        &self,
        time: u32,
        target: u32,
        tick: i32,
        index: u16,
        liquidity: u128,
        cardinality: u16,
    ) -> Result<(Observation, Observation), SimulationError> {
        let idx = index as usize;
        if idx >= self.observations.len() {
            return Err(self.observation_index_err(idx, index, cardinality));
        }
        let mut before_or_at = self.observations[idx];

        if lte(time, before_or_at.block_timestamp, target) {
            if before_or_at.block_timestamp == target {
                return Ok((before_or_at, before_or_at));
            }
            return Ok((before_or_at, transform(&before_or_at, target, tick, liquidity)?));
        }

        let next_idx = (index as usize + 1) % cardinality as usize;
        if next_idx >= self.observations.len() {
            return Err(self.observation_index_err(next_idx, index, cardinality));
        }
        before_or_at = self.observations[next_idx];
        if !before_or_at.initialized {
            if self.observations.is_empty() {
                return Err(SimulationError::FatalError("No observations available".to_string()));
            }
            before_or_at = self.observations[0];
        }

        if !lte(time, before_or_at.block_timestamp, target) {
            return Err(SimulationError::FatalError(
                "Target too old (after oldest observation)".to_string(),
            ));
        }

        self.binary_search(time, target, index, cardinality)
    }

    pub fn observe_single(
        &self,
        time: u32,
        seconds_ago: u32,
        tick: i32,
        index: u16,
        liquidity: u128,
        cardinality: u16,
    ) -> Result<(i64, U256), SimulationError> {
        if seconds_ago == 0 {
            let idx = index as usize;
            if idx >= self.observations.len() {
                return Err(self.observation_index_err(idx, index, cardinality));
            }
            let mut last = self.observations[idx];
            if last.block_timestamp != time {
                last = transform(&last, time, tick, liquidity)?;
            }
            return Ok((last.tick_cumulative, last.seconds_per_liquidity_cumulative_x128));
        }

        let target = time - seconds_ago;
        let (before_or_at, at_or_after) =
            self.get_surrounding_observations(time, target, tick, index, liquidity, cardinality)?;

        if target == before_or_at.block_timestamp {
            // we're at the left boundary
            Ok((before_or_at.tick_cumulative, before_or_at.seconds_per_liquidity_cumulative_x128))
        } else if target == at_or_after.block_timestamp {
            // we're at the right boundary
            Ok((at_or_after.tick_cumulative, at_or_after.seconds_per_liquidity_cumulative_x128))
        } else {
            // we're in the middle
            let observation_time_delta = at_or_after
                .block_timestamp
                .wrapping_sub(before_or_at.block_timestamp)
                as i64;
            let target_delta = target.wrapping_sub(before_or_at.block_timestamp) as i64;

            let tick_cumulative = before_or_at.tick_cumulative +
                ((at_or_after.tick_cumulative - before_or_at.tick_cumulative) * target_delta /
                    observation_time_delta);

            let seconds_per_liquidity_cumulative_x128 = {
                let delta = safe_sub_u256(
                    at_or_after.seconds_per_liquidity_cumulative_x128,
                    before_or_at.seconds_per_liquidity_cumulative_x128,
                )?;
                let scaled = safe_div_u256(
                    safe_mul_u256(delta, U256::from(target_delta as u128))?,
                    U256::from(observation_time_delta as u128),
                )?;
                safe_add_u256(before_or_at.seconds_per_liquidity_cumulative_x128, scaled)?
            };

            Ok((tick_cumulative, seconds_per_liquidity_cumulative_x128))
        }
    }
}

fn lte(time: u32, a: u32, b: u32) -> bool {
    if a <= time && b <= time {
        return a <= b;
    }

    let a_adjusted = if a > time { a as u64 } else { a as u64 + (1u64 << 32) };
    let b_adjusted = if b > time { b as u64 } else { b as u64 + (1u64 << 32) };

    a_adjusted <= b_adjusted
}

fn transform(
    before: &Observation,
    target: u32,
    tick: i32,
    liquidity: u128,
) -> Result<Observation, SimulationError> {
    let delta = target - before.block_timestamp;
    let tick_cumulative = before.tick_cumulative + tick as i64 * delta as i64;

    // seconds_per_liquidity_cumulative_x128 += delta / liquidity
    let seconds_per_liquidity_cumulative_x128 = if liquidity > 0 {
        safe_add_u256(
            before.seconds_per_liquidity_cumulative_x128,
            (U256::from(delta as u128) << 128) / U256::from(liquidity),
        )?
    } else {
        before.seconds_per_liquidity_cumulative_x128
    };

    Ok(Observation {
        block_timestamp: target,
        tick_cumulative,
        seconds_per_liquidity_cumulative_x128,
        initialized: true,
        index: before.index,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hex_literal::hex;

    use super::*;

    #[test]
    fn test_lte_basic_cases() {
        assert!(lte(100, 90, 95));
        assert!(!lte(100, 95, 90));
        assert!(lte(100, 100, 100));
    }

    #[test]
    fn test_lte_wrap_around_cases() {
        let time = 50;
        let a = u32::MAX - 10;
        let b = 20;
        assert!(lte(time, a, b));
        assert!(!lte(time, b, a));
        assert!(lte(200, 10, 20));
    }

    #[test]
    fn test_transform_normal_case() {
        let before = Observation {
            block_timestamp: 100,
            tick_cumulative: 10,
            seconds_per_liquidity_cumulative_x128: U256::from(0),
            initialized: true,
            index: 0,
        };

        let tick = 5;
        let liquidity = 10u128;
        let target = 110;

        let result = transform(&before, target, tick, liquidity).unwrap();

        assert_eq!(result.tick_cumulative, 60);
        assert_eq!(result.block_timestamp, target);
        assert!(result.initialized);
        assert_eq!(result.index, before.index);

        assert!(
            result.seconds_per_liquidity_cumulative_x128 >
                before.seconds_per_liquidity_cumulative_x128
        );
    }

    #[test]
    fn test_transform_zero_liquidity() {
        let before = Observation {
            block_timestamp: 100,
            tick_cumulative: 10,
            seconds_per_liquidity_cumulative_x128: U256::from(1234),
            initialized: true,
            index: 0,
        };

        let tick = 3;
        let liquidity = 0u128;
        let target = 110;

        let result = transform(&before, target, tick, liquidity).unwrap();

        assert_eq!(result.seconds_per_liquidity_cumulative_x128, U256::from(1234));
        assert_eq!(result.tick_cumulative, 10 + 3 * 10);
    }

    #[test]
    fn test_transform_large_delta() {
        let before = Observation {
            block_timestamp: 0,
            tick_cumulative: 0,
            seconds_per_liquidity_cumulative_x128: U256::from(0),
            initialized: true,
            index: 1,
        };
        let result = transform(&before, 100_000, 1, 1000).unwrap();
        assert_eq!(result.tick_cumulative, 100_000);
    }

    fn encode_observation_bytes(
        block_timestamp: u32,
        tick_cumulative: i64,
        seconds_per_liquidity_cumulative_x128: U256,
        initialized: bool,
    ) -> [u8; 32] {
        let mut value = U256::from(block_timestamp);

        let tick_encoded = if tick_cumulative < 0 {
            U256::from((1i128 << 56) + tick_cumulative as i128)
        } else {
            U256::from(tick_cumulative as u64)
        };
        value |= tick_encoded << 32u32;

        value |= (seconds_per_liquidity_cumulative_x128 & ((U256::from(1) << 160) - U256::from(1))) <<
            88u32;

        if initialized {
            value |= U256::from(1) << 248u32;
        }
        value.to_be_bytes()
    }

    #[test]
    fn test_from_attribute_and_upsert() {
        let bytes = encode_observation_bytes(100, 10, U256::from(123456), true);

        let obs = Observation::from_attribute(5, &bytes).unwrap();
        assert_eq!(obs.index, 5);
        assert_eq!(obs.block_timestamp, 100);

        // Upsert basic insert
        let mut obs_vec = Observations::new(vec![]);
        obs_vec
            .upsert_observation(0, &bytes)
            .unwrap();
        assert_eq!(obs_vec.observations.len(), 1);

        // Update existing
        let bytes2 = encode_observation_bytes(200, 50, U256::from(9999), true);
        obs_vec
            .upsert_observation(0, &bytes2)
            .unwrap();
        assert_eq!(obs_vec.observations[0].block_timestamp, 200);

        // Delete existing
        obs_vec
            .upsert_observation(0, &[])
            .unwrap();
        assert_eq!(obs_vec.observations.len(), 0);
    }

    #[test]
    fn test_observe_single() {
        let bytes = encode_observation_bytes(
            1762309495,
            -16009850,
            U256::from_str("2566142326090727851500728328109973377").unwrap(),
            true,
        );
        let mut obs = Observations::new(vec![]);
        assert!(obs
            .upsert_observation(216, &bytes)
            .is_ok());
        assert_eq!(
            obs.observe(1762330020, &[600, 0], 0, 216, 85863056191940832, 360)
                .unwrap(),
            (
                vec![-16009850, -16009850],
                vec![
                    U256::from_str("2566142326169692268347750439884046480").unwrap(),
                    U256::from_str("2566142326172070117788564154667707401").unwrap()
                ]
            )
        );
    }

    #[test]
    fn test_observe_multiple() {
        // Pool: 0xb2cc224c1c9fee385f8ad6a55b4d94e92359dc59
        let items: &[(i32, [u8; 32])] = &[
            (534, hex!("01000006a0000001485073e3b9c1b1ba599e933717fff778eb3ff056690b05a1")),
            (535, hex!("01000006a0000001485072f3c785d8212da7414577fff77952e1b5ec690ae2d7")),
            (2039, hex!("01000006a00000014850737217b73678a71c3a98fdfff7791d60d77e690af4cd")),
            (2040, hex!("01000006a00000014850737239d75dce70d02120fffff7791d5ae25e690af4cf")),
            (2792, hex!("01000006a0000001485073a83aca03f518567293a7fff779047fd580690afd27")),
            (2793, hex!("01000006a0000001485073a84425ed86707cf3426afff7790473eae0690afd2b")),
            (158, hex!("01000006a0000001485073c3bd34f1a8b1e3595ef2fff778f7fab090690b015b")),
            (159, hex!("01000006a0000001485073c3bfcaf719d5a10d1babfff778f7f4bb8c690b015d")),
            (346, hex!("01000006a0000001485073d0992e1120d6162dec08fff778f1d01820690b036d")),
            (347, hex!("01000006a0000001485073d0a6e39cdb5daeb0b3dcfff778f1be390e690b0373")),
            (252, hex!("01000006a0000001485073ca9b84bedd861255f8f6fff778f4ca8f18690b026d")),
            (253, hex!("01000006a0000001485073caa9f9acbba60b0ac592fff778f4c49a22690b026f")),
            (299, hex!("01000006a0000001485073cc668d277afc9908bf5afff778f35f3120690b02e7")),
            (300, hex!("01000006a0000001485073cc6cec51a251dcd597a7fff778f3593c34690b02e9")),
            (322, hex!("01000006a0000001485073ce6eb2883c6b9c109292fff778f2a688ec690b0325")),
            (323, hex!("01000006a0000001485073ce7c9a63260beda5874ffff778f2a09414690b0327")),
            (334, hex!("01000006a0000001485073cf5c7f6da6dee6f54694fff778f21d8926690b0353")),
            (335, hex!("01000006a0000001485073cf7b9869740c05f0bf44fff778f2179430690b0355")),
            (328, hex!("01000006a0000001485073cecedc4b2732bd3fb882fff778f24d3044690b0343")),
            (329, hex!("01000006a0000001485073ced7e92fc61bfda7ccd4fff778f2414694690b0347")),
            (331, hex!("01000006a0000001485073cee01b7e722a6b0763d3fff778f2355ce2690b034b")),
            (332, hex!("01000006a0000001485073ceff347a3f578a02dc83fff778f22f67f6690b034d")),
        ];
        let mut obs = Observations::new(vec![]);
        for (idx, bytes) in items.iter().copied() {
            assert!(obs
                .upsert_observation(idx, &bytes)
                .is_ok());
        }

        assert_eq!(
            obs.observe(1762330020, &[600, 0], -195239, 534, 1102101691356476042, 3010)
                .unwrap(),
            (
                vec![-9376145186196, -9376262324127],
                vec![
                    U256::from_str("577118894323923370143045372175062197936171").unwrap(),
                    U256::from_str("577118894323923370242147069684646219844755").unwrap()
                ]
            )
        );
    }
}
