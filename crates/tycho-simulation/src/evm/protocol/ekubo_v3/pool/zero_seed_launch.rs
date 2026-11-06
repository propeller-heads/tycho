use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{Evm, EvmPoolKey, EvmTokenAmount},
    math::{
        swap::{amount_before_fee, compute_fee},
        tick::to_sqrt_ratio,
    },
    U256,
};
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{concentrated::ConcentratedPool, EkuboPool, EkuboPoolQuote};
use crate::{evm::protocol::ekubo_v3::state::EkuboV3State, protocol::errors::InvalidSnapshotError};

const MAX_DECAY_PRODUCT: u64 = ((88_722_835_u64 + 1) << 32) - 1;

/// Final tick-rate revision. The four u64s and two i32s are big-endian in the
/// extractor's 40-byte launch_config attribute. No wall-clock time is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZeroSeedLaunchPool {
    underlying: ConcentratedPool,
    trading_start: u64,
    decay_rate_x32: u64,
    initial_fee: u64,
    final_fee: u64,
    tick_lower: i32,
    tick_upper: i32,
    block_timestamp: u64,
}

pub(super) fn decayed_fee(initial: u64, final_fee: u64, rate: u64, elapsed: u64) -> u64 {
    if rate == 0 {
        return initial;
    }
    if elapsed > MAX_DECAY_PRODUCT / rate {
        return final_fee;
    }
    let magnitude = ((rate * elapsed) >> 32) as i32;
    let factor = to_sqrt_ratio::<Evm>(-magnitude).expect("guarded tick magnitude");
    let excess: U256 = (U256::from(initial - final_fee) * factor) >> 128;
    final_fee + excess.to::<u64>()
}

impl ZeroSeedLaunchPool {
    pub fn new(
        underlying: ConcentratedPool,
        config: &[u8],
        block_timestamp: u64,
    ) -> Result<Self, InvalidSnapshotError> {
        if config.len() != 40 || underlying.key().config.fee != 0 || block_timestamp == 0 {
            return Err(InvalidSnapshotError::ValueError(
                "invalid launch configuration or timestamp".into(),
            ));
        }
        let word = |offset| {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(&config[offset..offset + 8]);
            u64::from_be_bytes(bytes)
        };
        let pool = Self {
            underlying,
            trading_start: word(0),
            decay_rate_x32: word(8),
            initial_fee: word(16),
            final_fee: word(24),
            tick_lower: i32::from_be_bytes([config[32], config[33], config[34], config[35]]),
            tick_upper: i32::from_be_bytes([config[36], config[37], config[38], config[39]]),
            block_timestamp,
        };
        if pool.final_fee > pool.initial_fee || pool.tick_lower >= pool.tick_upper {
            return Err(InvalidSnapshotError::ValueError(
                "invalid launch schedule or bounds".into(),
            ));
        }
        let lower = to_sqrt_ratio::<Evm>(pool.tick_lower)
            .ok_or_else(|| InvalidSnapshotError::ValueError("invalid launch lower bound".into()))?;
        let upper = to_sqrt_ratio::<Evm>(pool.tick_upper)
            .ok_or_else(|| InvalidSnapshotError::ValueError("invalid launch upper bound".into()))?;
        if pool.sqrt_ratio() < lower || pool.sqrt_ratio() > upper {
            return Err(InvalidSnapshotError::ValueError(
                "launch price outside installed bounds".into(),
            ));
        }
        Ok(pool)
    }

    pub fn set_block_timestamp(&mut self, timestamp: u64) {
        self.block_timestamp = timestamp;
    }

    pub fn fee(&self) -> u64 {
        decayed_fee(
            self.initial_fee,
            self.final_fee,
            self.decay_rate_x32,
            self.block_timestamp
                .saturating_sub(self.trading_start),
        )
    }
}

