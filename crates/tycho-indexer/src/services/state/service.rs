//! Serves `/contract_state` and `/protocol_state` from the entity cache.
//!
//! A response is `cached entry ⊕ window changes up to the requested version`. The service never
//! reads the database. A request it cannot serve fails with [`StateServiceError::Fallback`],
//! and the RPC handler answers it with the database path, which stays untouched: it is both the
//! fallback and the instant rollback (`ENTITY_CACHE_MODE=off`).
//!
//! # Read order
//!
//! A read holds the window lock while it resolves the version and copies the window changes for
//! its ids, releases it, then takes the cache read lock and copies the entries out. Folds hold the
//! window lock while they take the cache write lock, so a fold can land between the two steps.
//! That is harmless: the fold moves blocks from the copied changes into the entries, and a change
//! applies only when it is newer than the entry's write timestamp, so nothing is applied twice or
//! lost. Accounts check the timestamp of each value, because several extractors can write one
//! account. For the same reason an account read also copies the other windows' blocks up to the
//! resolved version, one window lock at a time. Components check one timestamp for the whole
//! entry, because one extractor writes each component, in order.
//! A fold can also carry an entry past the requested version; the request then falls back with
//! [`FallbackReason::EntryNewer`].
//!
//! # Versions the cache cannot rebuild
//!
//! The cache keeps only the newest value of each entry, so it cannot serve a version older than
//! a value it holds. The version is then below the window, or it was inside the window and a
//! value is newer anyway: a fold landed after the version was resolved (which also moves the
//! version below the window), or another extractor that shares the entity is ahead of this one.
//! The database path answers every such version: from the versioned query when the database holds
//! it, otherwise as `latest from the DB ⊕ uncommitted window changes`.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use thiserror::Error;
use tracing::debug;
use tycho_common::{
    dto::{self, PaginationResponse},
    models::{
        blockchain::BlockAggregatedChanges, contract::Account, protocol::ProtocolComponentState,
        MergeError, PaginationParams,
    },
    storage::{BlockOrTimestamp, StorageError, WriteTimestamp},
    Bytes,
};

use super::{
    cache::{CachedAccount, CachedComponentState, EntityCache},
    window::{account_changes, component_changes, DeltaWindow, WindowResolution},
};

/// Which path answers state requests, holding what the cache modes need: the loaded
/// [`EntityCache`] when building the services, the [`StateService`] once built.
///
/// A cache mode without a cache, or `Off` with one, cannot be expressed.
#[derive(Clone, Debug)]
pub enum EntityCacheSetup<T, S = T> {
    /// See [`EntityCacheMode::Off`](super::EntityCacheMode::Off).
    Off,
    /// See [`EntityCacheMode::Shadow`](super::EntityCacheMode::Shadow).
    Shadow(S),
    /// See [`EntityCacheMode::Serve`](super::EntityCacheMode::Serve).
    Serve(T),
}

impl<T> EntityCacheSetup<T> {
    /// The value a cache mode holds; `None` for `Off`.
    pub fn cache(&self) -> Option<&T> {
        match self {
            EntityCacheSetup::Off => None,
            EntityCacheSetup::Shadow(cache) | EntityCacheSetup::Serve(cache) => Some(cache),
        }
    }

    /// Replaces the held value, keeping the mode.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> EntityCacheSetup<U> {
        match self {
            EntityCacheSetup::Off => EntityCacheSetup::Off,
            EntityCacheSetup::Shadow(cache) => EntityCacheSetup::Shadow(f(cache)),
            EntityCacheSetup::Serve(cache) => EntityCacheSetup::Serve(f(cache)),
        }
    }

    /// Replaces the value `Shadow` holds, keeping the mode and the value of `Serve`.
    pub fn map_shadow<S>(self, f: impl FnOnce(T) -> S) -> EntityCacheSetup<T, S> {
        match self {
            EntityCacheSetup::Off => EntityCacheSetup::Off,
            EntityCacheSetup::Shadow(value) => EntityCacheSetup::Shadow(f(value)),
            EntityCacheSetup::Serve(value) => EntityCacheSetup::Serve(value),
        }
    }
}

/// Why the cache handed a request to the database path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackReason {
    /// The version is below the window.
    BelowWindow,
    /// The window holds no block: at startup, or after the extractor restarted.
    EmptyWindow,
    /// The version is a block hash the window does not hold.
    UnknownHash,
    /// A cached entry is newer than the version: a fold landed during the read, or another
    /// extractor that shares the account is ahead of this one.
    EntryNewer,
    /// The request has no ids, so it lists every entity.
    NoIds,
    /// No extractor in this process indexes the requested protocol system: the request names
    /// none, a stopped extractor or an unknown one.
    UnknownSystem,
}

impl FallbackReason {
    /// Every reason, to register each metric series at zero.
    pub(crate) const ALL: [Self; 6] = [
        Self::BelowWindow,
        Self::EmptyWindow,
        Self::UnknownHash,
        Self::EntryNewer,
        Self::NoIds,
        Self::UnknownSystem,
    ];

    /// Label for the `db_path_requests` metric.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BelowWindow => "below_window",
            Self::EmptyWindow => "empty_window",
            Self::UnknownHash => "unknown_hash",
            Self::EntryNewer => "entry_newer",
            Self::NoIds => "no_ids",
            Self::UnknownSystem => "unknown_system",
        }
    }
}

