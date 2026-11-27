use std::{collections::HashMap, ops::Sub};

use alloy::{
    primitives::{aliases::U24, Address, I128, I256, U256},
    sol,
};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::protocol::uniswap_v4::{
    hooks::utils::{get_lower_i128, get_upper_i128, to_int128},
    state::UniswapV4Fees,
};

#[derive(Debug, Clone)]
pub struct StateContext {
    pub currency_0: Address,
    pub currency_1: Address,
    pub fees: UniswapV4Fees,
    pub tick_spacing: i32,
    /// The pool's square-root price before the swap, as a Q64.96.
    pub sqrt_price_x96: U256,
    /// The pool's current tick before the swap.
    pub tick: i32,
}

#[derive(Debug, Clone)]
pub struct SwapParams {
    pub zero_for_one: bool,
    pub amount_specified: I256,
    pub sqrt_price_limit: U256,
}

#[derive(Debug)]
pub struct BeforeSwapParameters {
    pub context: StateContext,
    pub sender: Address,
    pub swap_params: SwapParams,
    pub hook_data: Bytes,
}

/// Replicating https://github.com/Uniswap/v4-core/blob/59d3ecf53afa9264a16bba0e38f4c5d2231f80bc/src/types/BeforeSwapDelta.sol#L6
/// Upper 128 bits is the delta in specified tokens. Lower 128 bits is delta in unspecified tokens
/// (to match the afterSwap hook)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeforeSwapDelta(pub I256);

impl BeforeSwapDelta {
    /// This is the specified delta returned by beforeSwap
    pub fn get_specified_delta(self) -> I128 {
        get_upper_i128(self.0)
    }

    /// This is the unspecified delta returned by beforeSwap and afterSwap
    pub fn get_unspecified_delta(self) -> I128 {
        get_lower_i128(self.0)
    }

    pub fn as_i256(&self) -> I256 {
        self.0
    }
}

/// Replicating https://github.com/Uniswap/v4-core/blob/main/src/types/BalanceDelta.sol#L8
/// Two `I128` values packed into a single `I256` where the upper 128 bits represent the amount0
/// and the lower 128 bits represent the amount1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceDelta(pub I256);

impl BalanceDelta {
    pub fn new(amount0: I128, amount1: I128) -> BalanceDelta {
        let specified_shifted = I256::from(amount0) << 128;
        let unspecified_mask = I256::from_raw(U256::from_limbs([u64::MAX, u64::MAX, 0, 0]));
        let unspecified_masked = I256::from(amount1) & unspecified_mask;
        BalanceDelta(specified_shifted | unspecified_masked)
    }

    /// Creates a BalanceDelta from swap result's amount_calculated based on swap direction
    ///
    /// For exact input swaps:
    /// - amount_calculated is negative (amount of output token received)
    /// - We convert it to positive for the appropriate amount field
    /// - The other amount field is set to zero
    ///
    /// An output past `int128` keeps only its low 128 bits, where v4's `Pool.swap` reverts;
    /// [`Self::try_from_swap_result`] refuses it.
    pub fn from_swap_result(amount_calculated: I256, zero_for_one: bool) -> BalanceDelta {
        // For exact input swaps, amount_calculated is negative (output amount from pool's
        // perspective) We negate it to convert to positive amount from user's perspective
        if zero_for_one {
            // Swapping token0 for token1: amount_calculated represents change in token1
            BalanceDelta::new(I128::ZERO, get_lower_i128(-amount_calculated))
        } else {
            // Swapping token1 for token0: amount_calculated represents change in token0
            BalanceDelta::new(get_lower_i128(-amount_calculated), I128::ZERO)
        }
    }

    /// The balance delta of an exact-input swap whose output, from the pool's side, is
    /// `amount_calculated` (negative): the output, positive, in the bought token's lane and 0 in
    /// the other.
    ///
    /// Refuses with `InvalidInput` an output past `int128`, on which v4's `Pool.swap` reverts.
    pub fn try_from_swap_result(
        amount_calculated: I256,
        zero_for_one: bool,
    ) -> Result<BalanceDelta, SimulationError> {
        let amount_out = to_int128(-amount_calculated, "The swap's output")?;
        Ok(if zero_for_one {
            BalanceDelta::new(I128::ZERO, amount_out)
        } else {
            BalanceDelta::new(amount_out, I128::ZERO)
        })
    }

    pub fn as_i256(&self) -> I256 {
        self.0
    }

    pub fn amount0(self) -> I128 {
        get_upper_i128(self.0)
    }

    pub fn amount1(self) -> I128 {
        get_lower_i128(self.0)
    }

    /// `self - rhs`, lane by lane, as v4-core's `BalanceDelta` `sub` computes it.
    ///
    /// Refuses with `InvalidInput` a lane whose difference is past `int128`, on which that
    /// `sub` reverts `SafeCastOverflow`.
    pub fn checked_sub(self, rhs: BalanceDelta) -> Result<BalanceDelta, SimulationError> {
        let lane = |a: I128, b: I128, name: &str| {
            a.checked_sub(b).ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!("{name} of {a} - {b} does not fit an int128: the pool manager reverts"),
                    None,
                )
            })
        };
        Ok(BalanceDelta::new(
            lane(self.amount0(), rhs.amount0(), "amount0")?,
            lane(self.amount1(), rhs.amount1(), "amount1")?,
        ))
    }
}

