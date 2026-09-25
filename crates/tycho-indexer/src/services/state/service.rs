//! Serves `/contract_state` and `/protocol_state` from the entity cache.
//!
//! A response is `cached entry ⊕ window changes up to the requested version`. The service never
//! reads the database. A request it cannot serve fails with [`StateServiceError::DbPath`], and the
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
//! A fold can also carry an entry past the requested version; see
//! [`DbPathReason::EntryNewerThanVersion`].
//!
//! # Two kinds of DB path
//!
//! - A version below the window floor: the database holds that history, so the versioned query
//!   answers it.
//! - An entry newer than the requested version: the version is inside the window, but the cache
//!   only keeps the newest value. The database may not hold that block yet; today's handler already
//!   serves such versions as `latest from the DB ⊕ uncommitted window changes`.
//!
//! Today's handler picks between the two by the version's commit status, so both reasons route
//! to the same code.

// Constructed once the startup load builds the cache (ENG-6292).
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use thiserror::Error;
use tycho_common::dto;

use super::{cache::EntityCache, window::DeltaWindow};
use crate::services::rpc::RpcError;

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
    /// A cached entry holds a value written after the requested version. The cache keeps only
    /// the newest value of each entry, so it cannot rebuild the older one. This happens when a
    /// fold moves the entry past a version close to the window floor, between the version
    /// resolution and the cache read. Expected to be very rare.
    EntryNewerThanVersion,
}

impl DbPathReason {
    /// Metric label value.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            DbPathReason::BelowWindow => "below_window",
            DbPathReason::EntryNewerThanVersion => "entry_newer_than_version",
        }
    }
}

/// Why the state service did not answer a request.
#[derive(Debug, Error)]
pub(crate) enum StateServiceError {
    /// The cache cannot serve this request; the database path answers it instead.
    #[error("Entity cache cannot serve the request: {}", .0.as_str())]
    DbPath(DbPathReason),
    /// The request is invalid; the client gets this error.
    #[error(transparent)]
    Rpc(#[from] RpcError),
}

/// Answers state requests from the delta windows and the entity cache. Never reads the database.
pub(crate) struct StateService {
    /// One window per protocol system, shared with the pump that writes them.
    windows: HashMap<String, Arc<Mutex<DeltaWindow>>>,
    cache: Arc<EntityCache>,
}

impl StateService {
    pub(crate) fn new(
        windows: HashMap<String, Arc<Mutex<DeltaWindow>>>,
        cache: Arc<EntityCache>,
    ) -> Self {
        Self { windows, cache }
    }

    /// Serves `/contract_state` from the cache.
    ///
    /// Paginates `contract_ids` the way the database path does (slice, then page) and reports
    /// `total` as the number of requested ids. Addresses that exist nowhere are omitted.
    ///
    /// # Errors
    ///
    /// [`StateServiceError::DbPath`] when the cache cannot serve the version or an entry. Otherwise
    /// [`StateServiceError::Rpc`] with:
    ///
    /// - `RpcError::Parse` (400) when `contract_ids` is `None`: the cache serves explicit ids only.
    /// - `RpcError::Parse` (400) when `protocol_system` is empty or has no window. Today this
    ///   silently reads the database.
    /// - `RpcError::Storage(StorageError::NotFound("Block", ..))` when the version is a block
    ///   number above the tip ([`WindowResolution::AboveTip`]). tycho-client retries a body that
    ///   contains `"Could not find Block"` and may blacklist a component on any other text, so the
    ///   entity name must be `Block`. Today's `calculate_versions` says `Version` here, which the
    ///   client does not match; do not copy it.
    ///
    /// [`WindowResolution::AboveTip`]: super::window::WindowResolution::AboveTip
    pub(crate) fn contract_state(
        &self,
        request: &dto::StateRequestBody,
    ) -> Result<dto::StateRequestResponse, StateServiceError> {
        let _ = request;
        todo!("ENG-6307: resolve + capture under the window lock, then read the cache")
    }

    /// Serves `/protocol_state` from the cache.
    ///
    /// Same shape as [`Self::contract_state`]. Components are looked up under
    /// `request.protocol_system`, the key folds use. Deleted attributes stay deleted. With
    /// `include_balances == false` the balances are removed from the response, like the
    /// database path.
    ///
    /// # Errors
    ///
    /// Same as [`Self::contract_state`], with `protocol_ids` in place of `contract_ids`.
    pub(crate) fn protocol_state(
        &self,
        request: &dto::ProtocolStateRequestBody,
    ) -> Result<dto::ProtocolStateRequestResponse, StateServiceError> {
        let _ = request;
        todo!("ENG-6308")
    }
}
