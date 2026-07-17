use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{
        EvmFullRangePool, EvmFullRangePoolResources, EvmFullRangePoolState, EvmPoolKey,
        EvmTokenAmount,
    },
    quoting::{
        pools::full_range::{FullRangePoolKey, FullRangePoolState},
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
use crate::protocol::errors::InvalidSnapshotError;

const BASE_GAS_COST: u64 = 15_920;

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct FullRangePool {
    imp: EvmFullRangePool,
    swap_state: FullRangePoolSwapState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FullRangePoolSwapState {
    sdk_state: EvmFullRangePoolState,
}

impl FullRangePool {
    pub fn new(
        key: FullRangePoolKey,
        sdk_state: FullRangePoolState,
    ) -> Result<Self, InvalidSnapshotError> {
        EvmFullRangePool::new(key, sdk_state)
            .map(|imp| Self { swap_state: FullRangePoolSwapState { sdk_state }, imp })
            .map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating full range pool: {err:?}"))
            })
    }
}

impl EkuboPool for FullRangePool {
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
        self.imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(self.swap_state.sdk_state),
                meta: (),
            })
            .map(|quote| EkuboPoolQuote {
                consumed_amount: quote.consumed_amount,
                calculated_amount: quote.calculated_amount,
                gas: gas_costs(quote.execution_resources),
                new_state: Self {
                    imp: self.imp.clone(),
                    swap_state: FullRangePoolSwapState { sdk_state: quote.state_after },
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
                override_state: Some(self.swap_state.sdk_state),
                meta: (),
            })
            .map(|quote| quote.consumed_amount)
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err:?}")))
    }

    fn finish_transition(
        &mut self,
        _updated_attributes: HashMap<String, Bytes>,
        _deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        Ok(())
    }
}

impl PartialEq for FullRangePool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() && self.swap_state == swap_state
    }
}

pub(super) fn gas_costs(
    EvmFullRangePoolResources { no_override_price_change: _ }: EvmFullRangePoolResources,
) -> u64 {
    BASE_GAS_COST
}
