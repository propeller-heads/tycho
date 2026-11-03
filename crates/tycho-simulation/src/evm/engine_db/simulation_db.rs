use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use alloy::{
    primitives::{Address, Bytes as AlloyBytes, StorageValue, B256, U256},
    providers::{
        fillers::{BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller},
        Provider, RootProvider,
    },
    transports::{RpcError, TransportErrorKind},
};
use revm::{
    context::DBErrorMarker,
    state::{AccountInfo, Bytecode},
    DatabaseRef,
};
use thiserror::Error;
use tracing::{debug, info};
use tycho_client::feed::BlockHeader;

use super::{
    super::account_storage::{AccountStorage, StateUpdate},
    engine_db_interface::EngineDatabaseInterface,
};

/// A wrapper over an actual SimulationDB that allows overriding specific storage slots
/// and native balances
pub struct OverriddenSimulationDB<'a, DB: DatabaseRef> {
    /// Wrapped database. Will be queried if a requested item is not found in the overrides.
    pub inner_db: &'a DB,
    /// A mapping from account address to storage.
    /// Storage is a mapping from slot index to slot value.
    pub overrides: &'a HashMap<Address, HashMap<U256, U256>>,
    /// A mapping from account address to its overridden native balance.
    /// An override for an account absent from the inner database materializes it as an
    /// empty account holding that balance.
    pub native_balance_overrides: &'a HashMap<Address, U256>,
}

impl<'a, DB: DatabaseRef> OverriddenSimulationDB<'a, DB> {
    /// Creates a new OverriddenSimulationDB.
    pub fn new(
        inner_db: &'a DB,
        overrides: &'a HashMap<Address, HashMap<U256, U256>>,
        native_balance_overrides: &'a HashMap<Address, U256>,
    ) -> Self {
        OverriddenSimulationDB { inner_db, overrides, native_balance_overrides }
    }
}

impl<DB: DatabaseRef> DatabaseRef for OverriddenSimulationDB<'_, DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(balance) = self
            .native_balance_overrides
            .get(&address)
        else {
            return self.inner_db.basic_ref(address);
        };
        let info = self
            .inner_db
            .basic_ref(address)
            .ok()
            .flatten();
        Ok(Some(AccountInfo { balance: *balance, ..info.unwrap_or_default() }))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner_db
            .code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self.overrides.get(&address) {
            None => self
                .inner_db
                .storage_ref(address, index),
            Some(slot_overrides) => match slot_overrides.get(&index) {
                Some(value) => {
                    debug!(%address, %index, %value, "Requested storage of account {:x?} slot {}", address, index);
                    Ok(*value)
                }
                None => self
                    .inner_db
                    .storage_ref(address, index),
            },
        }
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner_db.block_hash_ref(number)
    }
}

/// A wrapper over an Alloy Provider with local storage cache and overrides.
#[derive(Clone, Debug)]
pub struct SimulationDB<P: Provider + Debug> {
    /// Client to connect to the RPC
    client: Arc<P>,
    /// Cached data
    account_storage: Arc<RwLock<AccountStorage>>,
    /// Current block
    block: Option<BlockHeader>,
    /// Tokio runtime to execute async code
    pub runtime: Option<Arc<tokio::runtime::Runtime>>,
}

pub type EVMProvider = FillProvider<
    JoinFill<
        alloy::providers::Identity,
        JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
    >,
    RootProvider,
>;

