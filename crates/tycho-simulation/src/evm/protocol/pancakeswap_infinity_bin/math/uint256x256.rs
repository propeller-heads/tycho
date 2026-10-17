//! [`Uint256x256Math.sol`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint256x256Math.sol).
//!
//! The Solidity fakes a 512-bit product with a `(prod0, prod1)` pair. `utils::solidity_math` has
//! the real thing: `U512` via `safe_math`, erroring when the result outgrows 256 bits, which is
//! where the Solidity reverts.

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::utils::solidity_math::{mul_div, mul_div_rounding_up};

/// `(x * y) >> offset`, rounded down.
///
/// [`Uint256x256Math.sol#L57-L69`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint256x256Math.sol#L57-L69)
pub fn mul_shift_round_down(x: U256, y: U256, offset: u8) -> Result<U256, SimulationError> {
    mul_div(x, y, U256::ONE << offset)
}

/// `(x * y) >> offset`, rounded up. `get_amounts_out` needs this on `max_amount_in`: rounding down
/// would let a quote claim a bin absorbs more than it can.
///
/// [`Uint256x256Math.sol#L83-L86`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint256x256Math.sol#L83-L86)
pub fn mul_shift_round_up(x: U256, y: U256, offset: u8) -> Result<U256, SimulationError> {
    mul_div_rounding_up(x, y, U256::ONE << offset)
}

/// `(x << offset) / denominator`, rounded down.
///
/// [`Uint256x256Math.sol#L100-L110`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint256x256Math.sol#L100-L110)
pub fn shift_div_round_down(
    x: U256,
    offset: u8,
    denominator: U256,
) -> Result<U256, SimulationError> {
    mul_div(x, U256::ONE << offset, denominator)
}

/// `(x << offset) / denominator`, rounded up.
///
/// [`Uint256x256Math.sol#L124-L127`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/math/Uint256x256Math.sol#L124-L127)
pub fn shift_div_round_up(x: U256, offset: u8, denominator: U256) -> Result<U256, SimulationError> {
    mul_div_rounding_up(x, U256::ONE << offset, denominator)
}
