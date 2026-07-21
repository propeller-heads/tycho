use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{Evm, EvmPoolKey, EvmTokenAmount},
    math::swap::{amount_before_fee, compute_fee},
    U256,
};
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{
    concentrated::ConcentratedPool, full_range::FullRangePool, stableswap::StableswapPool,
    EkuboPool, EkuboPoolQuote,
};
use crate::{evm::protocol::ekubo_v3::state::EkuboV3State, protocol::errors::InvalidSnapshotError};

const GAS_COST_OF_FEE_ACCUMULATION: u64 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ve33UnderlyingPool {
    Concentrated(ConcentratedPool),
    FullRange(FullRangePool),
    Stableswap(StableswapPool),
}

impl Ve33UnderlyingPool {
    fn from_state(state: EkuboV3State) -> Result<Self, SimulationError> {
        match state {
            EkuboV3State::Concentrated(pool) => Ok(Self::Concentrated(pool)),
            EkuboV3State::FullRange(pool) => Ok(Self::FullRange(pool)),
            EkuboV3State::Stableswap(pool) => Ok(Self::Stableswap(pool)),
            _ => Err(SimulationError::FatalError(
                "Ve33 underlying quote returned an unexpected pool type".to_string(),
            )),
        }
    }
}

impl EkuboPool for Ve33UnderlyingPool {
    fn key(&self) -> EvmPoolKey {
        match self {
            Self::Concentrated(pool) => pool.key(),
            Self::FullRange(pool) => pool.key(),
            Self::Stableswap(pool) => pool.key(),
        }
    }

    fn sqrt_ratio(&self) -> U256 {
        match self {
            Self::Concentrated(pool) => pool.sqrt_ratio(),
            Self::FullRange(pool) => pool.sqrt_ratio(),
            Self::Stableswap(pool) => pool.sqrt_ratio(),
        }
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        match self {
            Self::Concentrated(pool) => pool.set_sqrt_ratio(sqrt_ratio),
            Self::FullRange(pool) => pool.set_sqrt_ratio(sqrt_ratio),
            Self::Stableswap(pool) => pool.set_sqrt_ratio(sqrt_ratio),
        }
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        match self {
            Self::Concentrated(pool) => pool.set_liquidity(liquidity),
            Self::FullRange(pool) => pool.set_liquidity(liquidity),
            Self::Stableswap(pool) => pool.set_liquidity(liquidity),
        }
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        match self {
            Self::Concentrated(pool) => {
                pool.finish_transition(updated_attributes, deleted_attributes)
            }
            Self::FullRange(pool) => pool.finish_transition(updated_attributes, deleted_attributes),
            Self::Stableswap(pool) => {
                pool.finish_transition(updated_attributes, deleted_attributes)
            }
        }
    }