impl<P: Provider + Debug + 'static> SimulationDB<P> {
    pub fn new(
        client: Arc<P>,
        runtime: Option<Arc<tokio::runtime::Runtime>>,
        block: Option<BlockHeader>,
    ) -> Self {
        Self {
            client,
            account_storage: Arc::new(RwLock::new(AccountStorage::new())),
            block,
            runtime,
        }
    }

    /// Set the block that will be used when querying a node
    pub fn set_block(&mut self, block: Option<BlockHeader>) {
        self.block = block;
    }

    /// Update the simulation state.
    ///
    /// Updates the underlying smart contract storage. Any previously missed account,
    /// which was queried and whose state now is in the account_storage will be cleared.
    ///
    /// # Arguments
    ///
    /// * `updates` - Values for the updates that should be applied to the accounts
    /// * `block` - The newest block
    ///
    /// Returns a state update struct to revert this update.
    pub fn update_state(
        &mut self,
        updates: &HashMap<Address, StateUpdate>,
        block: BlockHeader,
    ) -> Result<HashMap<Address, StateUpdate>, SimulationDBError> {
        info!("Received account state update.");
        let mut revert_updates = HashMap::new();
        self.block = Some(block);
        for (address, update_info) in updates.iter() {
            let mut revert_entry = StateUpdate::default();
            if let Some(current_account) = self
                .read_account_storage()?
                .get_account_info(address)
            {
                revert_entry.balance = Some(current_account.balance);
            }
            if let Some(storage_updates) = update_info.storage.as_ref() {
                let mut revert_storage = HashMap::default();
                for index in storage_updates.keys() {
                    if let Some(s) = self
                        .read_account_storage()?
                        .get_permanent_storage(address, index)
                    {
                        revert_storage.insert(*index, s);
                    }
                }
                revert_entry.storage = Some(revert_storage);
            }
            revert_updates.insert(*address, revert_entry);

            self.write_account_storage()?
                .update_account(address, update_info);
        }
        Ok(revert_updates)
    }

    /// Query information about an Ethereum account.
    /// Gets account information not including storage.
    ///
    /// # Arguments
    ///
    /// * `address` - The Ethereum address to query.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing either an `AccountInfo` object with balance, nonce, and code
    /// information, or an error of type `SimulationDB<M>::Error` if the query fails.
    fn query_account_info(
        &self,
        address: Address,
    ) -> Result<AccountInfo, <SimulationDB<P> as DatabaseRef>::Error> {
        debug!("Querying account info of {:x?} at block {:?}", address, self.block);

        let (balance, nonce, code) = self.block_on(async {
            let mut balance_request = self.client.get_balance(address);
            let mut nonce_request = self
                .client
                .get_transaction_count(address);
            let mut code_request = self.client.get_code_at(address);

            if let Some(block) = &self.block {
                balance_request = balance_request.number(block.number);
                nonce_request = nonce_request.number(block.number);
                code_request = code_request.number(block.number);
            }

            tokio::join!(balance_request, nonce_request, code_request,)
        });
        let code = Bytecode::new_raw(AlloyBytes::copy_from_slice(&code?));

        Ok(AccountInfo::new(balance?, nonce?, code.hash_slow(), code))
    }

    /// Queries a value from storage at the specified index for a given Ethereum account.
    ///
    /// # Arguments
    ///
    /// * `address` - The Ethereum address of the account.
    /// * `index` - The index of the storage value to query.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing the value from storage at the specified index as an `U256`,
    /// or an error of type `SimulationDB<M>::Error` if the query fails.
    pub fn query_storage(
        &self,
        address: Address,
        index: U256,
    ) -> Result<StorageValue, <SimulationDB<P> as DatabaseRef>::Error> {
        let mut request = self
            .client
            .get_storage_at(address, index);
        if let Some(block) = &self.block {
            request = request.number(block.number);
        }

        let storage_future = async move {
            request.await.map_err(|err| {
                SimulationDBError::SimulationError(format!(
                    "Failed to fetch storage for {address:?} slot {index}: {err}"
                ))
            })
        };

        self.block_on(storage_future)
    }

    fn read_account_storage(
        &self,
    ) -> Result<RwLockReadGuard<'_, AccountStorage>, SimulationDBError> {
        self.account_storage
            .read()
            .map_err(|_| SimulationDBError::Internal("Account storage read lock poisoned".into()))
    }

    fn write_account_storage(
        &self,
    ) -> Result<RwLockWriteGuard<'_, AccountStorage>, SimulationDBError> {
        self.account_storage
            .write()
            .map_err(|_| SimulationDBError::Internal("Account storage write lock poisoned".into()))
    }

    fn block_on<F: core::future::Future>(&self, f: F) -> F::Output {
        // If we get here and have to block the current thread, we really
        // messed up indexing / filling the storage. In that case this will save us
        // at the price of a very high time penalty.
        match &self.runtime {
            Some(runtime) => runtime.block_on(f),
            None => futures::executor::block_on(f),
        }
    }
}

