//! Views of cached entries, which state responses are written from.
//!
//! A view holds a cached entry behind its `Arc`, plus only the values that the window changes
//! override. It writes the response JSON directly from both, so serving an entry never copies it.
//! The `From` impls build the owned DTOs from a view.

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::Arc,
};

use serde::{ser::SerializeStruct, Serialize, Serializer};
use tycho_common::{
    dto::{self, PaginationResponse},
    models::{
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Address, AttrStoreKey, Balance, ComponentId, StoreKey, StoreVal,
    },
    serde_primitives::{hex_hashmap_key_value, hex_hashmap_value},
    storage::WriteTimestamp,
    Bytes,
};

use super::{
    apply_account_block, AccountWrites, CachedAccount, CachedCode, CachedComponentState,
    Timestamped, WriteOutcome,
};

/// A cached account with the window changes up to the requested version laid over it. Each overlay
/// field holds only the values that win over the base.
#[derive(Debug)]
pub(crate) struct AccountView {
    base: Arc<CachedAccount>,
    /// Slots the window changes override or add.
    slots: HashMap<StoreKey, Timestamped<StoreVal>>,
    token_balances: HashMap<Address, Timestamped<AccountBalance>>,
    native_balance: Option<Timestamped<Balance>>,
    code: Option<Timestamped<CachedCode>>,
}

impl AccountView {
    pub(crate) fn new(base: Arc<CachedAccount>) -> Self {
        Self {
            base,
            slots: HashMap::new(),
            token_balances: HashMap::new(),
            native_balance: None,
            code: None,
        }
    }

    /// Lays one block's changes over the view with the rule of [`apply_account_block`]. The base
    /// entry does not change.
    pub(crate) fn apply_block(
        &mut self,
        delta: Option<&AccountDelta>,
        balances: Option<&HashMap<Address, AccountBalance>>,
        at: WriteTimestamp,
    ) {
        apply_account_block(self, delta, balances, at);
    }

    /// Slots plus token balances the JSON holds, where an overridden key counts twice: an upper
    /// bound for the JSON size estimate.
    pub(crate) fn json_key_count_hint(&self) -> usize {
        self.base.slots.len() +
            self.slots.len() +
            self.base.token_balances.len() +
            self.token_balances.len()
    }

    /// Length of the code the JSON holds, in bytes.
    pub(crate) fn code_len(&self) -> usize {
        self.code().code.len()
    }
}

/// The first write to the native balance or the code starts from the base value.
impl AccountWrites for AccountView {
    fn address(&self) -> &Address {
        &self.base.address
    }

    fn write_slot(&mut self, key: &StoreKey, value: StoreVal, at: WriteTimestamp) -> WriteOutcome {
        write_overlay(&mut self.slots, &self.base.slots, key, value, at)
    }

    fn write_native_balance(&mut self, balance: Balance, at: WriteTimestamp) -> WriteOutcome {
        self.native_balance
            .get_or_insert_with(|| self.base.native_balance.clone())
            .write(balance, at)
    }

    fn write_code(&mut self, code: CachedCode, at: WriteTimestamp) -> WriteOutcome {
        self.code
            .get_or_insert_with(|| self.base.code.clone())
            .write(code, at)
    }

    fn write_token_balance(
        &mut self,
        token: &Address,
        balance: AccountBalance,
        at: WriteTimestamp,
    ) -> WriteOutcome {
        write_overlay(&mut self.token_balances, &self.base.token_balances, token, balance, at)
    }
}

