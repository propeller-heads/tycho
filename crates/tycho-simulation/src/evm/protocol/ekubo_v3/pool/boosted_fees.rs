use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{
        EvmBoostedFeesConcentratedPool, EvmBoostedFeesConcentratedPoolKey,
        EvmBoostedFeesConcentratedPoolResources, EvmBoostedFeesConcentratedPoolState,
        EvmConcentratedPoolState, EvmPoolKey, EvmTokenAmount,
    },
    quoting::{
        pools::boosted_fees::concentrated::BoostedFeesConcentratedStandalonePoolResources,
        types::{LastTimeInfo, Pool, QuoteParams, Tick, TimeRateDelta},
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
        concentrated,
        timed::{self, estimate_block_timestamp, TimedTransition, GAS_COST_OF_ONE_BITMAP_SLOAD},
    },
    protocol::errors::InvalidSnapshotError,
};

const EXTRA_BASE_GAS_COST: u64 = 2_743;
const GAS_COST_OF_EXECUTING_VIRTUAL_DONATIONS: u64 = 6_814;
const GAS_COST_OF_CROSSING_ONE_VIRTUAL_DONATE_DELTA: u64 = 4_271;
const GAS_COST_OF_FEE_ACCUMULATION: u64 = 19_279;

#[derive(Debug, Eq, Clone, Serialize, Deserialize)]
pub struct BoostedFeesPool {
    imp: EvmBoostedFeesConcentratedPool,
    swap_state: BoostedFeesPoolSwapState,
}

#[derive(Debug, Eq, Clone, Copy, Serialize, Deserialize)]
struct BoostedFeesPoolSwapState {
    sdk_state: EvmBoostedFeesConcentratedPoolState,
    swapped_this_block: bool,
    last_real_time: u64,
    active_tick: Option<i32>,
}

impl BoostedFeesPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: EvmBoostedFeesConcentratedPoolKey,
        concentrated_sdk_state: EvmConcentratedPoolState,
        donate_rate0: u128,
        donate_rate1: u128,
        last_donate_time: u64,
        donate_rate_deltas: Vec<TimeRateDelta>,
        ticks: Vec<Tick>,
        tick: i32,
    ) -> Result<Self, InvalidSnapshotError> {
        impl_from_state(
            key,
            concentrated_sdk_state,
            donate_rate0,
            donate_rate1,
            last_donate_time,
            donate_rate_deltas,
            ticks,
        )
        .map(|imp| Self {
            imp,
            swap_state: BoostedFeesPoolSwapState {
                sdk_state: EvmBoostedFeesConcentratedPoolState {
                    concentrated_pool_state: concentrated_sdk_state,
                    donate_rate0,
                    donate_rate1,
                    last_donate_time: last_donate_time as u32,
                },
                swapped_this_block: false,
                last_real_time: last_donate_time,
                active_tick: Some(tick),
            },
        })
        .map_err(InvalidSnapshotError::ValueError)
    }
}

impl EkuboPool for BoostedFeesPool {
    fn key(&self) -> EvmPoolKey {
        self.imp.key().map_into_config()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.swap_state
            .sdk_state
            .concentrated_pool_state
            .sqrt_ratio
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.swap_state
            .sdk_state
            .concentrated_pool_state
            .sqrt_ratio = sqrt_ratio;
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.swap_state
            .sdk_state
            .concentrated_pool_state
            .liquidity = liquidity;
    }

