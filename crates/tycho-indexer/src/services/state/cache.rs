//! Long-lived in-memory entity state.
//!
//! Two entity families are cached:
//!
//! - **Accounts** (contract state), keyed by address. Several extractors can write the same
//!   account, so every cached value carries the time it was last written — the writing block's
//!   timestamp, the same unit the database's `valid_from` versioning uses. A newer write always
//!   wins; re-applying an equal-time change is a no-op because delta values are absolute.
//! - **Component states** (protocol state), keyed by protocol system, then component id. Exactly
//!   one extractor writes each protocol system, in order, so one write time per entry is enough.
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
    collections::{hash_map::Entry, HashMap},
    hash::Hash,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use chrono::NaiveDateTime;
use deepsize::{Context, DeepSizeOf};
use tycho_common::{
    keccak256,
    models::{
        blockchain::BlockAggregatedChanges,
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponentState, ProtocolComponentStateDelta},
        Address, AttrStoreKey, Balance, Chain, Code, CodeHash, ComponentId, StoreKey, StoreVal,
        TxHash,
    },
    storage::StorageError,
    Bytes,
};

use super::window::FoldSink;

/// A cached value together with the time it was last written (the writing block's timestamp).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tagged<T>(pub(crate) T, pub(crate) NaiveDateTime);

// `NaiveDateTime` has no `DeepSizeOf` impl; it is inline, so only the value has children.
impl<T: DeepSizeOf> DeepSizeOf for Tagged<T> {
    fn deep_size_of_children(&self, context: &mut Context) -> usize {
        self.0.deep_size_of_children(context)
    }
}

impl<T> Tagged<T> {
    /// Writes `value` at `at` unless this slot holds a newer value. Equal times apply: delta
    /// values are absolute, so re-applying a block is harmless, and consecutive blocks can share
    /// a timestamp on fast chains.
    pub(crate) fn write(&mut self, value: T, at: NaiveDateTime) {
        if at < self.1 {
            return;
        }
        *self = Tagged(value, at);
    }
}

/// [`Tagged::write`] for a map entry; a missing key is inserted.
fn write<K: Eq + Hash, V>(map: &mut HashMap<K, Tagged<V>>, key: K, value: V, at: NaiveDateTime) {
    match map.entry(key) {
        Entry::Occupied(mut e) => e.get_mut().write(value, at),
        Entry::Vacant(e) => {
            e.insert(Tagged(value, at));
        }
    }
}

/// Write times of one loaded account's values, each its row's `valid_from`.
#[derive(Debug, Clone)]
pub(crate) struct AccountTags {
    pub slots: HashMap<StoreKey, NaiveDateTime>,
    pub native_balance: NaiveDateTime,
    pub code: NaiveDateTime,
    pub token_balances: HashMap<Address, NaiveDateTime>,
}

