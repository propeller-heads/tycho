//! Montra launchpad pools on Elysium: Uniswap V4 pools of the Elysium `PoolManager` whose
//! `PoolKey.hooks` is a Montra `LaunchHook`, indexed by the Elysium manifest
//! (`elysium-uniswap-v4-with-hooks.yaml`). The same modules index the Ascend launchpad's pools
//! ([`crate::ascend`]), with the parameters and word layout of their own hook.
//!
//! The pool math is stock V4, so the shared V4 pipeline indexes ticks, liquidity, price and
//! balances. The Montra modules add:
//!
//! - pool creation from `PoolManager.Initialize`, limited to the hooks in the `montra_hooks` and
//!   `ascend_hooks` parameters (`map_montra_pools_created`);
//! - the hook's per-pool pricing state (fee terms, stage, LP-line hold, seed range, anti-snipe
//!   window), decoded from the hook's `PoolConfig` storage writes into `montra_*` state attributes
//!   (`map_montra_enriched_block_changes`), with a store that maps each watched slot back to its
//!   pool (`store_montra_slots`);
//! - the `paused` state of a pool whose seed is not placed or whose `PoolKey` changed
//!   (`map_montra_enriched_block_changes`).
//!
//! The hook's source was not verified on Blockscout when checked on 2026-11-13. Its storage
//! layout (mapping bases, word offsets) was established from its storage writes and its
//! `poolConfig(bytes32)` getter; [`pool_config`] cites the evidence.

pub(crate) mod params;
pub(crate) mod pool_config;
pub(crate) mod storage_writes;
#[cfg(test)]
pub(crate) mod test_fixtures;
