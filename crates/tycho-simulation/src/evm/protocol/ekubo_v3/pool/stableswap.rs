use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{
        EvmPoolKey, EvmStableswapPool, EvmStableswapPoolKey, EvmStableswapPoolState, EvmTokenAmount,
    },
    quoting::types::{Pool, QuoteParams, TokenAmount},
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

// The names of the constants reflect the exact method from the tenderly log.
const START_PAYMENT_GAS_COST: u64 = 6_000;
const BASE_SWAP_GAS_COST: u64 = 17_400;
const WITHDRAW_GAS_COST: u64 = 16_500;
const COMPLETE_PAYMENT_GAS_COST: u64 = 2_000;
const TRANSFER_FROM_USER_GAS_COST: u64 = 20_000;
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct StableswapPool {
    imp: EvmStableswapPool,
    swap_state: StableswapPoolSwapState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct StableswapPoolSwapState {
    sdk_state: EvmStableswapPoolState,
}

impl StableswapPool {
    pub fn new(
        key: EvmStableswapPoolKey,
        sdk_state: EvmStableswapPoolState,
    ) -> Result<Self, InvalidSnapshotError> {
        EvmStableswapPool::new(key, sdk_state)
            .map(|imp| Self { swap_state: StableswapPoolSwapState { sdk_state }, imp })
            .map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating stableswap pool: {err:?}"))
            })
    }

    pub(super) fn gas_costs() -> u64 {
        START_PAYMENT_GAS_COST +
            BASE_SWAP_GAS_COST +
            WITHDRAW_GAS_COST +
            COMPLETE_PAYMENT_GAS_COST +
            TRANSFER_FROM_USER_GAS_COST
    }
}

impl EkuboPool for StableswapPool {
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
                gas: Self::gas_costs(),
                new_state: Self {
                    imp: self.imp.clone(),
                    swap_state: StableswapPoolSwapState { sdk_state: quote.state_after },
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

impl PartialEq for StableswapPool {
    fn eq(&self, &Self { ref imp, swap_state }: &Self) -> bool {
        self.imp.key() == imp.key() && self.swap_state == swap_state
    }
}