impl EkuboPool for ZeroSeedLaunchPool {
    fn key(&self) -> EvmPoolKey {
        self.underlying.key()
    }
    fn sqrt_ratio(&self) -> U256 {
        self.underlying.sqrt_ratio()
    }
    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.underlying
            .set_sqrt_ratio(sqrt_ratio);
    }
    fn set_liquidity(&mut self, liquidity: u128) {
        self.underlying.set_liquidity(liquidity);
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        self.underlying
            .finish_transition(updated_attributes, deleted_attributes)
    }

    fn quote(
        &self,
        token_amount: EvmTokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<EkuboPoolQuote, SimulationError> {
        if self.block_timestamp == 0 || self.block_timestamp < self.trading_start {
            return Err(SimulationError::RecoverableError(
                "launch trading has not started at block timestamp".into(),
            ));
        }
        let increasing = (token_amount.amount < 0) != (token_amount.token == self.key().token1);
        let bound =
            to_sqrt_ratio::<Evm>(if increasing { self.tick_upper } else { self.tick_lower })
                .ok_or_else(|| SimulationError::FatalError("invalid launch bound".into()))?;
        let limit = sqrt_ratio_limit.map_or(bound, |limit| {
            if increasing {
                limit.min(bound)
            } else {
                limit.max(bound)
            }
        });
        let quote = self
            .underlying
            .quote(token_amount, Some(limit))?;
        if quote.calculated_amount > (1u128 << 127) {
            return Err(SimulationError::RecoverableError(
                "launch Core amount exceeds signed output limit".into(),
            ));
        }
        let calculated_amount = if token_amount.amount >= 0 {
            quote.calculated_amount - compute_fee::<Evm>(quote.calculated_amount, self.fee())
        } else {
            amount_before_fee::<Evm>(quote.calculated_amount, self.fee())
                .filter(|amount| *amount <= i128::MAX as u128)
                .ok_or_else(|| {
                    SimulationError::RecoverableError(
                        "launch exact-output fee exceeds signed input limit".into(),
                    )
                })?
        };
        let EkuboV3State::Concentrated(underlying) = quote.new_state else {
            return Err(SimulationError::FatalError("unexpected launch underlying state".into()));
        };
        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount,
            // Conservative frozen-source cold-router execution bound; provider
            // executor gas calibration is a separate acceptance check.
            gas: 1_900_000,
            new_state: Self { underlying, ..self.clone() }.into(),
        })
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        self.quote(EvmTokenAmount { token: token_in, amount: i128::MAX }, None)
            .map(|q| q.consumed_amount)
    }
}

#[cfg(test)]
mod tests {
    use ekubo_sdk::{
        chain::evm::{EvmConcentratedPoolKey, EvmConcentratedPoolState},
        quoting::{
            pools::concentrated::TickSpacing,
            types::{PoolConfig, Tick},
        },
    };
    use revm::primitives::address;
    use tycho_common::simulation::protocol_sim::{BlockContext, ProtocolSim};

    use super::*;

    // Local Core + forwarding-router vectors from evm-contracts
    // 1d74ed414a5a91100440b128ef07fbe15acf37a5, zero-seed-launch.json v8.
    const CASES: [(i32, i32, u128, u128, u128, u128, u128); 4] = [
        (
            -13798330,
            -9210880,
            1121810267768648385524834,
            1000000000000000000,
            932986871256060603263574,
            1001724835881546318,
            904056664383095550,
        ),
        (
            -41430970,
            -36843520,
            1120910338181752946,
            1000000,
            934484255350952965119991,
            1001729,
            904058,
        ),
        (
            9210880,
            13798330,
            1121810267768648385525007,
            1000000000000000000,
            932986871256060603067126,
            1001724835881546317,
            904056664383095550,
        ),
        (
            36843520,
            41430970,
            1120910338181752946,
            1000000,
            934483764936307894582149,
            1001728,
            904058,
        ),
    ];

    fn pool(i: usize) -> ZeroSeedLaunchPool {
        let (lower, upper, liquidity, ..) = CASES[i];
        let initial = if i < 2 { lower } else { upper };
        let key = EvmConcentratedPoolKey {
            token0: Address::ZERO,
            token1: address!("0000000000000000000000000000000000000100"),
            config: PoolConfig {
                extension: address!("517621Ea1bf51F31826819b118fa4dfEb4D198D2"),
                fee: 0,
                pool_type_config: TickSpacing(17990),
            },
        };
        let underlying = ConcentratedPool::new(
            key,
            EvmConcentratedPoolState {
                sqrt_ratio: to_sqrt_ratio::<Evm>(initial).unwrap(),
                liquidity: if i < 2 { liquidity } else { 0 },
                active_tick_index: Some(if i < 2 { 0 } else { 1 }),
            },
            initial,
            vec![
                Tick { index: lower, liquidity_delta: i128::try_from(liquidity).unwrap() },
                Tick { index: upper, liquidity_delta: -i128::try_from(liquidity).unwrap() },
            ],
        )
        .unwrap();
        let mut config = Vec::new();
        for word in [1000u64, 68913100933, 922337203685477580, 55340232221128654] {
            config.extend_from_slice(&word.to_be_bytes());
        }
        config.extend_from_slice(&lower.to_be_bytes());
        config.extend_from_slice(&upper.to_be_bytes());
        ZeroSeedLaunchPool::new(underlying, &config, 1000).unwrap()
    }

    #[test]
    fn frozen_sequential_trades() {
        for (i, &(_, _, _, input, bought, paid, sold)) in CASES.iter().enumerate() {
            let mut state: EkuboV3State = pool(i).into();
            let key = state.key();
            let (launch_token, quote_token) =
                if i < 2 { (key.token0, key.token1) } else { (key.token1, key.token0) };
            assert_eq!(
                state
                    .quote(EvmTokenAmount { token: launch_token, amount: 1 }, None)
                    .unwrap()
                    .consumed_amount,
                0
            );
            for (token, amount, expected) in [
                (quote_token, i128::try_from(input).unwrap(), bought),
                (launch_token, -i128::try_from(bought).unwrap(), paid),
                (launch_token, i128::try_from(bought).unwrap(), sold),
            ] {
                let before = state.clone();
                let result = state
                    .quote(EvmTokenAmount { token, amount }, None)
                    .unwrap();
                assert_eq!(state, before, "quote must be pure");
                assert_eq!(result.consumed_amount, amount, "case {i}");
                assert_eq!(result.calculated_amount, expected, "case {i}, amount {amount}");
                state = result.new_state;
            }
        }
    }

