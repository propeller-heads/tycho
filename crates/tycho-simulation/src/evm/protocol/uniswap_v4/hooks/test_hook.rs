//! A configurable hook for tests of the [`HookHandler`] extension points. It answers the
//! extension points the way its fields say and nothing else, so a test can exercise a trait
//! method through the pool without naming any protocol's hook.

use std::{any::Any, collections::HashMap};

use alloy::primitives::{Address, U256};
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
                AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapOutput,
                BeforeSwapParameters, HookSpotFees, SwapOutcome, SwapParams, WithGasEstimate,
            },
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

/// A hook with no permissions and no swap behaviour of its own, that answers the extension points
/// per its fields. The defaults leave every extension point at the trait's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionTestHook {
    pub address: Address,
    pub spot_fees: SpotFeesAnswer,
    /// What `max_amount_in` answers, in either direction.
    pub max_amount_in: Option<U256>,
    /// `check_swap_outcome` refuses a swap that ends below this sqrt price.
    pub refuse_outcome_below: Option<U256>,
    /// `check_swap_outcome` refuses a swap that leaves part of its input unfilled.
    pub refuse_unfilled_input: bool,
    /// What `swaps_from_empty_range` answers.
    pub swaps_from_empty_range: bool,
}

impl Default for ExtensionTestHook {
    fn default() -> Self {
        Self {
            address: Address::ZERO,
            spot_fees: SpotFeesAnswer::NoFees,
            max_amount_in: None,
            refuse_outcome_below: None,
            refuse_unfilled_input: false,
            swaps_from_empty_range: false,
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

    fn spot_price(&self, _: &Token, _: &Token) -> Result<f64, SimulationError> {
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
        if self.refuse_unfilled_input && !outcome.amount_unfilled.is_zero() {
            return Err(SimulationError::InvalidInput("part of the input is unfilled".into(), None));
        }
        match self.refuse_outcome_below {
            Some(floor) if outcome.sqrt_price_x96 < floor => {
                Err(SimulationError::InvalidInput("the swap ends below the floor".into(), None))
            }
            _ => Ok(()),
        }
    }

    fn get_amount_ranges(&self, _: Bytes, _: Bytes) -> Result<AmountRanges, SimulationError> {
        Err(SimulationError::RecoverableError("not implemented".into()))
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
