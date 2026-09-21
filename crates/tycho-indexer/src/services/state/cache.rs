//! Long-lived in-memory entity state.
//!
//! Two entity families are cached:
//!
//! - **Accounts** (contract state), keyed by address. Several extractors can write the same
//!   account, so every cached value carries a [`WriteTag`]: the writing block's timestamp and
//!   number. A newer write always wins; re-applying an equal-tag change is a no-op because delta
//!   values are absolute.
//! - **Component states** (protocol state), keyed by protocol system, then component id. Exactly
//!   one extractor writes each protocol system, in order, so one tag per entry is enough.
//!
//! Tags compare with "not older" (>=), never "strictly newer", so a replay of a block converges.
//! Removals follow the same rule: a deletion older than the entry's newest write is skipped.
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
use tracing::trace;
use tycho_common::{
    keccak256,
    models::{
        blockchain::{Block, BlockAggregatedChanges},
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponentState, ProtocolComponentStateDelta},
        Address, AttrStoreKey, Balance, Chain, ChangeType, Code, CodeHash, ComponentId,
        ProtocolSystem, StoreKey, StoreVal, TxHash,
    },
    storage::StorageError,
    Bytes,
};

use super::window::FoldSink;

/// When a value was written: the writing block's timestamp, then its number.
///
/// The timestamp is the unit of the database's `valid_from`. The number orders blocks that share
/// a timestamp — consecutive blocks do on fast chains — the way the transaction index does in the
/// database. A snapshot row loads with number 0, so a folded block at the same timestamp still
/// applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WriteTag {
    pub(crate) block_ts: NaiveDateTime,
    pub(crate) block_number: u64,
}

impl WriteTag {
    pub(crate) fn snapshot(valid_from: NaiveDateTime) -> Self {
        Self { block_ts: valid_from, block_number: 0 }
    }
}

impl From<&Block> for WriteTag {
    fn from(block: &Block) -> Self {
        Self { block_ts: block.ts, block_number: block.number }
    }
}

/// A cached value together with the tag of the write that set it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tagged<T> {
    value: T,
    written_at: WriteTag,
}

impl<T> Tagged<T> {
    pub(crate) fn new(value: T, written_at: WriteTag) -> Self {
        Self { value, written_at }
    }

    pub(crate) fn value(&self) -> &T {
        &self.value
    }

    pub(crate) fn written_at(&self) -> WriteTag {
        self.written_at
    }

    /// Writes `value` at `tag` unless this already holds a newer value. Equal tags apply: delta
    /// values are absolute, so re-applying a block is harmless.
    pub(crate) fn write(&mut self, value: T, tag: WriteTag) {
        if tag < self.written_at {
            return;
        }
        self.value = value;
        self.written_at = tag;
    }
}

/// [`Tagged::write`] for a map entry; a missing key is inserted.
fn write_tagged<K: Eq + Hash, V>(map: &mut HashMap<K, Tagged<V>>, key: K, value: V, tag: WriteTag) {
    match map.entry(key) {
        Entry::Occupied(mut e) => e.get_mut().write(value, tag),
        Entry::Vacant(e) => {
            e.insert(Tagged::new(value, tag));
        }
    }
}

/// Write tags of one loaded account's values, each [`WriteTag::snapshot`] of its row's
/// `valid_from`.
#[derive(Debug, Clone)]
pub(crate) struct AccountWriteTags {
    pub(crate) slots: HashMap<StoreKey, WriteTag>,
    pub(crate) native_balance: WriteTag,
    pub(crate) code: WriteTag,
    pub(crate) token_balances: HashMap<Address, WriteTag>,
}

impl AccountWriteTags {
    /// One tag for every value of `account`.
    pub(crate) fn uniform(account: &Account, tag: WriteTag) -> Self {
        Self {
            slots: account
                .slots
                .keys()
                .map(|key| (key.clone(), tag))
                .collect(),
            native_balance: tag,
            code: tag,
            token_balances: account
                .token_balances
                .keys()
                .map(|token| (token.clone(), tag))
                .collect(),
        }
    }
}

/// Contract code with its hash, so a code write replaces both or neither.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CachedCode {
    pub(crate) code: Code,
    pub(crate) hash: CodeHash,
}

impl CachedCode {
    pub(crate) fn new(code: Code) -> Self {
        Self { hash: keccak256(&code).into(), code }
    }
}

