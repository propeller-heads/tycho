//! Adapts raw on-chain Curve pool data into [`crate::evm::protocol::curve::math::Pool`] instances.
//!
//! # Modules
//!
//! **Start here:**
//! - [`build`] — `build_pool(RawPoolState) → Pool`. The main entry point. Takes raw balances,
//!   decimals, amp, fees and constructs a ready-to-use Pool.
//!
//! **Don't know the variant? Detect it:**
//! - [`detect`] — `detect_variant(probing_results) → CurveVariant`. Probe a pool's on-chain
//!   functions (gamma, offpeg, MATH version, etc.) and classify it.
//!
//! **Supporting:**
//! - [`variant`] — `CurveVariant` enum: which of the 11 pool types is this?
//!
//! # Quick start
//!
//! ```rust
//! use curve_adapter::{CurveVariant, RawPoolState, build_pool};
//! use alloy_primitives::U256;
//!
//! let state = RawPoolState {
//!     variant: CurveVariant::StableSwapV2,
//!     balances: vec![U256::from(1_000_000_000_000_000_000_000u128),
//!                    U256::from(1_000_000_000_000u128)],
//!     token_decimals: vec![18, 6],
//!     amp: U256::from(40_000u64), // A=400 * A_PRECISION=100
//!     fee: Some(U256::from(4_000_000u64)),
//!     ..Default::default()
//! };
//!
//! let pool = build_pool(&state).unwrap();
//! let dy = pool.get_amount_out(0, 1, U256::from(1_000_000_000_000_000_000u128));
//! ```

// Vendored from https://github.com/sunce86/curve-math (crate `curve-adapter`) at the last
// MIT-licensed revision (tree of commit fb3b25c, identical to 518aac1; see ../LICENSE-curve-math).
// Inlined so tycho-simulation carries no out-of-crates.io dependency. Kept close to upstream to
// ease future diffing; lints are relaxed across the vendored tree.
#![allow(dead_code, unused, clippy::all, clippy::pedantic)]

mod detect;
mod variant;

mod build;

pub use build::{build_pool, interpolate_a, BuildError, RawPoolState};
pub use detect::{detect_eth_variant, detect_variant, DetectError, ProbingResults};
pub use variant::CurveVariant;
