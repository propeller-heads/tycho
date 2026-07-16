use std::collections::{HashMap, HashSet};
#[cfg(not(test))]
use std::time::SystemTime;

use alloy::eips::merge::SLOT_DURATION_SECS;
use evm_ekubo_sdk::{
    math::uint::U256,
    quoting::{
        self,
        twamm_pool::{TwammPoolError, TwammPoolState, TwammSaleRateDelta},
        types::{NodeKey, Pool, QuoteParams, TokenAmount},
    },
};
use itertools::Itertools;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{full_range::FullRangePool, EkuboPool, EkuboPoolQuote};
use crate::{
    evm::protocol::ekubo::attributes::sale_rate_deltas_from_attributes,
    protocol::errors::InvalidSnapshotError,
};

#[derive(Debug, Eq, Clone, Serialize, Deserialize)]
pub struct TwammPool {
    imp: quoting::twamm_pool::TwammPool,
    state: TwammPoolState,

    swapped_this_block: bool,
}

impl PartialEq for TwammPool {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key() &&
            self.imp.get_sale_rate_deltas() == other.imp.get_sale_rate_deltas() &&
            self.state == other.state
    }
}

fn impl_from_state(
    key: &NodeKey,
    state: TwammPoolState,
    virtual_order_deltas: Vec<TwammSaleRateDelta>,
) -> Result<quoting::twamm_pool::TwammPool, TwammPoolError> {
    quoting::twamm_pool::TwammPool::new(
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
    const GAS_COST_OF_ONE_VIRTUAL_ORDER_DELTA: u64 = 25_000;
    const BASE_GAS_COST_OF_EXECUTING_VIRTUAL_ORDERS: u64 = 15_000;

    const UNDERESTIMATION_SLOT_COUNT: u64 = 4;

    pub fn new(
        key: &NodeKey,
        state: TwammPoolState,
        virtual_order_deltas: Vec<TwammSaleRateDelta>,
    ) -> Result<Self, InvalidSnapshotError> {
        Ok(Self {
            imp: impl_from_state(key, state, virtual_order_deltas).map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating TWAMM pool: {err:?}"))
            })?,
            state,
            swapped_this_block: false,
        })
    }

    fn estimate_block_timestamp(&self) -> Result<u64, SimulationError> {
        if self.swapped_this_block {
            Ok(self.state.last_execution_time)
        } else {
            // TODO How accurate is it to take the current timestamp?
            Ok(Ord::max(self.state.last_execution_time + SLOT_DURATION_SECS, current_timestamp()?))
        }
    }
}

impl EkuboPool for TwammPool {
    fn key(&self) -> &NodeKey {
        self.imp.get_key()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.state
            .full_range_pool_state
            .sqrt_ratio
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.state
            .full_range_pool_state
            .sqrt_ratio = sqrt_ratio;
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.state
            .full_range_pool_state
            .liquidity = liquidity;
    }

    fn quote(
        &self,
        token_amount: TokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<EkuboPoolQuote, SimulationError> {
        let quote = self
            .imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(self.state),
                meta: self.estimate_block_timestamp()?,
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))?;

        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount: quote.calculated_amount,
            gas: FullRangePool::gas_costs() +
                u64::from(
                    quote
                        .execution_resources
                        .virtual_orders_executed,
                ) * Self::BASE_GAS_COST_OF_EXECUTING_VIRTUAL_ORDERS +
                u64::from(
                    quote
                        .execution_resources
                        .virtual_order_delta_times_crossed,
                ) * Self::GAS_COST_OF_ONE_VIRTUAL_ORDER_DELTA,
            new_state: Self {
                imp: self.imp.clone(),
                state: quote.state_after,
                swapped_this_block: true,
            }
            .into(),
        })
    }

    fn get_limit(&self, token_in: U256) -> Result<i128, SimulationError> {
        let key = self.key();
        let estimated_timestamp = self.estimate_block_timestamp()?;

        // Only execute the virtual orders up to a given timestamp
        let virtual_order_quote = self
            .imp
            .quote(QuoteParams {
                token_amount: TokenAmount { token: token_in, amount: 0 },
                sqrt_ratio_limit: None,
                override_state: Some(self.state),
                meta: estimated_timestamp + Self::UNDERESTIMATION_SLOT_COUNT * SLOT_DURATION_SECS,
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
            self.state
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
            (self.state, estimated_timestamp)
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
        if let Some(token0_sale_rate) = updated_attributes.get("token0_sale_rate") {
            self.state.token0_sale_rate = token0_sale_rate.clone().into();
        }

        if let Some(token1_sale_rate) = updated_attributes.get("token1_sale_rate") {
            self.state.token1_sale_rate = token1_sale_rate.clone().into();
        }

        let first_active_virtual_order_idx =
            if let Some(last_execution_time) = updated_attributes.get("last_execution_time") {
                let last_execution_time = last_execution_time.clone().into();

                self.state.last_execution_time = last_execution_time;

                self.imp
                    .get_sale_rate_deltas()
                    .partition_point(|srd| srd.time <= last_execution_time)
            } else {
                0
            };

        let changed_virtual_order_deltas = sale_rate_deltas_from_attributes(
            updated_attributes.into_iter().chain(
                deleted_attributes
                    .into_iter()
                    .map(|key| (key, Bytes::new())),
            ),
            self.state.last_execution_time,
        )
        .map_err(TransitionError::DecodeError)?
        .collect_vec();

        if !changed_virtual_order_deltas.is_empty() || !first_active_virtual_order_idx.is_zero() {
            let mut virtual_order_deltas =
                self.imp.get_sale_rate_deltas()[first_active_virtual_order_idx..].to_vec();

            for virtual_order_delta in changed_virtual_order_deltas {
                let res = virtual_order_deltas
                    .binary_search_by_key(&virtual_order_delta.time, |d| d.time);

                match res {
                    Ok(idx) => {
                        if virtual_order_delta
                            .sale_rate_delta0
                            .is_zero() &&
                            virtual_order_delta
                                .sale_rate_delta1
                                .is_zero()
                        {
                            virtual_order_deltas.remove(idx);
                        } else {
                            virtual_order_deltas[idx] = virtual_order_delta;
                        }
                    }
                    Err(idx) => {
                        virtual_order_deltas.insert(idx, virtual_order_delta);
                    }
                }
            }

            self.imp =
                impl_from_state(self.key(), self.state, virtual_order_deltas).map_err(|err| {
                    TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                        "reinstantiate TWAMM pool: {err:?}"
                    )))
                })?;
        }

        self.swapped_this_block = false;

        Ok(())
    }
}

#[cfg(test)]
fn current_timestamp() -> Result<u64, SimulationError> {
    Ok(crate::evm::protocol::ekubo::test_cases::TEST_TIMESTAMP)
}

#[cfg(not(test))]
fn current_timestamp() -> Result<u64, SimulationError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| SimulationError::FatalError(format!("System time before UNIX EPOCH: {e:?}")))
        .map(|d| d.as_secs())
}