    #[test]
    fn timestamps_partial_fills_and_maximum_fee() {
        for i in 0..4 {
            let mut pool = pool(i);
            let quote_token = if i < 2 { pool.key().token1 } else { pool.key().token0 };
            pool.set_block_timestamp(999);
            assert!(pool
                .quote(EvmTokenAmount { token: quote_token, amount: 1 }, None)
                .is_err());
            let mut state: EkuboV3State = pool.into();
            assert!(state.apply_block(&BlockContext::new(42, 1000)));
            let at_start = ProtocolSim::fee(&state);
            let encoded = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<EkuboV3State>(&encoded).unwrap(), state);
            state.apply_block(&BlockContext::new(43, 186400));
            assert!(ProtocolSim::fee(&state) < at_start);
            state.apply_block(&BlockContext::new(42, 1000));
            assert_eq!(ProtocolSim::fee(&state), at_start, "reorg timestamp must restore fee");
            let EkuboV3State::ZeroSeedLaunch(mut pool) = state else { panic!() };
            pool.initial_fee = u64::MAX;
            pool.final_fee = 0;
            let dust = pool
                .quote(EvmTokenAmount { token: quote_token, amount: 1 }, None)
                .unwrap();
            assert_eq!(dust.calculated_amount, 0);
            let full = pool
                .quote(EvmTokenAmount { token: quote_token, amount: i128::MAX }, None)
                .unwrap();
            assert!(full.consumed_amount < i128::MAX);
            assert_eq!(
                full.new_state.sqrt_ratio(),
                to_sqrt_ratio::<Evm>(if i < 2 { pool.tick_upper } else { pool.tick_lower })
                    .unwrap()
            );
        }
    }

    #[test]
    fn rejects_invalid_configuration() {
        let p = pool(0);
        assert!(ZeroSeedLaunchPool::new(p.underlying.clone(), &[0; 39], 1000).is_err());
        assert!(ZeroSeedLaunchPool::new(p.underlying.clone(), &[0; 40], 0).is_err());
        assert!(ZeroSeedLaunchPool::new(p.underlying, &[0; 40], 1000).is_err());
    }

    #[test]
    fn frozen_fee_curve_digests_and_saturation() {
        use sha2::{Digest, Sha256};
        let curves = [
            (
                9223372036854775808,
                55340232221128654,
                68913100933,
                3600,
                1,
                "c3de4d91d476acc61f73455efe7fb8c750b89de3759a0c500bbde0bdf9bcd062",
            ),
            (
                9223372036854775808,
                55340232221128654,
                68913100933,
                2592000,
                60,
                "b2c022f5cbbc90cedca9834bcf64f636b97c45dee08c730ab4dbf17d8d5acc74",
            ),
            (
                922337203685477580,
                55340232221128654,
                9923486534471,
                3600,
                1,
                "c4db35cd45f48222463cb9a940c506599715162db6c37b01428331ff5263bdc6",
            ),
            (
                18428297329635842064,
                0,
                8589934592,
                2592000,
                3600,
                "8fdefff5fe4cfb98c43bbbf6ed6f0b9a0ae398f1f810ffea0522d5504e1321da",
            ),
            (
                u64::MAX,
                0,
                1073741824,
                3600,
                1,
                "11ff0a905ab22707579e714aba31b895ed3234788a0dab10b0daa0881a9ccf36",
            ),
        ];
        for (initial, floor, rate, last, step, expected) in curves {
            let mut digest = [0u8; 32];
            let mut previous = initial;
            for elapsed in (0..=last).step_by(step) {
                let fee = decayed_fee(initial, floor, rate, elapsed);
                assert!((floor..=previous).contains(&fee));
                previous = fee;
                digest = Sha256::digest([digest.as_slice(), &fee.to_be_bytes()].concat()).into();
            }
            assert_eq!(hex::encode(digest), expected);
        }
        assert_eq!(decayed_fee(u64::MAX, 0, 0, u64::MAX), u64::MAX);
        assert_eq!(decayed_fee(u64::MAX, 0, 1, (1u64 << 32) - 1), u64::MAX);
        assert_eq!(decayed_fee(u64::MAX, 0, 1, MAX_DECAY_PRODUCT), 1);
        assert_eq!(decayed_fee(u64::MAX, 0, 1, MAX_DECAY_PRODUCT + 1), 0);
        assert_eq!(decayed_fee(u64::MAX, 123, u64::MAX, u64::MAX), 123);
    }
}
