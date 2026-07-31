use std::collections::HashMap;

use deepsize::DeepSizeOf;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
    dto, keccak256,
    models::{
        blockchain::Transaction, Address, Balance, Chain, ChangeType, Code, CodeHash, ContractId,
        ContractStore, ContractStoreDeltas, MergeError, StoreKey, TxHash,
    },
    Bytes,
};

#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub chain: Chain,
    pub address: Address,
    pub title: String,
    pub slots: ContractStore,
    pub native_balance: Balance,
    pub token_balances: HashMap<Address, AccountBalance>,
    pub code: Code,
    pub code_hash: CodeHash,
    pub balance_modify_tx: TxHash,
    pub code_modify_tx: TxHash,
    pub creation_tx: Option<TxHash>,
}

impl Account {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        address: Address,
        title: String,
        slots: ContractStore,
        native_balance: Balance,
        token_balances: HashMap<Address, AccountBalance>,
        code: Code,
        code_hash: CodeHash,
        balance_modify_tx: TxHash,
        code_modify_tx: TxHash,
        creation_tx: Option<TxHash>,
    ) -> Self {
        Self {
            chain,
            address,
            title,
            slots,
            native_balance,
            token_balances,
            code,
            code_hash,
            balance_modify_tx,
            code_modify_tx,
            creation_tx,
        }
    }

    pub fn set_balance(&mut self, new_balance: &Balance, modified_at: &Balance) {
        self.native_balance = new_balance.clone();
        self.balance_modify_tx = modified_at.clone();
    }

    pub fn apply_delta(&mut self, delta: &AccountDelta) -> Result<(), MergeError> {
        let self_id = (self.chain, &self.address);
        let other_id = (delta.chain, &delta.address);
        if self_id != other_id {
            return Err(MergeError::IdMismatch(
                "AccountDeltas".to_string(),
                format!("{self_id:?}"),
                format!("{other_id:?}"),
            ));
        }
        if let Some(balance) = delta.balance.as_ref() {
            self.native_balance.clone_from(balance);
        }
        if let Some(code) = delta.code.as_ref() {
            self.code.clone_from(code);
        }
        self.slots.extend(
            delta
                .slots
                .clone()
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default())),
        );
        // TODO: Update modify_tx, code_modify_tx and code_hash.
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default, DeepSizeOf)]
pub struct AccountDelta {
    pub chain: Chain,
    pub address: Address,
    pub slots: ContractStoreDeltas,
    pub balance: Option<Balance>,
    code: Option<Code>,
    change: ChangeType,
}

impl AccountDelta {
    pub fn deleted(chain: &Chain, address: &Address) -> Self {
        Self {
            chain: *chain,
            address: address.clone(),
            change: ChangeType::Deletion,
            ..Default::default()
        }
    }

    pub fn new(
        chain: Chain,
        address: Address,
        slots: ContractStoreDeltas,
        balance: Option<Balance>,
        code: Option<Code>,
        change: ChangeType,
    ) -> Self {
        if code.is_none() && matches!(change, ChangeType::Creation) {
            warn!(?address, "Instantiated AccountDelta without code marked as creation!")
        }
        Self { chain, address, slots, balance, code, change }
    }

    pub fn contract_id(&self) -> ContractId {
        ContractId::new(self.chain, self.address.clone())
    }

