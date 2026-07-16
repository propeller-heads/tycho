use std::collections::{HashMap, HashSet};

use evm_ekubo_sdk::{
    math::uint::U256,
    quoting::{
        self,
        full_range_pool::FullRangePoolState,
        types::{NodeKey, Pool, QuoteParams, TokenAmount},
    },
};
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{EkuboPool, EkuboPoolQuote};
use crate::protocol::errors::InvalidSnapshotError;

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct FullRangePool {
    imp: quoting::full_range_pool::FullRangePool,
    state: FullRangePoolState,
}

impl PartialEq for FullRangePool {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key() && self.state == other.state
    }
}

impl FullRangePool {
    const BASE_GAS_COST: u64 = 20_000;

    pub fn new(key: NodeKey, state: FullRangePoolState) -> Result<Self, InvalidSnapshotError> {
        Ok(Self {
            state,
            imp: quoting::full_range_pool::FullRangePool::new(key, state).map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating full range pool: {err:?}"))
            })?,
        })
    }

    pub(super) fn gas_costs() -> u64 {
        Self::BASE_GAS_COST
    }
}

impl EkuboPool for FullRangePool {
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
                meta: (),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("{err:?}")))?;

        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount: quote.calculated_amount,
            gas: Self::gas_costs(),
            new_state: Self { imp: self.imp.clone(), state: quote.state_after }.into(),
        })
    }

    fn get_limit(&self, token_in: U256) -> Result<i128, SimulationError> {
        Ok(self
            .imp
            .quote(QuoteParams {
                token_amount: TokenAmount { amount: i128::MAX, token: token_in },
                sqrt_ratio_limit: None,
                override_state: Some(self.state),
                meta: (),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err:?}")))?
            .consumed_amount)
    }

    fn finish_transition(
        &mut self,
        _updated_attributes: HashMap<String, Bytes>,
        _deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        Ok(())
    }
}
