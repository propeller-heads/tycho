//! Algebra Integral: the parts every DEX built on it shares.
//!
//! A DEX reaches these through its own protocol system and registers the plugin implementations
//! it deploys in [`plugin::registry`].

mod decoder;
pub mod plugin;
pub mod state;
mod swap_math;
