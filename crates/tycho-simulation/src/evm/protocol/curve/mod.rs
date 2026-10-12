//! Hybrid Curve implementation: pure-Rust quote math via the vendored `math` / `adapter` modules,
//! with pool state read from the locally indexed VM storage at decode and update time.
//!
//! `math` and `adapter` are vendored from the MIT-licensed `curve-math` / `curve-adapter` crates
//! (see `LICENSE-curve-math`); the post-MIT features (StableSwapSTETH, TwoCryptoV1 `eth_variant`,
//! the ALend `+1` `get_D` quirk) are re-derived from Curve's public Vyper sources.
pub mod adapter;
mod decoder;
mod math;
mod state;
mod swap_to_price;
mod variant;
mod vm;

pub use state::CurveState;
pub use variant::resolve_variant;
pub use vm::{decode_raw_state, encode_raw_state, read_raw_pool_state, POOL_STATE_ADJUSTED};
