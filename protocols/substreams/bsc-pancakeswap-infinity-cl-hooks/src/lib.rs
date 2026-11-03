#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! PancakeSwap Infinity CL pools whose hook is a `CLAlphaHook`, the hook Binance Alpha listings
//! launch their pools with.
//!
//! `CLAlphaHook` registers `beforeSwap` and no other swap callback, and its `_beforeSwap` does
//! nothing but revert
//! `PoolNotStarted` until `poolStartedTimestamp[id]` has passed. After that a swap is exactly a
//! hookless swap. The CL package skips every pool with a swap hook; this package indexes the
//! `CLAlphaHook` ones with the same modules and attribute schema, plus:
//!
//! - `hooks` carries the real hook address, so `UniswapV4State` attaches the native `CLAlphaHook`
//!   handler registered for these addresses;
//! - `alpha_start_timestamp` carries `poolStartedTimestamp[id]`, kept current from the hook's
//!   `PoolStartedAtUpdated` event. The first hook version lets the owner move it after launch,
//!   which pauses the pool.
//!
//! Solidity references are paths in `pancakeswap/infinity-core` at commit `7c04695f` and in the
//! verified `CLAlphaHook` sources on BscScan.

mod variant_modules;

// The CL package's handlers are `#[no_mangle]`, so linking the crate already exports them from
// this wasm; the manifest's modules without "alpha" in their name run them.
pub use base_pancakeswap_infinity_cl::*;
pub use variant_modules::*;
