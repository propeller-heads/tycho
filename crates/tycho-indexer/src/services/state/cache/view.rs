//! Views of cached entries, which contract state answers are built from.
//!
//! A view holds a cached entry behind its `Arc`, plus only the values that the window changes
//! override. It writes the response JSON directly from both, without a copy of the entry.
//! The `From` impls build the owned DTOs from a view.

use std::{collections::HashMap, hash::Hash, sync::Arc};

use serde::{ser::SerializeStruct, Serialize, Serializer};
use tycho_common::{
    dto::{self, PaginationResponse},
    models::{
        contract::{AccountBalance, AccountDelta},
        Address, Balance, StoreKey, StoreVal,
    },
    serde_primitives::hex_hashmap_key_value,
    storage::WriteTimestamp,
    Bytes,
};

use super::{
    apply_account_block, AccountWrites, CachedAccount, CachedCode, Timestamped, WriteOutcome,
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

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::{
        models::{contract::Account, Chain, ChangeType},
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
}
