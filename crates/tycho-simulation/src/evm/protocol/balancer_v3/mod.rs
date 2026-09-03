//! Hybrid Balancer V3 implementation: quote maths from the `balancer-maths-rust` crate Balancer
//! publishes, with pool state read from the locally indexed VM storage at decode and update time.
//!
//! `vm:balancer_v3` keeps its VM-shaped indexing — full contract storage, plus the DCI entrypoints
//! that keep rate providers and other referenced contracts fresh. What changes is the hot path: a
//! quote no longer executes the Vault in REVM, it evaluates the pool's maths directly.
//!
//! Weighted, stable and reCLAMM pools are decoded here. The V3-generation reCLAMM pools we index
//! share their swap maths with the library's V2 implementation, which Balancer confirmed, so they
//! reuse it. Gyro, QuantAMM and LBP pools, and anything carrying a swap hook, still need maths this
//! decoder does not build, so [`vm::resolve_pool_type`] and the hook check reject them rather than
//! pricing them with the wrong curve.
mod decoder;
mod state;
#[cfg(test)]
mod tests;
mod vm;

pub use state::BalancerV3State;
