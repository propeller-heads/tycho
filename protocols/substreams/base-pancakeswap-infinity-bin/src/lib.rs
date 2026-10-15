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
//!    (`6_map_bin_changes.rs`). A swap visits the bins in the pool's tree, not every id between its
//!    start and end, so the tree is read the same way (`4_map_tree_changes.rs`,
//!    `5_store_bin_trees.rs`). None of the three has a CL counterpart.
//! 2. There is no existing simulation state to target, so the attribute schema is defined by this
//!    package and consumed by `PancakeswapInfinityBinState` on the simulation side.
//!    `UniswapV4State` cannot decode these components.
//!
//! Solidity references are paths in `pancakeswap/infinity-core`:
//! <https://github.com/pancakeswap/infinity-core/tree/7c04695f/src/pool-bin>
//!
//! Attributes. Static: `pool_id`, `key_lp_fee`, `bin_step`, `parameters`, `pool_manager`,
//! optional `hook_address`. State: `balance_owner`, `active_id`, `fee`,
//! `protocol_fees/zero2one`, `protocol_fees/one2zero`, `bins/{bin_id}`.

pub mod abi;
pub mod modules;
pub mod parameters;
pub mod pb;

pub use modules::*;