/// Cached state of one contract account.
///
/// Every value carries the time it was last written, so writes from different extractors (which
/// run at different points of the chain) can never regress a value: newer wins, equal-time
/// re-application is a no-op.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CachedAccount {
    title: String,
    slots: HashMap<StoreKey, Tagged<StoreVal>>,
    native_balance: Tagged<Balance>,
    token_balances: HashMap<Address, Tagged<AccountBalance>>,
    code: Tagged<CachedCode>,
    /// Transaction references come from the startup load only — folds don't carry them.
    balance_modify_tx: TxHash,
    code_modify_tx: TxHash,
    creation_tx: Option<TxHash>,
}

impl CachedAccount {
    /// Builds an entry from the startup snapshot. Every slot and token balance of `account` must
    /// have a tag in `tags`; a missing tag is a loader bug and panics.
    pub(crate) fn from_snapshot(account: Account, tags: AccountWriteTags) -> Self {
        let slots = account
            .slots
            .into_iter()
            .map(|(key, value)| {
                let tag = *tags
                    .slots
                    .get(&key)
                    .unwrap_or_else(|| panic!("snapshot slot {key} has no write tag"));
                (key, Tagged::new(value, tag))
            })
            .collect();
        let token_balances = account
            .token_balances
            .into_iter()
            .map(|(token, balance)| {
                let tag = *tags
                    .token_balances
                    .get(&token)
                    .unwrap_or_else(|| {
                        panic!("snapshot balance of token {token} has no write tag")
                    });
                (token, Tagged::new(balance, tag))
            })
            .collect();
        Self {
            title: account.title,
            slots,
            native_balance: Tagged::new(account.native_balance, tags.native_balance),
            token_balances,
            code: Tagged::new(
                CachedCode { code: account.code, hash: account.code_hash },
                tags.code,
            ),
            balance_modify_tx: account.balance_modify_tx,
            code_modify_tx: account.code_modify_tx,
            creation_tx: account.creation_tx,
        }
    }

    /// Builds an entry from a `Creation` delta folded at `tag` — after startup, the only way a new
    /// contract enters the cache. The account is the one
    /// [`AccountDelta::into_account_without_tx`] builds; every value carries `tag`.
    pub(crate) fn from_creation(delta: &AccountDelta, tag: WriteTag) -> Self {
        let account = delta.clone().into_account_without_tx();
        let tags = AccountWriteTags::uniform(&account, tag);
        Self::from_snapshot(account, tags)
    }

    /// Applies one delta; every changed value gets `tag`, values with a newer tag stay. A deleted
    /// slot becomes the zero value, as in [`Account::apply_delta`]. A delta that carries code
    /// replaces the code and its hash together.
    pub(crate) fn apply_delta(&mut self, delta: &AccountDelta, tag: WriteTag) {
        for (key, value) in &delta.slots {
            write_tagged(&mut self.slots, key.clone(), value.clone().unwrap_or_default(), tag);
        }
        if let Some(balance) = &delta.balance {
            self.native_balance
                .write(balance.clone(), tag);
        }
        if let Some(code) = delta.code() {
            self.code
                .write(CachedCode::new(code.clone()), tag);
        }
    }

    /// Applies token balances under the same tag rule as [`CachedAccount::apply_delta`].
    pub(crate) fn apply_balances(
        &mut self,
        balances: &HashMap<Address, AccountBalance>,
        tag: WriteTag,
    ) {
        for (token, balance) in balances {
            write_tagged(&mut self.token_balances, token.clone(), balance.clone(), tag);
        }
    }

    /// The newest write among this account's values.
    fn newest_write(&self) -> WriteTag {
        let mut newest = self
            .native_balance
            .written_at()
            .max(self.code.written_at());
        for slot in self.slots.values() {
            newest = newest.max(slot.written_at());
        }
        for balance in self.token_balances.values() {
            newest = newest.max(balance.written_at());
        }
        newest
    }

    /// Materializes the cached state as an [`Account`] for response assembly.
    pub(crate) fn materialize(&self, chain: Chain, address: &Address) -> Account {
        Account::new(
            chain,
            address.clone(),
            self.title.clone(),
            self.slots
                .iter()
                .map(|(k, v)| (k.clone(), v.value().clone()))
                .collect(),
            self.native_balance.value().clone(),
            self.token_balances
                .iter()
                .map(|(k, v)| (k.clone(), v.value().clone()))
                .collect(),
            self.code.value().code.clone(),
            self.code.value().hash.clone(),
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

    pub(crate) fn code(&self) -> &Tagged<CachedCode> {
        &self.code
    }
}

/// Cached state of one protocol component.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CachedComponentState {
    attributes: HashMap<AttrStoreKey, StoreVal>,
    balances: HashMap<Address, Balance>,
    /// One write tag covers the whole entry: a single extractor writes each protocol system,
    /// in order.
    updated_at: WriteTag,
}

impl CachedComponentState {
    /// Builds an entry from the startup snapshot, tagged with its newest `valid_from`.
    pub(crate) fn from_snapshot(state: ProtocolComponentState, tag: WriteTag) -> Self {
        Self { attributes: state.attributes, balances: state.balances, updated_at: tag }
    }

