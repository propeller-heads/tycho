//! Long-lived in-memory entity state, fed by window folds and database fills.
//!
//! Two entity families are cached:
//!
//! - **Accounts** (contract state), keyed by address. Shared: several extractors can fold into the
//!   same account at different block heights, so every cached value carries the block that last
//!   wrote it (per-slot / per-field tags).
//! - **Component states** (protocol state), keyed by `(protocol system, component id)`. Each
//!   protocol system has exactly one extractor and its folds are monotonic, so one height per entry
//!   is enough: a write applies only when its block is higher than the stored height.
//!
//! Invariants the cache upholds:
//!
//! - **Highest block wins.** Delta values are absolute, so re-applying an already-applied block is
//!   a no-op and out-of-order writers can never regress a value.
//! - **Entries exist only complete.** An entry is created by a database fill or by a `Creation`
//!   delta (both carry the entity's whole tracked state) — never by a partial update. Presence in
//!   the cache is therefore proof that a hit is servable.
//! - **Fills publish value-by-value.** A fill is a snapshot of the database at a stamped block; it
//!   is published with set-if-newer per value and never replaces a whole entry, so a fold that
//!   landed while the fill was in flight cannot be overwritten with older data.
//!
//! Locking: one mutex per entity, no whole-cache lock. The outer maps are only locked to look up
//! or insert entry handles; folds and reads then lock the single entity they touch, so a fold on
//! one account never blocks a read of another.
//!
//! Database fills are deliberately **not** deduplicated. Requests that miss the same entity
//! concurrently may each query the database for it — accepted and measured rather than
//! prevented, because three existing mechanisms already make duplicates rare and harmless:
//!
//! - set-if-newer publication makes concurrent fills of one entity converge, so a duplicate costs a
//!   redundant indexed point-read, never a wrong value;
//! - identical request bodies are collapsed upstream by the HTTP response cache's in-flight
//!   deduplication, which covers reconnecting clients snapshotting the same block;
//! - the fill path's semaphore bounds total concurrent fill queries regardless.
//!
//! What remains is overlapping-but-not-identical requests (adjacent blocks, different page
//! slicing). Their rate is observable via [`EntityCache::record_fill_start`]; per-entity
//! single-flight (a guard map with waiter wake-up) can be reintroduced behind the same call
//! sites if that metric ever shows the redundancy matters.

// Not yet constructed by production code; wired into the pending-deltas facade and the fill path
// in follow-ups.
#![allow(dead_code)]

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, RwLock},
};

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

/// A cached value together with the block number that last wrote it.
type Tagged<T> = (T, u64);

/// Handle to one cached entry; hold the lock for the shortest possible span.
type EntryHandle<T> = Arc<Mutex<T>>;

/// Entry handles by key. The outer lock guards map membership only; entry state is behind the
/// per-entry mutex.
type EntryMap<K, T> = RwLock<HashMap<K, EntryHandle<T>>>;

/// Cached state of one contract account.
///
/// Every mutable value is tagged with its writing block (see the module doc); `set_if_newer`
/// compares tags so fills and folds can interleave in any order and converge to the
/// highest-block value per field.
pub(crate) struct CachedAccount {
    chain: Chain,
    title: String,
    slots: HashMap<StoreKey, Tagged<StoreVal>>,
    native_balance: Tagged<Balance>,
    token_balances: HashMap<Address, Tagged<AccountBalance>>,
    code: Tagged<Code>,
    /// Maintained alongside `code`: recomputed when a fold carries code, taken verbatim from
    /// fills. The database path and the delta path are known to disagree on this field today;
    /// matching the delta path here is intentional.
    code_hash: CodeHash,
    /// Transaction references come from fills only — folded deltas do not carry them, matching
    /// the delta-patched reads served today.
    balance_modify_tx: TxHash,
    code_modify_tx: TxHash,
    creation_tx: Option<TxHash>,
}

impl CachedAccount {
    /// Builds a complete entry from a database fill stamped at block `stamp`.
    #[allow(unused_variables)]
    fn from_fill(filled: &Account, stamp: u64) -> Self {
        // Every value starts tagged with `stamp`: the fill is a snapshot of the database at that
        // block, no per-value provenance exists or is needed.
        todo!("build entry from filled account")
    }

    /// Builds a complete entry from a `Creation` delta folded at block `block`.
    #[allow(unused_variables)]
    fn from_creation(delta: &AccountDelta, block: u64) -> Self {
        // Creation deltas carry the account's whole initial tracked state (the equivalent of
        // `AccountDelta::into_account_without_tx`); transaction references stay empty until a
        // fill provides them.
        todo!("build entry from creation delta")
    }

