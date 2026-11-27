//! Launchpad pools on Elysium: Uniswap V4 pools of the Elysium `PoolManager` whose `PoolKey.hooks`
//! is a Montra `LaunchHook` ([`crate::montra`]) or an Ascend launchpad hook ([`crate::ascend`]),
//! indexed by the Elysium manifest (`elysium-uniswap-v4-with-hooks.yaml`).
//!
//! The pool math is stock V4, so the shared V4 pipeline indexes ticks, liquidity, price and
//! balances. The launchpad modules add:
//!
//! - pool creation from `PoolManager.Initialize`, limited to the hooks in the `montra_hooks` and
//!   `ascend_hooks` parameters (`map_launchpad_pools_created`);
//! - the hook's per-pool pricing state, decoded from the hook's storage writes into `montra_*` or
//!   `ascend_*` state attributes by the layout of the pool's hook
//!   (`map_launchpad_enriched_block_changes`), with a store that maps each watched slot back to its
//!   pool (`store_launchpad_slots`);
//! - the `paused` state of a Montra pool whose seed is not placed or whose `PoolKey` changed
//!   (`map_launchpad_enriched_block_changes`).

pub(crate) mod params;
pub(crate) mod slots;
pub(crate) mod storage_writes;
#[cfg(test)]
pub(crate) mod test_fixtures;

/// The storage layout of a pool's hook, which decides the slots watched and the attributes its
/// writes become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    Montra,
    Ascend,
}