    /// An entry for a component created at `tag`, before its first attributes arrive.
    pub(crate) fn from_creation(tag: WriteTag) -> Self {
        Self { attributes: HashMap::new(), balances: HashMap::new(), updated_at: tag }
    }

    /// Applies one state delta unless the entry is newer than `tag`. Updates apply first, then
    /// deletions, like [`ProtocolComponentState::apply_state_delta`].
    pub(crate) fn apply_delta(&mut self, delta: &ProtocolComponentStateDelta, tag: WriteTag) {
        if !self.advance_to(tag) {
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

    /// Applies balances unless the entry is newer than `tag`.
    pub(crate) fn apply_balances(
        &mut self,
        balances: &HashMap<Bytes, ComponentBalance>,
        tag: WriteTag,
    ) {
        if !self.advance_to(tag) {
            return;
        }
        self.balances.extend(
            balances
                .iter()
                .map(|(token, balance)| (token.clone(), balance.balance.clone())),
        );
    }

    /// Moves the entry to `tag` and returns `true`, unless `tag` is older: the single writer
    /// folds in order, so an older block is a replay and is skipped.
    fn advance_to(&mut self, tag: WriteTag) -> bool {
        if tag < self.updated_at {
            return false;
        }
        self.updated_at = tag;
        true
    }

    /// Materializes the cached state for response assembly.
    pub(crate) fn materialize(&self, component_id: &str) -> ProtocolComponentState {
        ProtocolComponentState::new(component_id, self.attributes.clone(), self.balances.clone())
    }

    /// Tag of the last write, for readers that apply only newer window changes.
    pub(crate) fn updated_at(&self) -> WriteTag {
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
    /// One chain per process: every extractor indexes it.
    chain: Chain,
    accounts: HashMap<Address, CachedAccount>,
    /// Component states by protocol system, then component id. The system is the extractor name:
    /// the RPC resolves one window per protocol system by extractor name, so both are one string.
    components: HashMap<ProtocolSystem, HashMap<ComponentId, CachedComponentState>>,
}

impl CacheState {
    pub(crate) fn chain(&self) -> Chain {
        self.chain
    }

    pub(crate) fn account(&self, address: &Address) -> Option<&CachedAccount> {
        self.accounts.get(address)
    }

    pub(crate) fn component(&self, system: &str, id: &str) -> Option<&CachedComponentState> {
        self.components
            .get(system)
            .and_then(|components| components.get(id))
    }
}

impl EntityCache {
    pub(crate) fn new(chain: Chain) -> Self {
        Self {
            state: RwLock::new(CacheState {
                chain,
                accounts: HashMap::new(),
                components: HashMap::new(),
            }),
        }
    }

    /// Read access for response assembly. Folds wait until the guard is dropped — hold it only
    /// long enough to copy out what the response needs.
    pub(crate) fn read(&self) -> RwLockReadGuard<'_, CacheState> {
        self.state
            .read()
            .expect("entity cache lock poisoned")
    }

    fn write_lock(&self) -> RwLockWriteGuard<'_, CacheState> {
        self.state
            .write()
            .expect("entity cache lock poisoned")
    }

    /// Write handle for the startup load. Nothing reads or folds while it exists.
    pub(crate) fn loader(&self) -> CacheLoader<'_> {
        CacheLoader(self.write_lock())
    }
}

/// Inserts entries without a tag check, under one write lock held for as long as the handle
/// lives. It exists only for the startup load, which runs before the extractors start.
pub(crate) struct CacheLoader<'a>(RwLockWriteGuard<'a, CacheState>);

impl CacheLoader<'_> {
    pub(crate) fn insert_account(&mut self, address: Address, entry: CachedAccount) {
        self.0.accounts.insert(address, entry);
    }

    pub(crate) fn insert_component(
        &mut self,
        system: ProtocolSystem,
        component_id: ComponentId,
        entry: CachedComponentState,
    ) {
        self.0
            .components
            .entry(system)
            .or_default()
            .insert(component_id, entry);
    }
}

