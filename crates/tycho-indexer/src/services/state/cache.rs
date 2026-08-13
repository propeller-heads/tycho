//! Long-lived in-memory entity state.
//!
//! Two entity families are cached:
//!
//! - **Accounts** (contract state), keyed by address. Several extractors can write the same
//!   account, so every cached value carries the time it was last written — the writing block's
//!   timestamp, the same unit the database's `valid_from` versioning uses. A newer write always
//!   wins; re-applying an equal-time change is a no-op because delta values are absolute.
//! - **Component states** (protocol state), keyed by `(protocol system, component id)`. Exactly one
//!   extractor writes each protocol system, in order, so one write time per entry is enough.
//!
//! Tags compare with "not older" (>=), never "strictly newer": consecutive blocks can share a
//! timestamp on fast chains, and a strict comparison would silently drop the second block.
//!
//! The cache is written from exactly two places: the startup load, which runs before the
//! extractors start, and the folds coming out of the block windows. It never reads the database,
//! and it never evicts — an entity missing from the cache does not exist.
//!
//! Reads and folds take turns behind one read-write lock: a fold takes the write side and
//! applies one whole block atomically, reads take the read side. Folds are expected to take well
//! under a millisecond, so blocking is acceptable and a reader never observes half a block.

// Not yet constructed by production code; wired into the loader and the pump in follow-ups.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{RwLock, RwLockReadGuard},
};

use chrono::NaiveDateTime;
use tycho_common::{
    models::{
        blockchain::BlockAggregatedChanges,
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{ProtocolComponentState, ProtocolComponentStateDelta},
        Address, AttrStoreKey, Balance, Chain, Code, CodeHash, ComponentId, StoreKey, StoreVal,
        TxHash,
    },
    storage::StorageError,
};

use super::window::FoldSink;

/// A cached value together with the time it was last written (the writing block's timestamp).
type Tagged<T> = (T, NaiveDateTime);

/// Cached state of one contract account.
///
/// Every value carries the time it was last written, so writes from different extractors (which
/// run at different points of the chain) can never regress a value: newer wins, equal-time
/// re-application is a no-op.
pub(crate) struct CachedAccount {
    chain: Chain,
    title: String,
    slots: HashMap<StoreKey, Tagged<StoreVal>>,
    native_balance: Tagged<Balance>,
    token_balances: HashMap<Address, Tagged<AccountBalance>>,
    code: Tagged<Code>,
    /// Kept in sync when a fold carries code.
    code_hash: CodeHash,
    /// Transaction references come from the startup load only — folds don't carry them.
    balance_modify_tx: TxHash,
    code_modify_tx: TxHash,
    creation_tx: Option<TxHash>,
}

impl CachedAccount {
    /// Builds an entry from the startup snapshot. Each value's tag is its row's `valid_from`
    /// timestamp, taken as-is — the cache versions by time exactly like the database does.
    #[allow(unused_variables)]
    fn from_snapshot(filled: &Account, value_times: &HashMap<StoreKey, NaiveDateTime>) -> Self {
        todo!("build entry from snapshot")
    }

    /// Builds an entry from a `Creation` delta folded at time `at` — after startup, the only way
    /// a new contract enters the cache. Creation deltas carry the whole initial tracked state.
    #[allow(unused_variables)]
    fn from_creation(delta: &AccountDelta, at: NaiveDateTime) -> Self {
        todo!("build entry from creation delta")
    }

    /// Applies one folded delta; every changed value gets the block's timestamp as its tag.
    #[allow(unused_variables)]
    fn fold(&mut self, delta: &AccountDelta, at: NaiveDateTime) {
        // Deleted slots become the zero value (as in `Account::apply_delta`). A delta that
        // carries code also refreshes `code_hash` — `Account::apply_delta` does not maintain
        // that field, so don't reuse it blindly.
        todo!("apply folded delta")
    }

    /// Materializes the cached state as an [`Account`] for response assembly.
    #[allow(unused_variables)]
    fn materialize(&self, address: &Address) -> Account {
        todo!("assemble account")
    }
}

