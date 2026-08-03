//! Hybrid Balancer V3 implementation: quote maths from the `balancer-maths-rust` crate Balancer
//! publishes, with pool state read from the locally indexed VM storage at decode and update time.
//!
//! `vm:balancer_v3` keeps its VM-shaped indexing — full contract storage, plus the DCI entrypoints
//! that keep rate providers and other referenced contracts fresh. What changes is the hot path: a
//! quote no longer executes the Vault in REVM, it evaluates the pool's maths directly.
//!
//! Only weighted and stable pools are decoded here. Balancer V3's hook system is open-ended by
//! design, and reCLAMM, Gyro, QuantAMM and LBP pools each need maths this decoder does not build
//! yet, so [`vm::resolve_pool_type`] rejects them instead of pricing them with the wrong curve.
mod decoder;
mod state;
#[cfg(test)]
mod tests;
mod vm;

pub use state::BalancerV3State;
