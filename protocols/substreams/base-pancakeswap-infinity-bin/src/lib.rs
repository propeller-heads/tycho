#![allow(clippy::not_unsafe_ptr_arg_deref)]
//! PancakeSwap Infinity Liquidity Book (Bin) pools.
//!
//! Sibling of `../base-pancakeswap-infinity-cl`. Same Vault, same PoolKey, same hook bitmap, same
//! filtering. What differs is the pool math and therefore the whole state model: a Bin pool is a
//! map of discrete bins to `(reserveX, reserveY)` plus an active bin, not a sqrt price with a tick
//! map.
//!
//! Two consequences that shape this package:
//!
//! 1. Per-bin reserves are not in the events. They are read from BinPoolManager storage diffs
//!    (`5_map_bin_changes.rs`), which is the only module here with no CL counterpart.
//! 2. There is no existing simulation state to target, so the attribute schema is defined by this
//!    package and consumed by `PancakeswapInfinityBinState` on the simulation side.
//!    `UniswapV4State` cannot decode these components.
//!
//! Solidity references are paths in `pancakeswap/infinity-core`:
//! <https://github.com/pancakeswap/infinity-core/tree/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-bin>
//!
//! Attributes. Static: `pool_id`, `key_lp_fee`, `bin_step`, `parameters`, `pool_manager`,
//! optional `hook_address`. State: `balance_owner`, `active_id`, `fee`,
//! `protocol_fees/zero2one`, `protocol_fees/one2zero`, `bins/{bin_id}`.

pub mod abi;
pub mod modules;
pub mod parameters;
pub mod pb;

pub use modules::*;
