//! Nami V4 Decentralized Exchange
//!
//! Nami V4 is a fork of Algebra Integral 1.2. Pools are concentrated-liquidity pools whose swap
//! math differs from Uniswap V3 in how it walks ticks (a linked list of initialized ticks, which
//! may carry zero net liquidity) and in a few rounding branches, so this module has its own swap
//! loop and step math.
//!
//! The swap fee is set per swap by the pool's plugin: when the plugin config enables `beforeSwap`
//! and `DYNAMIC_FEE`, each quote runs the plugin's `beforeSwap` hook in the EVM against indexed
//! plugin state. Pools without such a plugin charge their stored fee.
mod decoder;
mod plugin;
pub mod state;
mod swap_math;
