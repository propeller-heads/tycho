//! A configurable hook for tests of the [`HookHandler`] extension points. It answers the
//! extension points the way its fields say and nothing else, so a test can exercise a trait
//! method through the pool without naming any protocol's hook.

use std::{any::Any, collections::HashMap};

use alloy::primitives::{Address, I256, U256};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{token::Token, Chain},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::Balances,
    },
    Bytes,
};

use crate::{
    evm::protocol::uniswap_v4::{
        hooks::{
            hook_handler::HookHandler,
            hook_handler_creator::{HookCreationParams, HookHandlerCreator},
            models::{
                AfterSwapDelta, AfterSwapParameters, BeforeSwapOutput, BeforeSwapParameters,
                HookSpotFees, StateContext, SwapOutcome, SwapParams, WithGasEstimate,
            },
        },
        state::{UniswapV4Fees, UniswapV4State},
    },
    protocol::errors::InvalidSnapshotError,
};

/// What [`ExtensionTestHook::spot_fees`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotFeesAnswer {
    /// `Ok(None)`: the hook states no fees.
    NoFees,
    /// `Ok(Some(..))`.
    Fees(HookSpotFees),
    /// `InvalidInput`: the hook refuses the pool's current terms.
    Refuses,
    /// `FatalError`: the hook cannot answer.
    Fails,
}

/// What [`ExtensionTestHook::handler_after_swap`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerAfterSwapAnswer {
    /// `Ok(None)`: the swap leaves the hook as it is, the trait's default.
    Keep,
    /// `Ok(Some(..))`: the hook of the same kind at this address.
    ReplaceWithAddress(Address),
    /// `InvalidInput`: the hook refuses the swap.
    Refuses,
    /// `FatalError`: the hook cannot answer.
    Fails,
}

/// What [`ExtensionTestHook::check_swap_outcome`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeCheckAnswer {
    /// `Ok(())`: the trait's default.
    Accepts,
    /// `InvalidInput` for a swap that leaves part of its input unfilled.
    RefusesUnfilledInput,
    /// `InvalidInput` for a swap that ends below this sqrt price.
    RefusesBelow(U256),
    /// `RecoverableError`, as a hook that cannot check would.
    Fails,
}

/// A hook with no permissions and no swap behaviour of its own, that answers the extension points
/// per its fields. The defaults leave every extension point at the trait's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTestHook {
    pub address: Address,
    pub spot_fees: SpotFeesAnswer,
    /// What `max_amount_in` answers, in either direction.
    pub max_amount_in: Option<U256>,
    /// What `check_swap_outcome` answers.
    pub outcome_check: OutcomeCheckAnswer,
    /// What `swaps_from_empty_range` answers.
    pub swaps_from_empty_range: bool,
    /// What `handler_after_swap` answers.
    pub handler_after_swap: HandlerAfterSwapAnswer,
}

impl Default for ExtensionTestHook {
    fn default() -> Self {
        Self {
            address: Address::ZERO,
            spot_fees: SpotFeesAnswer::NoFees,
            max_amount_in: None,
            outcome_check: OutcomeCheckAnswer::Accepts,
            swaps_from_empty_range: false,
            handler_after_swap: HandlerAfterSwapAnswer::Keep,
        }
    }
}

impl HookHandler for ExtensionTestHook {
    fn address(&self) -> Address {
        self.address
    }

    fn swaps_from_empty_range(&self) -> bool {
        self.swaps_from_empty_range
    }

    fn before_swap(
        &self,
        _: BeforeSwapParameters,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        Err(SimulationError::FatalError("the test hook has no beforeSwap".into()))
    }

    fn after_swap(
        &self,
        _: AfterSwapParameters,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        Err(SimulationError::FatalError("the test hook has no afterSwap".into()))
    }