/// [`Timestamped::write`] for a key of an overlay map. The first write to a key starts from the
/// base value, if the base has one. The overlay keeps the key only when the write applies, so it
/// holds only values that win over the base.
fn write_overlay<K: Clone + Eq + Hash, V: Clone + PartialEq>(
    overlay: &mut HashMap<K, Timestamped<V>>,
    base: &HashMap<K, Timestamped<V>>,
    key: &K,
    value: V,
    at: WriteTimestamp,
) -> WriteOutcome {
    if let Some(current) = overlay.get_mut(key) {
        return current.write(value, at);
    }
    let Some(current) = base.get(key) else {
        overlay.insert(key.clone(), Timestamped::new(value, at));
        return WriteOutcome::Applied;
    };
    let mut current = current.clone();
    let outcome = current.write(value, at);
    if outcome == WriteOutcome::Applied {
        overlay.insert(key.clone(), current);
    }
    outcome
}

/// The entries of `base`, with the value of `overlay` in place where it has one, then the entries
/// of `overlay` that `base` does not have.
fn overlaid<'a, K: Eq + Hash, V>(
    base: &'a HashMap<K, Timestamped<V>>,
    overlay: &'a HashMap<K, Timestamped<V>>,
) -> impl Iterator<Item = (&'a K, &'a V)> + Clone + 'a {
    let replaced = base.iter().map(move |(key, value)| {
        (
            key,
            overlay
                .get(key)
                .unwrap_or(value)
                .value(),
        )
    });
    let added = overlay
        .iter()
        .filter(move |(key, _)| !base.contains_key(*key))
        .map(|(key, value)| (key, value.value()));
    replaced.chain(added)
}

/// Writes the pairs an iterator yields as a map of hex keys and hex values. Serializing borrows
/// `self`, so the iterator is cloned.
struct HexKeyValueMap<I>(I);

impl<'a, I> Serialize for HexKeyValueMap<I>
where
    I: Iterator<Item = (&'a Bytes, &'a Bytes)> + Clone,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        hex_hashmap_key_value::serialize_iter(self.0.clone(), serializer)
    }
}

/// Writes the pairs an iterator yields as a map of plain keys and hex values.
struct HexValueMap<I>(I);

impl<'a, I> Serialize for HexValueMap<I>
where
    I: Iterator<Item = (&'a AttrStoreKey, &'a Bytes)> + Clone,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        hex_hashmap_value::serialize_iter(self.0.clone(), serializer)
    }
}

impl AccountView {
    /// The slots of the account with the window changes laid over them.
    fn slots(&self) -> impl Iterator<Item = (&StoreKey, &StoreVal)> + Clone {
        overlaid(&self.base.slots, &self.slots)
    }

    /// The token balances of the account with the window changes laid over them.
    fn token_balances(&self) -> impl Iterator<Item = (&Address, &Balance)> + Clone {
        overlaid(&self.base.token_balances, &self.token_balances)
            .map(|(token, balance)| (token, &balance.balance))
    }

    fn native_balance(&self) -> &Balance {
        self.native_balance
            .as_ref()
            .unwrap_or(&self.base.native_balance)
            .value()
    }

    fn code(&self) -> &CachedCode {
        self.code
            .as_ref()
            .unwrap_or(&self.base.code)
            .value()
    }
}

/// Writes the JSON of [`dto::ResponseAccount`]: the same fields, the same hex strings.
impl Serialize for AccountView {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let base = &*self.base;
        let code = self.code();
        let mut account = serializer.serialize_struct("Account", 11)?;
        account.serialize_field("chain", &dto::Chain::from(base.chain))?;
        account.serialize_field("address", &base.address)?;
        account.serialize_field("title", &base.title)?;
        account.serialize_field("slots", &HexKeyValueMap(self.slots()))?;
        account.serialize_field("native_balance", self.native_balance())?;
        account.serialize_field("token_balances", &HexKeyValueMap(self.token_balances()))?;
        account.serialize_field("code", &code.code)?;
        account.serialize_field("code_hash", &code.hash)?;
        account.serialize_field("balance_modify_tx", &base.balance_modify_tx)?;
        account.serialize_field("code_modify_tx", &base.code_modify_tx)?;
        account.serialize_field("creation_tx", &base.creation_tx)?;
        account.end()
    }
}