/// Why the state service did not answer a request. The RPC handler picks the response for each.
#[derive(Debug, Error)]
pub(crate) enum StateServiceError {
    /// The cache cannot answer this request; the database path answers it instead. See the
    /// module doc for when this happens.
    #[error("Entity cache fallback: {0:?}")]
    Fallback(FallbackReason),
    /// The requested version cannot be parsed.
    #[error("Invalid version: {0}")]
    InvalidVersion(String),
    /// The version is a block number above the window tip.
    #[error("Version {0:?} is above the window tip")]
    VersionAboveTip(BlockOrTimestamp),
    /// An uncached address has no delta in any window.
    #[error("Contract {0} not found")]
    ContractNotFound(Bytes),
    /// The window lock of `system` is poisoned.
    #[error("Window lock of {system} is poisoned: {reason}")]
    LockPoisoned { system: String, reason: String },
    /// The window cannot be read.
    #[error("Window cannot be read: {0}")]
    WindowRead(StorageError),
    /// A window change cannot be merged into an entry.
    #[error("Window change cannot be merged: {0}")]
    Merge(#[from] MergeError),
}

/// What a read depends on besides the request: the window of the requested system and the entity
/// cache. A read straddles a change when the tokens taken before and after it differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StraddleToken {
    /// `None` when the system has no window or its lock is poisoned; the read then fails anyway.
    window_generation: Option<u64>,
    folds: u64,
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

    /// Reads the [`StraddleToken`] of `protocol_system`.
    pub(crate) fn straddle_token(&self, protocol_system: &str) -> StraddleToken {
        let window_generation = self
            .windows
            .get(protocol_system)
            .and_then(|window| window.lock().ok())
            .map(|window| window.generation());
        StraddleToken { window_generation, folds: self.cache.folds() }
    }

    /// Serves `/contract_state` from the cache.
    ///
    /// Paginates `contract_ids` the way the database path does (slice, then page) and reports
    /// `total` as the number of requested ids. The changes of every window up to the version apply,
    /// as on the database path: several extractors can write one account. An address the cache
    /// does not hold is built from its first delta in those windows.
    ///
    /// # Errors
    ///
    /// - [`StateServiceError::Fallback`] when the cache cannot rebuild the version, the request has
    ///   no `contract_ids`, or no window exists for `protocol_system`.
    /// - [`StateServiceError::ContractNotFound`] when an address is neither cached nor changed by a
    ///   delta in any window.
    /// - [`StateServiceError::InvalidVersion`] when the version is malformed.
    /// - [`StateServiceError::VersionAboveTip`] when the version is a block number above the tip.
    /// - [`StateServiceError::LockPoisoned`], [`StateServiceError::WindowRead`] or
    ///   [`StateServiceError::Merge`] when the window cannot be read or a change cannot be merged.
    pub(crate) fn contract_state(
        &self,
        request: &dto::StateRequestBody,
    ) -> Result<dto::StateRequestResponse, StateServiceError> {
        // The cache serves explicit ids only; the database path lists every entity.
        let Some(ids) = request.contract_ids.as_deref() else {
            return Err(StateServiceError::Fallback(FallbackReason::NoIds));
        };
        // Slice the page out of the requested ids and keep each id once, like the database path
        // does. `total` still counts every requested id.
        let pagination = PaginationParams::from(&request.pagination);
        let mut seen = HashSet::new();
        let page: Vec<Bytes> = ids
            .iter()
            .skip(pagination.offset() as usize)
            .take(pagination.page_size as usize)
            .filter(|id| seen.insert(*id))
            .cloned()
            .collect();
        // Resolve the version and copy the window's blocks up to it under one window lock, so both
        // see the same blocks. The page's changes are collected after the lock is released.
        let (version, mut blocks) = self.read_window(&request.protocol_system, &request.version)?;
        // Several extractors can write one account, so the other windows' changes up to the
        // version apply too, as on the database path. Sorting by timestamp restores block order
        // across windows, which building an uncached account from its first delta needs. The sort
        // is stable: among blocks with the same timestamp, this extractor's comes first.
        // TODO: serve each extractor's account state from its own window only. Reading the other
        // windows only keeps the answers the same as on the database path.
        blocks.extend(self.other_window_blocks(&request.protocol_system, version)?);
        blocks.sort_by_key(|block| WriteTimestamp::from(&block.block));
        let window_changes = account_changes(&blocks, &page);

        // Copy only the `Arc` of each cached entry under the cache read lock, so folds wait for
        // pointer copies rather than for whole storage maps.
        let mut entries = Vec::with_capacity(page.len());
        {
            let cache = self.cache.read();
            for address in &page {
                let entry = cache.account(address);
                // The cache keeps only the newest value, so an entry written after `version`
                // cannot be rolled back to it. Resolving `version` in the window is not enough to
                // rule this out: a fold can land between the capture and this read, and another
                // extractor that shares the account folds blocks this window has not reached.
                if entry.is_some_and(|entry| entry.updated_at() > version) {
                    return Err(StateServiceError::Fallback(FallbackReason::EntryNewer));
                }
                entries.push(entry.cloned());
            }
        }

        // Apply the window changes on top of each entry, without holding any lock. The full copy of
        // an entry happens here, unless a fold already replaced it in the cache.
        let mut accounts = Vec::with_capacity(page.len());
        for (address, entry) in page.iter().zip(entries) {
            let entry = entry.map(Arc::unwrap_or_clone);
            let changes = window_changes
                .get(address)
                .map_or(&[][..], Vec::as_slice);
            let (mut entry, changes) = match entry {
                Some(entry) => (entry, changes),
                // Not cached: build the account from its first delta in the windows and apply the
                // rest. An address with no delta fails the whole request.
                // Token balances from blocks before the first delta are dropped, as on the
                // database path.
                // TODO: keep token balances an address received before its first delta, e.g. tokens
                // sent to a CREATE2 address before the contract is deployed.
                // TODO: serve unknown ids the same way for accounts and components: both as an
                // empty entity or both as an error.
                None => {
                    let Some((start, delta)) =
                        changes
                            .iter()
                            .enumerate()
                            .find_map(|(i, change)| {
                                change
                                    .delta
                                    .as_ref()
                                    .map(|delta| (i, delta))
                            })
                    else {
                        return Err(StateServiceError::ContractNotFound(address.clone()));
                    };
                    let first = &changes[start];
                    (
                        CachedAccount::from_creation(delta, first.balances.as_ref(), first.at),
                        &changes[start + 1..],
                    )
                }
            };
            for change in changes {
                entry.apply_block(change.delta.as_ref(), change.balances.as_ref(), change.at);
            }
            accounts.push(dto::ResponseAccount::from(Account::from(entry)));
        }

        Ok(dto::StateRequestResponse::new(
            accounts,
            PaginationResponse::new(pagination.page, pagination.page_size, ids.len() as i64),
        ))
    }

