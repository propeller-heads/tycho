use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{
        EvmConcentratedPoolState, EvmMevCapturePool, EvmMevCapturePoolKey,
        EvmMevCapturePoolResources, EvmMevCapturePoolState, EvmPoolKey, EvmTokenAmount,
    },
    quoting::{
        pools::mev_capture::{MevCapturePoolKey, MevCaptureStandalonePoolResources},
        types::{Pool, QuoteParams, Tick},
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
    evm::protocol::ekubo_v3::pool::concentrated::{self},
    protocol::errors::InvalidSnapshotError,
};

const EXTRA_BASE_GAS_COST: u64 = 15_840;
const GAS_COST_OF_ONE_STATE_UPDATE: u64 = 16_418;

#[derive(Debug, Eq, Clone, Serialize, Deserialize)]
pub struct MevCapturePool {
    imp: EvmMevCapturePool,
    swap_state: MevCapturePoolSwapState,
}

#[derive(Debug, Eq, Clone, Copy, Serialize, Deserialize)]
struct MevCapturePoolSwapState {
    sdk_state: EvmConcentratedPoolState,
    last_tick: i32,
    active_tick: Option<i32>,
}

impl MevCapturePool {
    pub fn new(
        key: MevCapturePoolKey,
        tick: i32,
        concentrated_state: EvmConcentratedPoolState,
        ticks: Vec<Tick>,
    ) -> Result<Self, InvalidSnapshotError> {
        impl_from_state(key, concentrated_state, ticks, tick)
            .map(|imp| Self {
                imp,
                swap_state: MevCapturePoolSwapState {
                    sdk_state: concentrated_state,
                    active_tick: Some(tick),
                    last_tick: tick,
                },
            })
            .map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating MEVCapture pool: {err:?}"))
            })
    }
}

impl EkuboPool for MevCapturePool {
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
        let first_swap_this_block = self.swap_state.active_tick.is_some();

        self.imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(EvmMevCapturePoolState {
                    last_update_time: 0,
                    concentrated_pool_state: self.swap_state.sdk_state,
                }),
                meta: u64::from(first_swap_this_block),
            })
            .map(|quote| EkuboPoolQuote {
                consumed_amount: quote.consumed_amount,
                calculated_amount: quote.calculated_amount,
                gas: gas_costs(quote.execution_resources),
                new_state: Self {
                    imp: self.imp.clone(),
                    swap_state: MevCapturePoolSwapState {
                        sdk_state: quote
                            .state_after
                            .concentrated_pool_state,
                        last_tick: self.swap_state.last_tick,
                        active_tick: None,
                    },
                }
                .into(),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err:?}")))
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        concentrated::get_limit(
            token_in,
            self.sqrt_ratio(),
            &self.imp,
            EvmMevCapturePoolState {
                last_update_time: 0,
                concentrated_pool_state: self.swap_state.sdk_state,
            },
            0,
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
            &mut self.swap_state.sdk_state,
            self.imp.concentrated_pool().ticks(),
            &updated_attributes,
            &deleted_attributes,
        )?;

        if let Some(active_tick) = self.swap_state.active_tick {
            self.swap_state.last_tick = active_tick;
        }

        if let Some(new_ticks) = ticks {
            self.imp = impl_from_state(
                self.imp.key(),
                self.swap_state.sdk_state,
                new_ticks,
                self.swap_state.last_tick,
            )
            .map_err(|err| {
                TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                    "reinstantiate MEVCapture pool: {err:?}"
                )))
            })?;
        }

        Ok(())
    }
}

impl PartialEq for MevCapturePool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() &&
            self.imp.concentrated_pool().ticks() == imp.concentrated_pool().ticks() &&
            self.swap_state == swap_state
    }
}

impl PartialEq for MevCapturePoolSwapState {
    fn eq(&self, &Self { last_tick, active_tick, sdk_state }: &Self) -> bool {
        self.sdk_state == sdk_state &&
            self.last_tick == last_tick &&
            self.active_tick
                .zip(active_tick)
                .is_none_or(|(t1, t2)| t1 == t2)
    }
}

fn impl_from_state(
    key: EvmMevCapturePoolKey,
    concentrated_state: EvmConcentratedPoolState,
    ticks: Vec<Tick>,
    tick: i32,
) -> Result<EvmMevCapturePool, String> {
    EvmMevCapturePool::new(
        concentrated::impl_from_state(key, concentrated_state, ticks)
            .map_err(|err| format!("creating concentrated pool: {err:?}"))?,
        0,
        tick,
    )
    .map_err(|err| format!("creating MEVCapture pool: {err:?}"))
}

fn gas_costs(
    EvmMevCapturePoolResources {
        concentrated,
        mev_capture: MevCaptureStandalonePoolResources { state_update_count },
    }: EvmMevCapturePoolResources,
) -> u64 {
    concentrated::gas_costs(concentrated) +
        EXTRA_BASE_GAS_COST +
        u64::from(state_update_count) * GAS_COST_OF_ONE_STATE_UPDATE
}

#[cfg(test)]
mod tests {
    use ekubo_sdk::{
        chain::evm::{Evm, EvmMevCapturePoolConfig},
        math::tick::to_sqrt_ratio,
        quoting::pools::concentrated::TickSpacing,
    };

    use super::*;

    const NEW_TICK: i32 = 5_000;

    fn key() -> MevCapturePoolKey {
        EvmMevCapturePoolKey {
            token0: Address::ZERO,
            token1: Address::repeat_byte(1),
            config: EvmMevCapturePoolConfig {
                fee: u64::MAX / 1_000,
                pool_type_config: TickSpacing(10),
                extension: Address::repeat_byte(2),
            },
        }
    }

    fn ticks() -> Vec<Tick> {
        vec![
            Tick { index: -100_000, liquidity_delta: 1_000_000_000_000 },
            Tick { index: 100_000, liquidity_delta: -1_000_000_000_000 },
        ]
    }

    fn state_at(tick: i32) -> EvmConcentratedPoolState {
        EvmConcentratedPoolState {
            sqrt_ratio: to_sqrt_ratio::<Evm>(tick).unwrap(),
            liquidity: 1_000_000_000_000,
            active_tick_index: Some(0),
        }
    }

    fn quote_out(pool: &MevCapturePool) -> u128 {
        let token_amount = EvmTokenAmount { amount: 1_000_000_000, token: pool.key().token0 };
        pool.quote(token_amount, None)
            .unwrap()
            .calculated_amount
    }

    #[test]
    fn test_tick_move_without_tick_list_change_matches_fresh_pool() {
        let mut moved = MevCapturePool::new(key(), 0, state_at(0), ticks()).unwrap();
        moved.set_sqrt_ratio(to_sqrt_ratio::<Evm>(NEW_TICK).unwrap());
        moved
            .finish_transition(
                HashMap::from([("tick".to_string(), NEW_TICK.to_be_bytes().into())]),
                HashSet::new(),
            )
            .unwrap();

        let fresh = MevCapturePool::new(key(), NEW_TICK, state_at(NEW_TICK), ticks()).unwrap();

        assert_eq!(quote_out(&moved), quote_out(&fresh));
    }
}
