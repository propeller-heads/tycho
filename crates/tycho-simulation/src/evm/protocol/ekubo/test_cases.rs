use std::{
    collections::{HashMap, HashSet},
    ops::Neg,
};

use evm_ekubo_sdk::{
    math::{tick::MIN_SQRT_RATIO, uint::U256},
    quoting::{
        full_range_pool::FullRangePoolState,
        oracle_pool::OraclePoolState,
        twamm_pool::{TwammPoolState, TwammSaleRateDelta},
        types::{Config, NodeKey, Tick},
    },
};
use num_bigint::BigUint;
use rstest::*;
use rstest_reuse::template;
use tycho_common::{
    models::{protocol::ProtocolComponent, token::Token, Chain},
    Bytes,
};

use super::{
    pool::{base::BasePool, EkuboPool},
    state::EkuboState,
};
use crate::evm::protocol::ekubo::pool::{
    full_range::FullRangePool, mev_resist::MevResistPool, oracle::OraclePool, twamm::TwammPool,
};

pub struct TestCase {
    pub component: ProtocolComponent,

    pub state_before_transition: EkuboState,
    pub state_after_transition: EkuboState,

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
                .to_big_endian()
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
                .to_big_endian()
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
pub fn base() -> TestCase {
    const POOL_KEY: NodeKey = NodeKey {
        token0: U256([1, 0, 0, 0]),
        token1: U256([2, 0, 0, 0]),
        config: Config { fee: 0, tick_spacing: 10, extension: U256::zero() },
    };

    const LOWER_TICK: Tick = Tick { index: -10, liquidity_delta: 100_000_000 };
    const UPPER_TICK: Tick =
        Tick { index: -LOWER_TICK.index, liquidity_delta: -LOWER_TICK.liquidity_delta };

    const TICK_INDEX_BETWEEN: i32 = 0;
    const SQRT_RATIO_BETWEEN: U256 = U256([0, 0, 1, 0]);
    const LIQUIDITY_BETWEEN: u128 = LOWER_TICK.liquidity_delta as u128;

    TestCase {
        component: component([
            ("extension_id".to_string(), 1_i32.to_be_bytes().into()), // Base pool
            ("token0".to_string(), POOL_KEY.token0.to_big_endian().into()),
            ("token1".to_string(), POOL_KEY.token1.to_big_endian().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            ("tick_spacing".to_string(), POOL_KEY.config.tick_spacing.into()),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .to_big_endian()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboState::Base(
            BasePool::new(POOL_KEY, vec![], SQRT_RATIO_BETWEEN, 0, TICK_INDEX_BETWEEN).unwrap(),
        ),
        state_after_transition: EkuboState::Base(
            BasePool::new(
                POOL_KEY,
                vec![LOWER_TICK, UPPER_TICK],
                SQRT_RATIO_BETWEEN,
                LIQUIDITY_BETWEEN,
                TICK_INDEX_BETWEEN,
            )
            .unwrap(),
        ),
        required_attributes: [
            "extension_id".to_string(),
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "tick_spacing".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "tick".to_string(),
        ]
        .into(),
        transition_attributes: [
            ("liquidity".to_string(), LIQUIDITY_BETWEEN.to_be_bytes().into()),
            (
                format!("ticks/{}", LOWER_TICK.index),
                LOWER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("ticks/{}", UPPER_TICK.index),
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
                    .to_big_endian()
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
    const POOL_KEY: NodeKey = NodeKey {
        token0: U256([1, 0, 0, 0]),
        token1: U256([2, 0, 0, 0]),
        config: Config { fee: 0, tick_spacing: 0, extension: U256::zero() },
    };

    const SQRT_RATIO: U256 = U256([0, 0, 1, 0]);
    const LIQUIDITY: u128 = 100_000_000;

    TestCase {
        component: component([
            ("extension_id".to_string(), 1_i32.to_be_bytes().into()), // Base pool
            ("token0".to_string(), POOL_KEY.token0.to_big_endian().into()),
            ("token1".to_string(), POOL_KEY.token1.to_big_endian().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            ("tick_spacing".to_string(), POOL_KEY.config.tick_spacing.into()),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .to_big_endian()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboState::FullRange(
            FullRangePool::new(
                POOL_KEY,
                FullRangePoolState { sqrt_ratio: MIN_SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        state_after_transition: EkuboState::FullRange(
            FullRangePool::new(
                POOL_KEY,
                FullRangePoolState { sqrt_ratio: SQRT_RATIO, liquidity: LIQUIDITY },
            )
            .unwrap(),
        ),
        required_attributes: [
            "extension_id".to_string(),
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "tick_spacing".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
        ]
        .into(),
        transition_attributes: [("sqrt_ratio".to_string(), SQRT_RATIO.to_big_endian().into())]
            .into(),
        state_attributes: [
            ("sqrt_ratio".to_string(), MIN_SQRT_RATIO.to_big_endian().into()),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 99_u8.into()),
        expected_limit_token0: 1844629699405272373941016055_u128.into(),
    }
}

pub fn oracle() -> TestCase {
    const POOL_KEY: NodeKey = NodeKey {
        token0: U256([1, 0, 0, 0]),
        token1: U256([2, 0, 0, 0]),
        config: Config { fee: 0, tick_spacing: 0, extension: U256::one() },
    };

    const SQRT_RATIO: U256 = U256([0, 0, 1, 0]);
    const LIQUIDITY: u128 = 100_000_000;

    TestCase {
        component: component([
            ("extension_id".to_string(), 2_i32.to_be_bytes().into()), // Oracle pool
            ("token0".to_string(), POOL_KEY.token0.to_big_endian().into()),
            ("token1".to_string(), POOL_KEY.token1.to_big_endian().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            ("tick_spacing".to_string(), POOL_KEY.config.tick_spacing.into()),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .to_big_endian()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboState::Oracle(
            OraclePool::new(
                &POOL_KEY,
                OraclePoolState {
                    full_range_pool_state: FullRangePoolState {
                        sqrt_ratio: MIN_SQRT_RATIO,
                        liquidity: 0,
                    },
                    last_snapshot_time: 0,
                },
            )
            .unwrap(),
        ),
        state_after_transition: EkuboState::Oracle(
            OraclePool::new(
                &POOL_KEY,
                OraclePoolState {
                    full_range_pool_state: FullRangePoolState {
                        sqrt_ratio: SQRT_RATIO,
                        liquidity: LIQUIDITY,
                    },
                    last_snapshot_time: 0,
                },
            )
            .unwrap(),
        ),
        required_attributes: [
            "extension_id".to_string(),
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "tick_spacing".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
        ]
        .into(),
        transition_attributes: [
            ("sqrt_ratio".to_string(), SQRT_RATIO.to_big_endian().into()),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
        ]
        .into(),
        state_attributes: [
            ("sqrt_ratio".to_string(), MIN_SQRT_RATIO.to_big_endian().into()),
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
        ]
        .into(),
        swap_token0: (100_u8.into(), 99_u8.into()),
        expected_limit_token0: 1844629699405272373941016055_u128.into(),
    }
}

pub const TEST_TIMESTAMP: u64 = 1_000;

pub fn twamm() -> TestCase {
    const POOL_KEY: NodeKey = NodeKey {
        token0: U256([1, 0, 0, 0]),
        token1: U256([2, 0, 0, 0]),
        config: Config { fee: 0, tick_spacing: 0, extension: U256::one() },
    };

    const SQRT_RATIO: U256 = U256([0, 0, 1, 0]);
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
            ("extension_id".to_string(), 3_i32.to_be_bytes().into()), // TWAMM pool
            ("token0".to_string(), POOL_KEY.token0.to_big_endian().into()),
            ("token1".to_string(), POOL_KEY.token1.to_big_endian().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            ("tick_spacing".to_string(), POOL_KEY.config.tick_spacing.into()),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .to_big_endian()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboState::Twamm(
            TwammPool::new(
                &POOL_KEY,
                TwammPoolState {
                    full_range_pool_state: FullRangePoolState {
                        sqrt_ratio: MIN_SQRT_RATIO,
                        liquidity: 0,
                    },
                    token0_sale_rate: 0,
                    token1_sale_rate: 0,
                    last_execution_time: 0,
                },
                vec![
                    TwammSaleRateDelta {
                        time: FIRST_ORDER_START_TIME,
                        sale_rate_delta0: TOKEN0_SALE_RATE as i128,
                        sale_rate_delta1: TOKEN1_SALE_RATE as i128,
                    },
                    TwammSaleRateDelta {
                        time: FIRST_ORDER_END_TIME,
                        sale_rate_delta0: (TOKEN0_SALE_RATE as i128).neg(),
                        sale_rate_delta1: (TOKEN1_SALE_RATE as i128).neg(),
                    },
                ],
            )
            .unwrap(),
        ),
        state_after_transition: EkuboState::Twamm(
            TwammPool::new(
                &POOL_KEY,
                TwammPoolState {
                    full_range_pool_state: FullRangePoolState {
                        sqrt_ratio: SQRT_RATIO,
                        liquidity: LIQUIDITY,
                    },
                    token0_sale_rate: TOKEN0_SALE_RATE,
                    token1_sale_rate: TOKEN1_SALE_RATE,
                    last_execution_time: LAST_EXECUTION_TIME,
                },
                vec![
                    TwammSaleRateDelta {
                        time: FIRST_ORDER_END_TIME,
                        sale_rate_delta0: (TOKEN0_SALE_RATE as i128).neg(),
                        sale_rate_delta1: (TOKEN1_SALE_RATE as i128).neg(),
                    },
                    TwammSaleRateDelta {
                        time: SECOND_ORDER_START_TIME,
                        sale_rate_delta0: TOKEN0_SALE_RATE as i128,
                        sale_rate_delta1: TOKEN1_SALE_RATE as i128,
                    },
                    TwammSaleRateDelta {
                        time: SECOND_ORDER_END_TIME,
                        sale_rate_delta0: (TOKEN0_SALE_RATE as i128).neg(),
                        sale_rate_delta1: (TOKEN1_SALE_RATE as i128).neg(),
                    },
                ],
            )
            .unwrap(),
        ),
        required_attributes: [
            "extension_id".to_string(),
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "tick_spacing".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "last_execution_time".to_string(),
            "token0_sale_rate".to_string(),
            "token1_sale_rate".to_string(),
            format!("orders/token0/{FIRST_ORDER_START_TIME}"),
            format!("orders/token1/{FIRST_ORDER_START_TIME}"),
            format!("orders/token0/{FIRST_ORDER_END_TIME}"),
            format!("orders/token1/{FIRST_ORDER_END_TIME}"),
        ]
        .into(),
        transition_attributes: [
            ("sqrt_ratio".to_string(), SQRT_RATIO.to_big_endian().into()),
            ("liquidity".to_string(), LIQUIDITY.to_be_bytes().into()),
            ("token0_sale_rate".to_string(), TOKEN0_SALE_RATE.to_be_bytes().into()),
            ("token1_sale_rate".to_string(), TOKEN1_SALE_RATE.to_be_bytes().into()),
            ("last_execution_time".to_string(), LAST_EXECUTION_TIME.to_be_bytes().into()),
            (
                format!("orders/token0/{SECOND_ORDER_START_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("orders/token1/{SECOND_ORDER_START_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("orders/token0/{SECOND_ORDER_END_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("orders/token1/{SECOND_ORDER_END_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
        ]
        .into(),
        state_attributes: [
            ("sqrt_ratio".to_string(), MIN_SQRT_RATIO.to_big_endian().into()),
            ("liquidity".to_string(), 0_u128.to_be_bytes().into()),
            ("token0_sale_rate".to_string(), 0_u128.to_be_bytes().into()),
            ("token1_sale_rate".to_string(), 0_u128.to_be_bytes().into()),
            ("last_execution_time".to_string(), 0_u64.to_be_bytes().into()),
            (
                format!("orders/token0/{FIRST_ORDER_START_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("orders/token1/{FIRST_ORDER_START_TIME}"),
                (TOKEN1_SALE_RATE as i128)
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("orders/token0/{FIRST_ORDER_END_TIME}"),
                (TOKEN0_SALE_RATE as i128)
                    .neg()
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("orders/token1/{FIRST_ORDER_END_TIME}"),
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

#[fixture]
pub fn mev_resist() -> TestCase {
    const POOL_KEY: NodeKey = NodeKey {
        token0: U256([1, 0, 0, 0]),
        token1: U256([2, 0, 0, 0]),
        config: Config { fee: u64::MAX / 10, tick_spacing: 10, extension: U256::one() },
    };

    const LOWER_TICK: Tick = Tick { index: -10, liquidity_delta: 100_000_000 };
    const UPPER_TICK: Tick =
        Tick { index: -LOWER_TICK.index, liquidity_delta: -LOWER_TICK.liquidity_delta };

    const TICK_INDEX_BETWEEN: i32 = 0;
    const SQRT_RATIO_BETWEEN: U256 = U256([0, 0, 1, 0]);
    const LIQUIDITY_BETWEEN: u128 = LOWER_TICK.liquidity_delta as u128;

    TestCase {
        component: component([
            ("extension_id".to_string(), 4_i32.to_be_bytes().into()), // MEV-resist pool
            ("token0".to_string(), POOL_KEY.token0.to_big_endian().into()),
            ("token1".to_string(), POOL_KEY.token1.to_big_endian().into()),
            ("fee".to_string(), POOL_KEY.config.fee.into()),
            ("tick_spacing".to_string(), POOL_KEY.config.tick_spacing.into()),
            (
                "extension".to_string(),
                POOL_KEY
                    .config
                    .extension
                    .to_big_endian()
                    .into(),
            ),
        ]),
        state_before_transition: EkuboState::MevResist(
            MevResistPool::new(POOL_KEY, vec![], SQRT_RATIO_BETWEEN, 0, TICK_INDEX_BETWEEN)
                .unwrap(),
        ),
        state_after_transition: EkuboState::MevResist(
            MevResistPool::new(
                POOL_KEY,
                vec![LOWER_TICK, UPPER_TICK],
                SQRT_RATIO_BETWEEN,
                LIQUIDITY_BETWEEN,
                TICK_INDEX_BETWEEN,
            )
            .unwrap(),
        ),
        required_attributes: [
            "extension_id".to_string(),
            "token0".to_string(),
            "token1".to_string(),
            "fee".to_string(),
            "tick_spacing".to_string(),
            "extension".to_string(),
            "liquidity".to_string(),
            "sqrt_ratio".to_string(),
            "tick".to_string(),
        ]
        .into(),
        transition_attributes: [
            ("liquidity".to_string(), LIQUIDITY_BETWEEN.to_be_bytes().into()),
            (
                format!("ticks/{}", LOWER_TICK.index),
                LOWER_TICK
                    .liquidity_delta
                    .to_be_bytes()
                    .into(),
            ),
            (
                format!("ticks/{}", UPPER_TICK.index),
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
                    .to_big_endian()
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
#[case::base(base())]
#[case::full_range(full_range())]
#[case::oracle(oracle())]
#[case::twamm(twamm())]
#[case::mev_resist(mev_resist())]
pub fn all_cases(#[case] case: TestCase) {}

fn component<const N: usize>(static_attributes: [(String, Bytes); N]) -> ProtocolComponent {
    ProtocolComponent { static_attributes: static_attributes.into(), ..Default::default() }
}