    /// Serves `/protocol_state` from the cache.
    ///
    /// Same shape as [`Self::contract_state`]. Components are looked up under
    /// `request.protocol_system`, the key folds use. An id the cache does not hold is served as an
    /// empty state with its window changes applied, so no id fails the request. The database path
    /// does the same only at an uncommitted version; at a committed version it leaves the id out.
    /// Deleted attributes stay deleted. With
    /// `include_balances == false` the balances are removed from the response, like the
    /// database path.
    ///
    /// # Errors
    ///
    /// Same as [`Self::contract_state`], with `protocol_ids` in place of `contract_ids`, except
    /// that an unknown id is never an error.
    pub(crate) fn protocol_state(
        &self,
        request: &dto::ProtocolStateRequestBody,
    ) -> Result<dto::ProtocolStateRequestResponse, StateServiceError> {
        // The cache serves explicit ids only; the database path lists every entity.
        let Some(ids) = request.protocol_ids.as_deref() else {
            return Err(StateServiceError::Fallback(FallbackReason::NoIds));
        };
        // Slice the page out of the requested ids and keep each id once, like the database path
        // does. `total` still counts every requested id.
        let pagination = PaginationParams::from(&request.pagination);
        let mut seen = HashSet::new();
        let page: Vec<&str> = ids
            .iter()
            .skip(pagination.offset() as usize)
            .take(pagination.page_size as usize)
            .map(String::as_str)
            .filter(|id| seen.insert(*id))
            .collect();
        let system = &request.protocol_system;
        // Resolve the version and copy the window's blocks up to it under one window lock, so both
        // see the same blocks. The page's changes are collected after the lock is released.
        let (version, blocks) = self.read_window(system, &request.version)?;
        let window_changes = component_changes(&blocks, &page);

        // Copy the cached entries under the cache read lock; folds wait until it is released.
        let mut entries = Vec::with_capacity(page.len());
        {
            let cache = self.cache.read();
            for id in &page {
                let entry = cache.component(system, id);
                // One extractor writes each component, and a fold evicts what it folds under the
                // window lock, so the entry can pass `version` only when a fold lands between the
                // window read and this read, with `version` near the window floor. The database
                // path answers it.
                if let Some(entry) = entry.filter(|entry| entry.updated_at() > version) {
                    debug!(
                        component = %id,
                        entry_ts = %entry.updated_at().block_ts(),
                        version_ts = %version.block_ts(),
                        "Cached component is newer than the requested version"
                    );
                    return Err(StateServiceError::Fallback(FallbackReason::EntryNewer));
                }
                entries.push(entry.cloned());
            }
        }

        // Apply the window changes on top of each entry, without holding any lock, with the
        // database path's merge.
        let mut states = Vec::with_capacity(page.len());
        for (id, entry) in page.iter().zip(entries) {
            // Not cached: start from an empty state. An id the window never changed is served as
            // that empty state. The database path does this only at an uncommitted version; at a
            // committed version it leaves the id out.
            // TODO: serve unknown ids the same way for accounts and components: both as an empty
            // entity or both as an error.
            let updated_at = entry
                .as_ref()
                .map(CachedComponentState::updated_at);
            let mut state = entry.map_or_else(
                || ProtocolComponentState::new(id, HashMap::new(), HashMap::new()),
                ProtocolComponentState::from,
            );
            for change in window_changes
                .get(*id)
                .into_iter()
                .flatten()
                // Skip blocks a fold already moved into the entry.
                .filter(|change| updated_at.is_none_or(|at| change.at > at))
            {
                if let Some(delta) = &change.delta {
                    state.apply_state_delta(delta)?;
                }
                if let Some(balances) = &change.balances {
                    state.apply_balance_delta(balances)?;
                }
            }
            if !request.include_balances {
                state.balances.clear();
            }
            states.push(dto::ResponseProtocolState::from(state));
        }

        Ok(dto::ProtocolStateRequestResponse::new(
            states,
            PaginationResponse::new(pagination.page, pagination.page_size, ids.len() as i64),
        ))
    }

