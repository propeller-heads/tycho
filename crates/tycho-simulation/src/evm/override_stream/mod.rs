//! Generic, protocol-agnostic core for injecting live per-block VM state overrides into pools.
//!
//! Some protocols (e.g. pAMMs such as FermiSwap) rely on off-chain oracle prices that are not part
//! of the on-chain state Tycho indexes, and that change *within* a block. To simulate them
//! accurately, a side channel publishes per-block VM storage overrides and an overridden block
//! environment that must be reflected by the pool on every simulation — not just once per Tycho
//! block.
//!
//! This module defines the generic plumbing:
//! - [`OverrideSnapshot`](crate::evm::override_stream::OverrideSnapshot) — the resolved overrides
//!   for one protocol at a point in time.
//! - [`StateOverrideProvider`](crate::evm::override_stream::StateOverrideProvider) — a source that
//!   maintains a *live* [`watch`](tokio::sync::watch) channel of snapshots per protocol (kept fresh
//!   in the background, e.g. from a WebSocket).
//! - [`FailurePolicy`](crate::evm::override_stream::FailurePolicy) — the provider-set reaction of a
//!   pool to a simulation that fails while a snapshot's overrides are applied.
//!
//! Providers are registered per `protocol_system` (see
//! [`ProtocolStreamBuilder::with_override_provider`](crate::evm::stream::ProtocolStreamBuilder::with_override_provider)).
//! A pool subscribes to the provider registered for its protocol at creation time and reads the
//! freshest snapshot at simulation time.
//!
//! It deliberately knows nothing about any specific venue or stream format; all such specifics live
//! in the concrete provider implementations (see [`titan`](crate::evm::override_stream::titan)).

pub mod titan;

use std::{collections::HashMap, sync::Arc};

use alloy::primitives::{Address, U256};
use tokio::sync::watch;

/// What a pool should do when a simulation fails while a snapshot's overrides are applied.
///
/// Set by the provider, which knows whether its overrides are authoritative or an advisory
/// enhancement of the indexed state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Propagate the failure to the caller.
    #[default]
    Error,
    /// Retry the failed simulation without any overrides, on the plain indexed state.
    ///
    /// Suits providers whose overrides are only intermittently applicable — e.g. Titan pAMM
    /// snapshots, which target the pending block and can make a pair transiently unquotable
    /// (oracle lane not yet re-stamped for that block) even though the indexed state simulates
    /// fine.
    FallbackToIndexedState,
}

/// Resolved per-block VM overrides for a single protocol at a point in time.
///
/// `block_number` and `block_timestamp` are already resolved by the provider (e.g. Titan derives
/// `block_timestamp` from the frame's beacon slot) so this core stays protocol-agnostic. Either
/// field may be `None`, in which case the pool's existing block environment is left intact.
///
/// The struct is `#[non_exhaustive]` so fields can be added without breaking downstream
/// providers; construct it outside this crate by mutating a [`Default::default`] value.
#[derive(Clone, Default, Debug)]
#[non_exhaustive]
pub struct OverrideSnapshot {
    /// The L1 block number these overrides apply to, if known.
    pub block_number: Option<u64>,
    /// The block timestamp to inject into the pool's simulation environment, if known.
    pub block_timestamp: Option<u64>,
    /// Storage overrides keyed by contract address, then by storage slot. `Arc`-wrapped so a pool
    /// can read the latest snapshot on every simulation with a refcount bump rather than a deep
    /// clone of the nested map.
    pub storage: Arc<HashMap<Address, HashMap<U256, U256>>>,
    /// Unix-seconds instant after which these overrides are stale and must not be used. Set by the
    /// provider from its own freshness rules (e.g. Titan uses the quote timestamp plus one block
    /// time). `None` means the snapshot never expires — e.g. the empty initial snapshot.
    pub expires_at: Option<u64>,
    /// How a pool should react when a simulation fails with these overrides applied.
    pub failure_policy: FailurePolicy,
}

impl OverrideSnapshot {
    /// Returns whether these overrides are stale at `now` (unix seconds).
    ///
    /// A snapshot without an [`expires_at`](Self::expires_at) never expires. Callers that read the
    /// snapshot on every simulation should skip it entirely once this returns `true`, falling back
    /// to whatever state they held before.
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| now >= expires_at)
    }
}

/// Supplies a *live* stream of resolved per-block VM overrides for one or more protocols.
///
/// Implementations maintain their snapshots in the background (e.g. from a WebSocket stream) and
/// expose them through a [`watch`] channel per protocol. A single provider may serve several
/// protocols sharing one underlying connection; the same provider can be registered for each of
/// them (it is shared via `Arc`, not duplicated).
pub trait StateOverrideProvider: Send + Sync {
    /// Returns the live override channel for `protocol_system`, or `None` if unsupported.
    ///
    /// The returned [`watch::Receiver`] always reflects the latest snapshot; reading it
    /// ([`watch::Receiver::borrow`]) never blocks, so a pool may read it on every simulation.
    fn subscribe(&self, protocol_system: &str) -> Option<watch::Receiver<OverrideSnapshot>>;
}

/// The default registry of built-in override providers, keyed by `protocol_system`.
///
/// `protocols` yields the protocol systems the caller wants a built-in default for (i.e. registered
/// exchanges minus any the consumer explicitly overrode — the builder computes that difference and
/// forwards it as an owned iterator, avoiding clones). This is the single place that wires concrete
/// providers (e.g. the Titan pAMM stream) into the otherwise protocol-agnostic stream builder,
/// keeping all venue-specific knowledge out of both the builder and the generic override core.
pub(crate) fn default_override_providers(
    protocols: impl IntoIterator<Item = String>,
) -> HashMap<String, Arc<dyn StateOverrideProvider>> {
    let mut providers: HashMap<String, Arc<dyn StateOverrideProvider>> = HashMap::new();
    providers.extend(titan::default_providers(protocols));
    providers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_without_expiry_never_expires() {
        let snapshot = OverrideSnapshot::default();
        assert!(!snapshot.is_expired(u64::MAX));
    }

    #[test]
    fn snapshot_expires_at_or_after_deadline() {
        let snapshot = OverrideSnapshot { expires_at: Some(100), ..Default::default() };
        assert!(!snapshot.is_expired(99));
        assert!(snapshot.is_expired(100));
        assert!(snapshot.is_expired(101));
    }
}
