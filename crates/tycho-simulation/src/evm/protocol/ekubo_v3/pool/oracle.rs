use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{
        EvmFullRangePoolState, EvmOraclePool, EvmOraclePoolKey, EvmOraclePoolResources,
        EvmOraclePoolState, EvmPoolKey, EvmTokenAmount,
    },
    quoting::{
        pools::oracle::OracleStandalonePoolResources,
        types::{Pool, QuoteParams, TokenAmount},
    },
    U256,
};
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{EkuboPool, EkuboPoolQuote};
use crate::{evm::protocol::ekubo_v3::pool::full_range, protocol::errors::InvalidSnapshotError};

// The negative costs come from savings compared to a full range swap which usually touches
// fee-related storage slots
const REDUCED_BASE_GAS_COST: u64 = 1_801;
const GAS_COST_OF_UPDATING_SNAPSHOT: u64 = 9_709;

#[derive(Debug, Eq, Clone, Serialize, Deserialize)]
pub struct OraclePool {
    imp: EvmOraclePool,
    swap_state: OraclePoolSwapState,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
struct OraclePoolSwapState {
    sdk_state: EvmFullRangePoolState,
    swapped_this_block: bool,
}

impl OraclePool {
    pub fn new(
        key: EvmOraclePoolKey,
        full_range_sdk_state: EvmFullRangePoolState,
    ) -> Result<Self, InvalidSnapshotError> {
        EvmOraclePool::new(
            key.token0,
            key.token1,
            key.config.extension,
            full_range_sdk_state.sqrt_ratio,
            full_range_sdk_state.liquidity,
            0,
        )
        .map(|imp| Self {
            imp,
            swap_state: OraclePoolSwapState {
                sdk_state: full_range_sdk_state,
                swapped_this_block: false,
            },
        })
        .map_err(|err| InvalidSnapshotError::ValueError(format!("creating oracle pool: {err:?}")))
    }
}

impl EkuboPool for OraclePool {
    fn key(&self) -> EvmPoolKey {
        self.imp.key().map_into_config()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.swap_state.sdk_state.sqrt_ratio
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.swap_state.sdk_state.sqrt_ratio = sqrt_ratio;
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.swap_state.sdk_state.liquidity = liquidity;
    }

    fn quote(
        &self,
        token_amount: EvmTokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<EkuboPoolQuote, SimulationError> {
        let first_swap_this_block = !self.swap_state.swapped_this_block;

        self.imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(EvmOraclePoolState {
                    full_range_pool_state: self.swap_state.sdk_state,
                    last_snapshot_time: 0,
                }),
                meta: u64::from(first_swap_this_block),
            })
            .map(|quote| EkuboPoolQuote {
                consumed_amount: quote.consumed_amount,
                calculated_amount: quote.calculated_amount,
                gas: gas_costs(quote.execution_resources),
                new_state: Self {
                    imp: self.imp.clone(),
                    swap_state: OraclePoolSwapState {
                        sdk_state: quote.state_after.full_range_pool_state,
                        swapped_this_block: true,
                    },
                }
                .into(),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        self.imp
            .quote(QuoteParams {
                token_amount: TokenAmount { amount: i128::MAX, token: token_in },
                sqrt_ratio_limit: None,
                override_state: Some(EvmOraclePoolState {
                    full_range_pool_state: self.swap_state.sdk_state,
                    last_snapshot_time: 0,
                }),
                meta: 0,
            })
            .map(|quote| quote.consumed_amount)
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err:?}")))
    }

    fn finish_transition(
        &mut self,
        _updated_attributes: HashMap<String, Bytes>,
        _deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        self.swap_state.swapped_this_block = false;

        Ok(())
    }
}

impl PartialEq for OraclePool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() && self.swap_state == swap_state
    }
}

fn gas_costs(
    EvmOraclePoolResources {
        full_range,
        oracle: OracleStandalonePoolResources { snapshots_written },
    }: EvmOraclePoolResources,
) -> u64 {
    full_range::gas_costs(full_range) - REDUCED_BASE_GAS_COST +
        u64::from(snapshots_written) * GAS_COST_OF_UPDATING_SNAPSHOT
}
