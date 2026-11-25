//! The Montra `LaunchHook`'s pool storage on Elysium: the `PoolConfig` and `PoolKey` words of a
//! pool and the `montra_*` attributes decoded from them. [`crate::launchpad`] has the modules that
//! index Montra pools, and [`pool_config`] cites the evidence for the layout.
//!
//! The hook's source was not verified on Blockscout when checked on 2026-11-13. Its storage
//! layout (mapping bases, word offsets) was established from its storage writes and its
//! `poolConfig(bytes32)` getter.

pub(crate) mod pool_config;