    fn quote(
        &self,
        token_amount: EvmTokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<EkuboPoolQuote, SimulationError> {
        let timestamp = estimate_block_timestamp(
            self.swap_state.swapped_this_block,
            self.swap_state.last_real_time,
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
                    swap_state: BoostedFeesPoolSwapState {
                        sdk_state: quote.state_after,
                        swapped_this_block: true,
                        last_real_time: timestamp,
                        active_tick: None,
                    },
                }
                .into(),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        let sdk_state = self.swap_state.sdk_state;

        concentrated::get_limit(
            token_in,
            sdk_state
                .concentrated_pool_state
                .sqrt_ratio,
            &self.imp,
            sdk_state,
            self.swap_state.last_real_time, // Timestamp doesn't affect the calculated amount
            |r| r.concentrated,
        )
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        let ticks = concentrated::finish_transition(
            &mut self.swap_state.active_tick,
            &mut self
                .swap_state
                .sdk_state
                .concentrated_pool_state,
            self.imp.concentrated_pool().ticks(),
            &updated_attributes,
            &deleted_attributes,
        )?;

        let TimedTransition {
            rate_token0,
            rate_token1,
            last_time,
            time_rate_deltas: donate_rate_deltas,
        } = timed::finish_transition(
            self.swap_state.last_real_time,
            self.imp.donate_rate_deltas(),
            updated_attributes,
            deleted_attributes,
        )?;

        if let Some(donate_rate0) = rate_token0 {
            self.swap_state.sdk_state.donate_rate0 = donate_rate0;
        }
        if let Some(donate_rate1) = rate_token1 {
            self.swap_state.sdk_state.donate_rate1 = donate_rate1;
        }
        if let Some(last_donate_time) = last_time {
            self.swap_state.last_real_time = last_donate_time;
            self.swap_state
                .sdk_state
                .last_donate_time = last_donate_time as u32;
        }

        if ticks.is_some() || donate_rate_deltas.is_some() {
            let sdk_state = self.swap_state.sdk_state;

            self.imp = impl_from_state(
                self.imp.key(),
                sdk_state.concentrated_pool_state,
                sdk_state.donate_rate0,
                sdk_state.donate_rate1,
                self.swap_state.last_real_time,
                donate_rate_deltas.unwrap_or_else(|| self.imp.donate_rate_deltas().clone()),
                ticks.unwrap_or_else(|| {
                    self.imp
                        .concentrated_pool()
                        .ticks()
                        .to_vec()
                }),
            )
            .map_err(|err| {
                TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                    "reinstantiate BoostedFees pool: {err:?}"
                )))
            })?;
        }

        self.swap_state.swapped_this_block = false;

        Ok(())
    }
}

impl PartialEq for BoostedFeesPool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() &&
            self.imp.donate_rate_deltas() == imp.donate_rate_deltas() &&
            self.imp.concentrated_pool().ticks() == imp.concentrated_pool().ticks() &&
            self.swap_state == swap_state
    }
}

impl PartialEq for BoostedFeesPoolSwapState {
    fn eq(
        &self,
        &Self { sdk_state, swapped_this_block, last_real_time, active_tick }: &Self,
    ) -> bool {
        self.sdk_state == sdk_state &&
            self.swapped_this_block == swapped_this_block &&
            self.last_real_time == last_real_time &&
            self.active_tick
                .zip(active_tick)
                .is_none_or(|(t1, t2)| t1 == t2)
    }
}

fn impl_from_state(
    key: EvmBoostedFeesConcentratedPoolKey,
    concentrated_pool_state: EvmConcentratedPoolState,
    donate_rate0: u128,
    donate_rate1: u128,
    last_donate_time: u64,
    donate_rate_deltas: Vec<TimeRateDelta>,
    ticks: Vec<Tick>,
) -> Result<EvmBoostedFeesConcentratedPool, String> {
    EvmBoostedFeesConcentratedPool::new(
        concentrated::impl_from_state(key, concentrated_pool_state, ticks)
            .map_err(|err| format!("creating concentrated pool: {err:?}"))?,
        LastTimeInfo::Real(last_donate_time),
        donate_rate0,
        donate_rate1,
        donate_rate_deltas,
    )
    .map_err(|err| format!("creating BoostedFees pool: {err:?}"))
}

fn gas_costs(
    EvmBoostedFeesConcentratedPoolResources {
        concentrated,
        boosted_fees:
            BoostedFeesConcentratedStandalonePoolResources {
                extra_distinct_bitmap_lookups,
                virtual_donate_delta_times_crossed,
                virtual_donations_executed,
                fees_accumulated,
            },
    }: EvmBoostedFeesConcentratedPoolResources,
) -> u64 {
    concentrated::gas_costs(concentrated) +
        EXTRA_BASE_GAS_COST +
        u64::from(extra_distinct_bitmap_lookups) * GAS_COST_OF_ONE_BITMAP_SLOAD +
        u64::from(virtual_donate_delta_times_crossed) *
            GAS_COST_OF_CROSSING_ONE_VIRTUAL_DONATE_DELTA +
        u64::from(virtual_donations_executed) * GAS_COST_OF_EXECUTING_VIRTUAL_DONATIONS +
        u64::from(fees_accumulated) * GAS_COST_OF_FEE_ACCUMULATION
}
