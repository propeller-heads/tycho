//! Serves `/contract_state` and `/protocol_state` from the entity cache.
//!
//! A response is `cached entry ⊕ window changes up to the requested version`. The service never
//! reads the database. A request it cannot serve comes back as [`CacheOutcome::DbPath`], and the
//! RPC handler answers it with today's code, which stays untouched: it is both the fallback and
//! the instant rollback (`ENTITY_CACHE_MODE=off`).
//!
//! # Read order
//!
//! A read holds the window lock while it resolves the version and captures the patch, releases
//! it, then takes the cache read lock and copies the entries out. Folds hold the window lock
//! while they take the cache write lock, so a fold can land between the two steps. That is
//! harmless: the fold moves blocks from the patch into the entries, and a patch change applies
//! only when it is newer than the value's write timestamp, so nothing is applied twice or lost.
//! An entry that moved past the requested version trips the read guard (see
//! [`DbPathReason::ReadGuard`]).
//!
//! # Two kinds of DB path
//!
//! - A version below the window floor: the database holds that history, so the versioned query
//!   answers it.
//! - A read-guard trip: the version is inside the window, but the cache only keeps the newest
//!   value. The database may not hold that block yet; today's handler already serves such versions
//!   as `latest from the DB ⊕ uncommitted window changes`.
//!
//! Today's handler picks between the two by the version's commit status, so both reasons route
//! to the same code.

// Constructed once the startup load builds the cache (ENG-6292) and the pump folds into it
// (ENG-6305).
#![allow(dead_code)]

use std::sync::Arc;

use tycho_common::{
    dto,
    models::{blockchain::Block, contract::Account, protocol::ProtocolComponentState},
};

use super::{
    cache::{CachedAccount, CachedComponentState, EntityCache},
    window::{AccountChange, ComponentChange, DeltaWindow},
};
use crate::services::{deltas_buffer::PendingDeltas, rpc::RpcError};

/// Which path answers `/contract_state` and `/protocol_state`. Set per deployment with
/// `ENTITY_CACHE_MODE`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntityCacheMode {
    /// Every request takes the database path, exactly as before the cache existed.
    #[default]
    Off,
    /// Every request is answered by the database path. A sample also runs the cache path and
    /// the two results are compared (ENG-6295). Until then, `shadow` behaves like `off`.
    Shadow,
    /// The cache answers every request it can; the rest take the database path.
    Serve,
}

/// Why a request took the database path. The `reason` label of the `db_path_requests` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbPathReason {
    /// The requested version is older than the window floor.
    BelowWindow,
    /// The request lists no ids, so the database decides which entities are on the page.
    NoIds,
    /// A needed value was written after the requested version. The cache keeps no history.
    /// Expected to be very rare.
    ReadGuard,
}

impl DbPathReason {
    /// Metric label value.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            DbPathReason::BelowWindow => "below_window",
            DbPathReason::NoIds => "no_ids",
            DbPathReason::ReadGuard => "read_guard",
        }
    }
}

/// Result of asking the cache to serve a request.
#[derive(Debug)]
pub(crate) enum CacheOutcome<T> {
    /// The cache built the full response.
    Served(T),
    /// The cache cannot serve this request; the caller takes the database path.
    DbPath(DbPathReason),
}

/// Where a requested version is served from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum VersionRoute {
    /// A block the window holds. Serve `cache ⊕ patch up to this block`.
    Window(Block),
    /// The window cannot answer this version; the caller takes the database path.
    Db(DbPathReason),
}

