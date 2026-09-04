//! Substreams package extracting trades routed through the TychoRouter contracts.
//!
//! Trades are recovered from EVM call traces (the router emits no swap event), decoded per
//! router ABI generation and emitted as `DatabaseChanges` for `substreams-sink-sql`.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abi;
mod decode;
mod executors;
mod executors_table;
mod modules;
mod params;
mod pb;

pub use modules::*;