/// Cached state of one contract account.
///
/// Every value carries the time it was last written, so writes from different extractors (which
/// run at different points of the chain) can never regress a value: newer wins, equal-time
/// re-application is a no-op.
#[derive(Debug, Clone, PartialEq, DeepSizeOf)]
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
    /// Builds an entry from the startup snapshot. Every slot and token balance of `account` must
    /// have a tag in `tags`; a missing tag is a loader bug and panics.
    pub(crate) fn from_snapshot(account: Account, tags: AccountTags) -> Self {
        let slots = account
            .slots
            .into_iter()
            .map(|(key, value)| {
                let at = tags.slots[&key];
                (key, Tagged(value, at))
            })
            .collect();
        let token_balances = account
            .token_balances
            .into_iter()
            .map(|(token, balance)| {
                let at = tags.token_balances[&token];
                (token, Tagged(balance, at))
            })
            .collect();
        Self {
            chain: account.chain,
            title: account.title,
            slots,
            native_balance: Tagged(account.native_balance, tags.native_balance),
            token_balances,
            code: Tagged(account.code, tags.code),
            code_hash: account.code_hash,
            balance_modify_tx: account.balance_modify_tx,
            code_modify_tx: account.code_modify_tx,
            creation_tx: account.creation_tx,
        }
    }

    /// Builds an entry from a `Creation` delta folded at `at` — after startup, the only way a new
    /// contract enters the cache. Fields the delta does not carry take the values
    /// [`AccountDelta::into_account_without_tx`] uses, so both paths build the same account.
    pub(crate) fn from_creation(delta: &AccountDelta, at: NaiveDateTime) -> Self {
        let code = delta.code().clone().unwrap_or_default();
        Self {
            chain: delta.chain,
            title: format!("{:#020x}", delta.address),
            slots: delta
                .slots
                .iter()
                .map(|(key, value)| (key.clone(), Tagged(value.clone().unwrap_or_default(), at)))
                .collect(),
            native_balance: Tagged(
                delta
                    .balance
                    .clone()
                    .unwrap_or_default(),
                at,
            ),
            token_balances: HashMap::new(),
            code_hash: keccak256(&code).into(),
            code: Tagged(code, at),
            balance_modify_tx: Bytes::from("0x00"),
            code_modify_tx: Bytes::from("0x00"),
            creation_tx: None,
        }
    }

    /// Applies one folded delta; every changed value gets `at` as its tag, values with a newer tag
    /// stay. A deleted slot becomes the zero value, as in [`Account::apply_delta`]. A delta that
    /// carries code also refreshes `code_hash`.
    pub(crate) fn fold(&mut self, delta: &AccountDelta, at: NaiveDateTime) {
        for (key, value) in &delta.slots {
            write(&mut self.slots, key.clone(), value.clone().unwrap_or_default(), at);
        }
        if let Some(balance) = &delta.balance {
            self.native_balance
                .write(balance.clone(), at);
        }
        if let Some(code) = delta.code() {
            if at >= self.code.1 {
                self.code_hash = keccak256(code).into();
            }
            self.code.write(code.clone(), at);
        }
    }

    /// Applies folded token balances under the same tag rule as [`CachedAccount::fold`].
    pub(crate) fn fold_balances(
        &mut self,
        balances: &HashMap<Address, AccountBalance>,
        at: NaiveDateTime,
    ) {
        for (token, balance) in balances {
            write(&mut self.token_balances, token.clone(), balance.clone(), at);
        }
    }

    /// Materializes the cached state as an [`Account`] for response assembly.
    pub(crate) fn materialize(&self, address: &Address) -> Account {
        Account::new(
            self.chain,
            address.clone(),
            self.title.clone(),
            self.slots
                .iter()
                .map(|(k, Tagged(v, _))| (k.clone(), v.clone()))
                .collect(),
            self.native_balance.0.clone(),
            self.token_balances
                .iter()
                .map(|(k, Tagged(v, _))| (k.clone(), v.clone()))
                .collect(),
            self.code.0.clone(),
            self.code_hash.clone(),
            self.balance_modify_tx.clone(),
            self.code_modify_tx.clone(),
            self.creation_tx.clone(),
        )
    }

    /// Tagged storage slots, for readers that apply only window changes newer than a value.
    pub(crate) fn slots(&self) -> &HashMap<StoreKey, Tagged<StoreVal>> {
        &self.slots
    }

    pub(crate) fn native_balance(&self) -> &Tagged<Balance> {
        &self.native_balance
    }

    pub(crate) fn token_balances(&self) -> &HashMap<Address, Tagged<AccountBalance>> {
        &self.token_balances
    }

    pub(crate) fn code(&self) -> &Tagged<Code> {
        &self.code
    }
}

/// Cached state of one protocol component.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CachedComponentState {
    attributes: HashMap<AttrStoreKey, StoreVal>,
    balances: HashMap<Address, Balance>,
    /// One write time covers the whole entry: a single extractor writes each protocol system,
    /// in order.
    updated_at: NaiveDateTime,
}

// `NaiveDateTime` has no `DeepSizeOf` impl; it is inline.
impl DeepSizeOf for CachedComponentState {
    fn deep_size_of_children(&self, context: &mut Context) -> usize {
        self.attributes
            .deep_size_of_children(context) +
            self.balances
                .deep_size_of_children(context)
    }
}

impl CachedComponentState {
    /// Builds an entry from the startup snapshot, tagged with its newest `valid_from`.
    pub(crate) fn from_snapshot(state: ProtocolComponentState, at: NaiveDateTime) -> Self {
        Self { attributes: state.attributes, balances: state.balances, updated_at: at }
    }

    /// An entry for a component created at `at`, before its first attributes arrive.
    pub(crate) fn created(at: NaiveDateTime) -> Self {
        Self { attributes: HashMap::new(), balances: HashMap::new(), updated_at: at }
    }

