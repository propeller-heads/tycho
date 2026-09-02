use std::{collections::HashMap, fmt::Debug};

use alloy::primitives::{Address, U256};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::Balances,
    },
    Bytes,
};

use crate::evm::protocol::uniswap_v4::{
    hooks::models::{
        AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapOutput, BeforeSwapParameters,
        SwapParams, WithGasEstimate,
    },
    state::UniswapV4State,
};

/// Trait for simulating the swap-related behavior of Uniswap V4 hooks.
/// https://github.com/Uniswap/v4-core/blob/main/src/interfaces/IHooks.sol
///
/// Implementations of this trait should encapsulate any custom logic tied to hook execution,
/// including spot price adjustments, swap constraints, and state transitions.
pub trait HookHandler: Debug + Send + Sync + 'static {
    fn address(&self) -> Address;
    /// Simulates the beforeSwap Solidity behaviour
    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError>;

    /// Simulates the afterSwap Solidity behaviour
    fn after_swap(
        &self,
        params: AfterSwapParameters,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        transient_storage_params: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError>;

    // Currently fee is not accessible on v4 pools, this is for future use
    // as soon as we adapt the ProtocolSim interface
    fn fee(&self, context: &UniswapV4State, params: SwapParams) -> Result<f64, SimulationError>;

    /// Hooks will likely modify spot price behaviour this function
    /// allows overriding it.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError>;

    // Advanced version also returning minimum swap amounts for future compatability
    // with updated ProtocolSim interface
    fn get_amount_ranges(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<AmountRanges, SimulationError>;

    // Called on each state update to update the internal state of the HookHandler
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError>;

    /// Whether this handler can serve the block `delta` describes after
    /// [`delta_transition`](Self::delta_transition), reading nothing beyond its arguments.
    ///
    /// Mirrors
    /// [`ProtocolSim::transitions_from_delta_alone`](tycho_common::simulation::protocol_sim::ProtocolSim::transitions_from_delta_alone)
    /// for the hook half of a pool: a
    /// handler that simulates against the shared VM database returns `false`, because that
    /// database only ever holds confirmed state.
    #[allow(unused)]
    fn transitions_from_delta_alone(&self, delta: &ProtocolStateDelta) -> bool {
        false
    }

    fn clone_box(&self) -> Box<dyn HookHandler>;

    fn as_any(&self) -> &dyn std::any::Any;

    fn is_equal(&self, other: &dyn HookHandler) -> bool;
}

impl Clone for Box<dyn HookHandler> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
