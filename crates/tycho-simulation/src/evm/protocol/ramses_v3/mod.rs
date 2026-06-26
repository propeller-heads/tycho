//! Ramses V3 Decentralized Exchange
//!
//! Ramses V3 is a Uniswap V3 fork. The swap math is identical to Uniswap V3, so this module reuses
//! the shared `utils::uniswap` math. The two relevant differences are handled here:
//! - the swap fee is governance-mutable (tracked as a dynamic attribute, stored as a raw `u32`)
//! - pools are keyed by `tick_spacing` rather than by fee tier, so `tick_spacing` is read directly
//!   from a static attribute instead of being derived from the fee.
mod decoder;
pub mod state;