    /// Applies one folded state delta unless the entry is newer than `at`. Updates apply first,
    /// then deletions, like [`ProtocolComponentState::apply_state_delta`].
    pub(crate) fn fold(&mut self, delta: &ProtocolComponentStateDelta, at: NaiveDateTime) {
        if !self.accepts(at) {
            return;
        }
        self.attributes.extend(
            delta
                .updated_attributes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        self.attributes
            .retain(|key, _| !delta.deleted_attributes.contains(key));
    }

    /// Applies folded balances unless the entry is newer than `at`.
    pub(crate) fn fold_balances(
        &mut self,
        balances: &HashMap<Bytes, ComponentBalance>,
        at: NaiveDateTime,
    ) {
        if !self.accepts(at) {
            return;
        }
        self.balances.extend(
            balances
                .iter()
                .map(|(token, balance)| (token.clone(), balance.balance.clone())),
        );
    }

    /// The single writer folds in order, so an older block is a replay: skip it. Otherwise the
    /// entry moves to `at`.
    fn accepts(&mut self, at: NaiveDateTime) -> bool {
        if at < self.updated_at {
            return false;
        }
        self.updated_at = at;
        true
    }

    /// Materializes the cached state for response assembly.
    pub(crate) fn materialize(&self, component_id: &str) -> ProtocolComponentState {
        ProtocolComponentState::new(component_id, self.attributes.clone(), self.balances.clone())
    }

    /// Time of the last write, for readers that apply only newer window changes.
    pub(crate) fn updated_at(&self) -> NaiveDateTime {
        self.updated_at
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
    /// Component states by protocol system, then component id. The system is the extractor name:
    /// the RPC resolves one window per protocol system by extractor name, so both are one string.
    pub(crate) components: HashMap<String, HashMap<ComponentId, CachedComponentState>>,
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

    fn write(&self) -> RwLockWriteGuard<'_, CacheState> {
        self.state
            .write()
            .expect("entity cache lock poisoned")
    }

    /// Startup load only: runs before the extractors start, so nothing else is writing.
    pub(crate) fn insert_loaded_account(&self, address: Address, entry: CachedAccount) {
        self.write()
            .accounts
            .insert(address, entry);
    }

    /// Startup load only.
    pub(crate) fn insert_loaded_component(
        &self,
        system: String,
        component_id: ComponentId,
        entry: CachedComponentState,
    ) {
        self.write()
            .components
            .entry(system)
            .or_default()
            .insert(component_id, entry);
    }
}

impl FoldSink for EntityCache {
    #[allow(unused_variables)]
    fn fold(&self, block: &BlockAggregatedChanges) -> Result<(), StorageError> {
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

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_common::models::ChangeType;

    use super::*;
    use crate::{extractor::models::fixtures, testing};

    const EXTRACTOR: &str = "ex";

    fn ts(n: u64) -> NaiveDateTime {
        testing::block(n).ts
    }

    fn cached_account(cache: &EntityCache, address: &Bytes) -> Option<Account> {
        cache
            .read()
            .accounts
            .get(address)
            .map(|a| a.materialize(address))
    }

    fn cached_component(cache: &EntityCache, id: &str) -> Option<ProtocolComponentState> {
        cache
            .read()
            .components
            .get(EXTRACTOR)
            .and_then(|m| m.get(id))
            .map(|c| c.materialize(id))
    }

    fn addr(n: u64) -> Bytes {
        Bytes::from(n).lpad(20, 0)
    }

    fn code(hex: &str) -> Bytes {
        Bytes::from_str(hex).unwrap()
    }

    fn account_balance(account: &Bytes, token: &Bytes, amount: u64) -> AccountBalance {
        AccountBalance::new(account.clone(), token.clone(), Bytes::from(amount), Bytes::default())
    }

    fn account(address: &Bytes) -> Account {
        let bytecode = code("0x6000");
        Account::new(
            Chain::Ethereum,
            address.clone(),
            "acc".to_string(),
            fixtures::slots([(1, 1), (2, 2)]),
            Bytes::from(10u64),
            HashMap::from([(addr(9), account_balance(address, &addr(9), 3))]),
            bytecode.clone(),
            keccak256(&bytecode).into(),
            Bytes::from("0x01"),
            Bytes::from("0x02"),
            Some(Bytes::from("0x03")),
        )
    }

    fn tags(account: &Account, at: NaiveDateTime) -> AccountTags {
        AccountTags {
            slots: account
                .slots
                .keys()
                .map(|k| (k.clone(), at))
                .collect(),
            native_balance: at,
            code: at,
            token_balances: account
                .token_balances
                .keys()
                .map(|k| (k.clone(), at))
                .collect(),
        }
    }

    fn creation(
        address: &Bytes,
        slots: impl IntoIterator<Item = (u64, u64)>,
        balance: u64,
        bytecode: &str,
    ) -> AccountDelta {
        AccountDelta::new(
            Chain::Ethereum,
            address.clone(),
            fixtures::optional_slots(slots),
            Some(Bytes::from(balance)),
            Some(code(bytecode)),
            ChangeType::Creation,
        )
    }

    fn update(address: &Bytes, slots: HashMap<Bytes, Option<Bytes>>) -> AccountDelta {
        AccountDelta::new(Chain::Ethereum, address.clone(), slots, None, None, ChangeType::Update)
    }

    fn component_balance(id: &str, token: &Bytes, amount: u64) -> ComponentBalance {
        ComponentBalance {
            token: token.clone(),
            balance: Bytes::from(amount),
            balance_float: amount as f64,
            modify_tx: Bytes::default(),
            component_id: id.to_string(),
        }
    }

    #[test]
    fn write_keeps_a_newer_value() {
        let mut slot = Tagged(1u64, ts(5));

        slot.write(2, ts(4));

        assert_eq!(slot, Tagged(1, ts(5)));
    }

    #[test]
    fn write_applies_an_equal_time_value() {
        let mut slot = Tagged(1u64, ts(5));

        slot.write(2, ts(5));

        assert_eq!(slot, Tagged(2, ts(5)));
    }

    #[test]
    fn write_inserts_a_missing_key_and_updates_a_present_one() {
        let mut map: HashMap<&str, Tagged<u64>> = HashMap::new();

        write(&mut map, "a", 1, ts(3));
        write(&mut map, "a", 2, ts(2));
        write(&mut map, "b", 9, ts(1));

        assert_eq!(map["a"], Tagged(1, ts(3)));
        assert_eq!(map["b"], Tagged(9, ts(1)));
    }

    #[test]
    fn account_snapshot_round_trips() {
        let address = addr(1);
        let loaded = account(&address);

        let cached = CachedAccount::from_snapshot(loaded.clone(), tags(&loaded, ts(1)));

        assert_eq!(cached.materialize(&address), loaded);
        let key1 = fixtures::slots([(1, 1)])
            .into_keys()
            .next()
            .unwrap();
        assert_eq!(cached.slots()[&key1].1, ts(1));
    }

    #[test]
    fn creation_builds_the_account_the_delta_path_builds() {
        let address = addr(1);
        let delta = creation(&address, [(1, 1)], 10, "0x6000");

        let cached = CachedAccount::from_creation(&delta, ts(1));

        assert_eq!(cached.materialize(&address), delta.into_account_without_tx());
        assert_eq!(cached.code().1, ts(1));
    }

    #[test]
    fn account_fold_keeps_newer_values_per_slot() {
        let address = addr(1);
        let mut cached =
            CachedAccount::from_snapshot(account(&address), tags(&account(&address), ts(5)));

        cached.fold(&update(&address, fixtures::optional_slots([(1, 11), (3, 3)])), ts(3));

        let slots = cached.materialize(&address).slots;
        assert_eq!(
            slots,
            fixtures::slots([(1, 1), (2, 2), (3, 3)]),
            "older slot 1 kept, new slot 3 added"
        );
        let key3 = fixtures::slots([(3, 3)])
            .into_keys()
            .next()
            .unwrap();
        assert_eq!(cached.slots()[&key3].1, ts(3));
    }

    #[test]
    fn account_fold_applies_an_equal_time_write() {
        let address = addr(1);
        let mut cached =
            CachedAccount::from_snapshot(account(&address), tags(&account(&address), ts(5)));

        cached.fold(&update(&address, fixtures::optional_slots([(1, 11)])), ts(5));

        assert_eq!(cached.materialize(&address).slots, fixtures::slots([(1, 11), (2, 2)]));
    }

    #[test]
    fn account_fold_twice_changes_nothing() {
        let address = addr(1);
        let mut cached =
            CachedAccount::from_creation(&creation(&address, [(1, 1)], 10, "0x6000"), ts(1));
        let delta = update(&address, fixtures::optional_slots([(1, 11), (2, 2)]));

        cached.fold(&delta, ts(2));
        let once = cached.clone();
        cached.fold(&delta, ts(2));

        assert_eq!(cached, once);
    }

    #[test]
    fn account_fold_zeroes_deleted_slots_and_refreshes_the_code_hash() {
        let address = addr(1);
        let mut cached =
            CachedAccount::from_creation(&creation(&address, [(1, 1)], 10, "0x6000"), ts(1));
        let key1 = fixtures::slots([(1, 1)])
            .into_keys()
            .next()
            .unwrap();
        let mut delta = update(&address, HashMap::from([(key1.clone(), None)]));
        delta.set_code(code("0x6001"));

        cached.fold(&delta, ts(2));

        let account = cached.materialize(&address);
        assert_eq!(account.slots[&key1], Bytes::default());
        assert_eq!(account.code, code("0x6001"));
        assert_eq!(account.code_hash, Bytes::from(keccak256(code("0x6001"))));
        assert_eq!(cached.code().1, ts(2));
    }

    #[test]
    fn account_fold_balances_follow_the_tag_rule() {
        let address = addr(1);
        let mut cached = CachedAccount::from_creation(&creation(&address, [], 0, "0x"), ts(5));

        cached.fold_balances(
            &HashMap::from([(addr(9), account_balance(&address, &addr(9), 7))]),
            ts(5),
        );
        cached.fold_balances(
            &HashMap::from([(addr(9), account_balance(&address, &addr(9), 1))]),
            ts(4),
        );

        assert_eq!(
            cached
                .materialize(&address)
                .token_balances[&addr(9)]
                .balance,
            Bytes::from(7u64)
        );
    }

    #[test]
    fn component_snapshot_round_trips() {
        let loaded = ProtocolComponentState::new(
            "c1",
            HashMap::from([("x".to_string(), Bytes::from(1u64))]),
            HashMap::from([(addr(9), Bytes::from(5u64))]),
        );

        let cached = CachedComponentState::from_snapshot(loaded.clone(), ts(3));

        assert_eq!(cached.materialize("c1"), loaded);
        assert_eq!(cached.updated_at(), ts(3));
    }

    #[test]
    fn component_fold_skips_an_older_block_and_removes_deleted_attributes() {
        let mut cached = CachedComponentState::from_snapshot(
            ProtocolComponentState::new(
                "c1",
                HashMap::from([("x".to_string(), Bytes::from(1u64))]),
                HashMap::new(),
            ),
            ts(5),
        );

        cached.fold(&testing::state_delta("c1", 9), ts(4));
        assert_eq!(
            cached.materialize("c1").attributes["x"],
            Bytes::from(1u64),
            "older block skipped"
        );

        let mut delta = testing::state_delta("c1", 2);
        delta
            .updated_attributes
            .insert("y".to_string(), Bytes::from(3u64));
        delta
            .deleted_attributes
            .insert("x".to_string());
        cached.fold(&delta, ts(5));
        cached.fold_balances(
            &HashMap::from([(addr(9), component_balance("c1", &addr(9), 5))]),
            ts(6),
        );

        let state = cached.materialize("c1");
        assert_eq!(state.attributes, HashMap::from([("y".to_string(), Bytes::from(3u64))]));
        assert_eq!(state.balances, HashMap::from([(addr(9), Bytes::from(5u64))]));
        assert_eq!(cached.updated_at(), ts(6));
    }

    #[test]
    fn loaded_entries_read_back_by_address_and_by_system_and_id() {
        let cache = EntityCache::new();
        let address = addr(1);
        let loaded = account(&address);
        let state = ProtocolComponentState::new("c1", HashMap::new(), HashMap::new());

        cache.insert_loaded_account(
            address.clone(),
            CachedAccount::from_snapshot(loaded.clone(), tags(&loaded, ts(1))),
        );
        cache.insert_loaded_component(
            EXTRACTOR.to_string(),
            "c1".to_string(),
            CachedComponentState::from_snapshot(state.clone(), ts(1)),
        );

        assert_eq!(cached_account(&cache, &address), Some(loaded));
        assert_eq!(cached_component(&cache, "c1"), Some(state));
        assert!(!cache
            .read()
            .components
            .contains_key("other"));
    }
}
