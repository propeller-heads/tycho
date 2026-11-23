//! Checks a test runner can share. protocol-testing runs all of them; the cluster test,
//! tycho-integration-test, runs the limit and execution checks on live pools.
//!
//! A check works on a snapshot or a decoded state, or on simulation and execution results, and
//! returns what it found. Where the blocks come from, which components are checked and where the
//! results go is each runner's own; protocol-testing, for one, writes a JSON report.

pub mod execution;
pub mod ladder;
pub mod limits;
pub mod outcome;
pub mod tvl_only;