/// Cached state of one protocol component.
pub(crate) struct CachedComponentState {
    attributes: HashMap<AttrStoreKey, StoreVal>,
    balances: HashMap<Address, Balance>,
    /// One write time covers the whole entry: a single extractor writes each protocol system,
    /// in order.
    updated_at: NaiveDateTime,
}

impl CachedComponentState {
    /// Builds an entry from the startup snapshot, tagged with its newest `valid_from`.
    #[allow(unused_variables)]
    fn from_snapshot(state: &ProtocolComponentState, at: NaiveDateTime) -> Self {
        todo!("build entry from snapshot")
    }

    /// Applies one folded state delta when it is not older than the entry.
    #[allow(unused_variables)]
    fn fold(&mut self, delta: &ProtocolComponentStateDelta, at: NaiveDateTime) {
        // Skip when `at < self.updated_at` (replayed block). Deleted attributes are plain key
        // removals. Out-of-order folds from the single writer are a bug — debug-assert.
        todo!("apply folded state delta")
    }

    /// Applies folded balance changes.
    #[allow(unused_variables)]
    fn fold_balances(&mut self, balances: &HashMap<Address, Balance>, at: NaiveDateTime) {
        todo!("apply folded balances")
    }

    /// Materializes the cached state for response assembly.
    #[allow(unused_variables)]
    fn materialize(&self, component_id: &str) -> ProtocolComponentState {
        todo!("assemble component state")
    }
}

/// The long-lived entity store. See the module doc for the data model and locking.
pub(crate) struct EntityCache {
    /// Folds take the write side and apply one whole block atomically; reads take the read
    /// side. Folds are fast, so waiting is fine and a reader never sees half a block.
    state: RwLock<CacheState>,
}

/// The maps behind the lock.
pub(crate) struct CacheState {
    pub(crate) accounts: HashMap<Address, CachedAccount>,
    pub(crate) components: HashMap<(String, ComponentId), CachedComponentState>,
    // Memory accounting (running byte total, reconciled periodically) attaches here.
}

impl EntityCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(CacheState { accounts: HashMap::new(), components: HashMap::new() }),
        }
    }

    /// Read access for response assembly. Folds wait until the guard is dropped — hold it only
    /// long enough to copy out what the response needs.
    pub(crate) fn read(&self) -> RwLockReadGuard<'_, CacheState> {
        self.state
            .read()
            .expect("entity cache lock poisoned")
    }

    /// Startup load only: runs before the extractors start, so nothing else is writing.
    #[allow(unused_variables)]
    pub(crate) fn insert_loaded_account(&self, address: Address, entry: CachedAccount) {
        todo!("insert loaded account")
    }

    /// Startup load only.
    #[allow(unused_variables)]
    pub(crate) fn insert_loaded_component(
        &self,
        system: String,
        component_id: ComponentId,
        entry: CachedComponentState,
    ) {
        todo!("insert loaded component")
    }
}

impl FoldSink for EntityCache {
    #[allow(unused_variables)]
    fn apply_folded(
        &self,
        extractor: &str,
        block: &BlockAggregatedChanges,
    ) -> Result<(), StorageError> {
        // Under the write lock, apply the whole block in an order where new components exist
        // before their first attributes arrive:
        //
        // 1. `new_protocol_components` — create entries.
        // 2. `state_deltas` — apply where not older than the entry.
        // 3. `component_balances` — apply.
        // 4. `deleted_protocol_components` — remove entries.
        // 5. `account_deltas` — apply per value, tagged with this block's timestamp; a `Creation`
        //    delta for an unknown address creates the entry, any other change for an unknown
        //    address is skipped (partial data must never create an entry).
        // 6. `account_balances` — apply.
        //
        // Not folded in phase 1: `new_tokens`, `component_tvl`, `dci_update` — DB-served.
        //
        // An error means the block was not applied: the window keeps it and the process stops.
        // Fail before mutating, so a replay of the same block converges.
        todo!("fold one block")
    }
}
