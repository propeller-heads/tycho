//! Entity-level cache for the state serving path.
//!
//! ## Why
//!
//! `contract_state` and `protocol_state` snapshots are served straight from Postgres. The
//! response-level [`RpcCache`](super::cache::RpcCache) only dedups byte-identical requests, so a
//! client reconnect stampede turns into a Postgres stampede: cache keys rotate every time the db
//! commit height moves (~12s) and client chunk boundaries shift whenever the tracked component set
//! drifts. This cache is keyed by *entity* (account address, or protocol system + component id)
//! instead of by request, so a tip snapshot is assembled from memory regardless of how the request
//! was sliced.
//!
//! ## Invariants
//!
//! * **I1 — highest block wins.** Every cached value carries the block number that produced it.
//!   Extractors commit at independent heights and several extractors can report the same account,
//!   so an incoming delta may be *older* than what is cached. Applying it unconditionally would
//!   reintroduce a stale value; Postgres never does that because it reads the row with the highest
//!   `valid_from`. Version tags reproduce that ordering in memory.
//! * **I2 — committed data only.** The cache is fed from the blocks that
//!   [`PendingDeltas`](super::deltas_buffer::PendingDeltas) drains after an extractor reports
//!   `db_committed_block_height`. Drained blocks are at or below `finalized_block_height` (the
//!   `into_aggregated` invariant), therefore they cannot be reverted, and partial-block messages
//!   never reach the buffer at all.
//! * **I3 — fills are pinned.** A db fill must be read at an explicit block number, not at
//!   `Latest`, because the fill's version is what decides whether a concurrently drained delta is
//!   newer (I1).
//! * **I4 — buffer before cache.** A reader captures the pending patch *first* and reads the cache
//!   *second*. The writer applies to the cache *before* dropping the block from the buffer. Those
//!   two orders together guarantee every block is visible in at least one of the two sources; the
//!   reverse read order can miss a block that is drained between the two reads.
//! * **I5 — deltas never create entries.** An account delta is not a full account (a DCI `Creation`
//!   delta for a token carries only the tracked slots), so an entry only ever originates from a db
//!   fill. Deltas for keys that are not cached are dropped, which is also what makes eviction safe.
//!
//! The cache is a scaffold: the types and the update/serve API are here, the wiring is not (see the
//! `TODO(entity-cache)` markers in `deltas_buffer.rs` and `rpc.rs`).