    /// Resolves `version` in the window of `protocol_system` and copies the window's blocks up to
    /// the resolved one, under one lock: a fold or revert in between could otherwise remove the
    /// resolved block from the window. Only the block `Arc`s are copied, so the lock is held
    /// briefly. Returns the resolved block's write timestamp with the blocks; the lock is
    /// released before this returns.
    fn read_window(
        &self,
        protocol_system: &str,
        version: &dto::VersionParam,
    ) -> Result<(WriteTimestamp, Vec<Arc<BlockAggregatedChanges>>), StateServiceError> {
        let Some(window) = self.windows.get(protocol_system) else {
            return Err(StateServiceError::Fallback(FallbackReason::UnknownSystem));
        };
        let version = BlockOrTimestamp::try_from(version)
            .map_err(|err| StateServiceError::InvalidVersion(err.to_string()))?;
        let window = window
            .lock()
            .map_err(|err| StateServiceError::LockPoisoned {
                system: protocol_system.to_string(),
                reason: err.to_string(),
            })?;
        let block = match window.resolve(&version) {
            WindowResolution::InWindow(block) => block,
            WindowResolution::BelowFloor => {
                return Err(StateServiceError::Fallback(FallbackReason::BelowWindow))
            }
            WindowResolution::Empty => {
                return Err(StateServiceError::Fallback(FallbackReason::EmptyWindow))
            }
            WindowResolution::UnknownHash => {
                return Err(StateServiceError::Fallback(FallbackReason::UnknownHash))
            }
            WindowResolution::AboveTip => return Err(StateServiceError::VersionAboveTip(version)),
        };
        let blocks = window
            .blocks_upto(block.number)
            .map_err(StateServiceError::WindowRead)?;
        Ok((WriteTimestamp::from(&block), blocks))
    }