    pub fn into_account(self, tx: &Transaction) -> Account {
        let empty_hash = keccak256(Vec::new());
        Account::new(
            self.chain,
            self.address.clone(),
            format!("{:#020x}", self.address),
            self.slots
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default()))
                .collect(),
            self.balance.unwrap_or_default(),
            // token balances are not set in the delta
            HashMap::new(),
            self.code.clone().unwrap_or_default(),
            self.code
                .as_ref()
                .map(keccak256)
                .unwrap_or(empty_hash)
                .into(),
            tx.hash.clone(),
            tx.hash.clone(),
            Some(tx.hash.clone()),
        )
    }

    /// Convert the delta into an account. Note that data not present in the delta, such as
    /// creation_tx etc, will be initialized to default values.
    pub fn into_account_without_tx(self) -> Account {
        let empty_hash = keccak256(Vec::new());
        Account::new(
            self.chain,
            self.address.clone(),
            format!("{:#020x}", self.address),
            self.slots
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default()))
                .collect(),
            self.balance.unwrap_or_default(),
            // token balances are not set in the delta
            HashMap::new(),
            self.code.clone().unwrap_or_default(),
            self.code
                .as_ref()
                .map(keccak256)
                .unwrap_or(empty_hash)
                .into(),
            Bytes::from("0x00"),
            Bytes::from("0x00"),
            None,
        )
    }

    // Convert AccountUpdate into Account using references.
    pub fn ref_into_account(&self, tx: &Transaction) -> Account {
        let empty_hash = keccak256(Vec::new());
        if self.change != ChangeType::Creation {
            warn!("Creating an account from a partial change!")
        }

        Account::new(
            self.chain,
            self.address.clone(),
            format!("{:#020x}", self.address),
            self.slots
                .clone()
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default()))
                .collect(),
            self.balance.clone().unwrap_or_default(),
            // token balances are not set in the delta
            HashMap::new(),
            self.code.clone().unwrap_or_default(),
            self.code
                .as_ref()
                .map(keccak256)
                .unwrap_or(empty_hash)
                .into(),
            tx.hash.clone(),
            tx.hash.clone(),
            Some(tx.hash.clone()),
        )
    }

    /// Merge this update (`self`) with another one (`other`)
    ///
    /// This function is utilized for aggregating multiple updates into a single
    /// update. The attribute values of `other` are set on `self`.
    /// Meanwhile, contract storage maps are merged, with keys from `other` taking precedence.
    ///
    /// Be noted that, this function will mutate the state of the calling
    /// struct. An error will occur if merging updates from different accounts.
    ///
    /// There are no further validation checks within this method, hence it
    /// could be used as needed.
    ///
    /// # Errors
    ///
    /// It returns an `CoreError::MergeError` error if `self.address` and
    /// `other.address` are not identical.
    ///
    /// # Arguments
    ///
    /// * `other`: An instance of `AccountUpdate`. The attribute values and keys of `other` will
    ///   overwrite those of `self`.
    pub fn merge(&mut self, other: AccountDelta) -> Result<(), MergeError> {
        if self.address != other.address {
            return Err(MergeError::IdMismatch(
                "AccountDelta".to_string(),
                format!("{:#020x}", self.address),
                format!("{:#020x}", other.address),
            ));
        }

        self.slots.extend(other.slots);

        if let Some(balance) = other.balance {
            self.balance = Some(balance)
        }
        self.code = other.code.or(self.code.take());

        if self.code.is_none() && matches!(self.change, ChangeType::Creation) {
            warn!(address=?self.address, "AccountDelta without code marked as creation after merge!")
        }

        Ok(())
    }

    pub fn is_update(&self) -> bool {
        self.change == ChangeType::Update
    }

    pub fn is_creation(&self) -> bool {
        self.change == ChangeType::Creation
    }

    pub fn change_type(&self) -> ChangeType {
        self.change
    }

    pub fn code(&self) -> &Option<Code> {
        &self.code
    }

    pub fn set_code(&mut self, code: Bytes) {
        self.code = Some(code)
    }
}

impl From<Account> for AccountDelta {
    fn from(value: Account) -> Self {
        Self::new(
            value.chain,
            value.address,
            value
                .slots
                .into_iter()
                .map(|(k, v)| (k, Some(v)))
                .collect(),
            Some(value.native_balance),
            Some(value.code),
            ChangeType::Creation,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, DeepSizeOf)]
pub struct AccountBalance {
    pub account: Address,
    pub token: Address,
    pub balance: Balance,
    pub modify_tx: TxHash,
}

impl AccountBalance {
    pub fn new(account: Address, token: Address, balance: Balance, modify_tx: TxHash) -> Self {
        Self { account, token, balance, modify_tx }
    }
}

#[derive(Debug, PartialEq, Clone, DeepSizeOf)]
pub struct ContractStorageChange {
    pub value: Bytes,
    pub previous: Bytes,
}

impl ContractStorageChange {
    pub fn new(value: impl Into<Bytes>, previous: impl Into<Bytes>) -> Self {
        Self { value: value.into(), previous: previous.into() }
    }

