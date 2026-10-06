use std::{collections::HashMap, fmt::Debug, sync::Arc};

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

use crate::evm::{
    protocol::uniswap_v4::{
        hooks::models::{
            AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapOutput,
            BeforeSwapParameters, SwapParams, WithGasEstimate,
        },
        state::UniswapV4State,
    },
    simulation::PendingOverrides,
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

    /// Runs every later call under a pending block's storage, native balances and block
    /// environment; a caller's own overwrites take precedence slot by slot. Hooks that do not
    /// simulate a contract ignore it.
    fn set_pending_overrides(&mut self, _overrides: Arc<PendingOverrides>) {}

    // Currently fee is not accessible on v4 pools, this is for future use
    // as soon as we adapt the ProtocolSim interface
    fn fee(&self, context: &UniswapV4State, params: SwapParams) -> Result<f64, SimulationError>;

    /// Hooks will likely modify spot price behaviour this function
    /// allows overriding it.
    ///
    /// A hook that returns `Some` from [`HookHandler::unspecified_fee_amount`] is priced as the
    /// pool's own buy price marked up by that fee rate and is never asked for a spot price, so
    /// such a hook does not need to implement this method meaningfully.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError>;

    /// Amount of the unspecified currency the hook takes out of a swap that would otherwise
    /// settle `unspecified` of it, in the direction given by `zero_for_one`.
    ///
    /// "Unspecified" is Uniswap V4's name for the leg of a swap whose amount the pool computes,
    /// as opposed to the leg the swapper fixes: the output of an exact-input swap and the input
    /// of an exact-output swap. Which of the two currencies that is does not follow from the
    /// token order, only from the swap kind and the direction. Tycho quotes exact-input swaps
    /// only, so callers pass the swap's output amount and the returned amount comes off that
    /// output. In an exact-output swap the unspecified leg is instead the input, and the fee is
    /// charged on top of what the swapper pays in.
    ///
    /// `None` means the hook does not model its fee as a pure function of that amount, and the
    /// caller has to simulate a swap to learn what it charges. The default returns `None`.
    ///
    /// Every `spot_price` of a hooked pool calls this method first, so an `Err` here stops the
    /// pool from pricing at all. Return `Ok(None)` for "this hook cannot answer analytically";
    /// reserve `Err` for a genuine failure.
    fn unspecified_fee_amount(
        &self,
        _unspecified: U256,
        _zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        Ok(None)
    }

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
    fn clone_box(&self) -> Box<dyn HookHandler>;

    fn as_any(&self) -> &dyn std::any::Any;

    fn is_equal(&self, other: &dyn HookHandler) -> bool;
}

impl Clone for Box<dyn HookHandler> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
