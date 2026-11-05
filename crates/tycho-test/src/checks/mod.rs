//! Checks a test runner can share, taken from protocol-testing, its only runner so far.
//!
//! A check works on a snapshot or a decoded state, or on simulation and execution results, and
//! returns what it found. Where the blocks come from, which components are checked and where the
//! results go is each runner's own; protocol-testing, for one, writes a JSON report.

pub mod execution;
pub mod ladder;
pub mod limits;
pub mod outcome;
pub mod sheet;
pub mod tvl_only;
