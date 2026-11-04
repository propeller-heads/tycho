//! The checks the test runners share.
//!
//! Each check works on a decoded state, or on simulation and execution results, and returns what
//! it found. Where the blocks come from, which components are checked and where the results go is
//! each runner's own: protocol-testing writes a JSON report, the in-cluster test exports metrics.

pub mod execution;
pub mod limits;
pub mod outcome;
pub mod sheet;
pub mod tvl_only;