    fn fee(&self, _: &UniswapV4State, _: SwapParams) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError("not implemented".into()))
    }

    fn spot_fees(&self, _: bool) -> Result<Option<HookSpotFees>, SimulationError> {
        match self.spot_fees {
            SpotFeesAnswer::NoFees => Ok(None),
            SpotFeesAnswer::Fees(fees) => Ok(Some(fees)),
            SpotFeesAnswer::Refuses => {
                Err(SimulationError::InvalidInput("the hook refuses the pool's terms".into(), None))
            }
            SpotFeesAnswer::Fails => {
                Err(SimulationError::FatalError("the hook cannot answer".into()))
            }
        }
    }

    fn max_amount_in(&self, _: &UniswapV4State, _: bool) -> Result<Option<U256>, SimulationError> {
        Ok(self.max_amount_in)
    }

    fn check_swap_outcome(&self, _: bool, outcome: &SwapOutcome) -> Result<(), SimulationError> {
        match self.outcome_check {
            OutcomeCheckAnswer::Fails => {
                Err(SimulationError::RecoverableError("the hook cannot check".into()))
            }
            OutcomeCheckAnswer::RefusesUnfilledInput if !outcome.amount_unfilled.is_zero() => {
                Err(SimulationError::InvalidInput("part of the input is unfilled".into(), None))
            }
            OutcomeCheckAnswer::RefusesBelow(floor) if outcome.sqrt_price_x96 < floor => {
                Err(SimulationError::InvalidInput("the swap ends below the floor".into(), None))
            }
            OutcomeCheckAnswer::Accepts |
            OutcomeCheckAnswer::RefusesUnfilledInput |
            OutcomeCheckAnswer::RefusesBelow(_) => Ok(()),
        }
    }

    fn handler_after_swap(
        &self,
        _: &StateContext,
        _: &SwapParams,
    ) -> Result<Option<Box<dyn HookHandler>>, SimulationError> {
        match self.handler_after_swap {
            HandlerAfterSwapAnswer::Keep => Ok(None),
            HandlerAfterSwapAnswer::ReplaceWithAddress(address) => Ok(Some(Box::new(Self {
                address,
                handler_after_swap: HandlerAfterSwapAnswer::Keep,
                ..self.clone()
            }))),
            HandlerAfterSwapAnswer::Refuses => {
                Err(SimulationError::InvalidInput("the hook refuses the swap".into(), None))
            }
            HandlerAfterSwapAnswer::Fails => {
                Err(SimulationError::FatalError("the hook cannot answer".into()))
            }
        }
    }

    fn delta_transition(
        &mut self,
        _: ProtocolStateDelta,
        _: &HashMap<Bytes, Token>,
        _: &Balances,
    ) -> Result<(), TransitionError> {
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn HookHandler> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_equal(&self, other: &dyn HookHandler) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

/// A creator that hands out a clone of its hook.
pub struct ExtensionTestHookCreator(pub(crate) ExtensionTestHook);

impl HookHandlerCreator for ExtensionTestHookCreator {
    fn instantiate_hook_handler(
        &self,
        _: HookCreationParams<'_>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        Ok(Box::new(self.0.clone()))
    }
}

/// The chain the decoder tests register their test hook on.
pub const TEST_HOOK_CHAIN: Chain = Chain::Ethereum;

/// A hook whose swaps write none of its own storage answers `handler_after_swap` with `None`, the
/// trait's default, for an exact-input swap of 1000 units of `currency_0`. Panics if `hook`
/// answers anything else.
pub fn assert_handler_after_swap_keeps_the_hook(hook: &dyn HookHandler) {
    let context = StateContext {
        currency_0: Address::repeat_byte(1),
        currency_1: Address::repeat_byte(2),
        fees: UniswapV4Fees::new(0, 0, 3000),
        tick_spacing: 60,
        sqrt_price_x96: U256::from(1u64) << 96,
        tick: 0,
    };
    let swap_params = SwapParams {
        zero_for_one: true,
        amount_specified: I256::try_from(-1_000).unwrap(),
        sqrt_price_limit: U256::ZERO,
    };

    let next = hook
        .handler_after_swap(&context, &swap_params)
        .unwrap();

    assert!(next.is_none());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook that does not override `handler_after_swap` leaves its handler as it is.
    #[test]
    fn test_handler_after_swap_defaults_to_keeping_the_hook() {
        assert_handler_after_swap_keeps_the_hook(&ExtensionTestHook::default());
    }
}