impl CacheState {
    /// Applies one block's component changes, in an order where a new component exists before
    /// its first attributes arrive. A deleted component is removed unless a newer block already
    /// wrote to it.
    fn fold_components(&mut self, block: &BlockAggregatedChanges) {
        let tag = WriteTag::from(&block.block);
        if !block.new_protocol_components.is_empty() {
            let system_components = self
                .components
                .entry(block.extractor.clone())
                .or_default();
            for id in block.new_protocol_components.keys() {
                system_components
                    .entry(id.clone())
                    .or_insert_with(|| CachedComponentState::from_creation(tag));
            }
        }
        let Some(system_components) = self
            .components
            .get_mut(&block.extractor)
        else {
            trace!(system = %block.extractor, "Changes for a system with no cached components skipped");
            return;
        };
        for (id, delta) in &block.state_deltas {
            match system_components.get_mut(id) {
                Some(entry) => entry.apply_delta(delta, tag),
                None => {
                    trace!(system = %block.extractor, %id, "State delta for an unknown component skipped")
                }
            }
        }
        for (id, balances) in &block.component_balances {
            match system_components.get_mut(id) {
                Some(entry) => entry.apply_balances(balances, tag),
                None => {
                    trace!(system = %block.extractor, %id, "Balances for an unknown component skipped")
                }
            }
        }
        for id in block.deleted_protocol_components.keys() {
            if system_components
                .get(id)
                .is_some_and(|entry| entry.updated_at() <= tag)
            {
                system_components.remove(id);
            }
        }
    }

    /// Applies one block's account changes. A `Creation` delta carries the whole initial state
    /// and may create an entry; anything else for an unknown address is partial data and is
    /// skipped. A `Deletion` removes the entry unless a newer block already wrote to it.
    fn fold_accounts(&mut self, block: &BlockAggregatedChanges) {
        let tag = WriteTag::from(&block.block);
        for (address, delta) in &block.account_deltas {
            if delta.change_type() == ChangeType::Deletion {
                if self
                    .accounts
                    .get(address)
                    .is_some_and(|entry| entry.newest_write() <= tag)
                {
                    self.accounts.remove(address);
                }
                continue;
            }
            match self.accounts.get_mut(address) {
                Some(entry) => entry.apply_delta(delta, tag),
                None if delta.is_creation() => {
                    self.accounts
                        .insert(address.clone(), CachedAccount::from_creation(delta, tag));
                }
                None => trace!(%address, "Change for an unknown account skipped"),
            }
        }
        for (address, balances) in &block.account_balances {
            match self.accounts.get_mut(address) {
                Some(entry) => entry.apply_balances(balances, tag),
                None => trace!(%address, "Balances for an unknown account skipped"),
            }
        }
    }
}

