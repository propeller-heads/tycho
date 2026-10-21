#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod variant_modules;

// Re-export all modules from shared library
pub use evm_uniswap_v4_shared::*;

// Re-export variant-specific modules
pub use variant_modules::*;
