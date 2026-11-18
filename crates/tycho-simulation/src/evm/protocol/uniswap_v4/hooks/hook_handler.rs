use std::{collections::HashMap, fmt::Debug, sync::Arc};

use alloy::primitives::{Address, U256};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext},
    },
    Bytes,
};

use crate::evm::{
    protocol::uniswap_v4::{
        hooks::{
            models::{
                AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapOutput,
                BeforeSwapParameters, HookSpotFees, SwapOutcome, SwapParams, WithGasEstimate,
            },
            utils::{has_permission, HookOptions},
        },
        state::{UniswapV4State, PM_PER_HOOK_CALL_OVERHEAD},
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

    /// Whether the pool manager calls the hook for `option`.
    ///
    /// Uniswap V4 encodes a hook's permissions in the low bits of its address, which is what the
    /// default reads. A hook whose pool manager takes the permissions from somewhere else, such as
    /// the `parameters` bitmap of a PancakeSwap Infinity pool key, overrides this.
    fn has_permission(&self, option: HookOptions) -> bool {
        has_permission(self.address(), option)
    }

    /// Advances the hook to the block a quote is expected to execute in.
    ///
    /// Returns `true` when the hook's quoting behavior changed, so the pool must be re-emitted.
    /// Hooks whose behavior does not depend on the block keep the default, which does nothing.
    fn apply_block(&mut self, _block: &BlockContext) -> bool {
        false
    }

    /// Whether the hook lets the pool swap at all in the current execution block. While this is
    /// `false` the pool offers no liquidity.
    fn swaps_enabled(&self) -> bool {
        true
    }

    /// Whether the pool swaps through ranges with no active liquidity, as v4-core's `Pool.swap`
    /// does on chain (it has no liquidity check): from such a range into the liquidity ahead, and
    /// past the last initialized tick on to the price limit, where the input left over reaches
    /// [`HookHandler::check_swap_outcome`] as unfilled.
    ///
    /// The default is `false`: such a pool refuses to swap from zero active liquidity, matching
    /// `get_limits`, which reports a zero limit there, and refuses an input that runs past the
    /// last initialized tick with `InvalidInput("Ticks exceeded")`. A hook returning `true` must
    /// also answer `Some` from [`HookHandler::max_amount_in`], so that limits and quotes agree
    /// on that state; `get_limits` fails with `FatalError` for a hook that does not.
    fn swaps_from_empty_range(&self) -> bool {
        false
    }

    /// Gas the pool manager spends on each call to this hook beyond what the hook's own gas
    /// estimates cover: encoding the call, the call itself and decoding the result.
    ///
    /// The default is the overhead measured for the Uniswap V4 PoolManager dispatching to a
    /// VM-simulated hook. A hook whose estimates already cover the whole call returns zero.
    fn pool_manager_call_overhead(&self) -> u64 {
        PM_PER_HOOK_CALL_OVERHEAD
    }

    /// Simulates the beforeSwap Solidity behaviour.
    ///
    /// An `InvalidInput` refuses this one swap, as a revert of the hook on this input would, and
    /// `get_amount_out` returns it to the caller unchanged; any other error means the hook
    /// cannot be simulated, and `get_amount_out` reports it as `FatalError`.
    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError>;

    /// Simulates the afterSwap Solidity behaviour.
    ///
    /// Errors are classified as for [`HookHandler::before_swap`]: an `InvalidInput` refuses this
    /// one swap and reaches the caller unchanged, and any other error becomes `FatalError`.
    fn after_swap(
        &self,
        params: AfterSwapParameters,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
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
    /// A hook that returns `Some` from [`HookHandler::spot_fees`] or from
    /// [`HookHandler::unspecified_fee_amount`] is priced from that answer and is never asked for a
    /// spot price, so such a hook does not need to implement this method meaningfully.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError>;

    /// The fees a marginal exact-input swap in the direction given by `zero_for_one` pays: the LP
    /// fee the hook makes the pool charge, and the hook's own fee rate.
    ///
    /// A hook that answers is priced as the pool's buy price at that LP fee, marked up by the
    /// hook's fee rate. That one markup serves a fee taken from the input and a fee taken from
    /// the output alike, since at the margin both scale the price by `1 / (1 - rate)`.
    /// `spot_price` consults this method first, so a hook that answers is never asked for
    /// [`HookHandler::unspecified_fee_amount`] or [`HookHandler::spot_price`] when pricing.
    ///
    /// The default returns `Ok(None)`. An `Err` stops the pool from pricing at all. An
    /// `InvalidInput` for `zero_for_one = true` says the hook refuses the pool's current terms:
    /// the decoder then keeps the pool without probing its price, so a later delta can change
    /// the terms. Any other `Err` fails the decode. `query_pool_swap`, which runs the pool's swap
    /// math without the hook, refuses a direction this method answers `Some` for.
    fn spot_fees(&self, _zero_for_one: bool) -> Result<Option<HookSpotFees>, SimulationError> {
        Ok(None)
    }

    /// The largest exact input the pool can swap in the direction given by `zero_for_one` that
    /// the hook lets settle, given the pool's current `state`.
    ///
    /// When it answers `Some`, `get_limits` reports that input and the output a swap of it
    /// returns, and does not derive limits from the pool's liquidity or from
    /// [`HookHandler::get_amount_ranges`]. `get_limits` asks only while
    /// [`HookHandler::swaps_enabled`] holds. A hook whose
    /// [`HookHandler::swaps_from_empty_range`] is `true` must answer `Some` here. The default
    /// returns `Ok(None)`.
    fn max_amount_in(
        &self,
        _state: &UniswapV4State,
        _zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        Ok(None)
    }

    /// Checks the pool's swap result the way the hook's `afterSwap` checks it on chain, before
    /// `after_swap` runs: an `Err` refuses the swap, as a revert would.
    ///
    /// `outcome` holds the price the pool's swap math ended at and the part of the input it
    /// could not swap. The default accepts every outcome.
    fn check_swap_outcome(
        &self,
        _zero_for_one: bool,
        _outcome: &SwapOutcome,
    ) -> Result<(), SimulationError> {
        Ok(())
    }

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
    /// Every `spot_price` of a hooked pool whose [`HookHandler::spot_fees`] returns `None` calls
    /// this method next, so an `Err` here stops the pool from pricing at all. Return `Ok(None)`
    /// for "this hook cannot answer analytically"; reserve `Err` for a genuine failure.
    fn unspecified_fee_amount(
        &self,
        _unspecified: U256,
        _zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        Ok(None)
    }

    /// The hook's minimum and maximum swap amounts for the pair, which `get_limits` consults for
    /// a pool with no liquidity and no ticks. It is never asked when
    /// [`HookHandler::max_amount_in`] answers `Some`, which supersedes it.
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