    fn quote(&self, token_amount: EvmTokenAmount) -> Result<EkuboPoolQuote, SimulationError> {
        match self {
            Self::Concentrated(pool) => pool.quote(token_amount),
            Self::FullRange(pool) => pool.quote(token_amount),
            Self::Stableswap(pool) => pool.quote(token_amount),
        }
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        match self {
            Self::Concentrated(pool) => pool.get_limit(token_in),
            Self::FullRange(pool) => pool.get_limit(token_in),
            Self::Stableswap(pool) => pool.get_limit(token_in),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ve33Pool {
    underlying_pool: Ve33UnderlyingPool,
    swap_fee: u64,
}

impl Ve33Pool {
    pub fn new(
        underlying_pool: Ve33UnderlyingPool,
        swap_fee: u64,
    ) -> Result<Self, InvalidSnapshotError> {
        let key = underlying_pool.key();
        if key.config.fee != 0 {
            return Err(InvalidSnapshotError::ValueError(
                "Ve33 pool must have a zero Core fee".to_string(),
            ));
        }
        if key.config.extension == Address::ZERO {
            return Err(InvalidSnapshotError::ValueError(
                "Ve33 pool must have an extension".to_string(),
            ));
        }

        Ok(Self { underlying_pool, swap_fee })
    }

    pub fn swap_fee(&self) -> u64 {
        self.swap_fee
    }
}

impl EkuboPool for Ve33Pool {
    fn key(&self) -> EvmPoolKey {
        self.underlying_pool.key()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.underlying_pool.sqrt_ratio()
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.underlying_pool
            .set_sqrt_ratio(sqrt_ratio);
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.underlying_pool
            .set_liquidity(liquidity);
    }

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        if let Some(swap_fee) = updated_attributes.get("swap_fee") {
            self.swap_fee = u64::from_be_bytes(
                swap_fee
                    .as_ref()
                    .try_into()
                    .map_err(|err| {
                        TransitionError::DecodeError(format!("swap_fee length mismatch: {err:?}"))
                    })?,
            );
        }

        self.underlying_pool
            .finish_transition(updated_attributes, deleted_attributes)
    }

    fn quote(&self, token_amount: EvmTokenAmount) -> Result<EkuboPoolQuote, SimulationError> {
        let quote = self
            .underlying_pool
            .quote(token_amount)?;
        let calculated_amount = if self.swap_fee == 0 {
            quote.calculated_amount
        } else if token_amount.amount >= 0 {
            quote.calculated_amount - compute_fee::<Evm>(quote.calculated_amount, self.swap_fee)
        } else {
            amount_before_fee::<Evm>(quote.calculated_amount, self.swap_fee).ok_or_else(|| {
                SimulationError::RecoverableError(
                    "Ve33 exact-output fee computation overflowed".to_string(),
                )
            })?
        };

        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount,
            gas: quote.gas + u64::from(self.swap_fee != 0) * GAS_COST_OF_FEE_ACCUMULATION,
            new_state: Self {
                underlying_pool: Ve33UnderlyingPool::from_state(quote.new_state)?,
                swap_fee: self.swap_fee,
            }
            .into(),
        })
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        self.underlying_pool.get_limit(token_in)
    }
}

#[cfg(test)]
mod tests {
    use ekubo_sdk::quoting::{
        pools::full_range::{FullRangePoolKey, FullRangePoolState, FullRangePoolTypeConfig},
        types::PoolConfig,
    };
    use revm::primitives::address;

    use super::*;

    const TOKEN0: Address = address!("0x0000000000000000000000000000000000000001");
    const TOKEN1: Address = address!("0x0000000000000000000000000000000000000002");
    const SWAP_FEE: u64 = u64::MAX / 100;

    fn pool() -> Ve33Pool {
        Ve33Pool::new(
            Ve33UnderlyingPool::FullRange(
                FullRangePool::new(
                    FullRangePoolKey {
                        token0: TOKEN0,
                        token1: TOKEN1,
                        config: PoolConfig {
                            fee: 0,
                            extension: address!("0xd100000000000000000000000000000000000000"),
                            pool_type_config: FullRangePoolTypeConfig,
                        },
                    },
                    FullRangePoolState {
                        sqrt_ratio: U256::from_limbs([0, 0, 1, 0]),
                        liquidity: 1_000_000,
                    },
                )
                .unwrap(),
            ),
            SWAP_FEE,
        )
        .unwrap()
    }

    #[test]
    fn exact_output_adds_fee_to_input() {
        let pool = pool();
        let token_amount = EvmTokenAmount { token: TOKEN1, amount: -100 };
        let underlying_quote = pool
            .underlying_pool
            .quote(token_amount)
            .unwrap();
        let quote = pool.quote(token_amount).unwrap();

        assert_eq!(quote.consumed_amount, underlying_quote.consumed_amount);
        assert_eq!(
            quote.calculated_amount,
            amount_before_fee::<Evm>(underlying_quote.calculated_amount, SWAP_FEE).unwrap()
        );
    }
}