impl FoldSink for EntityCache {
    /// Applies the whole block under the write lock. Not folded: `new_tokens`, `component_tvl`,
    /// `dci_update` — those stay database-served.
    ///
    /// No check can fail today. Any future check goes before the first mutation, so a replay of
    /// the same block converges.
    fn fold(&self, block: &BlockAggregatedChanges) -> Result<(), StorageError> {
        let mut state = self.write_lock();
        state.fold_components(block);
        state.fold_accounts(block);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::{extractor::models::fixtures, testing};

    const EXTRACTOR: &str = "ex";

    fn ts(n: u64) -> NaiveDateTime {
        testing::block(n).ts
    }

    fn tag(n: u64) -> WriteTag {
        WriteTag::from(&testing::block(n))
    }

    fn cached_account(cache: &EntityCache, address: &Bytes) -> Option<Account> {
        let state = cache.read();
        state
            .account(address)
            .map(|a| a.materialize(state.chain(), address))
    }

    fn cached_component(cache: &EntityCache, id: &str) -> Option<ProtocolComponentState> {
        cache
            .read()
            .component(EXTRACTOR, id)
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

    fn deletion(address: &Bytes) -> AccountDelta {
        AccountDelta::deleted(&Chain::Ethereum, address)
    }

    fn with_account_delta(
        mut m: BlockAggregatedChanges,
        delta: AccountDelta,
    ) -> BlockAggregatedChanges {
        m.account_deltas
            .insert(delta.address.clone(), delta);
        m
    }

    fn with_account_balance(
        mut m: BlockAggregatedChanges,
        address: &Bytes,
        token: &Bytes,
        amount: u64,
    ) -> BlockAggregatedChanges {
        m.account_balances
            .entry(address.clone())
            .or_default()
            .insert(token.clone(), account_balance(address, token, amount));
        m
    }

    fn msg(n: u64) -> BlockAggregatedChanges {
        testing::aggregated_changes(EXTRACTOR, n, n, Some(n))
    }

    fn component(id: &str) -> ProtocolComponent {
        ProtocolComponent::new(
            id,
            EXTRACTOR,
            "pool",
            Chain::Ethereum,
            vec![],
            vec![],
            HashMap::new(),
            ChangeType::Creation,
            Bytes::default(),
            ts(1),
        )
    }

    fn with_component(mut m: BlockAggregatedChanges, id: &str) -> BlockAggregatedChanges {
        m.new_protocol_components
            .insert(id.to_string(), component(id));
        m
    }

    fn with_state_delta(mut m: BlockAggregatedChanges, id: &str, x: u64) -> BlockAggregatedChanges {
        m.state_deltas
            .insert(id.to_string(), testing::state_delta(id, x));
        m
    }

    fn with_component_balance(
        mut m: BlockAggregatedChanges,
        id: &str,
        token: &Bytes,
        amount: u64,
    ) -> BlockAggregatedChanges {
        m.component_balances
            .entry(id.to_string())
            .or_default()
            .insert(token.clone(), component_balance(id, token, amount));
        m
    }

    fn with_deleted_component(mut m: BlockAggregatedChanges, id: &str) -> BlockAggregatedChanges {
        m.deleted_protocol_components
            .insert(id.to_string(), component(id));
        m
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
        let mut slot = Tagged::new(1u64, tag(5));

        slot.write(2, tag(4));

        assert_eq!(slot, Tagged::new(1, tag(5)));
    }

    #[test]
    fn write_applies_an_equal_tag() {
        let mut slot = Tagged::new(1u64, tag(5));

        slot.write(2, tag(5));

        assert_eq!(slot, Tagged::new(2, tag(5)));
    }

    #[test]
    fn write_keeps_the_higher_block_at_an_equal_timestamp() {
        let mut slot = Tagged::new(1u64, tag(5));
        let lower_block = WriteTag { block_ts: ts(5), block_number: 4 };

        slot.write(2, lower_block);

        assert_eq!(slot.value(), &1);
    }

    #[test]
    fn write_applies_a_folded_block_over_a_snapshot_at_an_equal_timestamp() {
        let mut slot = Tagged::new(1u64, WriteTag::snapshot(ts(5)));

        slot.write(2, tag(5));

        assert_eq!(slot.value(), &2);
    }

    #[test]
    fn write_inserts_a_missing_key_and_updates_a_present_one() {
        let mut map: HashMap<&str, Tagged<u64>> = HashMap::new();

        write_tagged(&mut map, "a", 1, tag(3));
        write_tagged(&mut map, "a", 2, tag(2));
        write_tagged(&mut map, "b", 9, tag(1));

        assert_eq!(map["a"], Tagged::new(1, tag(3)));
        assert_eq!(map["b"], Tagged::new(9, tag(1)));
    }

    #[test]
    fn account_snapshot_round_trips() {
        let address = addr(1);
        let loaded = account(&address);

        let cached = CachedAccount::from_snapshot(
            loaded.clone(),
            AccountWriteTags::uniform(&loaded, WriteTag::snapshot(ts(1))),
        );

        assert_eq!(cached.materialize(Chain::Ethereum, &address), loaded);
        let key1 = fixtures::slots([(1, 1)])
            .into_keys()
            .next()
            .unwrap();
        assert_eq!(cached.slots()[&key1].written_at(), WriteTag::snapshot(ts(1)));
    }

    #[test]
    fn creation_builds_the_account_the_delta_path_builds() {
        let address = addr(1);
        let delta = creation(&address, [(1, 1)], 10, "0x6000");

        let cached = CachedAccount::from_creation(&delta, tag(1));

        assert_eq!(cached.materialize(Chain::Ethereum, &address), delta.into_account_without_tx());
        assert_eq!(cached.code().written_at(), tag(1));
    }

    #[test]
    fn account_apply_keeps_newer_values_per_slot() {
        let address = addr(1);
        let mut cached = CachedAccount::from_snapshot(
            account(&address),
            AccountWriteTags::uniform(&account(&address), tag(5)),
        );

        cached.apply_delta(&update(&address, fixtures::optional_slots([(1, 11), (3, 3)])), tag(3));

        let slots = cached
            .materialize(Chain::Ethereum, &address)
            .slots;
        assert_eq!(
            slots,
            fixtures::slots([(1, 1), (2, 2), (3, 3)]),
            "older slot 1 kept, new slot 3 added"
        );
        let key3 = fixtures::slots([(3, 3)])
            .into_keys()
            .next()
            .unwrap();
        assert_eq!(cached.slots()[&key3].written_at(), tag(3));
    }

    #[test]
    fn account_apply_takes_an_equal_tag_write() {
        let address = addr(1);
        let mut cached = CachedAccount::from_snapshot(
            account(&address),
            AccountWriteTags::uniform(&account(&address), tag(5)),
        );

        cached.apply_delta(&update(&address, fixtures::optional_slots([(1, 11)])), tag(5));

        assert_eq!(
            cached
                .materialize(Chain::Ethereum, &address)
                .slots,
            fixtures::slots([(1, 11), (2, 2)])
        );
    }

    #[test]
    fn account_apply_twice_changes_nothing() {
        let address = addr(1);
        let mut cached =
            CachedAccount::from_creation(&creation(&address, [(1, 1)], 10, "0x6000"), tag(1));
        let delta = update(&address, fixtures::optional_slots([(1, 11), (2, 2)]));

        cached.apply_delta(&delta, tag(2));
        let once = cached.clone();
        cached.apply_delta(&delta, tag(2));

        assert_eq!(cached, once);
    }

    #[test]
    fn account_apply_zeroes_deleted_slots_and_refreshes_the_code_hash() {
        let address = addr(1);
        let mut cached =
            CachedAccount::from_creation(&creation(&address, [(1, 1)], 10, "0x6000"), tag(1));
        let key1 = fixtures::slots([(1, 1)])
            .into_keys()
            .next()
            .unwrap();
        let mut delta = update(&address, HashMap::from([(key1.clone(), None)]));
        delta.set_code(code("0x6001"));

        cached.apply_delta(&delta, tag(2));

        let account = cached.materialize(Chain::Ethereum, &address);
        assert_eq!(account.slots[&key1], Bytes::default());
        assert_eq!(account.code, code("0x6001"));
        assert_eq!(account.code_hash, Bytes::from(keccak256(code("0x6001"))));
        assert_eq!(cached.code().written_at(), tag(2));
    }

    #[test]
    fn account_apply_keeps_code_and_hash_against_an_older_write() {
        let address = addr(1);
        let mut cached = CachedAccount::from_creation(&creation(&address, [], 0, "0x6000"), tag(5));
        let mut delta = update(&address, HashMap::new());
        delta.set_code(code("0x6001"));

        cached.apply_delta(&delta, tag(4));

        let account = cached.materialize(Chain::Ethereum, &address);
        assert_eq!(account.code, code("0x6000"));
        assert_eq!(account.code_hash, Bytes::from(keccak256(code("0x6000"))));
        assert_eq!(cached.code().written_at(), tag(5));
    }

    #[test]
    fn account_apply_balances_follow_the_tag_rule() {
        let address = addr(1);
        let mut cached = CachedAccount::from_creation(&creation(&address, [], 0, "0x"), tag(5));

        cached.apply_balances(
            &HashMap::from([(addr(9), account_balance(&address, &addr(9), 7))]),
            tag(5),
        );
        cached.apply_balances(
            &HashMap::from([(addr(9), account_balance(&address, &addr(9), 1))]),
            tag(4),
        );

        assert_eq!(
            cached
                .materialize(Chain::Ethereum, &address)
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

        let cached = CachedComponentState::from_snapshot(loaded.clone(), WriteTag::snapshot(ts(3)));

        assert_eq!(cached.materialize("c1"), loaded);
        assert_eq!(cached.updated_at(), WriteTag::snapshot(ts(3)));
    }

    #[test]
    fn component_apply_skips_an_older_block_and_removes_deleted_attributes() {
        let mut cached = CachedComponentState::from_snapshot(
            ProtocolComponentState::new(
                "c1",
                HashMap::from([("x".to_string(), Bytes::from(1u64))]),
                HashMap::new(),
            ),
            tag(5),
        );

        cached.apply_delta(&testing::state_delta("c1", 9), tag(4));
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
        cached.apply_delta(&delta, tag(5));
        cached.apply_balances(
            &HashMap::from([(addr(9), component_balance("c1", &addr(9), 5))]),
            tag(6),
        );

        let state = cached.materialize("c1");
        assert_eq!(state.attributes, HashMap::from([("y".to_string(), Bytes::from(3u64))]));
        assert_eq!(state.balances, HashMap::from([(addr(9), Bytes::from(5u64))]));
        assert_eq!(cached.updated_at(), tag(6));
    }

    #[test]
    fn loaded_entries_read_back_by_address_and_by_system_and_id() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        let loaded = account(&address);
        let state = ProtocolComponentState::new("c1", HashMap::new(), HashMap::new());

        {
            let mut loader = cache.loader();
            loader.insert_account(
                address.clone(),
                CachedAccount::from_snapshot(
                    loaded.clone(),
                    AccountWriteTags::uniform(&loaded, WriteTag::snapshot(ts(1))),
                ),
            );
            loader.insert_component(
                EXTRACTOR.to_string(),
                "c1".to_string(),
                CachedComponentState::from_snapshot(state.clone(), WriteTag::snapshot(ts(1))),
            );
        }

        assert_eq!(cached_account(&cache, &address), Some(loaded));
        assert_eq!(cached_component(&cache, "c1"), Some(state));
    }

    #[test]
    fn fold_creates_components_before_their_first_attributes() {
        let cache = EntityCache::new(Chain::Ethereum);
        let block = with_component_balance(
            with_state_delta(with_component(msg(1), "c1"), "c1", 1),
            "c1",
            &addr(9),
            5,
        );

        cache.fold(&block).unwrap();

        let state = cached_component(&cache, "c1").unwrap();
        assert_eq!(state.attributes["x"], Bytes::from(1u64));
        assert_eq!(state.balances[&addr(9)], Bytes::from(5u64));
    }

    #[test]
    fn fold_skips_changes_for_an_unknown_component() {
        let cache = EntityCache::new(Chain::Ethereum);
        let block =
            with_component_balance(with_state_delta(msg(1), "ghost", 1), "ghost", &addr(9), 5);

        cache.fold(&block).unwrap();

        assert!(cached_component(&cache, "ghost").is_none());
    }

    #[test]
    fn fold_removes_deleted_components_and_skips_replayed_blocks() {
        let cache = EntityCache::new(Chain::Ethereum);
        cache
            .fold(&with_state_delta(with_component(msg(2), "c1"), "c1", 2))
            .unwrap();

        cache
            .fold(&with_state_delta(msg(1), "c1", 1))
            .unwrap();
        assert_eq!(
            cached_component(&cache, "c1")
                .unwrap()
                .attributes["x"],
            Bytes::from(2u64),
            "block 1 is a replay"
        );

        cache
            .fold(&with_deleted_component(msg(3), "c1"))
            .unwrap();
        assert!(cached_component(&cache, "c1").is_none());
    }

    #[test]
    fn fold_creation_creates_a_complete_account() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        let delta = creation(&address, [(1, 1), (2, 2)], 10, "0x6000");

        cache
            .fold(&with_account_balance(
                with_account_delta(msg(1), delta.clone()),
                &address,
                &addr(9),
                7,
            ))
            .unwrap();

        let mut expected = delta.into_account_without_tx();
        expected
            .token_balances
            .insert(addr(9), account_balance(&address, &addr(9), 7));
        assert_eq!(cached_account(&cache, &address), Some(expected));
    }

    #[test]
    fn fold_skips_changes_for_an_unknown_account() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        let block = with_account_balance(
            with_account_delta(msg(1), update(&address, fixtures::optional_slots([(1, 1)]))),
            &address,
            &addr(9),
            7,
        );

        cache.fold(&block).unwrap();

        assert!(cached_account(&cache, &address).is_none());
    }