impl<P: Provider + Debug> EngineDatabaseInterface for SimulationDB<P>
where
    P: Provider + Send + Sync + 'static,
{
    type Error = SimulationDBError;

    /// Sets up a single account
    ///
    /// Full control over setting up an accounts. Allows to set up EOAs as
    /// well as smart contracts.
    ///
    /// # Arguments
    ///
    /// * `address` - Address of the account
    /// * `account` - The account information
    /// * `permanent_storage` - Storage to init the account with this storage can only be updated
    ///   manually.
    /// * `mocked` - Whether this account should be considered mocked. For mocked accounts, nothing
    ///   is downloaded from a node; all data must be inserted manually.
    fn init_account(
        &self,
        address: Address,
        mut account: AccountInfo,
        permanent_storage: Option<HashMap<U256, U256>>,
        mocked: bool,
    ) -> Result<(), <Self as EngineDatabaseInterface>::Error> {
        if let Some(code) = account.code.clone() {
            account.code = Some(code);
        }

        self.write_account_storage()?
            .init_account(address, account, permanent_storage, mocked);

        Ok(())
    }

    /// Clears temp storage
    ///
    /// It is recommended to call this after a new block is received,
    /// to avoid stored state leading to wrong results.
    fn clear_temp_storage(&mut self) -> Result<(), <Self as EngineDatabaseInterface>::Error> {
        self.write_account_storage()?
            .clear_temp_storage();

        Ok(())
    }

    fn get_current_block(&self) -> Option<BlockHeader> {
        self.block.clone()
    }
}

#[derive(Error, Debug)]
pub enum SimulationDBError {
    #[error("Simulation error: {0} ")]
    SimulationError(String),
    #[error("Not implemented error: {0}")]
    NotImplementedError(String),
    #[error("Simulation DB internal error: {0}")]
    Internal(String),
}

impl DBErrorMarker for SimulationDBError {}

impl From<RpcError<TransportErrorKind>> for SimulationDBError {
    fn from(err: RpcError<TransportErrorKind>) -> Self {
        SimulationDBError::SimulationError(err.to_string())
    }
}