    pub fn initial(value: impl Into<Bytes>) -> Self {
        Self { value: value.into(), previous: Bytes::default() }
    }
}

#[derive(Debug, PartialEq, Default, Clone, DeepSizeOf)]
pub struct ContractChanges {
    pub account: Address,
    pub slots: HashMap<StoreKey, ContractStorageChange>,
    pub native_balance: Option<Balance>,
}

impl ContractChanges {
    pub fn new(
        account: Address,
        slots: HashMap<StoreKey, ContractStorageChange>,
        native_balance: Option<Balance>,
    ) -> Self {
        Self { account, slots, native_balance }
    }
}

/// Multiple binary key-value stores grouped by account address.
pub type AccountToContractChanges = HashMap<Address, ContractChanges>;

impl From<dto::AccountBalance> for AccountBalance {
    fn from(value: dto::AccountBalance) -> Self {
        Self {
            token: value.token,
            balance: value.balance,
            modify_tx: value.modify_tx,
            account: value.account,
        }
    }
}

impl From<dto::AccountUpdate> for AccountDelta {
    fn from(value: dto::AccountUpdate) -> Self {
        // Client receives zero-value writes, not explicit deletions via WS,
        // so all slot values are treated as Some(v).
        AccountDelta::new(
            value.chain.into(),
            value.address,
            value
                .slots
                .into_iter()
                .map(|(k, v)| (k, Some(v)))
                .collect(),
            value.balance,
            value.code,
            value.change.into(),
        )
    }
}

#[allow(deprecated)] // creation_tx field on ResponseAccount is deprecated
impl From<dto::ResponseAccount> for Account {
    fn from(value: dto::ResponseAccount) -> Self {
        // Snapshot responses don't carry per-balance modify_tx; use zero as placeholder.
        let token_balances = value
            .token_balances
            .into_iter()
            .map(|(token, balance)| {
                let acct = value.address.clone();
                (
                    token.clone(),
                    AccountBalance {
                        token,
                        balance,
                        modify_tx: crate::Bytes::zero(32),
                        account: acct,
                    },
                )
            })
            .collect();
        Account::new(
            value.chain.into(),
            value.address,
            value.title,
            value.slots,
            value.native_balance,
            token_balances,
            value.code,
            value.code_hash,
            value.balance_modify_tx,
            value.code_modify_tx,
            value.creation_tx,
        )
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use super::*;

    fn update_balance_delta() -> AccountDelta {
        AccountDelta::new(
            Chain::Ethereum,
            Bytes::from_str("e688b84b23f322a994A53dbF8E15FA82CDB71127").unwrap(),
            HashMap::new(),
            Some(Bytes::from(420u64).lpad(32, 0)),
            None,
            ChangeType::Update,
        )
    }

    fn update_slots_delta() -> AccountDelta {
        AccountDelta::new(
            Chain::Ethereum,
            Bytes::from_str("e688b84b23f322a994A53dbF8E15FA82CDB71127").unwrap(),
            slots([(0, 1), (1, 2)]),
            None,
            None,
            ChangeType::Update,
        )
    }

    // Utils function that return slots that match `AccountDelta` slots.
    // TODO: this is temporary, we shoud make AccountDelta.slots use Bytes instead of Option<Bytes>
    pub fn slots(data: impl IntoIterator<Item = (u64, u64)>) -> HashMap<Bytes, Option<Bytes>> {
        data.into_iter()
            .map(|(s, v)| (Bytes::from(s).lpad(32, 0), Some(Bytes::from(v).lpad(32, 0))))
            .collect()
    }

    #[test]
    fn test_merge_account_deltas() {
        let mut update_left = update_balance_delta();
        let update_right = update_slots_delta();
        let mut exp = update_slots_delta();
        exp.balance = Some(Bytes::from(420u64).lpad(32, 0));

        update_left.merge(update_right).unwrap();

        assert_eq!(update_left, exp);
    }

    #[test]
    fn test_merge_account_delta_wrong_address() {
        let mut update_left = update_balance_delta();
        let mut update_right = update_slots_delta();
        update_right.address = Bytes::zero(20);
        let exp = Err(MergeError::IdMismatch(
            "AccountDelta".to_string(),
            format!("{:#020x}", update_left.address),
            format!("{:#020x}", update_right.address),
        ));

        let res = update_left.merge(update_right);

        assert_eq!(res, exp);
    }

    #[test]
    fn test_account_from_delta_ref_into_account() {
        let code = vec![0, 0, 0, 0];
        let code_hash = Bytes::from(keccak256(&code));
        let tx = Transaction::new(
            Bytes::zero(32),
            Bytes::zero(32),
            Bytes::zero(20),
            Some(Bytes::zero(20)),
            10,
        );

        let delta = AccountDelta::new(
            Chain::Ethereum,
            Bytes::from_str("e688b84b23f322a994A53dbF8E15FA82CDB71127").unwrap(),
            HashMap::new(),
            Some(Bytes::from(10000u64).lpad(32, 0)),
            Some(code.clone().into()),
            ChangeType::Update,
        );

        let expected = Account::new(
            Chain::Ethereum,
            "0xe688b84b23f322a994A53dbF8E15FA82CDB71127"
                .parse()
                .unwrap(),
            "0xe688b84b23f322a994a53dbf8e15fa82cdb71127".into(),
            HashMap::new(),
            Bytes::from(10000u64).lpad(32, 0),
            HashMap::new(),
            code.into(),
            code_hash,
            Bytes::zero(32),
            Bytes::zero(32),
            Some(Bytes::zero(32)),
        );

        assert_eq!(delta.ref_into_account(&tx), expected);
    }
}
