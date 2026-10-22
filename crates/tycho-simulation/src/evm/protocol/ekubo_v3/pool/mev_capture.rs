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

        let start_tick = self.swap_state.last_tick;
        if let Some(active_tick) = self.swap_state.active_tick {
            self.swap_state.last_tick = active_tick;
        }

        // The SDK pool charges the MEV-capture fee by distance from the tick it was built with.
        // Rebuild it when `last_tick` moves, even if no tick entry changed.
        if ticks.is_some() || self.swap_state.last_tick != start_tick {
            let ticks = ticks.unwrap_or_else(|| {
                self.imp
                    .concentrated_pool()
                    .ticks()
                    .to_vec()
            });
            self.imp = impl_from_state(
                self.imp.key(),
                self.swap_state.sdk_state,
                ticks,
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
        quoting::{pools::concentrated::TickSpacing, types::TokenAmount},
    };
    use revm::primitives::address;

    use super::*;
    use crate::evm::protocol::ekubo_v3::addresses::MEV_CAPTURE_ADDRESS;

    const TOKEN0: Address = address!("0x0000000000000000000000000000000000000001");
    const TOKEN1: Address = address!("0x0000000000000000000000000000000000000002");
    const LIQUIDITY: u128 = 1_000_000_000_000;
    const TICKS: [Tick; 2] = [
        Tick { index: -10_000, liquidity_delta: LIQUIDITY as i128 },
        Tick { index: 10_000, liquidity_delta: -(LIQUIDITY as i128) },
    ];

    fn pool_at(tick: i32) -> MevCapturePool {
        MevCapturePool::new(
            EvmMevCapturePoolKey {
                token0: TOKEN0,
                token1: TOKEN1,
                config: EvmMevCapturePoolConfig {
                    fee: u64::MAX / 1_000,
                    pool_type_config: TickSpacing(10),
                    extension: MEV_CAPTURE_ADDRESS,
                },
            },
            tick,
            EvmConcentratedPoolState {
                sqrt_ratio: to_sqrt_ratio::<Evm>(tick).expect("tick in range"),
                liquidity: LIQUIDITY,
                active_tick_index: Some(0),
            },
            TICKS.to_vec(),
        )
        .expect("valid pool")
    }

    fn quote(pool: &MevCapturePool, token: Address) -> u128 {
        pool.quote(TokenAmount { token, amount: 1_000_000_000 }, None)
            .expect("quote")
            .calculated_amount
    }

    #[test]
    fn test_finish_transition_tick_only() {
        let tick = 50;
        let mut transitioned = pool_at(0);

        transitioned.set_sqrt_ratio(to_sqrt_ratio::<Evm>(tick).expect("tick in range"));
        transitioned
            .finish_transition(
                [("tick".to_string(), tick.to_be_bytes().into())].into(),
                HashSet::new(),
            )
            .expect("transition");

        let fresh = pool_at(tick);
        assert_eq!(quote(&transitioned, TOKEN0), quote(&fresh, TOKEN0));
        assert_eq!(quote(&transitioned, TOKEN1), quote(&fresh, TOKEN1));
    }
}