/// Copies the account out of the view.
impl From<AccountView> for dto::ResponseAccount {
    fn from(view: AccountView) -> Self {
        let code = view.code();
        dto::ResponseAccount::new(
            view.base.chain.into(),
            view.base.address.clone(),
            view.base.title.clone(),
            view.slots()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            view.native_balance().clone(),
            view.token_balances()
                .map(|(token, balance)| (token.clone(), balance.clone()))
                .collect(),
            code.code.clone(),
            code.hash.clone(),
            view.base.balance_modify_tx.clone(),
            view.base.code_modify_tx.clone(),
            view.base.creation_tx.clone(),
        )
    }
}

/// A `/contract_state` answer made of account views. Serializes like
/// [`dto::StateRequestResponse`].
#[derive(Debug, Serialize)]
pub(crate) struct ContractStateView {
    pub(crate) accounts: Vec<AccountView>,
    pub(crate) pagination: PaginationResponse,
}

impl From<ContractStateView> for dto::StateRequestResponse {
    fn from(view: ContractStateView) -> Self {
        dto::StateRequestResponse::new(
            view.accounts
                .into_iter()
                .map(dto::ResponseAccount::from)
                .collect(),
            view.pagination,
        )
    }
}

/// A cached component state with the window changes newer than the entry laid over it.
#[derive(Debug)]
pub(crate) struct ComponentView {
    component_id: ComponentId,
    /// `None` for an id the cache does not hold: the view then starts from an empty state.
    base: Option<Arc<CachedComponentState>>,
    /// Attributes the window changes set. Disjoint from `deleted`.
    updated: HashMap<AttrStoreKey, StoreVal>,
    /// Attributes the window changes delete. Disjoint from `updated`.
    deleted: HashSet<AttrStoreKey>,
    /// Balances the window changes set.
    updated_balances: HashMap<Address, Balance>,
    include_balances: bool,
}

impl ComponentView {
    pub(crate) fn new(
        component_id: &str,
        base: Option<Arc<CachedComponentState>>,
        include_balances: bool,
    ) -> Self {
        Self {
            component_id: component_id.to_string(),
            base,
            updated: HashMap::new(),
            deleted: HashSet::new(),
            updated_balances: HashMap::new(),
            include_balances,
        }
    }

    /// Lays one block's changes over the view in the order of
    /// `ProtocolComponentState::apply_state_delta`, then `apply_balance_delta`: updated
    /// attributes, then deleted ones, then balances. An update in a later block sets a deleted
    /// attribute again. A block at or before the base entry is already folded into it, so the
    /// method leaves the view as it is.
    pub(crate) fn apply_block(
        &mut self,
        delta: Option<&ProtocolComponentStateDelta>,
        balances: Option<&HashMap<Bytes, ComponentBalance>>,
        at: WriteTimestamp,
    ) {
        if self
            .base
            .as_ref()
            .is_some_and(|base| at <= base.updated_at())
        {
            return;
        }
        if let Some(delta) = delta {
            for (key, value) in &delta.updated_attributes {
                self.deleted.remove(key);
                self.updated
                    .insert(key.clone(), value.clone());
            }
            for key in &delta.deleted_attributes {
                self.updated.remove(key);
                self.deleted.insert(key.clone());
            }
        }
        if !self.include_balances {
            return;
        }
        for (token, balance) in balances.into_iter().flatten() {
            self.updated_balances
                .insert(token.clone(), balance.balance.clone());
        }
    }

    /// Attributes plus balances the JSON holds, where an overridden key counts twice: an upper
    /// bound for the JSON size estimate.
    pub(crate) fn json_key_count_hint(&self) -> usize {
        let (attributes, balances) = self
            .base
            .as_ref()
            .map_or((0, 0), |base| (base.attributes.len(), base.balances.len()));
        let balances =
            if self.include_balances { balances + self.updated_balances.len() } else { 0 };
        attributes + self.updated.len() + balances
    }

