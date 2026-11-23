//! What every PancakeSwap Infinity substreams package shares, whatever the pool type: the
//! `PoolKey.parameters` hook bitmap, the dynamic fee flag, `pool_state_base_slot`, the protocol
//! fee split and the deployment `params`.
//!
//! This crate defines no `#[substreams::handlers]` function. A handler is a `#[no_mangle]`
//! symbol and a wasm keeps every one from every crate it links, so one wasm cannot hold two
//! handlers of the same name. CL and Bin both define `map_pools_created`, `store_pools`,
//! `map_events` and more, so Bin cannot take the shared code from the CL crate the way the hooks
//! package does. Handlers, protobufs, ABIs, event maps and stores stay in their package.
//!
//! Solidity references are paths in `pancakeswap/infinity-core` at commit `7c04695f`
//! (<https://github.com/pancakeswap/infinity-core/tree/7c04695f>).

pub mod parameters;
pub mod params;
