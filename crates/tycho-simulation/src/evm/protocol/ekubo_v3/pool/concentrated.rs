use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    convert::identity,
};

use ekubo_sdk::{
    chain::evm::{
        Evm, EvmConcentratedPool, EvmConcentratedPoolConstructionError, EvmConcentratedPoolKey,
        EvmConcentratedPoolResources, EvmConcentratedPoolState, EvmPoolKey, EvmTokenAmount,
    },
    math::tick::to_sqrt_ratio,
    quoting::{
        types::{Pool, QuoteParams, Tick, TokenAmount},
        util::find_nearest_initialized_tick_index,
    },
    U256,
};
use num_traits::Zero;
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{EkuboPool, EkuboPoolQuote};
use crate::{
    evm::protocol::ekubo_v3::attributes::ticks_from_attributes,
    protocol::errors::InvalidSnapshotError,
};

// Factor to account for computation inaccuracies due to not using tick bitmaps
const WEI_UNDERESTIMATION_FACTOR: i128 = 2;

// The names of the constants reflect the exact method from the tenderly log.
const TRANSFER_FROM_USER_GAS_COST: u64 = 40_000;
const START_PAYMENT_GAS_COST: u64 = 6_000;
const BASE_SWAP_GAS_COST: u64 = 19_665;
const WITHDRAW_GAS_COST: u64 = 37_500;
const COMPLETE_PAYMENT_GAS_COST: u64 = 2_000;

const GAS_COST_OF_ONE_INITIALIZED_TICK_CROSSED: u64 = 14_259;
const GAS_COST_OF_ONE_EXTRA_TICK_BITMAP_SLOAD: u64 = 2_000;
const GAS_COST_OF_ONE_EXTRA_MATH_ROUND: u64 = 4_076;

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct ConcentratedPool {
    imp: EvmConcentratedPool,
    swap_state: ConcentratedPoolSwapState,
}

#[derive(Debug, Clone, Copy, Eq, Serialize, Deserialize)]
struct ConcentratedPoolSwapState {
    sdk_state: EvmConcentratedPoolState,
    active_tick: Option<i32>,
}

impl ConcentratedPool {
    pub fn new(
        key: EvmConcentratedPoolKey,
        sdk_state: EvmConcentratedPoolState,
        tick: i32,
        ticks: Vec<Tick>,
    ) -> Result<Self, InvalidSnapshotError> {
        Ok(Self {
            imp: impl_from_state(key, sdk_state, ticks).map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating concentrated pool: {err:?}"))
            })?,
            swap_state: ConcentratedPoolSwapState { sdk_state, active_tick: Some(tick) },
        })
    }
}

impl EkuboPool for ConcentratedPool {
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

    fn quote(&self, token_amount: EvmTokenAmount) -> Result<EkuboPoolQuote, SimulationError> {
        self.imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit: None,
                override_state: Some(self.swap_state.sdk_state),
                meta: (),
            })
            .map(|quote| EkuboPoolQuote {
                consumed_amount: quote.consumed_amount,
                calculated_amount: quote.calculated_amount,
                gas: gas_costs(quote.execution_resources),
                new_state: Self {
                    imp: self.imp.clone(),
                    swap_state: ConcentratedPoolSwapState {
                        sdk_state: quote.state_after,
                        active_tick: None,
                    },
                }
                .into(),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        get_limit(token_in, self.sqrt_ratio(), &self.imp, self.swap_state.sdk_state, (), identity)
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        let updated_ticks = finish_transition(
            &mut self.swap_state.active_tick,
            &mut self.swap_state.sdk_state,
            self.imp.ticks(),
            &updated_attributes,
            &deleted_attributes,
        )?;

        if let Some(ticks) = updated_ticks {
            self.imp = impl_from_state(self.imp.key(), self.swap_state.sdk_state, ticks).map_err(
                |err| {
                    TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                        "reinstantiate base pool: {err:?}"
                    )))
                },
            )?;
        }

        Ok(())
    }
}

impl PartialEq for ConcentratedPool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() &&
            self.imp.ticks() == imp.ticks() &&
            self.swap_state == swap_state
    }
}

