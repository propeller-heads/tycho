use std::{
    collections::{HashMap, HashSet},
    ops::Neg,
};

use alloy::primitives::aliases::B32;
use ekubo_sdk::{
    chain::evm::{
        EvmBoostedFeesConcentratedPoolKey, EvmConcentratedPoolConfig, EvmConcentratedPoolKey,
        EvmConcentratedPoolState, EvmFullRangePoolConfig, EvmFullRangePoolKey,
        EvmFullRangePoolState, EvmMevCapturePoolConfig, EvmMevCapturePoolKey, EvmOraclePoolConfig,
        EvmOraclePoolKey, EvmPoolTypeConfig, EvmStableswapPoolConfig, EvmStableswapPoolKey,
        EvmStableswapPoolState, EvmTwammPoolConfig, EvmTwammPoolKey, EvmTwammPoolState,
        EVM_MIN_SQRT_RATIO,
    },
    quoting::{
        pools::{
            concentrated::TickSpacing, full_range::FullRangePoolTypeConfig,
            stableswap::StableswapPoolTypeConfig,
        },
        types::{Tick, TimeRateDelta},
    },
    U256,
};
use num_bigint::BigUint;
use revm::primitives::{address, Address};
use rstest::*;
use rstest_reuse::template;
use tycho_common::{
    models::{protocol::ProtocolComponent, token::Token, Chain},
    Bytes,
};

use super::{pool::concentrated::ConcentratedPool, state::EkuboV3State};
use crate::evm::protocol::ekubo_v3::{
    addresses::{
        BOOSTED_FEES_CONCENTRATED_ADDRESS, MEV_CAPTURE_ADDRESS, ORACLE_ADDRESS, TWAMM_ADDRESS,
    },
    pool::{
        boosted_fees::BoostedFeesPool, full_range::FullRangePool, mev_capture::MevCapturePool,
        oracle::OraclePool, stableswap::StableswapPool, twamm::TwammPool, EkuboPool as _,
    },
};

pub const TEST_TIMESTAMP: u64 = 1_000;

const TOKEN0: Address = Address::ZERO;
const TOKEN1: Address = address!("0x0000000000000000000000000000000000000001");

pub struct TestCase {
    pub component: ProtocolComponent,

    pub state_before_transition: EkuboV3State,
    pub state_after_transition: EkuboV3State,

    pub required_attributes: HashSet<String>,
    pub transition_attributes: HashMap<String, Bytes>,
    pub state_attributes: HashMap<String, Bytes>,

    pub swap_token0: (BigUint, BigUint),
    pub expected_limit_token0: BigUint,
}

impl TestCase {
    pub fn token0(&self) -> Token {
        Token {
            address: self
                .state_after_transition
                .key()
                .token0
                .into_array()
                .into(),
            decimals: 18,
            symbol: "TOKEN0".to_string(),
            gas: vec![Some(0)],
            chain: Chain::Ethereum,
            tax: 0,
            quality: 100,
        }
    }

    pub fn token1(&self) -> Token {
        Token {
            address: self
                .state_after_transition
                .key()
                .token1
                .into_array()
                .into(),
            decimals: 18,
            symbol: "TOKEN1".to_string(),
            gas: vec![Some(0)],
            chain: Chain::Ethereum,
            tax: 0,
            quality: 100,
        }
    }
}