    /// The base attributes that the window changes neither set nor delete, then the ones they
    /// set.
    fn attributes(&self) -> impl Iterator<Item = (&AttrStoreKey, &StoreVal)> + Clone + '_ {
        let kept = self
            .base
            .iter()
            .flat_map(|base| base.attributes.iter())
            .filter(|(key, _)| !self.deleted.contains(*key) && !self.updated.contains_key(*key));
        kept.chain(self.updated.iter())
    }

    /// The base balances the window changes do not set, then the ones they set. Empty when the
    /// request leaves balances out.
    fn balances(&self) -> impl Iterator<Item = (&Address, &Balance)> + Clone + '_ {
        let base = self
            .base
            .as_ref()
            .filter(|_| self.include_balances);
        let kept = base
            .into_iter()
            .flat_map(|base| base.balances.iter())
            .filter(|(token, _)| {
                !self
                    .updated_balances
                    .contains_key(*token)
            });
        kept.chain(self.updated_balances.iter())
    }
}

/// Writes the JSON of [`dto::ResponseProtocolState`]: the same fields, the same hex strings.
impl Serialize for ComponentView {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ResponseProtocolState", 3)?;
        state.serialize_field("component_id", &self.component_id)?;
        state.serialize_field("attributes", &HexValueMap(self.attributes()))?;
        state.serialize_field("balances", &HexKeyValueMap(self.balances()))?;
        state.end()
    }
}

/// Copies the state out of the view.
impl From<ComponentView> for dto::ResponseProtocolState {
    fn from(view: ComponentView) -> Self {
        let attributes = view
            .attributes()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let balances = view
            .balances()
            .map(|(token, balance)| (token.clone(), balance.clone()))
            .collect();
        dto::ResponseProtocolState { component_id: view.component_id, attributes, balances }
    }
}

/// A `/protocol_state` answer made of component views. Serializes like
/// [`dto::ProtocolStateRequestResponse`].
#[derive(Debug, Serialize)]
pub(crate) struct ProtocolStateView {
    pub(crate) states: Vec<ComponentView>,
    pub(crate) pagination: PaginationResponse,
}

