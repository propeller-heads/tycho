//! Camelot V3 on Arbitrum One: an Algebra V1.9 (directional dynamic fee) deployment.
//!
//! The swap loop is Uniswap V3's over a tick table without spacing compression. What makes the
//! protocol its own is the fee: the first swap or in-range liquidity change at a new block
//! timestamp writes a timepoint into the pool's `DataStorageOperator` oracle ring and
//! recomputes both directional fees from the ring's 1-day volatility and volume averages
//! ([`timepoints`], [`adaptive_fee`]). [`state::CamelotV3State`] reproduces that write for the
//! block a quote executes in, so quotes pay the fee the chain will charge.
pub mod adaptive_fee;
mod attributes;
pub mod decoder;
pub mod state;
pub mod timepoints;
