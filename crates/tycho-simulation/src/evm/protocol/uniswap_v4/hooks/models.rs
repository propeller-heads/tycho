use std::{collections::HashMap, ops::Sub};

use alloy::{
    primitives::{aliases::U24, Address, I128, I256, U256},
    sol,
};
use tycho_common::Bytes;

use crate::evm::protocol::uniswap_v4::{
    hooks::utils::{get_lower_i128, get_upper_i128},
    state::UniswapV4Fees,
};

#[derive(Debug, Clone)]
pub struct StateContext {
    pub currency_0: Address,
    pub currency_1: Address,
    pub fees: UniswapV4Fees,
    pub tick_spacing: i32,
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

    pub fn as_i256(&self) -> I256 {
        self.0
    }

    pub fn amount0(self) -> I128 {
        get_upper_i128(self.0)
    }

    pub fn amount1(self) -> I128 {
        get_lower_i128(self.0)
    }
}

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
