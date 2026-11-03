use std::collections::{HashMap, HashSet};

use alloy::eips::merge::SLOT_DURATION_SECS;
use ekubo_sdk::{
    chain::evm::{
        EvmPoolKey, EvmTokenAmount, EvmTwammPool, EvmTwammPoolConstructionError, EvmTwammPoolKey,
        EvmTwammPoolResources, EvmTwammPoolState,
    },
    quoting::{
        pools::twamm::TwammStandalonePoolResources,
        types::{Pool, QuoteParams, TimeRateDelta, TokenAmount},
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
use crate::{
    evm::protocol::ekubo_v3::pool::{
        full_range,
        timed::{self, estimate_block_timestamp, TimedTransition, GAS_COST_OF_ONE_BITMAP_SLOAD},
    },
    protocol::errors::InvalidSnapshotError,
};

const UNDERESTIMATION_SLOT_COUNT: u64 = 4;

const EXTRA_BASE_GAS_COST: u64 = 5_302;
const GAS_COST_OF_EXECUTING_VIRTUAL_ORDERS: u64 = 20_554;
const GAS_COST_OF_CROSSING_ONE_VIRTUAL_ORDER_DELTA: u64 = 19_980;

#[derive(Debug, Eq, Clone, Serialize, Deserialize)]
pub struct TwammPool {
    imp: EvmTwammPool,
    swap_state: TwammPoolSwapState,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
struct TwammPoolSwapState {
    sdk_state: EvmTwammPoolState,
    swapped_this_block: bool,
}

fn impl_from_state(
    key: EvmTwammPoolKey,
    state: EvmTwammPoolState,
    virtual_order_deltas: Vec<TimeRateDelta>,
) -> Result<EvmTwammPool, EvmTwammPoolConstructionError> {
    EvmTwammPool::new(
        key.token0,
        key.token1,
        key.config.fee,
        key.config.extension,
        state.full_range_pool_state.sqrt_ratio,
        state.full_range_pool_state.liquidity,
        state.last_execution_time,
        state.token0_sale_rate,
        state.token1_sale_rate,
        virtual_order_deltas,
    )
}

impl TwammPool {
    pub fn new(
        key: EvmTwammPoolKey,
        sdk_state: EvmTwammPoolState,
        virtual_order_deltas: Vec<TimeRateDelta>,
    ) -> Result<Self, InvalidSnapshotError> {
        impl_from_state(key, sdk_state, virtual_order_deltas)
            .map(|imp| Self {
                imp,
                swap_state: TwammPoolSwapState { sdk_state, swapped_this_block: false },
            })
            .map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating TWAMM pool: {err:?}"))
            })
    }
}

impl EkuboPool for TwammPool {
    fn key(&self) -> EvmPoolKey {
        self.imp.key().map_into_config()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.swap_state
            .sdk_state
            .full_range_pool_state
            .sqrt_ratio
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.swap_state
            .sdk_state
            .full_range_pool_state
            .sqrt_ratio = sqrt_ratio;
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.swap_state
            .sdk_state
            .full_range_pool_state
            .liquidity = liquidity;
    }

    fn quote(
        &self,
        token_amount: EvmTokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<EkuboPoolQuote, SimulationError> {
        let timestamp = estimate_block_timestamp(
            self.swap_state.swapped_this_block,
            self.swap_state
                .sdk_state
                .last_execution_time,
        )?;

        self.imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(self.swap_state.sdk_state),
                meta: timestamp,
            })
            .map(|quote| EkuboPoolQuote {
                consumed_amount: quote.consumed_amount,
                calculated_amount: quote.calculated_amount,
                gas: gas_costs(quote.execution_resources),
                new_state: Self {
                    imp: self.imp.clone(),
                    swap_state: TwammPoolSwapState {
                        sdk_state: quote.state_after,
                        swapped_this_block: true,
                    },
                }
                .into(),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        let key = self.imp.key();
        let estimated_timestamp = estimate_block_timestamp(
            self.swap_state.swapped_this_block,
            self.swap_state
                .sdk_state
                .last_execution_time,
        )?;

        // Only execute the virtual orders up to a given timestamp
        let virtual_order_quote = self
            .imp
            .quote(QuoteParams {
                token_amount: TokenAmount { token: token_in, amount: 0 },
                sqrt_ratio_limit: None,
                override_state: Some(self.swap_state.sdk_state),
                meta: estimated_timestamp + UNDERESTIMATION_SLOT_COUNT * SLOT_DURATION_SECS,
            })
            .map_err(|err| {
                SimulationError::RecoverableError(format!(
                    "executing virtual orders quote: {err:?}"
                ))
            })?;

        // If letting some virtual orders execute leads to a less favorable price for the given swap
        // direction
        let moved_to_unfavorable_price = (virtual_order_quote
            .state_after
            .full_range_pool_state
            .sqrt_ratio <
            self.swap_state
                .sdk_state
                .full_range_pool_state
                .sqrt_ratio) ==
            (token_in == key.token0);

        let (override_state, meta) = if moved_to_unfavorable_price {
            (
                virtual_order_quote.state_after,
                virtual_order_quote
                    .state_after
                    .last_execution_time,
            )
        } else {
            (self.swap_state.sdk_state, estimated_timestamp)
        };

        // Quote with the less favorable state (either the current one or the one where future
        // virtual orders are already executed)
        Ok(self
            .imp
            .quote(QuoteParams {
                token_amount: TokenAmount { amount: i128::MAX, token: token_in },
                sqrt_ratio_limit: None,
                override_state: Some(override_state),
                meta,
            })
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err:?}")))?
            .consumed_amount)
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        let TimedTransition {
            rate_token0,
            rate_token1,
            last_time,
            time_rate_deltas: sale_rate_deltas,
        } = timed::finish_transition(
            self.swap_state
                .sdk_state
                .last_execution_time,
            self.imp.sale_rate_deltas(),
            updated_attributes,
            deleted_attributes,
        )?;

        if let Some(token0_sale_rate) = rate_token0 {
            self.swap_state
                .sdk_state
                .token0_sale_rate = token0_sale_rate;
        }
        if let Some(token1_sale_rate) = rate_token1 {
            self.swap_state
                .sdk_state
                .token1_sale_rate = token1_sale_rate;
        }
        if let Some(last_execution_time) = last_time {
            self.swap_state
                .sdk_state
                .last_execution_time = last_execution_time;
        }

        if let Some(sale_rate_deltas) = sale_rate_deltas {
            self.imp = impl_from_state(self.imp.key(), self.swap_state.sdk_state, sale_rate_deltas)
                .map_err(|err| {
                    TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                        "reinstantiate TWAMM pool: {err:?}"
                    )))
                })?;
        }

        self.swap_state.swapped_this_block = false;

        Ok(())
    }
}

impl PartialEq for TwammPool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() &&
            self.imp.sale_rate_deltas() == imp.sale_rate_deltas() &&
            self.swap_state == swap_state
    }
}

fn gas_costs(
    EvmTwammPoolResources {
        full_range,
        twamm:
            TwammStandalonePoolResources {
                extra_distinct_bitmap_lookups,
                virtual_order_delta_times_crossed,
                virtual_orders_executed,
            },
    }: EvmTwammPoolResources,
) -> u64 {
    full_range::gas_costs(full_range) +
        EXTRA_BASE_GAS_COST +
        u64::from(virtual_orders_executed) * GAS_COST_OF_EXECUTING_VIRTUAL_ORDERS +
        u64::from(virtual_order_delta_times_crossed) *
            GAS_COST_OF_CROSSING_ONE_VIRTUAL_ORDER_DELTA +
        u64::from(extra_distinct_bitmap_lookups) * GAS_COST_OF_ONE_BITMAP_SLOAD
}