    #[test]
    fn fold_removes_a_deleted_account() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        cache
            .fold(&with_account_delta(msg(1), creation(&address, [], 0, "0x")))
            .unwrap();

        cache
            .fold(&with_account_delta(msg(2), deletion(&address)))
            .unwrap();

        assert!(cached_account(&cache, &address).is_none());
    }

    #[test]
    fn fold_skips_an_account_deletion_older_than_the_entry() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        cache
            .fold(&with_account_delta(msg(1), creation(&address, [(1, 1)], 0, "0x")))
            .unwrap();
        cache
            .fold(&with_account_delta(
                msg(5),
                update(&address, fixtures::optional_slots([(1, 15)])),
            ))
            .unwrap();

        cache
            .fold(&with_account_delta(
                testing::aggregated_changes("other", 3, 3, Some(3)),
                deletion(&address),
            ))
            .unwrap();

        assert_eq!(
            cached_account(&cache, &address).map(|a| a.slots),
            Some(fixtures::slots([(1, 15)])),
            "block 3 cannot delete what block 5 wrote"
        );
    }

    #[test]
    fn fold_skips_a_component_deletion_older_than_the_entry() {
        let cache = EntityCache::new(Chain::Ethereum);
        cache
            .fold(&with_state_delta(with_component(msg(2), "c1"), "c1", 2))
            .unwrap();

        cache
            .fold(&with_deleted_component(msg(1), "c1"))
            .unwrap();

        assert!(cached_component(&cache, "c1").is_some(), "block 1 is a replay");
    }

    #[test]
    fn folding_the_same_block_twice_changes_nothing() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        cache
            .fold(&with_account_delta(
                with_component(msg(1), "c1"),
                creation(&address, [(1, 1)], 10, "0x6000"),
            ))
            .unwrap();
        let block = with_account_delta(
            with_state_delta(msg(2), "c1", 2),
            update(&address, fixtures::optional_slots([(1, 11), (2, 2)])),
        );

        cache.fold(&block).unwrap();
        let account_once = cached_account(&cache, &address);
        let component_once = cached_component(&cache, "c1");
        cache.fold(&block).unwrap();

        assert_eq!(cached_account(&cache, &address), account_once);
        assert_eq!(cached_component(&cache, "c1"), component_once);
    }

    #[test]
    fn two_extractors_folding_the_same_account_keep_both_values() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        cache
            .fold(&with_account_delta(msg(1), creation(&address, [(1, 1), (2, 2)], 0, "0x")))
            .unwrap();
        let ahead =
            with_account_delta(msg(5), update(&address, fixtures::optional_slots([(1, 15)])));
        let behind = with_account_delta(
            testing::aggregated_changes("other", 3, 3, Some(3)),
            update(&address, fixtures::optional_slots([(1, 13), (2, 23)])),
        );

        cache.fold(&ahead).unwrap();
        cache.fold(&behind).unwrap();

        let slots = cached_account(&cache, &address)
            .unwrap()
            .slots;
        assert_eq!(
            slots,
            fixtures::slots([(1, 15), (2, 23)]),
            "slot 1 keeps block 5, slot 2 takes block 3"
        );
    }

    #[test]
    fn a_lower_block_at_an_equal_timestamp_does_not_overwrite() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        cache
            .fold(&with_account_delta(msg(1), creation(&address, [(1, 1)], 0, "0x")))
            .unwrap();
        let ahead =
            with_account_delta(msg(5), update(&address, fixtures::optional_slots([(1, 15)])));
        let mut behind = with_account_delta(
            testing::aggregated_changes("other", 4, 4, Some(4)),
            update(&address, fixtures::optional_slots([(1, 14)])),
        );
        behind.block.ts = ahead.block.ts;

        cache.fold(&ahead).unwrap();
        cache.fold(&behind).unwrap();

        assert_eq!(
            cached_account(&cache, &address)
                .unwrap()
                .slots,
            fixtures::slots([(1, 15)]),
            "block 5 wins over block 4 at the same timestamp"
        );
    }

    #[test]
    fn folding_matches_the_delta_path() {
        let cache = EntityCache::new(Chain::Ethereum);
        let address = addr(1);
        let token = addr(9);
        let slot2 = fixtures::slots([(2, 2)])
            .into_keys()
            .next()
            .unwrap();
        let blocks = vec![
            with_component_balance(
                with_state_delta(
                    with_component(
                        with_account_delta(
                            msg(1),
                            creation(&address, [(1, 1), (2, 2)], 10, "0x6000"),
                        ),
                        "c1",
                    ),
                    "c1",
                    1,
                ),
                "c1",
                &token,
                5,
            ),
            with_state_delta(
                with_account_balance(
                    with_account_delta(
                        msg(2),
                        update(&address, fixtures::optional_slots([(1, 11), (3, 3)])),
                    ),
                    &address,
                    &token,
                    7,
                ),
                "c1",
                2,
            ),
            with_component_balance(
                with_account_delta(msg(3), update(&address, HashMap::from([(slot2, None)]))),
                "c1",
                &token,
                6,
            ),
        ];

        for block in &blocks {
            cache.fold(block).unwrap();
        }

        let mut expected_account: Option<Account> = None;
        let mut expected_state = ProtocolComponentState::new("c1", HashMap::new(), HashMap::new());
        for block in &blocks {
            if let Some(delta) = block.account_deltas.get(&address) {
                let account =
                    expected_account.get_or_insert_with(|| delta.clone().into_account_without_tx());
                account.apply_delta(delta).unwrap();
            }
            if let Some(balances) = block.account_balances.get(&address) {
                let account = expected_account.as_mut().unwrap();
                for (token, balance) in balances {
                    account
                        .token_balances
                        .insert(token.clone(), balance.clone());
                }
            }
            if let Some(delta) = block.state_deltas.get("c1") {
                expected_state
                    .apply_state_delta(delta)
                    .unwrap();
            }
            if let Some(balances) = block.component_balances.get("c1") {
                expected_state
                    .apply_balance_delta(balances)
                    .unwrap();
            }
        }
        assert_eq!(cached_account(&cache, &address), expected_account);
        assert_eq!(cached_component(&cache, "c1"), Some(expected_state));
    }
}
