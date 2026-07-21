pub mod boosted_fees;
pub mod concentrated;
pub mod full_range;
pub mod mev_capture;
pub mod oracle;
pub mod stableswap;
mod timed;
pub mod twamm;
pub mod ve33;

use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{EvmPoolKey, EvmTokenAmount},
    U256,
};
use revm::primitives::Address;
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::state::EkuboV3State;
pub struct EkuboPoolQuote {
    pub consumed_amount: i128,
    pub calculated_amount: u128,
    pub gas: u64,
    pub new_state: EkuboV3State,
}

#[enum_delegate::register]
pub trait EkuboPool {
    fn key(&self) -> EvmPoolKey;
    fn sqrt_ratio(&self) -> U256;

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256);
    fn set_liquidity(&mut self, liquidity: u128);

    fn finish_transition(
        &mut self,
        updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError>;

    fn quote(
        &self,
        token_amount: EvmTokenAmount,
    ) -> Result<super::pool::EkuboPoolQuote, SimulationError>;
    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError>;
}