use std::{
    collections::{
        hash_map::{DefaultHasher, Entry},
        HashMap,
    },
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use deepsize::{Context, DeepSizeOf};
use metrics::{counter, gauge};
use tokio::sync::Notify;
use tracing::warn;
use tycho_common::models::{
    blockchain::BlockAggregatedChanges,
    contract::{Account, AccountBalance, AccountDelta},
    protocol::{ComponentBalance, ProtocolComponentState, ProtocolComponentStateDelta},
    Address, AttrStoreKey, Balance, Chain, Code, CodeHash, ComponentId, StoreKey, StoreVal, TxHash,
};

/// Number of shards used by the entity maps. Reads are per-request and short (they materialize an
/// owned entity), writes happen once per extractor commit, so contention is low and 64 shards are
/// enough to keep a commit from stalling readers.
const SHARD_COUNT: usize = 64;

pub type BlockNumber = u64;

/// Identifies a cached component state. `protocol_component` is unique per
/// (chain, protocol system, external id), and `get_protocol_states` filters by system, so the
/// system has to be part of the key to keep today's response semantics.
pub type ComponentKey = (String, ComponentId);

/// A cached value together with the block that last set it. See invariant I1.
#[derive(Clone, Debug, PartialEq)]
struct Versioned<T> {
    value: T,
    block: BlockNumber,
}

impl<T> Versioned<T> {
    fn new(value: T, block: BlockNumber) -> Self {
        Self { value, block }
    }

    /// Overwrites the value if `block` is at least as recent as the stored one. Equal blocks
    /// overwrite because the same block can be seen twice (once from a drained delta, once from
    /// the pending patch) with identical values.
    fn set_if_newer(&mut self, value: T, block: BlockNumber) -> bool {
        if block >= self.block {
            self.value = value;
            self.block = block;
            true
        } else {
            false
        }
    }
}

impl<T: DeepSizeOf> DeepSizeOf for Versioned<T> {
    fn deep_size_of_children(&self, context: &mut Context) -> usize {
        self.value
            .deep_size_of_children(context)
    }
}

/// Cached committed state of a single account.
///
/// Mirrors [`Account`] field by field, with per-field version tags. It is not an `Account` because
/// serving has to materialize an owned value anyway (the response is serialized), so the version
/// tags cost nothing at read time.
#[derive(Clone, Debug)]
pub struct CachedAccount {
    chain: Chain,
    address: Address,
    title: String,
    slots: HashMap<StoreKey, Versioned<StoreVal>>,
    native_balance: Versioned<Balance>,
    code: Versioned<Code>,
    code_hash: CodeHash,
    balance_modify_tx: TxHash,
    code_modify_tx: TxHash,
    creation_tx: Option<TxHash>,
    token_balances: HashMap<Address, Versioned<AccountBalance>>,
    /// Block the db fill was pinned at (I3).
    filled_at: BlockNumber,
}

impl DeepSizeOf for CachedAccount {
    fn deep_size_of_children(&self, context: &mut Context) -> usize {
        self.address
            .deep_size_of_children(context) +
            self.title
                .deep_size_of_children(context) +
            self.slots
                .deep_size_of_children(context) +
            self.native_balance
                .deep_size_of_children(context) +
            self.code.deep_size_of_children(context) +
            self.code_hash
                .deep_size_of_children(context) +
            self.balance_modify_tx
                .deep_size_of_children(context) +
            self.code_modify_tx
                .deep_size_of_children(context) +
            self.creation_tx
                .deep_size_of_children(context) +
            self.token_balances
                .deep_size_of_children(context)
    }
}

impl CachedAccount {
    /// Builds an entry from a db read pinned at `filled_at` (I3).
    pub fn from_db(account: Account, filled_at: BlockNumber) -> Self {
        Self {
            chain: account.chain,
            address: account.address,
            title: account.title,
            slots: account
                .slots
                .into_iter()
                .map(|(k, v)| (k, Versioned::new(v, filled_at)))
                .collect(),
            native_balance: Versioned::new(account.native_balance, filled_at),
            code: Versioned::new(account.code, filled_at),
            code_hash: account.code_hash,
            balance_modify_tx: account.balance_modify_tx,
            code_modify_tx: account.code_modify_tx,
            creation_tx: account.creation_tx,
            token_balances: account
                .token_balances
                .into_iter()
                .map(|(k, v)| (k, Versioned::new(v, filled_at)))
                .collect(),
            filled_at,
        }
    }

    /// Applies a committed account delta. `None` slot values mean "zero" and are stored the way
    /// `Account::apply_delta` stores them, so the cache and the db+buffer path agree byte for byte.
    fn apply_delta(&mut self, block: BlockNumber, delta: &AccountDelta) {
        for (slot, value) in &delta.slots {
            let value = value.clone().unwrap_or_default();
            match self.slots.entry(slot.clone()) {
                Entry::Occupied(mut e) => {
                    if !e.get_mut().set_if_newer(value, block) {
                        counter!("entity_cache_stale_applies", "entity" => "account").increment(1);
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(Versioned::new(value, block));
                }
            }
        }
        if let Some(balance) = delta.balance.as_ref() {
            self.native_balance
                .set_if_newer(balance.clone(), block);
        }
        if let Some(code) = delta.code().as_ref() {
            // TODO(entity-cache): today's buffer path leaves `code_hash` at the db value when a
            // delta changes the code (see the TODO in `Account::apply_delta`). Decide whether to
            // keep that behaviour or recompute keccak256(code) here — the two differ in what
            // clients observe, so it is a compatibility decision, not a cleanup.
            self.code
                .set_if_newer(code.clone(), block);
        }
    }

    fn apply_token_balances(
        &mut self,
        block: BlockNumber,
        balances: &HashMap<Address, AccountBalance>,
    ) {
        for (token, balance) in balances {
            match self.token_balances.entry(token.clone()) {
                Entry::Occupied(mut e) => {
                    e.get_mut()
                        .set_if_newer(balance.clone(), block);
                }
                Entry::Vacant(e) => {
                    e.insert(Versioned::new(balance.clone(), block));
                }
            }
        }
    }

    /// Materializes the account the RPC handler serves: cached committed state with the
    /// still-uncommitted patch applied on top, per-field highest-block-wins (I1).
    fn materialize(&self, patch: Option<&AccountPatch>) -> Account {
        let mut account = Account::new(
            self.chain,
            self.address.clone(),
            self.title.clone(),
            self.slots
                .iter()
                .map(|(k, v)| (k.clone(), v.value.clone()))
                .collect(),
            self.native_balance.value.clone(),
            self.token_balances
                .iter()
                .map(|(k, v)| (k.clone(), v.value.clone()))
                .collect(),
            self.code.value.clone(),
            self.code_hash.clone(),
            self.balance_modify_tx.clone(),
            self.code_modify_tx.clone(),
            self.creation_tx.clone(),
        );

        let Some(patch) = patch else { return account };

        for (block, delta) in &patch.deltas {
            for (slot, value) in &delta.slots {
                if self.slot_version(slot) <= *block {
                    account
                        .slots
                        .insert(slot.clone(), value.clone().unwrap_or_default());
                }
            }
            if let Some(balance) = delta.balance.as_ref() {
                if self.native_balance.block <= *block {
                    account.native_balance = balance.clone();
                }
            }
            if let Some(code) = delta.code().as_ref() {
                if self.code.block <= *block {
                    account.code = code.clone();
                }
            }
        }
        for (block, balances) in &patch.token_balances {
            for (token, balance) in balances {
                let cached = self
                    .token_balances
                    .get(token)
                    .map_or(0, |v| v.block);
                if cached <= *block {
                    account
                        .token_balances
                        .insert(token.clone(), balance.clone());
                }
            }
        }
        account
    }

    fn slot_version(&self, slot: &StoreKey) -> BlockNumber {
        self.slots
            .get(slot)
            .map_or(0, |v| v.block)
    }
}

/// Cached committed state of a single protocol component.
#[derive(Clone, Debug)]
pub struct CachedComponentState {
    component_id: ComponentId,
    /// A `None` value is a tombstone: the attribute was deleted at that block. Tombstones are kept
    /// so an older update from a lagging extractor cannot resurrect a deleted attribute (I1).
    attributes: HashMap<AttrStoreKey, Versioned<Option<StoreVal>>>,
    balances: HashMap<Address, Versioned<Balance>>,
    filled_at: BlockNumber,
}

impl DeepSizeOf for CachedComponentState {
    fn deep_size_of_children(&self, context: &mut Context) -> usize {
        self.component_id
            .deep_size_of_children(context) +
            self.attributes
                .deep_size_of_children(context) +
            self.balances
                .deep_size_of_children(context)
    }
}

impl CachedComponentState {
    pub fn from_db(state: ProtocolComponentState, filled_at: BlockNumber) -> Self {
        Self {
            component_id: state.component_id,
            attributes: state
                .attributes
                .into_iter()
                .map(|(k, v)| (k, Versioned::new(Some(v), filled_at)))
                .collect(),
            balances: state
                .balances
                .into_iter()
                .map(|(k, v)| (k, Versioned::new(v, filled_at)))
                .collect(),
            filled_at,
        }
    }

    fn apply_delta(&mut self, block: BlockNumber, delta: &ProtocolComponentStateDelta) {
        for (attr, value) in &delta.updated_attributes {
            self.set_attribute(block, attr, Some(value.clone()));
        }
        for attr in &delta.deleted_attributes {
            self.set_attribute(block, attr, None);
        }
    }

    fn set_attribute(&mut self, block: BlockNumber, attr: &AttrStoreKey, value: Option<StoreVal>) {
        match self.attributes.entry(attr.clone()) {
            Entry::Occupied(mut e) => {
                if !e.get_mut().set_if_newer(value, block) {
                    counter!("entity_cache_stale_applies", "entity" => "component").increment(1);
                }
            }
            Entry::Vacant(e) => {
                e.insert(Versioned::new(value, block));
            }
        }
    }

    fn apply_balances(
        &mut self,
        block: BlockNumber,
        balances: &HashMap<Address, ComponentBalance>,
    ) {
        for (token, balance) in balances {
            match self.balances.entry(token.clone()) {
                Entry::Occupied(mut e) => {
                    e.get_mut()
                        .set_if_newer(balance.balance.clone(), block);
                }
                Entry::Vacant(e) => {
                    e.insert(Versioned::new(balance.balance.clone(), block));
                }
            }
        }
    }

    fn materialize(&self, patch: Option<&ComponentPatch>) -> ProtocolComponentState {
        let mut state = ProtocolComponentState::new(
            &self.component_id,
            self.attributes
                .iter()
                .filter_map(|(k, v)| {
                    v.value
                        .as_ref()
                        .map(|value| (k.clone(), value.clone()))
                })
                .collect(),
            self.balances
                .iter()
                .map(|(k, v)| (k.clone(), v.value.clone()))
                .collect(),
        );

        let Some(patch) = patch else { return state };

        for (block, delta) in &patch.deltas {
            for (attr, value) in &delta.updated_attributes {
                if self.attribute_version(attr) <= *block {
                    state
                        .attributes
                        .insert(attr.clone(), value.clone());
                }
            }
            for attr in &delta.deleted_attributes {
                if self.attribute_version(attr) <= *block {
                    state.attributes.remove(attr);
                }
            }
        }
        for (block, balances) in &patch.balances {
            for (token, balance) in balances {
                let cached = self
                    .balances
                    .get(token)
                    .map_or(0, |v| v.block);
                if cached <= *block {
                    state
                        .balances
                        .insert(token.clone(), balance.balance.clone());
                }
            }
        }
        state
    }

    fn attribute_version(&self, attr: &AttrStoreKey) -> BlockNumber {
        self.attributes
            .get(attr)
            .map_or(0, |v| v.block)
    }
}

/// Uncommitted deltas for one account, ascending by block number.
#[derive(Debug, Default, Clone)]
pub struct AccountPatch {
    deltas: Vec<(BlockNumber, AccountDelta)>,
    token_balances: Vec<(BlockNumber, HashMap<Address, AccountBalance>)>,
}

/// Uncommitted deltas for one component, ascending by block number.
#[derive(Debug, Default, Clone)]
pub struct ComponentPatch {
    deltas: Vec<(BlockNumber, ProtocolComponentStateDelta)>,
    balances: Vec<(BlockNumber, HashMap<Address, ComponentBalance>)>,
}

/// The pending (uncommitted) deltas a reader captured from the reorg buffer before reading the
/// cache (I4). Only the requested keys are collected; buffered blocks are few but a whole block's
/// deltas are large, so cloning entire messages is not an option.
#[derive(Debug, Default, Clone)]
pub struct PendingPatch {
    accounts: HashMap<Address, AccountPatch>,
    components: HashMap<ComponentId, ComponentPatch>,
}

impl PendingPatch {
    /// Collects the deltas of one buffered block for the given addresses. Must be called in
    /// ascending block order.
    pub fn extend_accounts(
        &mut self,
        changes: &BlockAggregatedChanges,
        addresses: impl IntoIterator<Item = Address>,
    ) {
        let block = changes.block.number;
        for address in addresses {
            if let Some(delta) = changes.account_deltas.get(&address) {
                self.accounts
                    .entry(address.clone())
                    .or_default()
                    .deltas
                    .push((block, delta.clone()));
            }
            if let Some(balances) = changes.account_balances.get(&address) {
                self.accounts
                    .entry(address)
                    .or_default()
                    .token_balances
                    .push((block, balances.clone()));
            }
        }
    }

    /// Collects the deltas of one buffered block for the given component ids. Must be called in
    /// ascending block order.
    pub fn extend_components<'a>(
        &mut self,
        changes: &BlockAggregatedChanges,
        ids: impl IntoIterator<Item = &'a str>,
    ) {
        let block = changes.block.number;
        for id in ids {
            if let Some(delta) = changes.state_deltas.get(id) {
                self.components
                    .entry(id.to_string())
                    .or_default()
                    .deltas
                    .push((block, delta.clone()));
            }
            if let Some(balances) = changes.component_balances.get(id) {
                self.components
                    .entry(id.to_string())
                    .or_default()
                    .balances
                    .push((block, balances.clone()));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.components.is_empty()
    }
}

/// Split of a cache lookup: entities served from memory and the keys that still need a db read.
#[derive(Debug, Default)]
pub struct AccountLookup {
    pub hits: Vec<Account>,
    pub misses: Vec<Address>,
}

#[derive(Debug, Default)]
pub struct ComponentStateLookup {
    pub hits: Vec<ProtocolComponentState>,
    pub misses: Vec<ComponentId>,
}

/// Bookkeeping for an in-flight db fill of one key.
///
/// Two jobs: collapse a stampede of concurrent misses for the same key into a single db read, and
/// hold the deltas that are committed while the read is in flight so they can be replayed onto the
/// fetched state before it is published (see [`EntityCache::complete_account_fill`]).
#[derive(Debug, Default)]
pub struct PendingFill {
    queued: Mutex<Vec<(BlockNumber, QueuedApply)>>,
    published: AtomicBool,
    ready: Notify,
}

#[derive(Debug)]
enum QueuedApply {
    AccountDelta(AccountDelta),
    AccountBalances(HashMap<Address, AccountBalance>),
    ComponentDelta(ProtocolComponentStateDelta),
    ComponentBalances(HashMap<Address, ComponentBalance>),
}

impl PendingFill {
    /// Waits until the leading task published its fill (or gave up). Callers retry the cache
    /// afterwards; a second miss then leads its own fill.
    pub async fn wait(&self) {
        // Register before re-checking the flag, otherwise a notify between check and await is lost.
        let notified = self.ready.notified();
        if self.published.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

impl Drop for PendingFill {
    fn drop(&mut self) {
        // Waiters must never be stranded, not even if the leading task returned early on a db
        // error and dropped its guard.
        self.published
            .store(true, Ordering::Release);
        self.ready.notify_waiters();
    }
}

/// Handle held by the task that performs the db read for a key.
pub struct FillGuard {
    cache: Arc<Inner>,
    key: FillKey,
    slot: Arc<PendingFill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FillKey {
    Account(Address),
    Component(ComponentKey),
}

impl Drop for FillGuard {
    fn drop(&mut self) {
        self.cache.remove_fill(&self.key);
        self.slot
            .published
            .store(true, Ordering::Release);
        self.slot.ready.notify_waiters();
    }
}

/// Outcome of joining the single-flight fill for one key.
pub enum FillOutcome {
    /// The caller performs the db read and publishes it.
    Lead(FillGuard),
    /// Another task is already reading this key; await [`PendingFill::wait`], then retry.
    Follow(Arc<PendingFill>),
}

/// Optional guard rails. The natural bound is the number of indexed accounts (~15k on ethereum,
/// ~1.2GB of raw state), so eviction is a safety net rather than a working mechanism.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheLimits {
    /// Hard cap on cached accounts. `None` disables the cap.
    pub max_accounts: Option<usize>,
    /// Hard cap on cached component states. `None` disables the cap.
    pub max_components: Option<usize>,
}

/// Delta-fed entity cache. Cheap to clone; all clones share one set of entries.
#[derive(Clone)]
pub struct EntityCache {
    inner: Arc<Inner>,
}

struct Inner {
    accounts: Sharded<Address, CachedAccount>,
    components: Sharded<ComponentKey, CachedComponentState>,
    fills: Mutex<HashMap<FillKey, Arc<PendingFill>>>,
    /// Last committed block applied per extractor. Observability and a readiness signal — the
    /// serving path does not consult it (see the composition note on `get_accounts`).
    applied_heights: Mutex<HashMap<String, BlockNumber>>,
    /// Incremented on every applied block. Lets a reader detect that a commit landed while it was
    /// assembling a response.
    applied_seq: AtomicU64,
    limits: CacheLimits,
}

impl EntityCache {
    pub fn new(limits: CacheLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                accounts: Sharded::new(),
                components: Sharded::new(),
                fills: Mutex::new(HashMap::new()),
                applied_heights: Mutex::new(HashMap::new()),
                applied_seq: AtomicU64::new(0),
                limits,
            }),
        }
    }

    /// Serves accounts from the cache, with `patch` (the pending deltas captured *before* this
    /// call, see I4) applied on top.
    ///
    /// The result is meant to replace the db read only for requests whose version resolved to
    /// `BlockIdentifier::Latest`, i.e. the `CommitStatus::Uncommitted` / timestamp-`Unseen` paths
    /// in `calculate_versions`. A request pinned to an already committed historical block still
    /// has to go to Postgres — the cache holds head state only.
    pub fn get_accounts(&self, addresses: &[Address], patch: &PendingPatch) -> AccountLookup {
        let mut lookup = AccountLookup::default();
        for address in addresses {
            match self
                .inner
                .accounts
                .read(address, |cached| cached.materialize(patch.accounts.get(address)))
            {
                Some(account) => lookup.hits.push(account),
                None => lookup.misses.push(address.clone()),
            }
        }
        counter!("entity_cache_hits", "entity" => "account").increment(lookup.hits.len() as u64);
        counter!("entity_cache_misses", "entity" => "account")
            .increment(lookup.misses.len() as u64);
        lookup
    }

    /// Component-state counterpart of [`Self::get_accounts`]. Keyed by protocol system as well as
    /// component id, because `get_protocol_states` filters by system.
    pub fn get_component_states(
        &self,
        protocol_system: &str,
        ids: &[&str],
        patch: &PendingPatch,
    ) -> ComponentStateLookup {
        let mut lookup = ComponentStateLookup::default();
        for id in ids {
            let key = (protocol_system.to_string(), id.to_string());
            match self
                .inner
                .components
                .read(&key, |cached| cached.materialize(patch.components.get(*id)))
            {
                Some(state) => lookup.hits.push(state),
                None => lookup.misses.push(id.to_string()),
            }
        }
        counter!("entity_cache_hits", "entity" => "component").increment(lookup.hits.len() as u64);
        counter!("entity_cache_misses", "entity" => "component")
            .increment(lookup.misses.len() as u64);
        lookup
    }

    /// Joins (or leads) the single-flight db fill for one address.
    pub fn begin_account_fill(&self, address: &Address) -> FillOutcome {
        self.inner
            .begin_fill(FillKey::Account(address.clone()))
    }

    pub fn begin_component_fill(&self, key: ComponentKey) -> FillOutcome {
        self.inner
            .begin_fill(FillKey::Component(key))
    }

    /// Publishes a db fill pinned at `pinned_at` (I3).
    ///
    /// Deltas that were committed while the read was in flight are replayed onto the fetched state
    /// before it becomes visible, and only those newer than `pinned_at` take effect. Both happen
    /// while the fill slot is still registered, so no apply can slip in between replay and insert.
    pub fn complete_account_fill(
        &self,
        guard: &FillGuard,
        account: Account,
        pinned_at: BlockNumber,
    ) {
        let mut cached = CachedAccount::from_db(account, pinned_at);
        let queued = std::mem::take(
            &mut *guard
                .slot
                .queued
                .lock()
                .expect("entity cache fill queue poisoned"),
        );
        for (block, apply) in queued {
            match apply {
                QueuedApply::AccountDelta(delta) => cached.apply_delta(block, &delta),
                QueuedApply::AccountBalances(balances) => {
                    cached.apply_token_balances(block, &balances)
                }
                // Component applies cannot be queued under an account key.
                QueuedApply::ComponentDelta(_) | QueuedApply::ComponentBalances(_) => {
                    warn!(?guard.key, "Unexpected component apply queued for an account fill")
                }
            }
        }
        // TODO(entity-cache): honour `limits.max_accounts` here. Eviction is safe at any time (I5:
        // a delta for an absent key is dropped, and the next request refills from the db), so the
        // policy can be as simple as evicting the least recently read entry.
        self.inner
            .accounts
            .insert(cached.address.clone(), cached);
        // The guard's `Drop` deregisters the slot and wakes followers.
    }

    /// Component counterpart of [`Self::complete_account_fill`].
    pub fn complete_component_fill(
        &self,
        guard: &FillGuard,
        protocol_system: &str,
        state: ProtocolComponentState,
        pinned_at: BlockNumber,
    ) {
        let mut cached = CachedComponentState::from_db(state, pinned_at);
        let queued = std::mem::take(
            &mut *guard
                .slot
                .queued
                .lock()
                .expect("entity cache fill queue poisoned"),
        );
        for (block, apply) in queued {
            match apply {
                QueuedApply::ComponentDelta(delta) => cached.apply_delta(block, &delta),
                QueuedApply::ComponentBalances(balances) => cached.apply_balances(block, &balances),
                QueuedApply::AccountDelta(_) | QueuedApply::AccountBalances(_) => {
                    warn!(?guard.key, "Unexpected account apply queued for a component fill")
                }
            }
        }
        let key = (protocol_system.to_string(), cached.component_id.clone());
        self.inner
            .components
            .insert(key, cached);
    }

    /// Update entry point for the deltas path: applies blocks that an extractor has committed to
    /// the database (I2). Call it *before* the blocks leave the reorg buffer (I4).
    pub fn apply_committed(&self, blocks: &[BlockAggregatedChanges]) {
        for changes in blocks {
            self.inner.apply_block(changes);
        }
    }

    /// Highest committed block applied per extractor.
    pub fn applied_heights(&self) -> HashMap<String, BlockNumber> {
        self.inner
            .applied_heights
            .lock()
            .expect("entity cache heights poisoned")
            .clone()
    }

    /// Monotonic counter of applied blocks, for readers that need to detect a concurrent commit.
    pub fn applied_seq(&self) -> u64 {
        self.inner
            .applied_seq
            .load(Ordering::Acquire)
    }

    /// Reports size gauges. Intended to be called from the same 60s reporting task that already
    /// publishes `pending_deltas_buffer_size`.
    pub fn report_metrics(&self) {
        let (accounts, account_bytes) = self.inner.accounts.size();
        let (components, component_bytes) = self.inner.components.size();
        gauge!("entity_cache_entries", "entity" => "account").set(accounts as f64);
        gauge!("entity_cache_size_bytes", "entity" => "account").set(account_bytes as f64);
        gauge!("entity_cache_entries", "entity" => "component").set(components as f64);
        gauge!("entity_cache_size_bytes", "entity" => "component").set(component_bytes as f64);
    }
}

impl Inner {
    fn apply_block(&self, changes: &BlockAggregatedChanges) {
        let block = changes.block.number;

        for (address, delta) in &changes.account_deltas {
            let key = FillKey::Account(address.clone());
            if self.queue_if_filling(&key, block, || QueuedApply::AccountDelta(delta.clone())) {
                continue;
            }
            // I5: a delta never creates an entry — it is not a full account.
            self.accounts
                .update(address, |cached| cached.apply_delta(block, delta));
        }
        for (address, balances) in &changes.account_balances {
            let key = FillKey::Account(address.clone());
            if self.queue_if_filling(&key, block, || QueuedApply::AccountBalances(balances.clone()))
            {
                continue;
            }
            self.accounts
                .update(address, |cached| cached.apply_token_balances(block, balances));
        }
        for (id, delta) in &changes.state_deltas {
            let key = (changes.extractor.clone(), id.clone());
            if self.queue_if_filling(&FillKey::Component(key.clone()), block, || {
                QueuedApply::ComponentDelta(delta.clone())
            }) {
                continue;
            }
            self.components
                .update(&key, |cached| cached.apply_delta(block, delta));
        }
        for (id, balances) in &changes.component_balances {
            let key = (changes.extractor.clone(), id.clone());
            if self.queue_if_filling(&FillKey::Component(key.clone()), block, || {
                QueuedApply::ComponentBalances(balances.clone())
            }) {
                continue;
            }
            self.components
                .update(&key, |cached| cached.apply_balances(block, balances));
        }

        self.applied_heights
            .lock()
            .expect("entity cache heights poisoned")
            .insert(changes.extractor.clone(), block);
        self.applied_seq
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Parks an apply on an in-flight fill so the fill can replay it before publishing. Returns
    /// true when the apply was queued and must not be applied to the (absent) entry.
    fn queue_if_filling(
        &self,
        key: &FillKey,
        block: BlockNumber,
        apply: impl FnOnce() -> QueuedApply,
    ) -> bool {
        let slot = {
            let fills = self
                .fills
                .lock()
                .expect("entity cache fills poisoned");
            fills.get(key).cloned()
        };
        let Some(slot) = slot else { return false };
        slot.queued
            .lock()
            .expect("entity cache fill queue poisoned")
            .push((block, apply()));
        true
    }

    fn begin_fill(self: &Arc<Self>, key: FillKey) -> FillOutcome {
        let mut fills = self
            .fills
            .lock()
            .expect("entity cache fills poisoned");
        match fills.entry(key.clone()) {
            Entry::Occupied(e) => FillOutcome::Follow(e.get().clone()),
            Entry::Vacant(e) => {
                let slot = Arc::new(PendingFill::default());
                e.insert(slot.clone());
                FillOutcome::Lead(FillGuard { cache: self.clone(), key, slot })
            }
        }
    }

    fn remove_fill(&self, key: &FillKey) {
        self.fills
            .lock()
            .expect("entity cache fills poisoned")
            .remove(key);
    }
}

/// A hash map split into [`SHARD_COUNT`] independently locked maps.
///
/// `DashMap` would give the same structure; a hand-rolled shard array avoids a new dependency and
/// keeps the lock scope obvious: guards are only ever held for one synchronous operation, never
/// across an await.
struct Sharded<K, V> {
    shards: Vec<RwLock<HashMap<K, V>>>,
}

impl<K, V> Sharded<K, V>
where
    K: Hash + Eq + DeepSizeOf,
    V: DeepSizeOf,
{
    fn new() -> Self {
        Self {
            shards: (0..SHARD_COUNT)
                .map(|_| RwLock::new(HashMap::new()))
                .collect(),
        }
    }

    fn shard(&self, key: &K) -> &RwLock<HashMap<K, V>> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        &self.shards[(hasher.finish() as usize) % SHARD_COUNT]
    }

    fn read<T>(&self, key: &K, f: impl FnOnce(&V) -> T) -> Option<T> {
        let guard = self
            .shard(key)
            .read()
            .expect("entity cache shard poisoned");
        guard.get(key).map(f)
    }

    /// Mutates an existing entry. Absent keys are ignored (I5).
    fn update(&self, key: &K, f: impl FnOnce(&mut V)) -> bool {
        let mut guard = self
            .shard(key)
            .write()
            .expect("entity cache shard poisoned");
        match guard.get_mut(key) {
            Some(value) => {
                f(value);
                true
            }
            None => false,
        }
    }

    fn insert(&self, key: K, value: V) {
        self.shard(&key)
            .write()
            .expect("entity cache shard poisoned")
            .insert(key, value);
    }

    fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                s.read()
                    .expect("entity cache shard poisoned")
                    .len()
            })
            .sum()
    }

    /// Entry count and deep size in bytes.
    fn size(&self) -> (usize, usize) {
        let bytes = self
            .shards
            .iter()
            .map(|s| {
                s.read()
                    .expect("entity cache shard poisoned")
                    .deep_size_of()
            })
            .sum();
        (self.len(), bytes)
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_common::{
        models::{blockchain::Block, ChangeType},
        Bytes,
    };

    use super::*;
    use crate::extractor::models::fixtures;

    fn address() -> Address {
        Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap()
    }

    fn db_account() -> Account {
        Account::new(
            Chain::Ethereum,
            address(),
            "Contract1".to_string(),
            fixtures::slots([(1, 10), (2, 20)]),
            Bytes::from("0x01"),
            HashMap::new(),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0xbabe"),
            Bytes::from("0x4200"),
            Bytes::from("0x4200"),
            None,
        )
    }

    fn account_delta(slots: Vec<(u64, u64)>) -> AccountDelta {
        AccountDelta::new(
            Chain::Ethereum,
            address(),
            fixtures::optional_slots(slots),
            None,
            None,
            ChangeType::Update,
        )
    }

    fn block_changes(extractor: &str, number: u64, delta: AccountDelta) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            extractor: extractor.to_string(),
            chain: Chain::Ethereum,
            block: Block { number, ..Default::default() },
            finalized_block_height: number,
            db_committed_block_height: Some(number),
            account_deltas: HashMap::from([(address(), delta)]),
            ..Default::default()
        }
    }

    fn slot(key: u64) -> StoreKey {
        Bytes::from(key).lpad(32, 0)
    }

    fn slot_value(cached: &CachedAccount, key: u64) -> Option<StoreVal> {
        cached
            .slots
            .get(&slot(key))
            .map(|v| v.value.clone())
    }

    #[test]
    fn test_committed_delta_updates_entry() {
        let cache = EntityCache::new(CacheLimits::default());
        cache
            .inner
            .accounts
            .insert(address(), CachedAccount::from_db(db_account(), 100));

        cache.apply_committed(&[block_changes("vm:a", 101, account_delta(vec![(1, 11)]))]);

        let hit = cache
            .get_accounts(&[address()], &PendingPatch::default())
            .hits
            .remove(0);
        assert_eq!(hit.slots.get(&slot(1)), Some(&Bytes::from(11u64).lpad(32, 0)));
        assert_eq!(hit.slots.get(&slot(2)), Some(&Bytes::from(20u64).lpad(32, 0)));
        assert_eq!(cache.applied_heights().get("vm:a"), Some(&101));
    }

    /// I5: extractors report accounts this pod has never filled from the db (and a `Creation`
    /// delta is not a full account), so an unknown address must not become a cache entry.
    #[test]
    fn test_delta_for_unknown_address_is_dropped() {
        let cache = EntityCache::new(CacheLimits::default());

        cache.apply_committed(&[block_changes("vm:a", 101, account_delta(vec![(1, 11)]))]);

        let lookup = cache.get_accounts(&[address()], &PendingPatch::default());
        assert!(lookup.hits.is_empty());
        assert_eq!(lookup.misses, vec![address()]);
    }

    /// I1: extractor B commits block 99 after extractor A already committed block 101 for the same
    /// slot. Postgres would keep A's value (higher `valid_from`); the cache must too.
    #[test]
    fn test_older_extractor_commit_does_not_overwrite_newer_slot() {
        let cache = EntityCache::new(CacheLimits::default());
        cache
            .inner
            .accounts
            .insert(address(), CachedAccount::from_db(db_account(), 98));

        cache.apply_committed(&[block_changes("vm:a", 101, account_delta(vec![(1, 11)]))]);
        cache.apply_committed(&[block_changes("vm:b", 99, account_delta(vec![(1, 99), (3, 33)]))]);

        cache
            .inner
            .accounts
            .read(&address(), |cached| {
                // slot 1 keeps block 101's value, slot 3 is new and accepted from block 99
                assert_eq!(slot_value(cached, 1), Some(Bytes::from(11u64).lpad(32, 0)));
                assert_eq!(slot_value(cached, 3), Some(Bytes::from(33u64).lpad(32, 0)));
            })
            .expect("entry missing");
    }

    /// I4: the same block can appear in the captured patch and in the cache (it was drained between
    /// the two reads). Applying it twice must be a no-op.
    #[test]
    fn test_patch_over_cache_is_idempotent() {
        let cache = EntityCache::new(CacheLimits::default());
        cache
            .inner
            .accounts
            .insert(address(), CachedAccount::from_db(db_account(), 100));
        let changes = block_changes("vm:a", 101, account_delta(vec![(1, 11)]));

        let mut patch = PendingPatch::default();
        patch.extend_accounts(&changes, [address()]);
        cache.apply_committed(&[changes]);

        let with_patch = cache
            .get_accounts(&[address()], &patch)
            .hits
            .remove(0);
        let without_patch = cache
            .get_accounts(&[address()], &PendingPatch::default())
            .hits
            .remove(0);
        assert_eq!(with_patch, without_patch);
    }

    /// I1 on the read path: a pending patch block older than a cached value must not win. This
    /// happens when the requested extractor lags behind another one that already committed.
    #[test]
    fn test_patch_older_than_cached_value_is_ignored() {
        let cache = EntityCache::new(CacheLimits::default());
        cache
            .inner
            .accounts
            .insert(address(), CachedAccount::from_db(db_account(), 100));
        cache.apply_committed(&[block_changes("vm:b", 105, account_delta(vec![(1, 55)]))]);

        let mut patch = PendingPatch::default();
        patch.extend_accounts(
            &block_changes("vm:a", 102, account_delta(vec![(1, 22)])),
            [address()],
        );

        let hit = cache
            .get_accounts(&[address()], &patch)
            .hits
            .remove(0);
        assert_eq!(hit.slots.get(&slot(1)), Some(&Bytes::from(55u64).lpad(32, 0)));
    }

    /// I3: a commit that lands while a fill is in flight is queued and replayed, so the fill cannot
    /// publish state that is missing an already committed block.
    #[test]
    fn test_commit_during_fill_is_replayed() {
        let cache = EntityCache::new(CacheLimits::default());
        let FillOutcome::Lead(guard) = cache.begin_account_fill(&address()) else {
            panic!("expected to lead the fill");
        };

        // Commit arrives while the db read is in flight.
        cache.apply_committed(&[block_changes("vm:a", 101, account_delta(vec![(1, 11)]))]);
        assert!(cache
            .get_accounts(&[address()], &PendingPatch::default())
            .hits
            .is_empty());

        cache.complete_account_fill(&guard, db_account(), 100);

        let hit = cache
            .get_accounts(&[address()], &PendingPatch::default())
            .hits
            .remove(0);
        assert_eq!(hit.slots.get(&slot(1)), Some(&Bytes::from(11u64).lpad(32, 0)));
    }

    /// The mirror case: a queued delta older than the pinned fill version must be discarded.
    #[test]
    fn test_commit_older_than_fill_version_is_discarded() {
        let cache = EntityCache::new(CacheLimits::default());
        let FillOutcome::Lead(guard) = cache.begin_account_fill(&address()) else {
            panic!("expected to lead the fill");
        };

        cache.apply_committed(&[block_changes("vm:b", 99, account_delta(vec![(1, 99)]))]);
        cache.complete_account_fill(&guard, db_account(), 100);

        let hit = cache
            .get_accounts(&[address()], &PendingPatch::default())
            .hits
            .remove(0);
        assert_eq!(hit.slots.get(&slot(1)), Some(&Bytes::from(10u64).lpad(32, 0)));
    }

    /// Dropping the guard must deregister the fill, otherwise a db error would leave every later
    /// miss for that address waiting forever.
    #[test]
    fn test_dropped_fill_guard_releases_slot() {
        let cache = EntityCache::new(CacheLimits::default());
        let FillOutcome::Lead(guard) = cache.begin_account_fill(&address()) else {
            panic!("expected to lead the fill");
        };
        assert!(matches!(cache.begin_account_fill(&address()), FillOutcome::Follow(_)));

        drop(guard);

        assert!(matches!(cache.begin_account_fill(&address()), FillOutcome::Lead(_)));
    }

    #[test]
    fn test_deleted_attribute_is_not_resurrected_by_older_update() {
        let cache = EntityCache::new(CacheLimits::default());
        let key = ("native:a".to_string(), "component1".to_string());
        let state = ProtocolComponentState::new(
            "component1",
            HashMap::from([("attr1".to_string(), Bytes::from("0x01"))]),
            HashMap::new(),
        );
        cache
            .inner
            .components
            .insert(key.clone(), CachedComponentState::from_db(state, 100));

        let deletion = ProtocolComponentStateDelta {
            component_id: "component1".to_string(),
            deleted_attributes: ["attr1".to_string()]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let older_update = ProtocolComponentStateDelta::new(
            "component1",
            HashMap::from([("attr1".to_string(), Bytes::from("0x02"))]),
            Default::default(),
        );
        let mut deletion_block = block_changes("native:a", 105, account_delta(vec![]));
        deletion_block.account_deltas.clear();
        deletion_block.state_deltas = HashMap::from([("component1".to_string(), deletion)]);
        let mut older_block = block_changes("native:a", 103, account_delta(vec![]));
        older_block.account_deltas.clear();
        older_block.state_deltas = HashMap::from([("component1".to_string(), older_update)]);

        cache.apply_committed(&[deletion_block, older_block]);

        let hit = cache
            .get_component_states("native:a", &["component1"], &PendingPatch::default())
            .hits
            .remove(0);
        assert!(hit.attributes.is_empty(), "deleted attribute was resurrected: {hit:?}");
    }

    // TODO(entity-cache): needs the RPC wiring. Asserts that a snapshot request served from the
    // cache equals the db+buffer response for the same version, including the case where a commit
    // lands between capturing the patch and reading the cache.
    #[test]
    #[ignore = "scaffold: requires the RPC handler wiring"]
    fn test_cache_response_matches_db_and_buffer_response() {
        unimplemented!("compare get_contract_state_inner with and without the cache")
    }
}
