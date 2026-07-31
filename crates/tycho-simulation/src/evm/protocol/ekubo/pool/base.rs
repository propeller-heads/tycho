use std::{
    collections::{HashMap, HashSet},
    convert::identity,
};

use evm_ekubo_sdk::{
    math::{tick::to_sqrt_ratio, uint::U256},
    quoting::{
        self,
        base_pool::{BasePoolError, BasePoolResources, BasePoolState},
        types::{NodeKey, Pool, QuoteParams, Tick, TokenAmount},
        util::find_nearest_initialized_tick_index,
    },
};
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{EkuboPool, EkuboPoolQuote};
use crate::{
    evm::protocol::ekubo::attributes::ticks_from_attributes, protocol::errors::InvalidSnapshotError,
};

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct BasePool {
    imp: quoting::base_pool::BasePool,
    state: BasePoolState,

    active_tick: Option<i32>,
}

impl PartialEq for BasePool {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key() &&
            self.imp.get_sorted_ticks() == other.imp.get_sorted_ticks() &&
            self.state == other.state
    }
}

pub(super) fn impl_from_state(
    key: NodeKey,
    state: BasePoolState,
    ticks: Vec<Tick>,
) -> Result<quoting::base_pool::BasePool, BasePoolError> {
    quoting::base_pool::BasePool::new(key, state, ticks)
}

impl BasePool {
    // The names of the constants reflect the exact method from the tenderly log.
    const BASE_SWAP_GAS_COST: u64 = 24_000;
    const PAY_METHOD_GAS_COST: u64 = 46_000;
    const WITHDRAW_METHOD_GAS_COST: u64 = 20_000;
    const GAS_COST_OF_ONE_TICK_SPACING_CROSSED: u64 = 4_000;
    const GAS_COST_OF_ONE_INITIALIZED_TICK_CROSSED: u64 = 20_000;

    pub fn new(
        key: NodeKey,
        ticks: Vec<Tick>,
        sqrt_ratio: U256,
        liquidity: u128,
        tick: i32,
    ) -> Result<Self, InvalidSnapshotError> {
        let state = BasePoolState {
            sqrt_ratio,
            liquidity,
            active_tick_index: find_nearest_initialized_tick_index(&ticks, tick),
        };

        Ok(Self {
            imp: impl_from_state(key, state, ticks).map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating base pool: {err:?}"))
            })?,
            state,
            active_tick: Some(tick),
        })
    }

    pub(super) fn gas_costs(resources: BasePoolResources) -> u64 {
        u64::from(resources.tick_spacings_crossed) * Self::GAS_COST_OF_ONE_TICK_SPACING_CROSSED +
            u64::from(resources.initialized_ticks_crossed) *
                Self::GAS_COST_OF_ONE_INITIALIZED_TICK_CROSSED
    }
}

impl EkuboPool for BasePool {
    fn key(&self) -> &NodeKey {
        self.imp.get_key()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.state.sqrt_ratio
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.state.sqrt_ratio = sqrt_ratio;
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.state.liquidity = liquidity;
    }

