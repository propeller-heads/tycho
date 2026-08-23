//! Pendle's on-chain math, ported to Rust.
//!
//! Provenance and licensing: see `../NOTICE.md`. The port is deliberately literal — a quote has to
//! agree with the contract bit for bit, including where each division truncates.

pub mod errors;
pub mod log_exp_math;
pub mod pmath;
pub mod sy_utils;
