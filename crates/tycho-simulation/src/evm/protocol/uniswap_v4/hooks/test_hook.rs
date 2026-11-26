//! A configurable hook for tests of the [`HookHandler`] extension points. It answers the
//! extension points the way its fields say and nothing else, so a test can exercise a trait
//! method through the pool without naming any protocol's hook.

use std::{any::Any, collections::HashMap};

use alloy::primitives::{Address, I128, U256};
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
                AfterSwapDelta, AfterSwapParameters, BalanceDelta, BeforeSwapDelta,
                BeforeSwapOutput, BeforeSwapParameters, HookSpotFees, StateContext, SwapOutcome,
                SwapParams, WithGasEstimate,
            },
            utils::HookOptions,
        },
        state::UniswapV4State,
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
    /// With a gas estimate, the hook has `beforeSwap` with a returned delta and takes the whole
    /// input of an exact-input swap, paying the same amount of the other token.
    pub takes_whole_input: Option<u64>,
}

/// The gas estimate of [`ExtensionTestHook::takes_whole_input`] in the tests that use it.
pub const WHOLE_INPUT_TEST_HOOK_GAS: u64 = 7;

impl Default for ExtensionTestHook {
    fn default() -> Self {
        Self {
            address: Address::ZERO,
            spot_fees: SpotFeesAnswer::NoFees,
            max_amount_in: None,
            outcome_check: OutcomeCheckAnswer::Accepts,
            swaps_from_empty_range: false,
            handler_after_swap: HandlerAfterSwapAnswer::Keep,
            takes_whole_input: None,
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

    fn has_permission(&self, option: HookOptions) -> bool {
        self.takes_whole_input.is_some() &&
            matches!(option, HookOptions::BeforeSwap | HookOptions::BeforeSwapReturnsDelta)
    }

    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        let Some(gas_estimate) = self.takes_whole_input else {
            return Err(SimulationError::FatalError("the test hook has no beforeSwap".into()));
        };
        let amount = u128::try_from(
            params
                .swap_params
                .amount_specified
                .unsigned_abs(),
        )
        .ok()
        .and_then(|amount| I128::try_from(amount).ok())
        .ok_or_else(|| SimulationError::FatalError("the test amount fits int128".into()))?;
        Ok(WithGasEstimate {
            gas_estimate,
            result: BeforeSwapOutput {
                amount_delta: BeforeSwapDelta(BalanceDelta::new(amount, -amount).as_i256()),
                fee: Default::default(),
                overwrites: HashMap::new(),
                transient_storage: HashMap::new(),
            },
        })
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
