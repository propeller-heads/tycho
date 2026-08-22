// `substreams::handlers::map` expands a `String` params argument into a raw-pointer `extern "C"`
// entrypoint, which clippy reads as an unmarked pointer dereference. Every substreams package in
// this workspace that takes params carries the same allow.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abi;
mod consts;
mod keys;
mod market_state;
mod modules;
mod registry;
mod sy;
mod sy_rates;

pub use modules::*;