impl<P: Provider> DatabaseRef for SimulationDB<P>
where
    P: Provider + Debug + Send + Sync + 'static,
{
    type Error = SimulationDBError;

    /// Retrieves basic information about an account.
    ///
    /// This function retrieves the basic account information for the specified address.
    /// If the account is present in the storage, the stored account information is returned.
    /// If the account is not present in the storage, the function queries the account information
    /// from the contract and initializes the account in the storage with the retrieved
    /// information.
    ///
    /// # Arguments
    ///
    /// * `address`: The address of the account to retrieve the information for.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing an `Option` that holds the account information if it exists.
    /// If the account is not found, `None` is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if there was an issue querying the account information from the contract or
    /// accessing the storage.
    ///
    /// # Notes
    ///
    /// * If the account is present in the storage, the function returns a clone of the stored
    ///   account information.
    ///
    /// * If the account is not present in the storage, the function queries the account information
    ///   from the contract, initializes the account in the storage with the retrieved information,
    ///   and returns a clone of the account information.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = {
            self.read_account_storage()?
                .get_account_info(&address)
                .cloned()
        } {
            return Ok(Some(account));
        }
        let account_info = self.query_account_info(address)?;
        self.init_account(address, account_info.clone(), None, false)?;
        Ok(Some(account_info))
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        Err(SimulationDBError::NotImplementedError(
            "Code by hash is not implemented in SimulationDB".to_string(),
        ))
    }

    /// Retrieves the storage value at the specified address and index.
    ///
    /// If we don't know the value, and the accessed contract is mocked, the function returns
    /// an empty slot instead of querying a node, to avoid potentially returning garbage values.
    ///
    /// # Arguments
    ///
    /// * `address`: The address of the contract to retrieve the storage value from.
    /// * `index`: The index of the storage value to retrieve.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing the storage value if it exists. If the contract is mocked
    /// and the storage value is not found locally, an empty slot is returned as `U256::ZERO`.
    ///
    /// # Errors
    ///
    /// Returns an error if there was an issue querying the storage value from the contract or
    /// accessing the storage.
    ///
    /// # Notes
    ///
    /// * If the contract is present locally and is mocked, the function first checks if the storage
    ///   value exists locally. If found, it returns the stored value. If not found, it returns an
    ///   empty slot. Mocked contracts are not expected to have valid storage values, so the
    ///   function does not query a node in this case.
    ///
    /// * If the contract is present locally and is not mocked, the function checks if the storage
    ///   value exists locally. If found, it returns the stored value. If not found, it queries the
    ///   storage value from a node, stores it locally, and returns it.
    ///
    /// * If the contract is not present locally, the function queries the account info and storage
    ///   value from a node, initializes the account locally with the retrieved information, and
    ///   returns the storage value.
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        debug!("Requested storage of account {:x?} slot {}", address, index);
        let (is_mocked, local_value) = {
            let account_storage = self.read_account_storage()?;
            (
                account_storage.is_mocked_account(&address),
                account_storage.get_storage(&address, &index),
            )
        };

        if let Some(storage_value) = local_value {
            debug!(
                "Got value locally. This is a {} account. Value: {}",
                if is_mocked.unwrap_or(false) { "mocked" } else { "non-mocked" },
                storage_value
            );
            return Ok(storage_value);
        }

        // At this point we know we don't have data for this storage slot.
        match is_mocked {
            Some(true) => {
                debug!("This is a mocked account for which we don't have data. Returning zero.");
                Ok(U256::ZERO)
            }
            Some(false) => {
                let storage_value = self.query_storage(address, index)?;
                self.write_account_storage()?
                    .set_temp_storage(address, index, storage_value);
                debug!(
                    "This is a non-mocked account for which we didn't have data. Fetched value: {}",
                    storage_value
                );
                Ok(storage_value)
            }
            None => {
                let account_info = self.query_account_info(address)?;
                let storage_value = self.query_storage(address, index)?;
                self.init_account(address, account_info, None, false)?;
                self.write_account_storage()?
                    .set_temp_storage(address, index, storage_value);
                debug!("This is non-initialised account. Fetched value: {}", storage_value);
                Ok(storage_value)
            }
        }
    }

    /// If block header is set, returns the hash. Otherwise returns a zero hash
    /// instead of querying a node.
    fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
        match &self.block {
            Some(header) => Ok(B256::from_slice(&header.hash)),
            None => Ok(B256::ZERO),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, str::FromStr};

    use alloy::primitives::U160;
    use rstest::rstest;
    use tycho_common::Bytes;

    use super::*;
    use crate::evm::engine_db::utils::{get_client, get_runtime};

    #[rstest]
    fn test_query_storage_latest_block() -> Result<(), Box<dyn Error>> {
        let db = SimulationDB::new(
            get_client(None).expect("Failed to create test client"),
            get_runtime().expect("Failed to create test runtime"),
            None,
        );
        let address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")?;
        let index = U256::from_limbs_slice(&[8]);
        db.init_account(address, AccountInfo::default(), None, false)
            .expect("Failed to init account");

        db.query_storage(address, index)
            .unwrap();

        // There is no assertion, but has the querying failed, we would have panicked by now.
        // This test is not deterministic as it depends on the current state of the blockchain.
        // See the next test where we do this for a specific block.
        Ok(())
    }

    #[rstest]
    fn test_query_account_info() {
        let mut db = SimulationDB::new(
            get_client(None).expect("Failed to create test client"),
            get_runtime().expect("Failed to create test runtime"),
            None,
        );
        let block = BlockHeader {
            number: 20308186,
            hash: Bytes::from_str(
                "0x61c51e3640b02ae58a03201be0271e84e02dac8a4826501995cbe4da24174b52",
            )
            .unwrap(),
            timestamp: 234,
            ..Default::default()
        };
        db.set_block(Some(block));
        let address = Address::from_str("0x168b93113fe5902c87afaecE348581A1481d0f93").unwrap();
        db.init_account(address, AccountInfo::default(), None, false)
            .expect("Failed to init account");

        let account_info = db.query_account_info(address).unwrap();

        assert_eq!(account_info.balance, U256::from_str("6246978663692389").unwrap());
        assert_eq!(account_info.nonce, 17);
    }

    #[rstest]
    fn test_mock_account_get_acc_info() {
        let db = SimulationDB::new(
            get_client(None).expect("Failed to create test client"),
            get_runtime().expect("Failed to create test runtime"),
            None,
        );
        let mock_acc_address =
            Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc").unwrap();
        db.init_account(mock_acc_address, AccountInfo::default(), None, true)
            .expect("Failed to init account");

        let acc_info = db
            .basic_ref(mock_acc_address)
            .unwrap()
            .unwrap();

        assert_eq!(
            db.account_storage
                .read()
                .unwrap()
                .get_account_info(&mock_acc_address)
                .unwrap(),
            &acc_info
        );
    }

    #[rstest]
    fn test_mock_account_get_storage() {
        let db = SimulationDB::new(
            get_client(None).expect("Failed to create test client"),
            get_runtime().expect("Failed to create test runtime"),
            None,
        );
        let mock_acc_address =
            Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc").unwrap();
        let storage_address = U256::ZERO;
        db.init_account(mock_acc_address, AccountInfo::default(), None, true)
            .expect("Failed to init account");

        let storage = db
            .storage_ref(mock_acc_address, storage_address)
            .unwrap();

        assert_eq!(storage, U256::ZERO);
    }

    #[rstest]
    fn test_update_state() {
        let mut db = SimulationDB::new(
            get_client(None).expect("Failed to create test client"),
            get_runtime().expect("Failed to create test runtime"),
            None,
        );
        let address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc").unwrap();
        db.init_account(address, AccountInfo::default(), None, false)
            .expect("Failed to init account");

        let mut new_storage = HashMap::default();
        let new_storage_value_index = U256::from_limbs_slice(&[123]);
        new_storage.insert(new_storage_value_index, new_storage_value_index);
        let new_balance = U256::from_limbs_slice(&[500]);
        let update = StateUpdate { storage: Some(new_storage), balance: Some(new_balance) };
        let mut updates = HashMap::default();
        updates.insert(address, update);
        let new_block = BlockHeader { number: 1, timestamp: 234, ..Default::default() };

        let reverse_update = db
            .update_state(&updates, new_block)
            .expect("State update should succeed");

        assert_eq!(
            db.account_storage
                .read()
                .expect("Storage entry should exist")
                .get_storage(&address, &new_storage_value_index)
                .unwrap(),
            new_storage_value_index
        );
        assert_eq!(
            db.account_storage
                .read()
                .unwrap()
                .get_account_info(&address)
                .unwrap()
                .balance,
            new_balance
        );
        assert_eq!(db.block.unwrap().number, 1);

        assert_eq!(
            reverse_update
                .get(&address)
                .unwrap()
                .balance
                .unwrap(),
            AccountInfo::default().balance
        );
        assert_eq!(
            reverse_update
                .get(&address)
                .unwrap()
                .storage,
            Some(HashMap::default())
        );
    }

    #[rstest]
    fn test_overridden_db() {
        let db = SimulationDB::new(
            get_client(None).expect("Failed to create test client"),
            get_runtime().expect("Failed to create test runtime"),
            None,
        );
        let slot1 = U256::from_limbs_slice(&[1]);
        let slot2 = U256::from_limbs_slice(&[2]);
        let orig_value1 = U256::from_limbs_slice(&[100]);
        let orig_value2 = U256::from_limbs_slice(&[200]);
        let original_storage: HashMap<U256, U256> = [(slot1, orig_value1), (slot2, orig_value2)]
            .iter()
            .cloned()
            .collect();

        let address1 = Address::from(U160::from(1));
        let address2 = Address::from(U160::from(2));
        let address3 = Address::from(U160::from(3));

        // override slot 1 of address 2
        // and slot 1 of address 3 which doesn't exist in the original DB
        db.init_account(address1, AccountInfo::default(), Some(original_storage.clone()), false)
            .expect("Failed to init account");
        db.init_account(address2, AccountInfo::default(), Some(original_storage), false)
            .expect("Failed to init account");

        let overridden_value1 = U256::from_limbs_slice(&[101]);
        let mut overrides: HashMap<Address, HashMap<U256, U256>> = HashMap::new();
        overrides.insert(
            address2,
            [(slot1, overridden_value1)]
                .iter()
                .cloned()
                .collect(),
        );
        overrides.insert(
            address3,
            [(slot1, overridden_value1)]
                .iter()
                .cloned()
                .collect(),
        );

        let native_balance_overrides = HashMap::new();
        let overriden_db = OverriddenSimulationDB::new(&db, &overrides, &native_balance_overrides);

        assert_eq!(
            overriden_db
                .storage_ref(address1, slot1)
                .expect("Value should be available"),
            orig_value1,
            "Slots of non-overridden account should hold original values."
        );

        assert_eq!(
            overriden_db
                .storage_ref(address1, slot2)
                .expect("Value should be available"),
            orig_value2,
            "Slots of non-overridden account should hold original values."
        );

        assert_eq!(
            overriden_db
                .storage_ref(address2, slot1)
                .expect("Value should be available"),
            overridden_value1,
            "Overridden slot of overridden account should hold an overridden value."
        );

        assert_eq!(
            overriden_db
                .storage_ref(address2, slot2)
                .expect("Value should be available"),
            orig_value2,
            "Non-overridden slot of an account with other slots overridden \
            should hold an original value."
        );

        assert_eq!(
            overriden_db
                .storage_ref(address3, slot1)
                .expect("Value should be available"),
            overridden_value1,
            "Overridden slot of an overridden non-existent account should hold an overriden value."
        );
    }

    #[derive(Debug, Default)]
    struct StaticDB {
        accounts: HashMap<Address, AccountInfo>,
        fail_basic_ref: bool,
    }

    impl DatabaseRef for StaticDB {
        type Error = SimulationDBError;

        fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            if self.fail_basic_ref {
                return Err(SimulationDBError::Internal("account unavailable".to_string()));
            }
            Ok(self.accounts.get(&address).cloned())
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Ok(U256::ZERO)
        }

        fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[rstest]
    fn test_balance_override_known_account() {
        let address = Address::from(U160::from(1));
        let bytecode = Bytecode::new_raw(AlloyBytes::from_static(&[0x00]));
        let account = AccountInfo::new(U256::from(100), 7, bytecode.hash_slow(), bytecode.clone());
        let db = StaticDB { accounts: HashMap::from([(address, account)]), ..Default::default() };
        let overrides = HashMap::new();
        let native_balance_overrides = HashMap::from([(address, U256::from(999))]);
        let overridden_db = OverriddenSimulationDB::new(&db, &overrides, &native_balance_overrides);

        let info = overridden_db
            .basic_ref(address)
            .expect("basic_ref should succeed")
            .expect("account should exist");

        assert_eq!(info.balance, U256::from(999), "Overridden balance should be returned.");
        assert_eq!(info.nonce, 7, "Other account fields should be preserved.");
        assert_eq!(info.code, Some(bytecode), "Contract code should be preserved.");
    }

    #[rstest]
    fn test_balance_override_unknown_account() {
        let address = Address::from(U160::from(1));
        let db = StaticDB::default();
        let overrides = HashMap::new();
        let native_balance_overrides = HashMap::from([(address, U256::from(999))]);
        let overridden_db = OverriddenSimulationDB::new(&db, &overrides, &native_balance_overrides);

        let info = overridden_db
            .basic_ref(address)
            .expect("basic_ref should succeed")
            .expect("overridden unknown account should be synthesized");

        assert_eq!(
            info,
            AccountInfo { balance: U256::from(999), ..Default::default() },
            "Unknown account should be synthesized as an EOA with the overridden balance."
        );
    }

    #[rstest]
    fn test_balance_override_materializes_account_when_inner_db_errors() {
        let address = Address::from(U160::from(1));
        let db = StaticDB { fail_basic_ref: true, ..Default::default() };
        let overrides = HashMap::new();
        let native_balance_overrides = HashMap::from([(address, U256::from(999))]);
        let overridden_db = OverriddenSimulationDB::new(&db, &overrides, &native_balance_overrides);

        let info = overridden_db
            .basic_ref(address)
            .expect("balance override should shield an inner account miss")
            .expect("overridden account should be synthesized");

        assert_eq!(info, AccountInfo { balance: U256::from(999), ..Default::default() });
    }

    #[rstest]
    fn test_missing_balance_override_propagates_inner_error() {
        let address = Address::from(U160::from(1));
        let db = StaticDB { fail_basic_ref: true, ..Default::default() };
        let overrides = HashMap::new();
        let native_balance_overrides = HashMap::new();
        let overridden_db = OverriddenSimulationDB::new(&db, &overrides, &native_balance_overrides);

        let result = overridden_db.basic_ref(address);

        match result {
            Err(SimulationDBError::Internal(message)) => {
                assert_eq!(message, "account unavailable")
            }
            other => panic!("expected inner database error, got {other:?}"),
        }
    }

    #[rstest]
    fn test_balance_override_absent_passthrough() {
        let address = Address::from(U160::from(1));
        let unknown_address = Address::from(U160::from(2));
        let account = AccountInfo { balance: U256::from(100), nonce: 7, ..Default::default() };
        let db = StaticDB {
            accounts: HashMap::from([(address, account.clone())]),
            ..Default::default()
        };
        let overrides = HashMap::new();
        let native_balance_overrides = HashMap::new();
        let overridden_db = OverriddenSimulationDB::new(&db, &overrides, &native_balance_overrides);

        assert_eq!(
            overridden_db
                .basic_ref(address)
                .expect("basic_ref should succeed"),
            Some(account),
            "Known account without override should pass through unchanged."
        );
        assert_eq!(
            overridden_db
                .basic_ref(unknown_address)
                .expect("basic_ref should succeed"),
            None,
            "Unknown account without override should stay absent."
        );
    }
}