    /// Applies one folded delta for block `block` to this entry.
    #[allow(unused_variables)]
    fn fold(&mut self, delta: &AccountDelta, block: u64) {
        // - Each slot in the delta gets its value (deleted slots become the zero value, as in
        //   `Account::apply_delta`) and `block` as its tag.
        // - Balance and code update likewise when present; code updates recompute `code_hash`.
        // - Tags make re-application of an already-folded block a no-op.
        todo!("apply folded delta")
    }

    /// Publishes a database fill stamped at block `stamp`, value by value.
    #[allow(unused_variables)]
    fn set_if_newer(&mut self, filled: &Account, stamp: u64) {
        // Per slot / field: write only when `stamp` is greater than the stored tag. Never clear
        // values absent from the fill — an absent slot means "not tracked by this query", not
        // "deleted".
        todo!("tag-compared publication")
    }

    /// Materializes the cached base state as an [`Account`] for response assembly.
    #[allow(unused_variables)]
    fn materialize(&self, address: &Address) -> Account {
        // Tags are dropped here; callers that patch window deltas on top must read the tags via
        // the entry handle instead (the patching read path lands with the routing layer).
        todo!("assemble account")
    }
}

/// Cached state of one protocol component.
pub(crate) struct CachedComponentState {
    attributes: HashMap<AttrStoreKey, StoreVal>,
    balances: HashMap<Address, Balance>,
    /// Block height of the whole entry. One extractor writes each protocol system and its folds
    /// are monotonic, so a single height replaces per-value tags; writes apply only when their
    /// block exceeds it.
    height: u64,
}

impl CachedComponentState {
    /// Applies one folded state delta for block `block`.
    #[allow(unused_variables)]
    fn fold(&mut self, delta: &ProtocolComponentStateDelta, block: u64) {
        // Skip when `block <= self.height` (re-applied block); debug-assert on `<` — an
        // out-of-order fold from the single writer is a bug, not a race. Otherwise extend
        // `attributes` with `updated_attributes`, remove `deleted_attributes` keys (plain
        // removal, no tombstones), set `height = block`.
        todo!("apply folded state delta")
    }

    /// Applies folded balance changes for block `block`.
    #[allow(unused_variables)]
    fn fold_balances(&mut self, balances: &HashMap<Address, Balance>, block: u64) {
        todo!("apply folded balances")
    }

    /// Publishes a database fill stamped at block `stamp` when it is newer than the entry.
    #[allow(unused_variables)]
    fn set_if_newer(&mut self, filled: &ProtocolComponentState, stamp: u64) {
        // Single-height entries publish atomically: apply the whole fill iff
        // `stamp > self.height`.
        todo!("height-compared publication")
    }

    /// Materializes the cached base state for response assembly.
    #[allow(unused_variables)]
    fn materialize(&self, component_id: &str) -> ProtocolComponentState {
        todo!("assemble component state")
    }
}

/// Identifies one cacheable entity; used to track in-flight fills for the duplicate metric.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EntityKey {
    Account(Address),
    Component(String, ComponentId),
}

/// Memory bounds for the cache.
pub(crate) struct CacheLimits {
    /// Byte cap for cached entries. Least-recently-used whole entities are evicted above it;
    /// eviction is safe because entries are complete and re-fillable.
    pub(crate) max_bytes: u64,
}

/// The long-lived entity store. See the module doc for the data model and invariants.
pub(crate) struct EntityCache {
    accounts: EntryMap<Address, CachedAccount>,
    components: EntryMap<(String, ComponentId), CachedComponentState>,
    /// Keys with a database fill currently in flight. Observability only: fills are not
    /// deduplicated (see the module doc), this set just counts concurrent duplicates.
    in_flight_fills: Mutex<HashSet<EntityKey>>,
    limits: CacheLimits,
}

impl EntityCache {
    pub(crate) fn new(limits: CacheLimits) -> Self {
        Self {
            accounts: RwLock::new(HashMap::new()),
            components: RwLock::new(HashMap::new()),
            in_flight_fills: Mutex::new(HashSet::new()),
            limits,
        }
    }