/// Lane by lane, wrapping a lane past `int128` where v4-core's `sub` reverts;
/// [`BalanceDelta::checked_sub`] refuses it.
impl Sub for BalanceDelta {
    type Output = BalanceDelta;

    fn sub(self, rhs: BalanceDelta) -> BalanceDelta {
        // Extract the high and low 128-bit parts and perform subtraction
        let a = self.as_i256();
        let b = rhs.as_i256();
        // High 128 bits (arithmetic right shift)
        let a0 = a >> 128;
        let b0 = b >> 128;

        // Low 128 bits (sign extension happens automatically with I256)
        let a1 = a;
        let b1 = b;

        // Perform subtraction on each part
        let res0 = a0 - b0;
        let res1 = a1 - b1;

        // Combine results: shift high part left by 128 bits and add low part
        // The low part needs to be masked to 128 bits
        BalanceDelta((res0 << 128) + (res1 << 128 >> 128))
    }
}

pub type AfterSwapDelta = I128;

#[derive(Debug)]
pub struct BeforeSwapOutput {
    pub amount_delta: BeforeSwapDelta,
    pub fee: U24,
    pub overwrites: HashMap<Address, HashMap<U256, U256>>,
    pub transient_storage: HashMap<Address, HashMap<U256, U256>>,
}

impl BeforeSwapOutput {
    #[expect(
        clippy::needless_pass_by_value,
        reason = "public API; taking the sol output by reference is a breaking change"
    )]
    pub fn new(
        before_swap_output: BeforeSwapSolOutput,
        overwrites: HashMap<Address, HashMap<U256, U256>>,
        transient_storage: HashMap<Address, HashMap<U256, U256>>,
    ) -> Self {
        Self {
            amount_delta: BeforeSwapDelta(before_swap_output.amountDelta),
            fee: before_swap_output.fee,
            overwrites,
            transient_storage,
        }
    }
}

pub struct AfterSwapParameters {
    pub context: StateContext,
    pub sender: Address,
    pub swap_params: SwapParams,
    pub delta: BalanceDelta,
    pub hook_data: Bytes,
}

#[derive(Debug, Clone)]
pub struct WithGasEstimate<T> {
    pub gas_estimate: u64,
    pub result: T,
}

/// The fees a hook charges a marginal exact-input swap in one direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookSpotFees {
    /// The LP fee in pips the pool charges the swap, which `beforeSwap` would return as its
    /// override. `None` leaves the pool's own LP fee in place.
    pub lp_fee_override: Option<u32>,
    /// The hook's own fee in pips (1_000_000 = 100%), charged on the input or on the output.
    pub hook_fee_pips: u32,
}

/// What the pool's swap math did with a swap, as a hook's `afterSwap` can read it back from the
/// pool manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapOutcome {
    /// The pool's square-root price after the swap, as a Q64.96.
    pub sqrt_price_x96: U256,
    /// The part of the specified amount the pool could not swap before reaching the swap's price
    /// limit. Zero when the swap filled in full.
    pub amount_unfilled: U256,
}

#[derive(Debug)]
pub struct AmountRanges {
    pub amount_in_range: (U256, U256),
    pub amount_out_range: (U256, U256),
}

sol! {
    #[derive(Debug)]
    struct BeforeSwapSolOutput {
        bytes4 selector;
        int256 amountDelta;
        uint24 fee;
    }

    #[derive(Debug)]
    struct AfterSwapSolReturn {
        bytes4 selector;
        int128 delta;
   }

    #[derive(Debug)]
    struct GetLimitsSolReturn {
        uint256 amount_in_upper_limit;
        uint256 amount_out_upper_limit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(amount0: i128, amount1: i128) -> BalanceDelta {
        BalanceDelta::new(I128::try_from(amount0).unwrap(), I128::try_from(amount1).unwrap())
    }

    /// A negative `amount1` difference leaves `amount0` as it is: v4-core masks each lane.
    #[test]
    fn test_checked_sub_subtracts_each_lane_apart() {
        let result = delta(5, 0)
            .checked_sub(delta(2, 3))
            .unwrap();

        assert_eq!(result, delta(3, -3));
    }

    #[test]
    fn test_checked_sub_refuses_a_lane_past_int128() {
        assert!(delta(i128::MIN, 0)
            .checked_sub(delta(1, 0))
            .is_err());
        assert!(delta(0, i128::MAX)
            .checked_sub(delta(0, -1))
            .is_err());
    }

    #[test]
    fn test_try_from_swap_result_refuses_an_output_past_int128() {
        let past_int128 = -(I256::from(I128::MAX) + I256::ONE);

        assert!(BalanceDelta::try_from_swap_result(past_int128, true).is_err());
        assert_eq!(
            BalanceDelta::try_from_swap_result(-I256::from(I128::MAX), false).unwrap(),
            BalanceDelta::new(I128::MAX, I128::ZERO)
        );
    }
}
