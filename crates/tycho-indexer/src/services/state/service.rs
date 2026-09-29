//! Serves `/contract_state` and `/protocol_state` from the entity cache.
//!
//! A response is `cached entry ⊕ window changes up to the requested version`. The service never
//! reads the database. A request it cannot serve fails with [`StateServiceError::VersionTooOld`],
//! and the RPC handler answers it with today's code, which stays untouched: it is both the
//! fallback and the instant rollback (`ENTITY_CACHE_MODE=off`).
//!
//! # Read order
//!
//! A read holds the window lock while it resolves the version and copies the window changes for
//! its ids, releases it, then takes the cache read lock and copies the entries out. Folds hold the
//! window lock while they take the cache write lock, so a fold can land between the two steps.
//! That is harmless: the fold moves blocks from the copied changes into the entries, and a change
//! applies only when it is newer than the value's write timestamp, so nothing is applied twice or
//! lost.
//! A fold can also carry an entry past the requested version; the request then fails with
//! [`StateServiceError::VersionTooOld`].
//!
//! # Versions the cache cannot rebuild
//!
//! The cache keeps only the newest value of each entry, so it cannot serve a version older than
//! a value it holds. The version is then below the window, or it was inside the window and a
//! value is newer anyway: a fold landed after the version was resolved (which also moves the
//! version below the window), or another extractor that shares the entity is ahead of this one.
//! Today's handler answers every such version: from the versioned query when the database holds
//! it, otherwise as `latest from the DB ⊕ uncommitted window changes`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use thiserror::Error;
use tracing::error;
use tycho_common::{
    dto::{self, PaginationResponse},
    models::{contract::Account, protocol::ProtocolComponentState, MergeError, PaginationParams},
    storage::{BlockOrTimestamp, StorageError, WriteTimestamp},
    Bytes,
};

use super::{
    cache::{CachedAccount, EntityCache},
    window::{DeltaWindow, WindowResolution},
};
use crate::services::{deltas_buffer::PendingDeltasError, rpc::RpcError};

/// Which path answers state requests, holding what the cache modes need: the loaded
/// [`EntityCache`] when building the services, the [`StateService`] once built.
///
/// A cache mode without a cache, or `Off` with one, cannot be expressed.
#[derive(Clone, Debug)]
pub enum EntityCacheSetup<T> {
    /// See [`EntityCacheMode::Off`](super::EntityCacheMode::Off).
    Off,
    /// See [`EntityCacheMode::Shadow`](super::EntityCacheMode::Shadow).
    Shadow(T),
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
}

