//! Simulates buying, forwarding and selling a token against the pool it was found in.

mod code;
mod owner_calls;
mod runner;
mod scenario;
mod swap;
mod trace;
mod verdict;

/// Identifies this analyzer and its version in stored analyses.
pub const ANALYZER_NAME: &str = "simulation/1";