impl PartialEq for ConcentratedPoolSwapState {
    fn eq(&self, &Self { sdk_state, active_tick }: &Self) -> bool {
        self.sdk_state == sdk_state &&
            self.active_tick
                .zip(active_tick)
                .is_none_or(|(t1, t2)| t1 == t2)
    }
}

pub(super) fn impl_from_state(
    key: EvmConcentratedPoolKey,
    state: EvmConcentratedPoolState,
    ticks: Vec<Tick>,
) -> Result<EvmConcentratedPool, EvmConcentratedPoolConstructionError> {
    EvmConcentratedPool::new(key, state, ticks)
}

pub(super) fn gas_costs(
    EvmConcentratedPoolResources {
        extra_distinct_bitmap_lookups,
        initialized_ticks_crossed,
        no_override_price_change: _,
    }: EvmConcentratedPoolResources,
) -> u64 {
    let (extra_distinct_bitmap_lookups, initialized_ticks_crossed) =
        (u64::from(extra_distinct_bitmap_lookups), u64::from(initialized_ticks_crossed));

    START_PAYMENT_GAS_COST +
        BASE_SWAP_GAS_COST +
        WITHDRAW_GAS_COST +
        COMPLETE_PAYMENT_GAS_COST +
        TRANSFER_FROM_USER_GAS_COST +
        extra_distinct_bitmap_lookups * GAS_COST_OF_ONE_EXTRA_TICK_BITMAP_SLOAD +
        initialized_ticks_crossed * GAS_COST_OF_ONE_INITIALIZED_TICK_CROSSED +
        (initialized_ticks_crossed + extra_distinct_bitmap_lookups) *
            GAS_COST_OF_ONE_EXTRA_MATH_ROUND
}

pub(super) fn get_limit<P, S, M, R>(
    token_in: Address,
    sqrt_ratio: U256,
    imp: &P,
    state: S,
    meta: M,
    resources_fn: impl FnOnce(R) -> EvmConcentratedPoolResources,
) -> Result<i128, SimulationError>
where
    P: Pool<Address = Address, State = S, Meta = M, Resources = R>,
{
    let sqrt_ratio_limit = if token_in == imp.key().token0 {
        imp.min_tick_with_liquidity()
            .map_or(Ok(sqrt_ratio), |tick| {
                to_sqrt_ratio::<Evm>(tick)
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
                to_sqrt_ratio::<Evm>(tick)
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
                    i128::from(resources.extra_distinct_bitmap_lookups) +
                    1),
        )
        .max(0))
}

pub(super) fn finish_transition(
    active_tick: &mut Option<i32>,
    sdk_state: &mut EvmConcentratedPoolState,
    ticks: &[Tick],
    updated_attributes: &HashMap<String, Bytes>,
    deleted_attributes: &HashSet<String>,
) -> Result<Option<Vec<Tick>>, TransitionError> {
    let active_tick_update = updated_attributes
        .get("tick")
        .and_then(|updated_tick| {
            let updated_tick = updated_tick.clone().into();

            (*active_tick != Some(updated_tick)).then_some(updated_tick)
        });

    let changed_ticks = ticks_from_attributes(
        updated_attributes
            .iter()
            .map(|(key, value)| (key, Cow::Borrowed(value)))
            .chain(
                deleted_attributes
                    .iter()
                    .map(|key| (key, Cow::Owned(Bytes::new()))),
            ),
    )
    .map_err(TransitionError::DecodeError)?;

    let new_initialized_ticks = (!changed_ticks.is_empty()).then(|| {
        let mut ticks = ticks.to_vec();

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
        *active_tick = Some(new_active_tick);
    }

    if active_tick_update.is_some() || new_initialized_ticks.is_some() {
        sdk_state.active_tick_index = find_nearest_initialized_tick_index(
            new_initialized_ticks
                .as_deref()
                .unwrap_or(ticks),
            active_tick.ok_or_else(|| {
                TransitionError::MissingAttribute(
                    "concentrated state should always have an active tick during transitions"
                        .to_string(),
                )
            })?,
        );
    }

    Ok(new_initialized_ticks)
}