    fn quote(&self, token_amount: TokenAmount) -> Result<EkuboPoolQuote, SimulationError> {
        let quote = self
            .imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit: None,
                override_state: Some(self.state),
                meta: (),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))?;

        let state_after = quote.state_after;

        let new_state =
            Self { imp: self.imp.clone(), state: state_after, active_tick: None }.into();

        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount: quote.calculated_amount,
            gas: Self::BASE_SWAP_GAS_COST +
                Self::PAY_METHOD_GAS_COST +
                Self::WITHDRAW_METHOD_GAS_COST +
                Self::gas_costs(quote.execution_resources),
            new_state,
        })
    }

    fn get_limit(&self, token_in: U256) -> Result<i128, SimulationError> {
        get_limit(token_in, self.sqrt_ratio(), &self.imp, self.state, (), identity)
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        let active_tick_update = updated_attributes
            .get("tick")
            .and_then(|updated_tick| {
                let updated_tick = updated_tick.clone().into();

                (self.active_tick != Some(updated_tick)).then_some(updated_tick)
            });

        let changed_ticks = ticks_from_attributes(
            updated_attributes.into_iter().chain(
                deleted_attributes
                    .into_iter()
                    .map(|key| (key, Bytes::new())),
            ),
        )
        .map_err(TransitionError::DecodeError)?;

        let new_initialized_ticks = (!changed_ticks.is_empty()).then(|| {
            let mut ticks = self.imp.get_sorted_ticks().clone();

            for tick in changed_ticks {
                let res = ticks.binary_search_by_key(&tick.index, |t| t.index);

                match res {
                    Ok(idx) => {
                        if tick.liquidity_delta.is_zero() {
                            ticks.remove(idx);
                        } else {
                            ticks[idx] = tick;
                        }
                    }
                    Err(idx) => {
                        ticks.insert(idx, tick);
                    }
                }
            }

            ticks
        });

        if let Some(new_active_tick) = active_tick_update {
            self.active_tick = Some(new_active_tick);
        }

        if active_tick_update.is_some() || new_initialized_ticks.is_some() {
            self.state.active_tick_index = find_nearest_initialized_tick_index(
                new_initialized_ticks
                    .as_ref()
                    .unwrap_or(self.imp.get_sorted_ticks()),
                self.active_tick.ok_or_else(|| {
                    TransitionError::MissingAttribute(
                        "base state should always have an active tick during transitions"
                            .to_string(),
                    )
                })?,
            );
        }

        if let Some(ticks) = new_initialized_ticks {
            self.imp = impl_from_state(*self.key(), self.state, ticks).map_err(|err| {
                TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                    "reinstantiate base pool: {err:?}"
                )))
            })?;
        }

        Ok(())
    }
}

// Factor to account for computation inaccuracies due to not using tick bitmaps
const WEI_UNDERESTIMATION_FACTOR: i128 = 2;

pub(super) fn get_limit<P, S, M, R>(
    token_in: U256,
    sqrt_ratio: U256,
    imp: &P,
    state: S,
    meta: M,
    resources_fn: impl FnOnce(R) -> BasePoolResources,
) -> Result<i128, SimulationError>
where
    P: Pool<State = S, Meta = M, Resources = R>,
{
    let sqrt_ratio_limit = if token_in == imp.get_key().token0 {
        imp.min_tick_with_liquidity()
            .map_or(Ok(sqrt_ratio), |tick| {
                to_sqrt_ratio(tick)
                    .ok_or_else(|| {
                        SimulationError::FatalError(
                            "sqrt_ratio should be computable from tick index".to_string(),
                        )
                    })
                    .map(|r| Ord::min(r, sqrt_ratio))
            })
    } else {
        imp.max_tick_with_liquidity()
            .map_or(Ok(sqrt_ratio), |tick| {
                to_sqrt_ratio(tick)
                    .ok_or_else(|| {
                        SimulationError::FatalError(
                            "sqrt_ratio should be computable from tick index".to_string(),
                        )
                    })
                    .map(|r| Ord::max(r, sqrt_ratio))
            })
    }?;

    let quote = imp
        .quote(QuoteParams {
            token_amount: TokenAmount { amount: i128::MAX, token: token_in },
            sqrt_ratio_limit: Some(sqrt_ratio_limit),
            override_state: Some(state),
            meta,
        })
        .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err:?}")))?;

    let resources = resources_fn(quote.execution_resources);

    Ok(quote
        .consumed_amount
        .saturating_sub(
            WEI_UNDERESTIMATION_FACTOR *
                (i128::from(resources.initialized_ticks_crossed) +
                    i128::from(resources.tick_spacings_crossed) / 256 +
                    1),
        )
        .max(0))
}