/// Maps a requested version to a window block, from memory only.
///
/// | Input                                    | Route                                     |
/// |------------------------------------------|-------------------------------------------|
/// | Timestamp newer than the tip (default)   | the tip                                   |
/// | Block number, hash or timestamp in window| that block                                |
/// | Below the window floor                   | `Db(BelowWindow)`                         |
/// | Block number above the tip               | error, the same `NotFound` as today       |
///
/// A hash the window does not hold routes to `Db(BelowWindow)`: only the database can say whether
/// it is old or unknown.
///
/// # Errors
///
/// - `RpcError::Parse` when the version carries neither a timestamp nor a usable block.
/// - `RpcError::Storage(StorageError::NotFound("Block", ..))` above the tip. tycho-client retries a
///   body that contains `"Could not find Block"` and may blacklist a component on any other text,
///   so the entity name must be `Block`. Today's `calculate_versions` says `Version` here, which
///   the client does not match; do not copy it.
pub(crate) fn resolve_version(
    window: &DeltaWindow,
    version: &dto::VersionParam,
) -> Result<VersionRoute, RpcError> {
    let _ = (window, version);
    todo!("ENG-6306: resolve against DeltaWindow::resolve, plus a hash lookup over the window")
}

/// Builds the account served at the requested block from its cached entry and its window
/// changes up to that block.
///
/// - `entry` present: copy it and apply every change newer than the value it writes.
/// - `entry` absent, changes present: the contract is newer than the cache. Build it from the
///   changes alone; the first one must be a creation.
/// - Both absent: the account does not exist. Returns `Ok(None)`; the caller omits it.
///
/// `code_hash` follows the delta path (`Account::apply_delta`), a known difference with the
/// database path. Transaction references come from the startup load and never advance.
///
/// # Errors
///
/// `DbPathReason::ReadGuard` when any value of `entry` was written after `at`.
pub(crate) fn materialize_account(
    entry: Option<&CachedAccount>,
    changes: &[AccountChange],
    at: &Block,
) -> Result<Option<Account>, DbPathReason> {
    let _ = (entry, changes, at);
    todo!("ENG-6307")
}

/// Builds the component state served at the requested block from its cached entry and its window
/// changes up to that block.
///
/// Same rules as [`materialize_account`], with one write timestamp per entry: apply the changes
/// newer than [`CachedComponentState::updated_at`]. Deleted attributes stay deleted. With
/// `include_balances == false` the balances are removed from the result, like the database path.
///
/// # Errors
///
/// `DbPathReason::ReadGuard` when `entry` was written after `at`.
pub(crate) fn materialize_component(
    entry: Option<&CachedComponentState>,
    changes: &[ComponentChange],
    include_balances: bool,
    at: &Block,
) -> Result<Option<ProtocolComponentState>, DbPathReason> {
    let _ = (entry, changes, include_balances, at);
    todo!("ENG-6308")
}

/// Answers state requests from the delta windows and the entity cache. Never reads the database.
///
/// Built only in `shadow` and `serve` mode, and only when extractors run: the standalone rpc
/// command has no windows and no cache, so it always behaves like `off`.
pub(crate) struct StateService {
    /// Shares the windows the pump writes; used for [`DeltaWindow`] reads only.
    windows: PendingDeltas,
    cache: Arc<EntityCache>,
}

impl StateService {
    pub(crate) fn new(windows: PendingDeltas, cache: Arc<EntityCache>) -> Self {
        Self { windows, cache }
    }

    /// Serves `/contract_state` from the cache.
    ///
    /// Paginates `contract_ids` the way the database path does (slice, then page) and reports
    /// `total` as the number of requested ids. Addresses that exist nowhere are omitted.
    ///
    /// # Errors
    ///
    /// - `RpcError::Parse` (400) when `protocol_system` is empty or has no window. Today this
    ///   silently reads the database.
    /// - Version errors from [`resolve_version`].
    pub(crate) fn contract_state(
        &self,
        request: &dto::StateRequestBody,
    ) -> Result<CacheOutcome<dto::StateRequestResponse>, RpcError> {
        let _ = request;
        todo!("ENG-6307: resolve + capture under the window lock, then read the cache")
    }

    /// Serves `/protocol_state` from the cache.
    ///
    /// Same shape as [`Self::contract_state`]. Components are looked up under
    /// `request.protocol_system`, the key folds use.
    ///
    /// # Errors
    ///
    /// Same as [`Self::contract_state`].
    pub(crate) fn protocol_state(
        &self,
        request: &dto::ProtocolStateRequestBody,
    ) -> Result<CacheOutcome<dto::ProtocolStateRequestResponse>, RpcError> {
        let _ = request;
        todo!("ENG-6308")
    }
}