    /// Returns the entry handle for `address`, if cached.
    ///
    /// Presence means the entry is complete and servable. Callers lock the returned handle for
    /// the shortest possible span — a held entry lock delays folds of that entity.
    #[allow(unused_variables)]
    pub(crate) fn account(&self, address: &Address) -> Option<EntryHandle<CachedAccount>> {
        // Clone the `Arc` under the outer read guard, drop the guard before the caller locks the
        // entry, record the access for eviction ordering.
        todo!("account lookup")
    }

    /// Returns the entry handle for `(system, component_id)`, if cached.
    #[allow(unused_variables)]
    pub(crate) fn component(
        &self,
        system: &str,
        component_id: &str,
    ) -> Option<EntryHandle<CachedComponentState>> {
        todo!("component lookup")
    }

    /// Publishes a filled account stamped at block `stamp`.
    ///
    /// Creates the entry when absent (fills are complete by construction); otherwise defers to
    /// [`CachedAccount::set_if_newer`] so concurrent folds are never regressed.
    #[allow(unused_variables)]
    pub(crate) fn set_account_if_newer(&self, address: Address, filled: &Account, stamp: u64) {
        todo!("publish account fill")
    }

    /// Publishes a filled component state stamped at block `stamp`.
    #[allow(unused_variables)]
    pub(crate) fn set_component_if_newer(
        &self,
        system: &str,
        filled: &ProtocolComponentState,
        stamp: u64,
    ) {
        todo!("publish component fill")
    }

    /// Records the start of a database fill for `keys`, returning how many of them already have
    /// a fill in flight from a concurrent caller.
    ///
    /// Observability only: nothing waits on this set and duplicate fills are allowed (see the
    /// module doc). The return value feeds the `entity_cache_fill_duplicates` counter — the
    /// signal that decides whether per-entity single-flight is ever worth reintroducing.
    #[allow(unused_variables)]
    pub(crate) fn record_fill_start(&self, keys: &[EntityKey]) -> usize {
        // Insert every key into `in_flight_fills`; count the ones that were already present.
        todo!("track in-flight fill keys")
    }

    /// Records the end of a database fill for `keys` (success or failure).
    ///
    /// Callers release from a drop guard so error paths unwind the set too. Because the set is
    /// a plain membership set, a key filled concurrently by two callers is released by the first
    /// to finish — the tail of the second fill goes uncounted. A leaked or early-released key
    /// only skews the duplicate metric; it can never block serving.
    #[allow(unused_variables)]
    pub(crate) fn record_fill_end(&self, keys: &[EntityKey]) {
        todo!("release in-flight fill keys")
    }

    /// Evicts least-recently-used entities until the cache fits `limits.max_bytes`.
    #[allow(unused_variables)]
    pub(crate) fn evict_to_cap(&self) {
        // - Size accounting: running byte total maintained on insert/update/remove, reconciled
        //   periodically against a full recomputation (the reporting task owns the cadence).
        // - Order: reads and fill publications count as use; folds do not — a folded-but-never-
        //   read entity is a fine eviction candidate.
        // - Evict whole entities only; count evictions for the sustained-eviction alert. Evicting
        //   an entity mid-fill is safe: the fill republishes a complete entry.
        todo!("lru eviction")
    }
}

impl FoldSink for EntityCache {
    #[allow(unused_variables)]
    fn apply_folded(
        &self,
        extractor: &str,
        block: &BlockAggregatedChanges,
    ) -> Result<(), StorageError> {
        // Component families first, so entries created by this block exist before the same
        // block's deltas and balances land on them:
        //
        // 1. `new_protocol_components` — create `(extractor, component id)` entries. New entries
        //    start empty with a height below this block so steps 2 and 3 fill their initial state.
        // 2. `state_deltas` — `CachedComponentState::fold` on existing entries; unknown component
        //    ids are skipped (partial data never creates entries).
        // 3. `component_balances` — `CachedComponentState::fold_balances`.
        // 4. `deleted_protocol_components` — remove entries.
        //
        // Then accounts:
        //
        // 5. `account_deltas` — `CachedAccount::fold` on existing entries; unknown addresses create
        //    an entry via `CachedAccount::from_creation` only for `Creation` deltas and are skipped
        //    otherwise.
        // 6. `account_balances` — token balances, tagged with this block.
        //
        // Not folded here: `new_tokens`, `component_tvl`, `dci_update` — those entities stay
        // database-served.
        //
        // Errors: per the [`FoldSink`] contract an error means the block was not fully applied
        // and the window keeps it buffered. Apply per entity family and fail before mutating on
        // malformed input (e.g. an id mismatch), so a re-fold of the same block converges via
        // the tag/height no-op rule.
        todo!("fold one block")
    }
}