/// Why the state service did not answer a request.
#[derive(Debug, Error)]
pub(crate) enum StateServiceError {
    /// The cache cannot rebuild the requested version; the database path answers it instead.
    /// See the module doc for when this happens.
    #[error("Requested version is older than the entity cache")]
    VersionTooOld,
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
    /// `total` as the number of requested ids. An address the cache does not hold is built from its
    /// window deltas, like the database path does.
    ///
    /// # Errors
    ///
    /// [`StateServiceError::VersionTooOld`] when the cache cannot rebuild the version. Otherwise
    /// [`StateServiceError::Rpc`] with:
    ///
    /// - `RpcError::Parse` (400) when `contract_ids` is `None`: the cache serves explicit ids only.
    /// - `RpcError::Storage(StorageError::NotFound("Contract", ..))` when an address is neither
    ///   cached nor changed by a delta in the window.
    /// - `RpcError::Parse` (400) when the version is malformed, or `protocol_system` is empty or
    ///   has no window. Today this silently reads the database.
    /// - `RpcError::DeltasError` (500) when the window cannot be read or a change cannot be merged,
    ///   as on the database path.
    /// - `RpcError::Storage(StorageError::NotFound("Block", ..))` when the version is a block
    ///   number above the tip. tycho-client retries a body that contains `"Could not find Block"`
    ///   and may blacklist a component on any other text, so the entity name must be `Block`.
    pub(crate) fn contract_state(
        &self,
        request: &dto::StateRequestBody,
    ) -> Result<dto::StateRequestResponse, StateServiceError> {
        let ids = request
            .contract_ids
            .as_deref()
            .ok_or_else(|| RpcError::Parse("contract_ids are required".to_string()))?;
        // Slice the page out of the requested ids, like the database path does.
        let pagination = PaginationParams::from(&request.pagination);
        let page: Vec<Bytes> = ids
            .iter()
            .skip(pagination.offset() as usize)
            .take(pagination.page_size as usize)
            .cloned()
            .collect();
        // Resolve the version and copy the window changes for the page under one window lock, so
        // both see the same blocks.
        let (version, window_changes) =
            self.read_window(&request.protocol_system, &request.version, |window, upto| {
                window.account_changes(&page, upto)
            })?;

        // Copy the cached entries under the cache read lock; folds wait until it is released.
        let mut entries = Vec::with_capacity(page.len());
        {
            let cache = self.cache.read();
            for address in &page {
                let entry = cache.account(address);
                // The cache keeps only the newest value, so an entry written after `version`
                // cannot be rolled back to it. Resolving `version` in the window is not enough to
                // rule this out: a fold can land between the capture and this read, and another
                // extractor that shares the account folds blocks this window has not reached.
                if entry.is_some_and(|entry| entry.newest_write() > version) {
                    return Err(StateServiceError::VersionTooOld);
                }
                entries.push(entry.cloned());
            }
        }

        // Apply the window changes on top of each entry, without holding any lock.
        let mut accounts = Vec::with_capacity(page.len());
        for (address, entry) in page.iter().zip(entries) {
            let changes = window_changes
                .get(address)
                .map_or(&[][..], Vec::as_slice);
            let (mut entry, changes) = match entry {
                Some(entry) => (entry, changes),
                // Not cached: build the account from its first delta in the window and apply the
                // rest, as the database path does for an address it does not hold. An address with
                // no delta fails the whole request, as it does on the database path.
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
                        return Err(RpcError::Storage(StorageError::NotFound(
                            "Contract".to_string(),
                            address.to_string(),
                        ))
                        .into());
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
    /// empty state with its window changes applied, like the database path does, so no id fails
    /// the request. Deleted attributes stay deleted. With
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
        let ids = request
            .protocol_ids
            .as_deref()
            .ok_or_else(|| RpcError::Parse("protocol_ids are required".to_string()))?;
        // Slice the page out of the requested ids, like the database path does.
        let pagination = PaginationParams::from(&request.pagination);
        let page: Vec<&str> = ids
            .iter()
            .skip(pagination.offset() as usize)
            .take(pagination.page_size as usize)
            .map(String::as_str)
            .collect();
        let system = &request.protocol_system;
        // Resolve the version and copy the window changes for the page under one window lock, so
        // both see the same blocks.
        let (version, window_changes) =
            self.read_window(system, &request.version, |window, upto| {
                window.component_changes(&page, upto)
            })?;

        // Copy the cached entries under the cache read lock; folds wait until it is released.
        let mut entries = Vec::with_capacity(page.len());
        {
            let cache = self.cache.read();
            for id in &page {
                let entry = cache.component(system, id);
                // One extractor owns each component, so the entry can pass `version` only if this
                // window's own fold lands between the capture and this read with `version` at the
                // window floor. Not expected: log it and let the database path answer.
                if let Some(entry) = entry.filter(|entry| entry.updated_at() > version) {
                    error!(
                        component = %id,
                        entry = entry.updated_at().block_number(),
                        version = version.block_number(),
                        "Cached component is newer than the requested version"
                    );
                    return Err(StateServiceError::VersionTooOld);
                }
                entries.push(entry.cloned());
            }
        }

        // Apply the window changes on top of each entry, without holding any lock, with the
        // database path's merge. The changes hold absolute values in block order, so re-applying
        // one that a fold already moved into the entry leaves the same state.
        let mut states = Vec::with_capacity(page.len());
        for (id, entry) in page.iter().zip(entries) {
            // Not cached: start from an empty state, as the database path does for an id it does
            // not hold. An id the window never changed is served as that empty state.
            // TODO: serve unknown ids the same way for accounts and components: both as an empty
            // entity or both as an error.
            let mut state = entry.map_or_else(
                || ProtocolComponentState::new(id, HashMap::new(), HashMap::new()),
                ProtocolComponentState::from,
            );
            let merge_error = |err: MergeError| RpcError::from(PendingDeltasError::from(err));
            for change in window_changes
                .get(*id)
                .into_iter()
                .flatten()
            {
                if let Some(delta) = &change.delta {
                    state
                        .apply_state_delta(delta)
                        .map_err(merge_error)?;
                }
                if let Some(balances) = &change.balances {
                    state
                        .apply_balance_delta(balances)
                        .map_err(merge_error)?;
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

    /// Resolves `version` in the window of `protocol_system` and runs `read` on that window, up
    /// to the resolved block, under one lock: a fold or revert in between could otherwise remove
    /// the resolved block from the window. Returns the resolved block's write timestamp with what
    /// `read` returned; the lock is released before this returns.
    fn read_window<T>(
        &self,
        protocol_system: &str,
        version: &dto::VersionParam,
        read: impl FnOnce(&DeltaWindow, u64) -> Result<T, StorageError>,
    ) -> Result<(WriteTimestamp, T), StateServiceError> {
        let window = self
            .windows
            .get(protocol_system)
            .ok_or_else(|| {
                RpcError::Parse(format!("Unknown protocol system `{protocol_system}`"))
            })?;
        let version = BlockOrTimestamp::try_from(version).map_err(RpcError::from)?;
        let window = window.lock().map_err(|err| {
            RpcError::from(PendingDeltasError::LockError(
                protocol_system.to_string(),
                err.to_string(),
            ))
        })?;
        let block = match window.resolve(&version) {
            WindowResolution::InWindow(block) => block,
            WindowResolution::BelowFloor => return Err(StateServiceError::VersionTooOld),
            WindowResolution::AboveTip => {
                return Err(RpcError::Storage(StorageError::NotFound(
                    "Block".to_string(),
                    format!("{version:?}"),
                ))
                .into())
            }
        };
        let value = read(&window, block.number)
            .map_err(|err| RpcError::from(PendingDeltasError::from(err)))?;
        Ok((WriteTimestamp::from(&block), value))
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use rstest::rstest;
    use tycho_common::models::{
        blockchain::BlockAggregatedChanges,
        contract::AccountDelta,
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

    /// A service over one window of `depth` blocks, folding into a cache that starts empty.
    struct Harness {
        service: StateService,
        window: Arc<Mutex<DeltaWindow>>,
        cache: Arc<EntityCache>,
    }

    impl Harness {
        fn new(depth: u64) -> Self {
            let window = Arc::new(Mutex::new(DeltaWindow::new(
                SYSTEM.to_string(),
                WindowConfig { depth, min_fold_batch: 1 },
            )));
            let cache = Arc::new(EntityCache::new());
            let service = StateService::new(
                HashMap::from([(SYSTEM.to_string(), window.clone())]),
                cache.clone(),
            );
            Self { service, window, cache }
        }

        /// Inserts `m` and folds every block that became evictable into the cache.
        fn push(&self, m: BlockAggregatedChanges) {
            let mut window = self.window.lock().unwrap();
            window.insert(&Arc::new(m)).unwrap();
            window
                .fold_evictable(self.cache.as_ref())
                .unwrap();
        }
    }

    /// Block `n`, finalized and committed.
    fn msg(n: u64) -> BlockAggregatedChanges {
        testing::aggregated_changes(SYSTEM, n, n, Some(n))
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

    /// Account `addr(1)` created in block 1, slot 1 set to `n` in each block `n` up to 5. With
    /// depth 2 the cache holds blocks 1-3 and the window blocks 4-5.
    fn accounts() -> Harness {
        let harness = Harness::new(2);
        harness.push(with_account(msg(1), account_delta(&addr(1), 1, ChangeType::Creation)));
        for n in 2..=5 {
            harness.push(with_account(msg(n), account_delta(&addr(1), n, ChangeType::Update)));
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
    }

    #[test]
    fn contract_state_below_the_window_is_too_old() {
        let harness = accounts();

        let result = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], at_block(3)));

        assert!(matches!(result, Err(StateServiceError::VersionTooOld)));
    }

    #[test]
    fn contract_state_above_the_tip_is_a_block_not_found() {
        let harness = accounts();

        let result = harness
            .service
            .contract_state(&contract_request(vec![addr(1)], at_block(6)));

        let Err(StateServiceError::Rpc(err @ RpcError::Storage(StorageError::NotFound(..)))) =
            result
        else {
            panic!("expected a not-found error, got {result:?}");
        };
        assert!(err
            .to_string()
            .contains("Could not find Block"));
    }

    #[test]
    fn contract_state_is_too_old_when_a_cached_value_is_newer_than_the_version() {
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

        assert!(matches!(result, Err(StateServiceError::VersionTooOld)));
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

        let Err(StateServiceError::Rpc(err)) = result else {
            panic!("expected a not-found error, got {result:?}");
        };
        assert!(
            matches!(&err, RpcError::Storage(StorageError::NotFound(entity, id)) if entity == "Contract" && *id == addr(4).to_string()),
            "{err:?}"
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
    fn contract_state_rejects_requests_without_ids_or_with_an_unknown_system() {
        let harness = accounts();
        let mut without_ids = contract_request(vec![], dto::VersionParam::default());
        without_ids.contract_ids = None;
        let mut unknown_system = contract_request(vec![addr(1)], dto::VersionParam::default());
        unknown_system.protocol_system = "unknown".to_string();

        for request in [without_ids, unknown_system] {
            let result = harness.service.contract_state(&request);
            assert!(
                matches!(result, Err(StateServiceError::Rpc(RpcError::Parse(_)))),
                "{result:?}"
            );
        }
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
    fn protocol_state_serves_uncached_ids_like_the_database_path() {
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
    fn protocol_state_reapplies_a_block_already_folded_into_the_entry() {
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
    fn protocol_state_is_too_old_when_the_cached_entry_is_newer_than_the_version() {
        let harness = components();
        harness
            .cache
            .fold(&testing::with_state_delta(msg(7), "c1", 7))
            .unwrap();

        let result = harness
            .service
            .protocol_state(&protocol_request(&["c1"], dto::VersionParam::default()));

        assert!(matches!(result, Err(StateServiceError::VersionTooOld)));
    }

    #[test]
    fn protocol_state_below_the_window_is_too_old() {
        let harness = components();

        let result = harness
            .service
            .protocol_state(&protocol_request(&["c1"], at_block(3)));

        assert!(matches!(result, Err(StateServiceError::VersionTooOld)));
    }
}