#[fixture]
pub fn concentrated() -> TestCase {
    const POOL_KEY: EvmConcentratedPoolKey = EvmConcentratedPoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmConcentratedPoolConfig {
            fee: 0,
            pool_type_config: TickSpacing(10),
            extension: Address::ZERO,
        },
    };

    const LOWER_TICK: Tick = Tick { index: -10, liquidity_delta: 100_000_000 };
    const UPPER_TICK: Tick =
        Tick { index: -LOWER_TICK.index, liquidity_delta: -LOWER_TICK.liquidity_delta };

    const TICK_INDEX_BETWEEN: i32 = 0;
    const SQRT_RATIO_BETWEEN: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY_BETWEEN: u128 = LOWER_TICK.liquidity_delta as u128;

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::Concentrated(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::Concentrated(
            ConcentratedPool::new(
                POOL_KEY,
                EvmConcentratedPoolState {
                    sqrt_ratio: SQRT_RATIO_BETWEEN,
                    liquidity: 0,
                    active_tick_index: None,
                },
                TICK_INDEX_BETWEEN,
                vec![],
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::Concentrated(
            ConcentratedPool::new(
                POOL_KEY,
                EvmConcentratedPoolState {
                    sqrt_ratio: SQRT_RATIO_BETWEEN,
                    liquidity: LIQUIDITY_BETWEEN,
                    active_tick_index: Some(0),
                },
                TICK_INDEX_BETWEEN,
                vec![LOWER_TICK, UPPER_TICK],
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "tick".to_string(),
        ]
        .into(),
        transition_attributes: [
            ("liquidity".to_string(), LIQUIDITY_BETWEEN.to_be_bytes().into()),
            (
                format!("tick/{}", LOWER_TICK.index),
                LOWER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("tick/{}", UPPER_TICK.index),
                UPPER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        state_attributes: [
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
            (
                "sqrt_ratio".to_string(),
                SQRT_RATIO_BETWEEN
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("tick".to_string(), TICK_INDEX_BETWEEN.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 99_u8.into()),
        expected_limit_token0: 497_u16.into(),
    }
}

#[fixture]
pub fn full_range() -> TestCase {
    const POOL_KEY: EvmFullRangePoolKey = EvmFullRangePoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmFullRangePoolConfig {
            fee: 0,
            pool_type_config: FullRangePoolTypeConfig,
            extension: Address::ZERO,
        },
    };

    const SQRT_RATIO: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY: u128 = 100_000_000;

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::FullRange(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::FullRange(
            FullRangePool::new(
                POOL_KEY,
                EvmFullRangePoolState { sqrt_ratio: EVM_MIN_SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::FullRange(
            FullRangePool::new(
                POOL_KEY,
                EvmFullRangePoolState { sqrt_ratio: SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
        ]
        .into(),
        transition_attributes: [("sqrt_ratio".to_string(), SQRT_RATIO.to_be_bytes_vec().into())]
            .into(),
        state_attributes: [
            (
                "sqrt_ratio".to_string(),
                EVM_MIN_SQRT_RATIO
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 99_u8.into()),
        expected_limit_token0: 1844629699405272373941016055_u128.into(),
    }
}

#[fixture]
pub fn stableswap() -> TestCase {
    const POOL_KEY: EvmStableswapPoolKey = EvmStableswapPoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmStableswapPoolConfig {
            fee: 0,
            pool_type_config: StableswapPoolTypeConfig {
                amplification_factor: 5,
                center_tick: -16,
            },
            extension: Address::ZERO,
        },
    };

    const SQRT_RATIO: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY: u128 = 100_000_000;

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::Stableswap(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::Stableswap(
            StableswapPool::new(
                POOL_KEY,
                EvmStableswapPoolState { sqrt_ratio: EVM_MIN_SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::Stableswap(
            StableswapPool::new(
                POOL_KEY,
                EvmStableswapPoolState { sqrt_ratio: SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
        ]
        .into(),
        transition_attributes: [("sqrt_ratio".to_string(), SQRT_RATIO.to_be_bytes_vec().into())]
            .into(),
        state_attributes: [
            (
                "sqrt_ratio".to_string(),
                EVM_MIN_SQRT_RATIO
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 99_u8.into()),
        expected_limit_token0: 300002779_u128.into(),
    }
}

pub fn oracle() -> TestCase {
    const POOL_KEY: EvmOraclePoolKey = EvmOraclePoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmOraclePoolConfig {
            fee: 0,
            pool_type_config: FullRangePoolTypeConfig,
            extension: ORACLE_ADDRESS,
        },
    };

    const SQRT_RATIO: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY: u128 = 100_000_000;

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::FullRange(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::Oracle(
            OraclePool::new(
                POOL_KEY,
                EvmFullRangePoolState { sqrt_ratio: EVM_MIN_SQRT_RATIO, liquidity: 0 },
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::Oracle(
            OraclePool::new(
                POOL_KEY,
                EvmFullRangePoolState { sqrt_ratio: SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
        ]
        .into(),
        transition_attributes: [
            ("sqrt_ratio".to_string(), SQRT_RATIO.to_be_bytes_vec().into()),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
        ]
        .into(),
        state_attributes: [
            (
                "sqrt_ratio".to_string(),
                EVM_MIN_SQRT_RATIO
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 99_u8.into()),
        expected_limit_token0: 1844629699405272373941016055_u128.into(),
    }
}

pub fn twamm() -> TestCase {
    const POOL_KEY: EvmTwammPoolKey = EvmTwammPoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmTwammPoolConfig {
            fee: 0,
            pool_type_config: FullRangePoolTypeConfig,
            extension: TWAMM_ADDRESS,
        },
    };

    const SQRT_RATIO: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY: u128 = 100_000_000;
    const LAST_EXECUTION_TIME: u64 = 10;
    const TOKEN0_SALE_RATE: u128 = 10 << 32;
    const TOKEN1_SALE_RATE: u128 = TOKEN0_SALE_RATE / 2;
    const FIRST_ORDER_START_TIME: u64 = LAST_EXECUTION_TIME;
    const FIRST_ORDER_END_TIME: u64 = TEST_TIMESTAMP / 2;
    const SECOND_ORDER_START_TIME: u64 = u64::midpoint(FIRST_ORDER_END_TIME, SECOND_ORDER_END_TIME);
    const SECOND_ORDER_END_TIME: u64 = TEST_TIMESTAMP;

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::FullRange(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::Twamm(
            TwammPool::new(
                POOL_KEY,
                EvmTwammPoolState {
                    full_range_pool_state: EvmFullRangePoolState {
                        sqrt_ratio: EVM_MIN_SQRT_RATIO,
                        liquidity: 0,
                    },
                    token0_sale_rate: 0,
                    token1_sale_rate: 0,
                    last_execution_time: 0,
                },
                vec![
                    TimeRateDelta {
                        time: FIRST_ORDER_START_TIME,
                        rate_delta0: TOKEN0_SALE_RATE as i128,
                        rate_delta1: TOKEN1_SALE_RATE as i128,
                    },
                    TimeRateDelta {
                        time: FIRST_ORDER_END_TIME,
                        rate_delta0: (TOKEN0_SALE_RATE as i128).neg(),
                        rate_delta1: (TOKEN1_SALE_RATE as i128).neg(),
                    },
                ],
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::Twamm(
            TwammPool::new(
                POOL_KEY,
                EvmTwammPoolState {
                    full_range_pool_state: EvmFullRangePoolState {
                        sqrt_ratio: SQRT_RATIO,
                        liquidity: LIQUIDITY,
                    },
                    token0_sale_rate: TOKEN0_SALE_RATE,
                    token1_sale_rate: TOKEN1_SALE_RATE,
                    last_execution_time: LAST_EXECUTION_TIME,
                },
                vec![
                    TimeRateDelta {
                        time: FIRST_ORDER_END_TIME,
                        rate_delta0: (TOKEN0_SALE_RATE as i128).neg(),
                        rate_delta1: (TOKEN1_SALE_RATE as i128).neg(),
                    },
                    TimeRateDelta {
                        time: SECOND_ORDER_START_TIME,
                        rate_delta0: TOKEN0_SALE_RATE as i128,
                        rate_delta1: TOKEN1_SALE_RATE as i128,
                    },
                    TimeRateDelta {
                        time: SECOND_ORDER_END_TIME,
                        rate_delta0: (TOKEN0_SALE_RATE as i128).neg(),
                        rate_delta1: (TOKEN1_SALE_RATE as i128).neg(),
                    },
                ],
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "last_time".to_string(),
            "rate_token0".to_string(),
            "rate_token1".to_string(),
            format!("rate_delta/token0/{FIRST_ORDER_START_TIME}"),
            format!("rate_delta/token1/{FIRST_ORDER_START_TIME}"),
            format!("rate_delta/token0/{FIRST_ORDER_END_TIME}"),
            format!("rate_delta/token1/{FIRST_ORDER_END_TIME}"),
        ]
        .into(),
        transition_attributes: [
            ("sqrt_ratio".to_string(), SQRT_RATIO.to_be_bytes_vec().into()),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
            ("rate_token0".to_string(), TOKEN0_SALE_RATE.to_be_bytes().into()),
            ("rate_token1".to_string(), TOKEN1_SALE_RATE.to_be_bytes().into()),
            ("last_time".to_string(), LAST_EXECUTION_TIME.to_be_bytes().into()),
            (
                format!("rate_delta/token0/{SECOND_ORDER_START_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{SECOND_ORDER_START_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token0/{SECOND_ORDER_END_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{SECOND_ORDER_END_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        state_attributes: [
            (
                "sqrt_ratio".to_string(),
                EVM_MIN_SQRT_RATIO
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
            ("rate_token0".to_string(), 0_u128.to_be_bytes().into()),
            ("rate_token1".to_string(), 0_u128.to_be_bytes().into()),
            ("last_time".to_string(), 0_u64.to_be_bytes().into()),
            (
                format!("rate_delta/token0/{FIRST_ORDER_START_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{FIRST_ORDER_START_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token0/{FIRST_ORDER_END_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{FIRST_ORDER_END_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        swap_token0: (100_000_000u64.into(), 49997225_u64.into()),
        expected_limit_token0: 1844629699405272373941012355_u128.into(),
    }
}

pub fn boosted_fees() -> TestCase {
    const POOL_KEY: EvmBoostedFeesConcentratedPoolKey = EvmBoostedFeesConcentratedPoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmConcentratedPoolConfig {
            fee: u64::MAX / 10,
            pool_type_config: TickSpacing(10),
            extension: BOOSTED_FEES_CONCENTRATED_ADDRESS,
        },
    };

    const LOWER_TICK: Tick = Tick { index: -10, liquidity_delta: 100_000_000 };
    const UPPER_TICK: Tick =
        Tick { index: -LOWER_TICK.index, liquidity_delta: -LOWER_TICK.liquidity_delta };

    const TICK_INDEX_BETWEEN: i32 = 0;
    const SQRT_RATIO_BETWEEN: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY_BETWEEN: u128 = LOWER_TICK.liquidity_delta as u128;
    const LAST_DONATE_TIME: u64 = 10;
    const DONATE_RATE0: u128 = 10 << 32;
    const DONATE_RATE1: u128 = DONATE_RATE0 / 2;
    const FIRST_DONATE_START_TIME: u64 = LAST_DONATE_TIME;
    const FIRST_DONATE_END_TIME: u64 = TEST_TIMESTAMP / 2;
    const SECOND_DONATE_END_TIME: u64 = TEST_TIMESTAMP;
    const SECOND_DONATE_START_TIME: u64 =
        u64::midpoint(FIRST_DONATE_END_TIME, SECOND_DONATE_END_TIME);

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::Concentrated(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::BoostedFees(
            BoostedFeesPool::new(
                POOL_KEY,
                EvmConcentratedPoolState {
                    sqrt_ratio: SQRT_RATIO_BETWEEN,
                    liquidity: 0,
                    active_tick_index: None,
                },
                0,
                0,
                0,
                vec![
                    TimeRateDelta {
                        time: FIRST_DONATE_START_TIME,
                        rate_delta0: DONATE_RATE0 as i128,
                        rate_delta1: DONATE_RATE1 as i128,
                    },
                    TimeRateDelta {
                        time: FIRST_DONATE_END_TIME,
                        rate_delta0: (DONATE_RATE0 as i128).neg(),
                        rate_delta1: (DONATE_RATE1 as i128).neg(),
                    },
                ],
                vec![],
                TICK_INDEX_BETWEEN,
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::BoostedFees(
            BoostedFeesPool::new(
                POOL_KEY,
                EvmConcentratedPoolState {
                    sqrt_ratio: SQRT_RATIO_BETWEEN,
                    liquidity: LIQUIDITY_BETWEEN,
                    active_tick_index: Some(0),
                },
                DONATE_RATE0,
                DONATE_RATE1,
                LAST_DONATE_TIME,
                vec![
                    TimeRateDelta {
                        time: FIRST_DONATE_END_TIME,
                        rate_delta0: (DONATE_RATE0 as i128).neg(),
                        rate_delta1: (DONATE_RATE1 as i128).neg(),
                    },
                    TimeRateDelta {
                        time: SECOND_DONATE_START_TIME,
                        rate_delta0: DONATE_RATE0 as i128,
                        rate_delta1: DONATE_RATE1 as i128,
                    },
                    TimeRateDelta {
                        time: SECOND_DONATE_END_TIME,
                        rate_delta0: (DONATE_RATE0 as i128).neg(),
                        rate_delta1: (DONATE_RATE1 as i128).neg(),
                    },
                ],
                vec![LOWER_TICK, UPPER_TICK],
                TICK_INDEX_BETWEEN,
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "tick".to_string(),
            "last_time".to_string(),
            "rate_token0".to_string(),
            "rate_token1".to_string(),
            format!("rate_delta/token0/{FIRST_DONATE_START_TIME}"),
            format!("rate_delta/token1/{FIRST_DONATE_START_TIME}"),
            format!("rate_delta/token0/{FIRST_DONATE_END_TIME}"),
            format!("rate_delta/token1/{FIRST_DONATE_END_TIME}"),
        ]
        .into(),
        transition_attributes: [
            ("liquidity".to_string(), LIQUIDITY_BETWEEN.to_be_bytes().into()),
            (
                format!("tick/{}", LOWER_TICK.index),
                LOWER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("tick/{}", UPPER_TICK.index),
                UPPER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
            ("rate_token0".to_string(), DONATE_RATE0.to_be_bytes().into()),
            ("rate_token1".to_string(), DONATE_RATE1.to_be_bytes().into()),
            ("last_time".to_string(), LAST_DONATE_TIME.to_be_bytes().into()),
            (
                format!("rate_delta/token0/{SECOND_DONATE_START_TIME}"),
                (DONATE_RATE0 as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{SECOND_DONATE_START_TIME}"),
                (DONATE_RATE1 as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token0/{SECOND_DONATE_END_TIME}"),
                (DONATE_RATE0 as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{SECOND_DONATE_END_TIME}"),
                (DONATE_RATE1 as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        state_attributes: [
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
            (
                "sqrt_ratio".to_string(),
                SQRT_RATIO_BETWEEN
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("tick".to_string(), TICK_INDEX_BETWEEN.to_be_bytes().into()),
            ("rate_token0".to_string(), 0_u128.to_be_bytes().into()),
            ("rate_token1".to_string(), 0_u128.to_be_bytes().into()),
            ("last_time".to_string(), 0_u64.to_be_bytes().into()),
            (
                format!("rate_delta/token0/{FIRST_DONATE_START_TIME}"),
                (DONATE_RATE0 as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{FIRST_DONATE_START_TIME}"),
                (DONATE_RATE1 as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token0/{FIRST_DONATE_END_TIME}"),
                (DONATE_RATE0 as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("rate_delta/token1/{FIRST_DONATE_END_TIME}"),
                (DONATE_RATE1 as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        swap_token0: (100_u8.into(), 89_u8.into()),
        expected_limit_token0: 553_u16.into(),
    }
}

#[fixture]
pub fn mev_capture() -> TestCase {
    const POOL_KEY: EvmMevCapturePoolKey = EvmMevCapturePoolKey {
        token0: TOKEN0,
        token1: TOKEN1,
        config: EvmMevCapturePoolConfig {
            fee: u64::MAX / 10,
            pool_type_config: TickSpacing(10),
            extension: MEV_CAPTURE_ADDRESS,
        },
    };

    const LOWER_TICK: Tick = Tick { index: -10, liquidity_delta: 100_000_000 };
    const UPPER_TICK: Tick =
        Tick { index: -LOWER_TICK.index, liquidity_delta: -LOWER_TICK.liquidity_delta };

    const TICK_INDEX_BETWEEN: i32 = 0;
    const SQRT_RATIO_BETWEEN: U256 = U256::from_limbs([0, 0, 1, 0]);
    const LIQUIDITY_BETWEEN: u128 = LOWER_TICK.liquidity_delta as u128;

    TestCase {
        component: component([
            ("token0".to_string(), POOL_KEY.token0.into_array().into()),
            ("token1".to_string(), POOL_KEY.token1.into_array().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            (
                "pool_type_config".to_string(),
                B32::from(EvmPoolTypeConfig::Concentrated(POOL_KEY.config.pool_type_config))
                    .0
                    .into(),
            ),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .into_array()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboV3State::MevCapture(
            MevCapturePool::new(
                POOL_KEY,
                TICK_INDEX_BETWEEN,
                EvmConcentratedPoolState {
                    sqrt_ratio: SQRT_RATIO_BETWEEN,
                    liquidity: 0,
                    active_tick_index: None,
                },
                vec![],
            )
            .unwrap(),
        ),
        state_after_transition: EkuboV3State::MevCapture(
            MevCapturePool::new(
                POOL_KEY,
                TICK_INDEX_BETWEEN,
                EvmConcentratedPoolState {
                    sqrt_ratio: SQRT_RATIO_BETWEEN,
                    liquidity: LIQUIDITY_BETWEEN,
                    active_tick_index: Some(0),
                },
                vec![LOWER_TICK, UPPER_TICK],
            )
            .unwrap(),
        ),
        required_attributes: [
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "pool_type_config".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "tick".to_string(),
        ]
        .into(),
        transition_attributes: [
            ("liquidity".to_string(), LIQUIDITY_BETWEEN.to_be_bytes().into()),
            (
                format!("tick/{}", LOWER_TICK.index),
                LOWER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("tick/{}", UPPER_TICK.index),
                UPPER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        state_attributes: [
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
            (
                "sqrt_ratio".to_string(),
                SQRT_RATIO_BETWEEN
                    .to_be_bytes_vec()
                    .into(),
            ),
            ("tick".to_string(), TICK_INDEX_BETWEEN.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 86_u8.into()),
        expected_limit_token0: 553_u16.into(),
    }
}

#[template]
#[rstest]
#[case::concentrated(concentrated())]
#[case::full_range(full_range())]
#[case::stableswap(stableswap())]
#[case::oracle(oracle())]
#[case::twamm(twamm())]
#[case::mev_capture(mev_capture())]
#[case::boosted_fees(boosted_fees())]
pub fn all_cases(#[case] case: TestCase) {}

fn component<const N: usize>(static_attributes: [(String, Bytes); N]) -> ProtocolComponent {
    ProtocolComponent { static_attributes: static_attributes.into(), ..Default::default() }
}
