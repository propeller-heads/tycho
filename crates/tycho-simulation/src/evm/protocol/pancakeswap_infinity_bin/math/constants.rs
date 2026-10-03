//! [`Constants.sol`](https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin/libraries/Constants.sol#L6-L18),
//! plus the two the price and fee helpers keep private.

use alloy::primitives::U256;

/// Bin prices are 128.128 fixed point.
pub const SCALE_OFFSET: u8 = 128;

/// 1.0 in 128.128.
pub const SCALE: U256 = U256::from_limbs([0, 0, 1, 0]);

/// Fee denominator. Fees reach this scale as `fee_pips * 1e12`.
pub const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// Pips denominator. LP and protocol fees are pips.
pub const PIPS_DENOMINATOR: u32 = 1_000_000;

/// `bin_step` denominator: step 25 is a 0.25% ratio between neighbouring bins.
pub const BASIS_POINT_MAX: u64 = 10_000;

/// Bin id of price 1.0. Ids are u24, so real exponents run `[-2^23, 2^23)`.
pub const REAL_ID_SHIFT: u32 = 1 << 23;

/// `(2^256 - 1) / (2 * log(2^128) / log(1.0001))`. `BinPool.swap` re-checks it after crediting a
/// bin.
pub const MAX_LIQUIDITY_PER_BIN: U256 = U256::from_limbs([
    0x6d336ca2a775b611,
    0xca44773dd596d31a,
    0xe0d0f4e400fce79a,
    0x000009745258e83d,
]);