    /// Copies the blocks of every window except `protocol_system`'s that are at or below
    /// `version`. Each window is locked on its own, never two at once, and only the block `Arc`s
    /// are copied. A window whose floor is above `version` contributes nothing: its blocks at or
    /// below `version` are already folded into the cache.
    ///
    /// This exists only so that account reads give the same answers as the database path.
    fn other_window_blocks(
        &self,
        protocol_system: &str,
        version: WriteTimestamp,
    ) -> Result<Vec<Arc<BlockAggregatedChanges>>, StateServiceError> {
        let at = BlockOrTimestamp::Timestamp(version.block_ts());
        let mut blocks = Vec::new();
        for (system, window) in &self.windows {
            if system == protocol_system {
                continue;
            }
            let window = window
                .lock()
                .map_err(|err| StateServiceError::LockPoisoned {
                    system: system.clone(),
                    reason: err.to_string(),
                })?;
            let upto = match window.resolve(&at) {
                WindowResolution::InWindow(block) => block.number,
                WindowResolution::BelowFloor | WindowResolution::Empty => continue,
                // A timestamp never resolves to these.
                WindowResolution::UnknownHash | WindowResolution::AboveTip => continue,
            };
            let window_blocks = window
                .blocks_upto(upto)
                .map_err(StateServiceError::WindowRead)?;
            // A timestamp between two blocks resolves to the later one, which is newer than
            // `version`.
            blocks.extend(
                window_blocks
                    .into_iter()
                    .filter(|block| WriteTimestamp::from(&block.block) <= version),
            );
        }
        Ok(blocks)
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use rstest::rstest;
    use tycho_common::models::{
        blockchain::BlockAggregatedChanges,
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        Chain, ChangeType,
    };

    use super::*;
    use crate::{
        extractor::models::fixtures,
        services::state::window::{FoldSink, WindowConfig},
        testing,
    };

    const SYSTEM: &str = "ex";
    /// A second extractor in the same process. Its window stays empty unless a test writes to it.
    const OTHER: &str = "other";

    /// A service over the windows of `SYSTEM` and `OTHER`, `depth` blocks each, folding into a
    /// cache that starts empty. Requests name `SYSTEM`.
    struct Harness {
        service: StateService,
        window: Arc<Mutex<DeltaWindow>>,
        other: Arc<Mutex<DeltaWindow>>,
        cache: Arc<EntityCache>,
    }

    impl Harness {
        fn new(depth: u64) -> Self {
            let config = WindowConfig { depth, min_fold_batch: 1 };
            let window = Arc::new(Mutex::new(DeltaWindow::new(SYSTEM.to_string(), config)));
            let other = Arc::new(Mutex::new(DeltaWindow::new(OTHER.to_string(), config)));
            let cache = Arc::new(EntityCache::new());
            let service = StateService::new(
                HashMap::from([
                    (SYSTEM.to_string(), window.clone()),
                    (OTHER.to_string(), other.clone()),
                ]),
                cache.clone(),
            );
            Self { service, window, other, cache }
        }

        /// Inserts `m` into the window of `SYSTEM` and folds every block that became evictable.
        fn push(&self, m: BlockAggregatedChanges) {
            Self::push_to(&self.window, &self.cache, m);
        }

        /// Inserts `m` into the window of `OTHER` and folds every block that became evictable.
        fn push_other(&self, m: BlockAggregatedChanges) {
            Self::push_to(&self.other, &self.cache, m);
        }

        fn push_to(window: &Mutex<DeltaWindow>, cache: &EntityCache, m: BlockAggregatedChanges) {
            let mut window = window.lock().unwrap();
            window.insert(&Arc::new(m)).unwrap();
            window.fold_evictable(cache).unwrap();
        }
    }

    #[test]
    fn straddle_token_moves_with_a_block_or_a_fold() {
        let harness = Harness::new(2);
        let empty = harness.service.straddle_token(SYSTEM);

        harness.push(msg(1));
        let one_block = harness.service.straddle_token(SYSTEM);
        // A fold from another extractor can change a shared account, so it moves the token too.
        harness
            .cache
            .fold(&testing::aggregated_changes("other", 7, 7, Some(7)))
            .unwrap();

        assert_ne!(empty, one_block);
        assert_ne!(one_block, harness.service.straddle_token(SYSTEM));
    }

    #[test]
    fn straddle_token_moves_when_the_window_lock_is_poisoned() {
        let harness = Harness::new(2);
        let before = harness.service.straddle_token(SYSTEM);
        let window = harness.window.clone();

        let _ = std::thread::spawn(move || {
            let _guard = window.lock().unwrap();
            panic!("cache path bug");
        })
        .join();

        assert_ne!(harness.service.straddle_token(SYSTEM), before);
    }

    /// Block `n`, finalized and committed.
    fn msg(n: u64) -> BlockAggregatedChanges {
        testing::aggregated_changes(SYSTEM, n, n, Some(n))
    }

    /// Block `n` of `OTHER`, finalized and committed.
    fn other_msg(n: u64) -> BlockAggregatedChanges {
        testing::aggregated_changes(OTHER, n, n, Some(n))
    }

    fn addr(n: u64) -> Bytes {
        Bytes::from(n).lpad(20, 0)
    }

    fn word(n: u64) -> Bytes {
        Bytes::from(n).lpad(32, 0)
    }

    fn account_delta(address: &Bytes, x: u64, change: ChangeType) -> AccountDelta {
        let (balance, code) = match change {
            ChangeType::Creation => (Some(Bytes::from(x)), Some(Bytes::from("0x6000"))),
            _ => (None, None),
        };
        AccountDelta::new(
            Chain::Ethereum,
            address.clone(),
            fixtures::optional_slots([(1, x)]),
            balance,
            code,
            change,
        )
    }

    fn with_account(mut m: BlockAggregatedChanges, delta: AccountDelta) -> BlockAggregatedChanges {
        m.account_deltas
            .insert(delta.address.clone(), delta);
        m
    }

    fn with_account_balance(
        mut m: BlockAggregatedChanges,
        account: &Bytes,
        token: &Bytes,
        amount: u64,
    ) -> BlockAggregatedChanges {
        m.account_balances
            .entry(account.clone())
            .or_default()
            .insert(
                token.clone(),
                AccountBalance::new(
                    account.clone(),
                    token.clone(),
                    Bytes::from(amount),
                    Bytes::default(),
                ),
            );
        m
    }

    fn with_component(mut m: BlockAggregatedChanges, id: &str) -> BlockAggregatedChanges {
        m.new_protocol_components.insert(
            id.to_string(),
            ProtocolComponent {
                id: id.to_string(),
                protocol_system: SYSTEM.to_string(),
                ..Default::default()
            },
        );
        m
    }

    fn with_component_balance(mut m: BlockAggregatedChanges, id: &str) -> BlockAggregatedChanges {
        m.component_balances.insert(
            id.to_string(),
            HashMap::from([(
                addr(9),
                ComponentBalance {
                    token: addr(9),
                    balance: Bytes::from(1u64),
                    balance_float: 1.0,
                    modify_tx: Bytes::default(),
                    component_id: id.to_string(),
                },
            )]),
        );
        m
    }

    /// Account `addr(1)` created in block 1, slot 1 and its balance of token `addr(9)` set to `n`
    /// in each block `n` up to 5. With depth 2 the cache holds blocks 1-3 and the window blocks
    /// 4-5.
    fn accounts() -> Harness {
        let harness = Harness::new(2);
        for n in 1..=5 {
            let change = if n == 1 { ChangeType::Creation } else { ChangeType::Update };
            harness.push(with_account_balance(
                with_account(msg(n), account_delta(&addr(1), n, change)),
                &addr(1),
                &addr(9),
                n,
            ));
        }
        harness
    }

    /// Component `c1` created in block 1 with a balance, attribute `x` set to `n` in each block
    /// `n` up to 5. With depth 2 the cache holds blocks 1-3 and the window blocks 4-5.
    fn components() -> Harness {
        let harness = Harness::new(2);
        harness.push(testing::with_state_delta(
            with_component_balance(with_component(msg(1), "c1"), "c1"),
            "c1",
            1,
        ));
        for n in 2..=5 {
            harness.push(testing::with_state_delta(msg(n), "c1", n));
        }
        harness
    }

    fn contract_request(ids: Vec<Bytes>, version: dto::VersionParam) -> dto::StateRequestBody {
        dto::StateRequestBody {
            contract_ids: Some(ids),
            protocol_system: SYSTEM.to_string(),
            version,
            chain: dto::Chain::Ethereum,
            pagination: dto::PaginationParams::new(0, 100),
        }
    }

    fn protocol_request(ids: &[&str], version: dto::VersionParam) -> dto::ProtocolStateRequestBody {
        dto::ProtocolStateRequestBody {
            protocol_ids: Some(
                ids.iter()
                    .map(|id| id.to_string())
                    .collect(),
            ),
            protocol_system: SYSTEM.to_string(),
            chain: dto::Chain::Ethereum,
            include_balances: true,
            version,
            pagination: dto::PaginationParams::new(0, 100),
        }
    }

    fn at_block(n: u64) -> dto::VersionParam {
        dto::VersionParam::at_block(dto::Chain::Ethereum, n)
    }

    fn at_hash(n: u64) -> dto::VersionParam {
        #[allow(deprecated)]
        let block =
            dto::BlockParam { hash: Some(testing::block(n).hash), chain: None, number: None };
        dto::VersionParam::new(None, Some(block))
    }

    fn at_timestamp(n: u64) -> dto::VersionParam {
        dto::VersionParam::new(Some(testing::block(n).ts), None)
    }

    fn served_addresses(response: &dto::StateRequestResponse) -> Vec<Bytes> {
        response
            .accounts
            .iter()
            .map(|account| account.address.clone())
            .collect()
    }

    #[rstest]
    #[case::number_in_window(at_block(4), 4)]
    #[case::tip(at_block(5), 5)]
    #[case::default_is_the_tip(dto::VersionParam::default(), 5)]
    #[case::hash(at_hash(4), 4)]
    #[case::timestamp(at_timestamp(4), 4)]
    fn contract_state_serves_the_cached_entry_with_the_window_changes_up_to_the_version(
        #[case] version: dto::VersionParam,
        #[case] expected: u64,
    ) {
        let harness = accounts();

        let response = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], version))
            .unwrap();

        assert_eq!(response.accounts.len(), 1);
        assert_eq!(response.accounts[0].slots[&word(1)], word(expected));
        assert_eq!(response.accounts[0].token_balances[&addr(9)], Bytes::from(expected));
    }

    #[test]
    fn contract_state_builds_an_uncached_account_from_its_first_delta() {
        let harness = Harness::new(10);
        let (account, early_token, token) = (addr(2), addr(9), addr(10));
        harness.push(with_account_balance(msg(6), &account, &early_token, 6));
        harness.push(with_account_balance(
            with_account(msg(7), account_delta(&account, 7, ChangeType::Creation)),
            &account,
            &token,
            8,
        ));

        let at_6 = harness
            .service
            .contract_state(&contract_request(vec![account.clone()], at_block(6)));
        let at_7 = harness
            .service
            .contract_state(&contract_request(vec![account], at_block(7)))
            .unwrap();

        // Block 6 changed only a balance, so the address does not exist yet, and that balance is
        // dropped once block 7 creates the address.
        assert!(
            matches!(at_6, Err(StateServiceError::ContractNotFound(ref address)) if *address == addr(2)),
            "{at_6:?}"
        );
        assert_eq!(at_7.accounts[0].slots[&word(1)], word(7));
        assert_eq!(at_7.accounts[0].token_balances, HashMap::from([(token, Bytes::from(8u64))]));
    }

    /// Two extractors track one account. The other extractor's change at or below the version
    /// applies, and its change above the version does not.
    #[rstest]
    #[case::other_change_at_the_version(2, 2)]
    #[case::other_change_below_the_version(3, 2)]
    fn contract_state_applies_another_extractors_window_up_to_the_version(
        #[case] version: u64,
        #[case] expected: u64,
    ) {
        let harness = Harness::new(10);
        let account = addr(1);
        harness.push(with_account(msg(1), account_delta(&account, 1, ChangeType::Creation)));
        harness.push(msg(2));
        harness.push(msg(3));
        harness.push_other(other_msg(1));
        harness
            .push_other(with_account(other_msg(2), account_delta(&account, 2, ChangeType::Update)));
        harness.push_other(other_msg(3));
        harness
            .push_other(with_account(other_msg(4), account_delta(&account, 4, ChangeType::Update)));

        let response = harness
            .service
            .contract_state(&contract_request(vec![account], at_block(version)))
            .unwrap();

        assert_eq!(response.accounts[0].slots[&word(1)], word(expected));
    }

    /// The other extractor stamped block 3 a microsecond later, so the version's timestamp falls
    /// between its blocks 2 and 3. Its block 3 is newer than the version and must not apply.
    #[test]
    fn contract_state_skips_another_extractors_block_newer_than_the_version() {
        let harness = Harness::new(10);
        let account = addr(1);
        harness.push(with_account(msg(1), account_delta(&account, 1, ChangeType::Creation)));
        harness.push(msg(2));
        harness.push(msg(3));
        harness.push_other(other_msg(1));
        harness.push_other(other_msg(2));
        let mut later = with_account(other_msg(3), account_delta(&account, 3, ChangeType::Update));
        later.block.ts += chrono::Duration::microseconds(1);
        harness.push_other(later);

        let response = harness
            .service
            .contract_state(&contract_request(vec![account], at_block(3)))
            .unwrap();

        assert_eq!(response.accounts[0].slots[&word(1)], word(1));
    }

    #[test]
    fn contract_state_builds_an_uncached_account_from_another_extractors_window() {
        let harness = Harness::new(10);
        let account = addr(2);
        harness.push(msg(1));
        harness.push(msg(2));
        harness.push_other(other_msg(1));
        harness.push_other(with_account(
            other_msg(2),
            account_delta(&account, 2, ChangeType::Creation),
        ));

        let response = harness
            .service
            .contract_state(&contract_request(vec![account], at_block(2)))
            .unwrap();

        assert_eq!(response.accounts[0].slots[&word(1)], word(2));
    }

    #[test]
    fn contract_state_without_a_delta_for_an_uncached_account_is_not_found() {
        let harness = Harness::new(10);
        let (account, token) = (addr(2), addr(9));
        harness.push(with_account_balance(msg(6), &account, &token, 6));
        harness.push(with_account_balance(msg(7), &account, &token, 7));

        let result = harness
            .service
            .contract_state(&contract_request(vec![account], at_block(7)));

        assert!(
            matches!(result, Err(StateServiceError::ContractNotFound(ref address)) if *address == addr(2)),
            "{result:?}"
        );
    }

    #[test]
    fn contract_state_below_the_window_falls_back() {
        let harness = accounts();

        let result = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], at_block(3)));

        assert!(matches!(result, Err(StateServiceError::Fallback(FallbackReason::BelowWindow))));
    }

    #[rstest]
    #[case::empty_window(
        Harness::new(2),
        dto::VersionParam::default(),
        FallbackReason::EmptyWindow
    )]
    #[case::unknown_hash(accounts(), at_hash(9), FallbackReason::UnknownHash)]
    fn contract_state_falls_back_when_the_window_cannot_resolve_the_version(
        #[case] harness: Harness,
        #[case] version: dto::VersionParam,
        #[case] reason: FallbackReason,
    ) {
        let result = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], version));

        assert!(matches!(result, Err(StateServiceError::Fallback(r)) if r == reason), "{result:?}");
    }

    #[test]
    fn contract_state_above_the_tip_is_a_version_above_the_tip() {
        let harness = accounts();

        let result = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], at_block(6)));

        assert!(matches!(result, Err(StateServiceError::VersionAboveTip(_))), "{result:?}");
    }

    #[test]
    fn contract_state_skips_a_block_already_folded_into_the_entry() {
        let harness = accounts();
        // A fold that lands after the capture: block 4 is both in the window and in the entry.
        harness
            .cache
            .fold(&with_account(msg(4), account_delta(&addr(1), 4, ChangeType::Update)))
            .unwrap();

        let response = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], dto::VersionParam::default()))
            .unwrap();

        assert_eq!(response.accounts[0].slots[&word(1)], word(5));
    }

    #[test]
    fn contract_state_falls_back_when_a_cached_value_is_newer_than_the_version() {
        let harness = accounts();
        // Another extractor that shares the account folds a block past this window's tip.
        harness
            .cache
            .fold(&with_account(
                testing::aggregated_changes("other", 7, 7, Some(7)),
                account_delta(&addr(1), 7, ChangeType::Update),
            ))
            .unwrap();

        let result = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], dto::VersionParam::default()));

        assert!(matches!(result, Err(StateServiceError::Fallback(FallbackReason::EntryNewer))));
    }

    #[test]
    fn contract_state_builds_uncached_accounts_from_their_window_deltas() {
        let harness = accounts();
        harness.push(with_account(
            with_account(msg(6), account_delta(&addr(2), 6, ChangeType::Creation)),
            account_delta(&addr(3), 6, ChangeType::Update),
        ));

        let response = harness
            .service
            .contract_state(&contract_request(
                vec![addr(1), addr(2), addr(3)],
                dto::VersionParam::default(),
            ))
            .unwrap();

        assert_eq!(served_addresses(&response), vec![addr(1), addr(2), addr(3)]);
        assert_eq!(response.accounts[1].slots[&word(1)], word(6));
        assert_eq!(response.accounts[2].slots[&word(1)], word(6));
    }

    #[test]
    fn contract_state_fails_for_an_unknown_address() {
        let harness = accounts();

        let result = harness
            .service
            .contract_state(&contract_request(
                vec![addr(1), addr(4)],
                dto::VersionParam::default(),
            ));

        assert!(
            matches!(result, Err(StateServiceError::ContractNotFound(ref address)) if *address == addr(4)),
            "{result:?}"
        );
    }

    #[test]
    fn contract_state_paginates_the_requested_ids() {
        let harness = accounts();
        let mut request = contract_request(vec![addr(4), addr(1)], dto::VersionParam::default());
        request.pagination = dto::PaginationParams::new(1, 1);

        let response = harness
            .service
            .contract_state(&request)
            .unwrap();

        assert_eq!(served_addresses(&response), vec![addr(1)]);
        assert_eq!(response.pagination, PaginationResponse::new(1, 1, 2));
    }

    #[test]
    fn contract_state_serves_a_repeated_id_once() {
        let harness = accounts();

        let response = harness
            .service
            .contract_state(&contract_request(vec![addr(1), addr(1)], dto::VersionParam::default()))
            .unwrap();

        assert_eq!(served_addresses(&response), vec![addr(1)]);
        assert_eq!(response.pagination, PaginationResponse::new(0, 100, 2));
    }

    #[test]
    fn protocol_state_serves_a_repeated_id_once() {
        let harness = components();

        let response = harness
            .service
            .protocol_state(&protocol_request(&["c1", "c1"], dto::VersionParam::default()))
            .unwrap();

        assert_eq!(response.states.len(), 1);
        assert_eq!(response.pagination, PaginationResponse::new(0, 100, 2));
    }

    #[test]
    fn contract_state_without_ids_falls_back() {
        let harness = accounts();
        let mut request = contract_request(vec![], dto::VersionParam::default());
        request.contract_ids = None;

        let result = harness.service.contract_state(&request);

        assert!(matches!(result, Err(StateServiceError::Fallback(FallbackReason::NoIds))));
    }

    #[rstest]
    #[case::unknown("unknown")]
    #[case::empty("")]
    fn contract_state_with_an_unknown_system_falls_back(#[case] system: &str) {
        let harness = accounts();
        let mut request = contract_request(vec![addr(1)], dto::VersionParam::default());
        request.protocol_system = system.to_string();

        let result = harness.service.contract_state(&request);

        assert!(
            matches!(result, Err(StateServiceError::Fallback(FallbackReason::UnknownSystem))),
            "{result:?}"
        );
    }

    #[test]
    fn protocol_state_paginates_the_requested_ids() {
        let harness = components();
        let mut request = protocol_request(&["c2", "c1"], dto::VersionParam::default());
        request.pagination = dto::PaginationParams::new(1, 1);

        let response = harness
            .service
            .protocol_state(&request)
            .unwrap();

        let served: Vec<&str> = response
            .states
            .iter()
            .map(|state| state.component_id.as_str())
            .collect();
        assert_eq!(served, vec!["c1"]);
        assert_eq!(response.pagination, PaginationResponse::new(1, 1, 2));
    }

    #[test]
    fn protocol_state_without_ids_falls_back() {
        let harness = components();
        let mut request = protocol_request(&[], dto::VersionParam::default());
        request.protocol_ids = None;

        let result = harness.service.protocol_state(&request);

        assert!(matches!(result, Err(StateServiceError::Fallback(FallbackReason::NoIds))));
    }

    #[rstest]
    #[case::unknown("unknown")]
    #[case::empty("")]
    fn protocol_state_with_an_unknown_system_falls_back(#[case] system: &str) {
        let harness = components();
        let mut request = protocol_request(&["c1"], dto::VersionParam::default());
        request.protocol_system = system.to_string();

        let result = harness.service.protocol_state(&request);

        assert!(
            matches!(result, Err(StateServiceError::Fallback(FallbackReason::UnknownSystem))),
            "{result:?}"
        );
    }

    #[rstest]
    #[case::number_in_window(at_block(4), 4)]
    #[case::default_is_the_tip(dto::VersionParam::default(), 5)]
    fn protocol_state_serves_the_cached_entry_with_the_window_changes_up_to_the_version(
        #[case] version: dto::VersionParam,
        #[case] expected: u64,
    ) {
        let harness = components();

        let response = harness
            .service
            .protocol_state(&protocol_request(&["c1"], version))
            .unwrap();

        assert_eq!(response.states.len(), 1);
        assert_eq!(response.states[0].attributes["x"], Bytes::from(expected));
        assert_eq!(response.states[0].balances[&addr(9)], Bytes::from(1u64));
    }

    #[test]
    fn protocol_state_removes_balances_when_not_requested() {
        let harness = components();
        let mut request = protocol_request(&["c1"], dto::VersionParam::default());
        request.include_balances = false;

        let response = harness
            .service
            .protocol_state(&request)
            .unwrap();

        assert!(response.states[0].balances.is_empty());
    }

    #[test]
    fn protocol_state_keeps_deleted_attributes_deleted() {
        let harness = components();
        let mut m = msg(6);
        m.state_deltas.insert(
            "c1".to_string(),
            ProtocolComponentStateDelta {
                component_id: "c1".to_string(),
                deleted_attributes: HashSet::from(["x".to_string()]),
                ..Default::default()
            },
        );
        harness.push(m);

        let response = harness
            .service
            .protocol_state(&protocol_request(&["c1"], dto::VersionParam::default()))
            .unwrap();

        assert!(!response.states[0]
            .attributes
            .contains_key("x"));
    }

    #[test]
    fn protocol_state_serves_uncached_ids_as_empty_states() {
        let harness = components();
        harness.push(testing::with_state_delta(
            testing::with_state_delta(with_component(msg(6), "c2"), "c2", 6),
            "c3",
            6,
        ));

        let response = harness
            .service
            .protocol_state(&protocol_request(
                &["c1", "c2", "c3", "c4"],
                dto::VersionParam::default(),
            ))
            .unwrap();

        let ids: Vec<&str> = response
            .states
            .iter()
            .map(|state| state.component_id.as_str())
            .collect();
        assert_eq!(ids, vec!["c1", "c2", "c3", "c4"]);
        assert_eq!(response.states[1].attributes["x"], Bytes::from(6u64));
        assert_eq!(response.states[2].attributes["x"], Bytes::from(6u64));
        assert!(response.states[3].attributes.is_empty() && response.states[3].balances.is_empty());
    }

    #[test]
    fn protocol_state_skips_a_block_already_folded_into_the_entry() {
        let harness = components();
        // A fold that lands after the capture: block 4 is both in the window and in the entry.
        harness
            .cache
            .fold(&testing::with_state_delta(msg(4), "c1", 4))
            .unwrap();

        let response = harness
            .service
            .protocol_state(&protocol_request(&["c1"], dto::VersionParam::default()))
            .unwrap();

        assert_eq!(response.states[0].attributes["x"], Bytes::from(5u64));
    }

    #[test]
    fn protocol_state_falls_back_when_the_cached_entry_is_newer_than_the_version() {
        let harness = components();
        harness
            .cache
            .fold(&testing::with_state_delta(msg(7), "c1", 7))
            .unwrap();

        let result = harness
            .service
            .protocol_state(&protocol_request(&["c1"], dto::VersionParam::default()));

        assert!(matches!(result, Err(StateServiceError::Fallback(FallbackReason::EntryNewer))));
    }

    #[test]
    fn protocol_state_below_the_window_falls_back() {
        let harness = components();

        let result = harness
            .service
            .protocol_state(&protocol_request(&["c1"], at_block(3)));

        assert!(matches!(result, Err(StateServiceError::Fallback(FallbackReason::BelowWindow))));
    }
}
