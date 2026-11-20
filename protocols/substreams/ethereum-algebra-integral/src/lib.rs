//! Algebra Integral: what every DEX built on it shares.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abi;
mod modules;
mod pb;
#[cfg(test)]
mod pb_tests;
pub mod plugin;

pub use modules::*;