impl From<ProtocolStateView> for dto::ProtocolStateRequestResponse {
    fn from(view: ProtocolStateView) -> Self {
        dto::ProtocolStateRequestResponse::new(
            view.states
                .into_iter()
                .map(dto::ResponseProtocolState::from)
                .collect(),
            view.pagination,
        )
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::{
        models::{contract::Account, protocol::ProtocolComponentState, Chain, ChangeType},
        storage::AccountWriteTimestamps,
    };

    use super::*;
    use crate::{extractor::models::fixtures, testing};

    fn at(n: u64) -> WriteTimestamp {
        WriteTimestamp::from(&testing::block(n))
    }

    fn addr(n: u64) -> Bytes {
        Bytes::from(n).lpad(20, 0)
    }

    fn word(n: u64) -> Bytes {
        Bytes::from(n).lpad(32, 0)
    }

    /// One block's changes to account `addr(1)`.
    struct AccountBlock {
        at: WriteTimestamp,
        delta: Option<AccountDelta>,
        balances: Option<HashMap<Address, AccountBalance>>,
    }

    fn update(
        n: u64,
        slots: HashMap<Bytes, Option<Bytes>>,
        native: Option<u64>,
        code: Option<&str>,
    ) -> AccountBlock {
        AccountBlock {
            at: at(n),
            delta: Some(AccountDelta::new(
                Chain::Ethereum,
                addr(1),
                slots,
                native.map(word),
                code.map(|code| Bytes::from_str(code).unwrap()),
                ChangeType::Update,
            )),
            balances: None,
        }
    }

    fn slots_block(n: u64, slots: &[(u64, Option<u64>)]) -> AccountBlock {
        let slots = slots
            .iter()
            .map(|(key, value)| (word(*key), value.map(word)))
            .collect();
        update(n, slots, None, None)
    }

    fn token_block(n: u64, token: u64, amount: u64) -> AccountBlock {
        let balance = AccountBalance::new(addr(1), addr(token), word(amount), Bytes::default());
        AccountBlock {
            at: at(n),
            delta: None,
            balances: Some(HashMap::from([(addr(token), balance)])),
        }
    }

    /// Account `addr(1)` created in block 1 with slots 1 = 1 and 2 = 2, native balance 10, code
    /// `0x01` and 5 of token `addr(9)`; block 3 then set slot 2 to 3.
    fn base_account() -> Arc<CachedAccount> {
        let creation = AccountDelta::new(
            Chain::Ethereum,
            addr(1),
            fixtures::optional_slots([(1, 1), (2, 2)]),
            Some(word(10)),
            Some(Bytes::from_str("0x01").unwrap()),
            ChangeType::Creation,
        );
        let balances = HashMap::from([(
            addr(9),
            AccountBalance::new(addr(1), addr(9), word(5), Bytes::default()),
        )]);
        let mut account = CachedAccount::from_creation(&creation, Some(&balances), at(1));
        let later = slots_block(3, &[(2, Some(3))]);
        account.apply_block(later.delta.as_ref(), None, later.at);
        Arc::new(account)
    }

    /// The account of [`base_account`] from the startup snapshot, with a title and distinct
    /// transaction hashes. These fields go from the base into the JSON, so a swapped field shows.
    fn loaded_account() -> Arc<CachedAccount> {
        let code = CachedCode::new(Bytes::from_str("0x01").unwrap());
        let account = Account::new(
            Chain::Ethereum,
            addr(1),
            "loaded".to_string(),
            HashMap::from([(word(1), word(1)), (word(2), word(3))]),
            word(10),
            HashMap::from([(addr(9), AccountBalance::new(addr(1), addr(9), word(5), word(24)))]),
            code.code,
            code.hash,
            word(21),
            word(22),
            Some(word(23)),
        );
        let timestamps = AccountWriteTimestamps {
            slots: HashMap::from([(word(1), at(1)), (word(2), at(3))]),
            native_balance: at(1),
            code: at(1),
            token_balances: HashMap::from([(addr(9), at(1))]),
        };
        Arc::new(CachedAccount::from_snapshot(account, timestamps))
    }

    /// The view must write the JSON of the owned answer: copy the entry, apply every block with
    /// `CachedAccount::apply_block`, convert to the DTO.
    #[rstest]
    #[case::no_changes(vec![])]
    #[case::newer_slot(vec![slots_block(4, &[(1, Some(11))])])]
    #[case::older_slot_is_skipped(vec![slots_block(2, &[(2, Some(22))])])]
    #[case::equal_timestamp_conflict_keeps_the_cached_value(vec![slots_block(3, &[(2, Some(33))])])]
    #[case::deleted_slot_is_zero(vec![slots_block(4, &[(1, None)])])]
    #[case::deleted_absent_slot_is_zero(vec![slots_block(4, &[(7, None)])])]
    #[case::new_slot(vec![slots_block(4, &[(7, Some(7))])])]
    #[case::native_balance(vec![update(4, HashMap::new(), Some(11), None)])]
    #[case::code_and_its_hash(vec![update(4, HashMap::new(), None, Some("0x0202"))])]
    #[case::overlay_key_conflict_keeps_the_first_value(vec![
        slots_block(4, &[(1, Some(11))]),
        slots_block(4, &[(1, Some(12))]),
    ])]
    #[case::native_balance_and_code_conflict_keeps_the_cached_values(vec![
        update(1, HashMap::new(), Some(11), Some("0x0202")),
    ])]
    #[case::new_token(vec![token_block(4, 8, 1)])]
    #[case::replaced_token(vec![token_block(4, 9, 6)])]
    #[case::several_blocks(vec![
        slots_block(4, &[(1, Some(11))]),
        slots_block(5, &[(1, Some(12)), (7, Some(7))]),
        update(5, HashMap::new(), Some(12), None),
        token_block(5, 9, 7),
    ])]
    fn account_view_writes_the_json_of_the_owned_path(
        #[case] blocks: Vec<AccountBlock>,
        #[values(base_account(), loaded_account())] base: Arc<CachedAccount>,
    ) {
        let mut owned = (*base).clone();
        let mut view = AccountView::new(base);
        for block in &blocks {
            owned.apply_block(block.delta.as_ref(), block.balances.as_ref(), block.at);
            view.apply_block(block.delta.as_ref(), block.balances.as_ref(), block.at);
        }
        let expected = dto::ResponseAccount::from(Account::from(owned));

        assert_eq!(serde_json::to_value(&view).unwrap(), serde_json::to_value(&expected).unwrap());
        // `to_value` merges duplicate keys, so only the counts catch a key written twice.
        assert_eq!(overlaid(&view.base.slots, &view.slots).count(), expected.slots.len());
        assert_eq!(
            overlaid(&view.base.token_balances, &view.token_balances).count(),
            expected.token_balances.len()
        );
        assert_eq!(dto::ResponseAccount::from(view), expected);
    }

    /// One block's changes to component `c1`.
    struct ComponentBlock {
        at: WriteTimestamp,
        delta: Option<ProtocolComponentStateDelta>,
        balances: Option<HashMap<Bytes, ComponentBalance>>,
    }

    fn attributes_block(n: u64, updated: &[(&str, u64)], deleted: &[&str]) -> ComponentBlock {
        ComponentBlock {
            at: at(n),
            delta: Some(ProtocolComponentStateDelta::new(
                "c1",
                updated
                    .iter()
                    .map(|(key, value)| (key.to_string(), Bytes::from(*value)))
                    .collect(),
                deleted
                    .iter()
                    .map(|key| key.to_string())
                    .collect::<HashSet<_>>(),
            )),
            balances: None,
        }
    }

    fn component_balance_block(n: u64, token: u64, amount: u64) -> ComponentBlock {
        let balance = ComponentBalance {
            token: addr(token),
            balance: Bytes::from(amount),
            balance_float: amount as f64,
            modify_tx: Bytes::default(),
            component_id: "c1".to_string(),
        };
        ComponentBlock {
            at: at(n),
            delta: None,
            balances: Some(HashMap::from([(addr(token), balance)])),
        }
    }

    /// Component `c1` at block 3 with attributes `a` = 1 and `b` = 2 and 5 of token `addr(9)`.
    fn base_component() -> Arc<CachedComponentState> {
        Arc::new(CachedComponentState::from_snapshot(
            ProtocolComponentState::new(
                "c1",
                HashMap::from([
                    ("a".to_string(), Bytes::from(1u64)),
                    ("b".to_string(), Bytes::from(2u64)),
                ]),
                HashMap::from([(addr(9), Bytes::from(5u64))]),
            ),
            at(3),
        ))
    }

    /// The owned answer: copy the entry, merge every block newer than it with the database path's
    /// merge, drop the balances when they are not requested.
    fn owned_component(
        base: Option<&CachedComponentState>,
        blocks: &[ComponentBlock],
        include_balances: bool,
    ) -> dto::ResponseProtocolState {
        let updated_at = base.map(CachedComponentState::updated_at);
        let mut state = base.cloned().map_or_else(
            || ProtocolComponentState::new("c1", HashMap::new(), HashMap::new()),
            ProtocolComponentState::from,
        );
        for block in blocks
            .iter()
            .filter(|block| updated_at.is_none_or(|at| block.at > at))
        {
            if let Some(delta) = &block.delta {
                state.apply_state_delta(delta).unwrap();
            }
            if let Some(balances) = &block.balances {
                state
                    .apply_balance_delta(balances)
                    .unwrap();
            }
        }
        if !include_balances {
            state.balances.clear();
        }
        dto::ResponseProtocolState::from(state)
    }

    fn component_view(
        base: Option<Arc<CachedComponentState>>,
        blocks: &[ComponentBlock],
        include_balances: bool,
    ) -> ComponentView {
        let mut view = ComponentView::new("c1", base, include_balances);
        for block in blocks {
            view.apply_block(block.delta.as_ref(), block.balances.as_ref(), block.at);
        }
        view
    }

    #[rstest]
    #[case::no_changes(Some(base_component()), vec![], true)]
    #[case::updated(Some(base_component()), vec![attributes_block(4, &[("a", 11)], &[])], true)]
    #[case::added(Some(base_component()), vec![attributes_block(4, &[("c", 3)], &[])], true)]
    #[case::deleted(Some(base_component()), vec![attributes_block(4, &[], &["a"])], true)]
    #[case::updated_and_deleted_in_one_block(
        Some(base_component()),
        vec![attributes_block(4, &[("a", 11)], &["a"])],
        true
    )]
    #[case::deleted_then_set_again(
        Some(base_component()),
        vec![attributes_block(4, &[], &["a"]), attributes_block(5, &[("a", 12)], &[])],
        true
    )]
    #[case::added_then_deleted(
        Some(base_component()),
        vec![attributes_block(4, &[("c", 3)], &[]), attributes_block(5, &[], &["c"])],
        true
    )]
    #[case::block_at_the_entry_is_skipped(
        Some(base_component()),
        vec![attributes_block(3, &[("a", 11)], &[])],
        true
    )]
    #[case::balances(
        Some(base_component()),
        vec![component_balance_block(4, 9, 6), component_balance_block(4, 8, 1)],
        true
    )]
    #[case::balances_left_out(
        Some(base_component()),
        vec![component_balance_block(4, 8, 1)],
        false
    )]
    #[case::uncached_id_starts_empty(
        None,
        vec![attributes_block(4, &[("a", 1)], &[]), component_balance_block(4, 8, 1)],
        true
    )]
    fn component_view_writes_the_json_of_the_owned_path(
        #[case] base: Option<Arc<CachedComponentState>>,
        #[case] blocks: Vec<ComponentBlock>,
        #[case] include_balances: bool,
    ) {
        let expected = owned_component(base.as_deref(), &blocks, include_balances);

        let view = component_view(base, &blocks, include_balances);

        assert_eq!(serde_json::to_value(&view).unwrap(), serde_json::to_value(&expected).unwrap());
        // `to_value` merges duplicate keys, so only the counts catch a key written twice.
        assert_eq!(view.attributes().count(), expected.attributes.len());
        assert_eq!(view.balances().count(), expected.balances.len());
        assert_eq!(dto::ResponseProtocolState::from(view), expected);
    }

    /// Stops compiling when a DTO gains a field: the view `Serialize` impls must then write it.
    #[test]
    #[allow(deprecated)]
    fn views_write_every_dto_field() {
        let _ = |dto::ResponseAccount {
                     chain: _,
                     address: _,
                     title: _,
                     slots: _,
                     native_balance: _,
                     token_balances: _,
                     code: _,
                     code_hash: _,
                     balance_modify_tx: _,
                     code_modify_tx: _,
                     creation_tx: _,
                 }: dto::ResponseAccount,
                 dto::ResponseProtocolState { component_id: _, attributes: _, balances: _ }: dto::ResponseProtocolState,
                 dto::StateRequestResponse { accounts: _, pagination: _ }: dto::StateRequestResponse,
                 dto::ProtocolStateRequestResponse { states: _, pagination: _ }: dto::ProtocolStateRequestResponse| {};
    }
}
