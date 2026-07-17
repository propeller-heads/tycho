use std::{
    collections::HashMap,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use alloy::primitives::{Address, Bytes as AlloyBytes, B256, U256};
use revm::{
    context::DBErrorMarker,
    primitives::KECCAK_EMPTY,
    state::{AccountInfo, Bytecode},
    DatabaseRef,
};
use thiserror::Error;
use tracing::{debug, error, instrument, warn};
use tycho_client::feed::BlockHeader;

use crate::evm::{
    account_storage::{AccountStorage, StateUpdate},
    engine_db::engine_db_interface::EngineDatabaseInterface,
    tycho_models::{AccountUpdate, ChangeType},
};

#[derive(Error, Debug)]
pub enum TychoClientError {
    #[error("Failed to parse URI: {0}. Error: {1}")]
    UriParsing(String, String),
    #[error("Failed to format request: {0}")]
    FormatRequest(String),
    #[error("Unexpected HTTP client error: {0}")]
    HttpClient(String),
    #[error("Failed to parse response: {0}")]
    ParseResponse(String),
}

#[derive(Error, Debug)]
pub enum PreCachedDBError {
    #[error("Account {0} not found")]
    MissingAccount(Address),
    #[error("Bad account update: {0} - {1:?}")]
    BadUpdate(String, Box<AccountUpdate>),
    #[error("Block needs to be set")]
    BlockNotSet(),
    #[error("Tycho Client error: {0}")]
    TychoClientError(#[from] TychoClientError),
    #[error("{0}")]
    Fatal(String),
}

impl DBErrorMarker for PreCachedDBError {}

#[derive(Clone, Debug)]
pub struct PreCachedDBInner {
    /// Storage for accounts
    accounts: AccountStorage,
    /// Current block
    block: Option<BlockHeader>,
}

#[derive(Clone, Debug)]
pub struct PreCachedDB {
    /// Cached inner data
    ///
    /// `inner` encapsulates `PreCachedDBInner` using `RwLock` for safe concurrent read or
    /// exclusive write access to the data and `Arc` for shared ownership of the lock across
    /// threads.
    pub inner: Arc<RwLock<PreCachedDBInner>>,
}

impl PreCachedDB {
    /// Create a new PreCachedDB instance
    pub fn new() -> Result<Self, PreCachedDBError> {
        Ok(PreCachedDB {
            inner: Arc::new(RwLock::new(PreCachedDBInner {
                accounts: AccountStorage::new(),
                block: None,
            })),
        })
    }

    /// Applies account changes for one block and advances the stored block header.
    ///
    /// Semantics per [`ChangeType`]:
    /// - `Creation`: an authoritative full-account snapshot (code, native balance, complete slot
    ///   map) that replaces any existing entry. Consumers re-deliver snapshots for already-known
    ///   contracts when healing after a disconnect; the snapshot must win, otherwise the account
    ///   keeps pre-outage storage whose changes were never delivered as deltas.
    /// - `Update`: per-slot storage and balance deltas merged into the existing account.
    /// - `Deletion`: not implemented.
    #[instrument(skip_all)]
    pub fn update(
        &self,
        account_updates: Vec<AccountUpdate>,
        block: Option<BlockHeader>,
    ) -> Result<(), PreCachedDBError> {
        // Hold the write lock for the duration of the function so that no other thread can
        // write to the storage.
        let mut write_guard = self.write_inner()?;

        write_guard.block = block;

        for update in account_updates {
            match update.change {
                ChangeType::Update => {
                    debug!(%update.address, "Updating account");

                    // If the account is not present, the internal storage will handle throwing
                    // an exception.
                    write_guard.accounts.update_account(
                        &update.address,
                        &StateUpdate {
                            storage: Some(update.slots.clone()),
                            balance: update.balance,
                        },
                    );
                }
                ChangeType::Deletion => {
                    debug!(%update.address, "Deleting account");

                    warn!(%update.address, "Deletion not implemented");
                }
                ChangeType::Creation => {
                    debug!(%update.address, "Creating account");

                    // We expect the code to be present.
                    let code = Bytecode::new_raw(AlloyBytes::from(
                        update.code.clone().ok_or_else(|| {
                            error!(%update.address, "MissingCode");
                            PreCachedDBError::BadUpdate(
                                "MissingCode".into(),
                                Box::new(update.clone()),
                            )
                        })?,
                    ));
                    // If the balance is not present, we set it to zero.
                    let balance = update.balance.unwrap_or(U256::ZERO);

                    // Replace, don't merge: snapshots omit zero-valued slots, so merging
                    // would keep stale non-zero values for slots that are zero on-chain.
                    write_guard.accounts.overwrite_account(
                        update.address,
                        AccountInfo::new(balance, 0, code.hash_slow(), code),
                        Some(update.slots.clone()),
                        true, /* Flag all accounts in TychoDB mocked to sign that we cannot
                               * call an RPC provider for an update */
                    );
                }
                ChangeType::Unspecified => {
                    warn!(%update.address, "Unspecified change type");
                }
            }
        }
        Ok(())
    }

    /// Like [`update()`](Self::update) restricted to `Creation` updates, but leaves the stored
    /// block header untouched. Used for proxy-token accounts written outside the regular
    /// snapshot/delta flow; generic callers should use [`update()`](Self::update).
    pub fn force_update_accounts(
        &self,
        account_updates: Vec<AccountUpdate>,
    ) -> Result<(), PreCachedDBError> {
        let mut write_guard = self.write_inner()?;

        for update in account_updates {
            if matches!(update.change, ChangeType::Creation) {
                let code =
                    Bytecode::new_raw(AlloyBytes::from(update.code.clone().ok_or_else(|| {
                        error!(%update.address, "MissingCode");
                        PreCachedDBError::BadUpdate("MissingCode".into(), Box::new(update.clone()))
                    })?));
                let balance = update.balance.unwrap_or(U256::ZERO);

                write_guard.accounts.overwrite_account(
                    update.address,
                    AccountInfo::new(balance, 0, code.hash_slow(), code),
                    Some(update.slots.clone()),
                    true,
                );
            } else {
                warn!(%update.address, "force_update_accounts called with non-Creation update; ignoring");
            }
        }
        Ok(())
    }

    /// Retrieves the storage value at the specified index for the given account, if it exists.
    ///
    /// If the account exists in the storage, the storage value at the specified `index` is returned
    /// as a reference. Temp storage takes priority over permanent storage.
    /// If the account does not exist, `None` is returned.
    ///
    /// # Arguments
    ///
    /// * `address`: A reference to the address of the account to retrieve the storage value from.
    /// * `index`: A reference to the index of the storage value to retrieve.
    ///
    /// # Returns
    ///
    /// Returns an `Option` containing a reference to the storage value if it exists, otherwise
    /// returns `None`.
    pub fn get_storage(&self, address: &Address, index: &U256) -> Option<U256> {
        self.inner
            .read()
            .unwrap()
            .accounts
            .get_storage(address, index)
    }

    /// Update the simulation state.
    ///
    /// This method modifies the current state of the simulation by applying the provided updates to
    /// the accounts in the smart contract storage. These changes correspond to a particular
    /// block in the blockchain.
    ///
    /// # Arguments
    ///
    /// * `new_state`: A struct containing all the state changes for a particular block.
    pub fn update_state(
        &mut self,
        updates: &HashMap<Address, StateUpdate>,
        block: BlockHeader,
    ) -> Result<HashMap<Address, StateUpdate>, PreCachedDBError> {
        // Hold the write lock for the duration of the function so that no other thread can
        // write to the storage.
        let mut write_guard = self.write_inner()?;

        let mut revert_updates = HashMap::new();
        write_guard.block = Some(block);

        for (address, update_info) in updates.iter() {
            let mut revert_entry = StateUpdate::default();

            if let Some(current_account) = write_guard
                .accounts
                .get_account_info(address)
            {
                revert_entry.balance = Some(current_account.balance);
            }

            if let Some(storage) = &update_info.storage {
                let mut revert_storage = HashMap::default();
                for index in storage.keys() {
                    if let Some(s) = write_guard
                        .accounts
                        .get_storage(address, index)
                    {
                        revert_storage.insert(*index, s);
                    }
                }
                revert_entry.storage = Some(revert_storage);
            }
            revert_updates.insert(*address, revert_entry);
            write_guard
                .accounts
                .update_account(address, update_info);
        }

        Ok(revert_updates)
    }

    #[cfg(test)]
    pub fn get_account_storage(&self) -> Result<AccountStorage, PreCachedDBError> {
        self.read_inner()
            .map(|guard| guard.accounts.clone())
    }

    /// If block is set, returns the number. Otherwise returns None.
    pub fn block_number(&self) -> Result<Option<u64>, PreCachedDBError> {
        self.read_inner().map(|guard| {
            guard
                .block
                .as_ref()
                .map(|header| header.number)
        })
    }

    /// Clear all state from the database.
    pub fn clear(&self) -> Result<(), PreCachedDBError> {
        let mut write_guard = self.write_inner()?;
        write_guard.accounts.clear();
        write_guard.block = None;
        Ok(())
    }

    fn read_inner(&self) -> Result<RwLockReadGuard<'_, PreCachedDBInner>, PreCachedDBError> {
        self.inner
            .read()
            .map_err(|_| PreCachedDBError::Fatal("Tycho state db lock poisoned".into()))
    }

    fn write_inner(&self) -> Result<RwLockWriteGuard<'_, PreCachedDBInner>, PreCachedDBError> {
        self.inner
            .write()
            .map_err(|_| PreCachedDBError::Fatal("Tycho state db lock poisoned".into()))
    }
}

impl EngineDatabaseInterface for PreCachedDB {
    type Error = PreCachedDBError;

    /// Sets up a single account
    ///
    /// Full control over setting up an accounts. Allows to set up EOAs as well as smart contracts.
    ///
    /// # Arguments
    ///
    /// * `address` - Address of the account
    /// * `account` - The account information
    /// * `permanent_storage` - Storage to init the account with, this storage can only be updated
    ///   manually
    fn init_account(
        &self,
        address: Address,
        account: AccountInfo,
        permanent_storage: Option<HashMap<U256, U256>>,
        _mocked: bool,
    ) -> Result<(), <Self as EngineDatabaseInterface>::Error> {
        if account.code.is_none() && account.code_hash != KECCAK_EMPTY {
            warn!("Code is None for account {address} but code hash is not KECCAK_EMPTY");
        } else if account.code.is_some() && account.code_hash == KECCAK_EMPTY {
            warn!("Code is Some for account {address} but code hash is KECCAK_EMPTY");
        }

        self.write_inner()?
            .accounts
            .init_account(address, account, permanent_storage, true);

        Ok(())
    }

    /// Deprecated in TychoDB
    fn clear_temp_storage(&mut self) -> Result<(), <Self as EngineDatabaseInterface>::Error> {
        debug!("Temp storage in TychoDB is never set, nothing to clear");

        Ok(())
    }

    fn get_current_block(&self) -> Option<BlockHeader> {
        self.inner.read().unwrap().block.clone()
    }
}

impl DatabaseRef for PreCachedDB {
    type Error = PreCachedDBError;
    /// Retrieves basic information about an account.
    ///
    /// This function retrieves the basic account information for the specified address.
    ///
    /// # Arguments
    ///
    /// * `address`: The address of the account to retrieve the information for.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing the account information or an error if the account is not
    /// found.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.read_inner()?
            .accounts
            .get_account_info(&address)
            .map(|acc| Some(acc.clone()))
            .ok_or(PreCachedDBError::MissingAccount(address))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Err(PreCachedDBError::Fatal(format!("Code by hash not supported: {code_hash}")))
    }

    /// Retrieves the storage value at the specified address and index.
    ///
    /// # Arguments
    ///
    /// * `address`: The address of the contract to retrieve the storage value from.
    /// * `index`: The index of the storage value to retrieve.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing the storage value if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage value is not found.
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        debug!(%address, %index, "Requested storage of account");
        let read_guard = self.read_inner()?;
        if let Some(storage_value) = read_guard
            .accounts
            .get_storage(&address, &index)
        {
            debug!(%address, %index, %storage_value, "Got value locally");
            Ok(storage_value)
        } else {
            // At this point we either don't know this address or we don't have anything at this
            if read_guard
                .accounts
                .account_present(&address)
            {
                // As we only store non-zero values, if the account is present it means this
                // slot is zero.
                debug!(%address, %index, "Account found, but slot is zero");
                Ok(U256::ZERO)
            } else {
                // At this point we know we don't have data for this address.
                debug!(%address, %index, "Account not found");
                Err(PreCachedDBError::MissingAccount(address))
            }
        }
    }

    /// If block header is set, returns the hash. Otherwise, returns a zero hash.
    fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
        match self.read_inner()?.block.clone() {
            Some(header) => Ok(B256::from_slice(&header.hash)),
            None => Ok(B256::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, str::FromStr};

    use revm::primitives::U256;
    use rstest::{fixture, rstest};
    use tycho_common::Bytes;

    use super::*;
    use crate::evm::tycho_models::{AccountUpdate, Chain, ChangeType};

    #[fixture]
    pub fn mock_db() -> PreCachedDB {
        PreCachedDB {
            inner: Arc::new(RwLock::new(PreCachedDBInner {
                accounts: AccountStorage::new(),
                block: None,
            })),
        }
    }

    #[rstest]
    #[tokio::test]
    async fn test_account_get_acc_info(mock_db: PreCachedDB) -> Result<(), Box<dyn Error>> {
        // Tests if the provider has not been queried.
        // Querying the mocked provider would cause a panic, therefore no assert is needed.
        let mock_acc_address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")?;
        mock_db
            .init_account(mock_acc_address, AccountInfo::default(), None, false)
            .expect("Account init should succeed");

        let acc_info = mock_db
            .basic_ref(mock_acc_address)
            .unwrap()
            .unwrap();

        assert_eq!(
            mock_db
                .basic_ref(mock_acc_address)
                .unwrap()
                .unwrap(),
            acc_info
        );
        Ok(())
    }

    #[rstest]
    fn test_account_storage(mock_db: PreCachedDB) -> Result<(), Box<dyn Error>> {
        let mock_acc_address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")?;
        let storage_address = U256::from(1);
        let mut permanent_storage: HashMap<U256, U256> = HashMap::new();
        permanent_storage.insert(storage_address, U256::from(10));
        mock_db
            .init_account(mock_acc_address, AccountInfo::default(), Some(permanent_storage), false)
            .expect("Account init should succeed");

        let storage = mock_db
            .storage_ref(mock_acc_address, storage_address)
            .unwrap();

        assert_eq!(storage, U256::from(10));
        Ok(())
    }

    #[rstest]
    fn test_account_storage_zero(mock_db: PreCachedDB) -> Result<(), Box<dyn Error>> {
        let mock_acc_address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")?;
        let storage_address = U256::from(1);
        mock_db
            .init_account(mock_acc_address, AccountInfo::default(), None, false)
            .expect("Account init should succeed");

        let storage = mock_db
            .storage_ref(mock_acc_address, storage_address)
            .unwrap();

        assert_eq!(storage, U256::ZERO);
        Ok(())
    }

    #[rstest]
    #[should_panic(
        expected = "called `Result::unwrap()` on an `Err` value: MissingAccount(0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc)"
    )]
    fn test_account_storage_missing(mock_db: PreCachedDB) {
        let mock_acc_address =
            Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc").unwrap();
        let storage_address = U256::from(1);

        // This will panic because this account isn't initialized
        mock_db
            .storage_ref(mock_acc_address, storage_address)
            .unwrap();
    }

    fn header(number: u64) -> BlockHeader {
        BlockHeader { number, ..Default::default() }
    }

    fn snapshot(address: Address, slots: &[(u64, u64)], balance: u64) -> AccountUpdate {
        AccountUpdate {
            address,
            chain: Chain::Ethereum,
            slots: slots
                .iter()
                .map(|(k, v)| (U256::from(*k), U256::from(*v)))
                .collect(),
            balance: Some(U256::from(balance)),
            code: Some(vec![0x60, 0x00]),
            change: ChangeType::Creation,
        }
    }

    /// A consumer that re-fetches a snapshot for an already-known contract (client reconnect
    /// healing, in-process stream rebuild, TVL re-entry) delivers it as `ChangeType::Creation`.
    /// Snapshots are authoritative full state: slots that changed while the account was warm
    /// must end up at the snapshot values, otherwise the DB is left mixed-epoch and VM-based
    /// protocols quote against a state that never existed on-chain.
    #[rstest]
    fn test_creation_reapply_overwrites_existing_account(
        mock_db: PreCachedDB,
    ) -> Result<(), Box<dyn Error>> {
        let address = Address::from_str("0xd51a44d3fae010294c616388b506acda1bfaae46")?;
        let slot = U256::from(1);

        mock_db.update(vec![snapshot(address, &[(1, 100)], 10)], Some(header(1)))?;
        // Account is now warm; the outage window ends and a fresh full snapshot arrives.
        mock_db.update(vec![snapshot(address, &[(1, 999), (2, 42)], 20)], Some(header(2)))?;

        assert_eq!(
            mock_db.get_storage(&address, &slot),
            Some(U256::from(999)),
            "re-applied snapshot must overwrite a stale slot on an existing account"
        );
        assert_eq!(
            mock_db.get_storage(&address, &U256::from(2)),
            Some(U256::from(42)),
            "slots first seen in the re-applied snapshot must be present"
        );
        assert_eq!(
            mock_db
                .basic_ref(address)
                .unwrap()
                .map(|acc| acc.balance),
            Some(U256::from(20)),
            "re-applied snapshot must refresh the native balance"
        );
        Ok(())
    }

    /// Contrast case: the continuously-connected path. Per-block deltas arrive as
    /// `ChangeType::Update` and must refresh slots on existing accounts.
    #[rstest]
    fn test_delta_update_refreshes_existing_slot(
        mock_db: PreCachedDB,
    ) -> Result<(), Box<dyn Error>> {
        let address = Address::from_str("0xd51a44d3fae010294c616388b506acda1bfaae46")?;
        let slot = U256::from(1);

        mock_db.update(vec![snapshot(address, &[(1, 100)], 10)], Some(header(1)))?;
        let delta = AccountUpdate {
            address,
            chain: Chain::Ethereum,
            slots: HashMap::from([(slot, U256::from(200))]),
            balance: None,
            code: None,
            change: ChangeType::Update,
        };
        mock_db.update(vec![delta], Some(header(2)))?;

        assert_eq!(
            mock_db.get_storage(&address, &slot),
            Some(U256::from(200)),
            "delta updates must refresh slots on existing accounts"
        );
        Ok(())
    }

    #[rstest]
    #[tokio::test]
    async fn test_update_state(mut mock_db: PreCachedDB) -> Result<(), Box<dyn Error>> {
        let address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")?;
        mock_db
            .init_account(address, AccountInfo::default(), None, false)
            .expect("Account init should succeed");

        let mut new_storage = HashMap::default();
        let new_storage_value_index = U256::from_limbs_slice(&[123]);
        new_storage.insert(new_storage_value_index, new_storage_value_index);
        let new_balance = U256::from_limbs_slice(&[500]);
        let update = StateUpdate { storage: Some(new_storage), balance: Some(new_balance) };
        let new_block = BlockHeader {
            number: 1,
            hash: Bytes::from_str(
                "0xc6b994ec855fb2b31013c7ae65074406fac46679b5b963469104e0bfeddd66d9",
            )
            .unwrap(),
            timestamp: 123,
            ..Default::default()
        };
        let mut updates = HashMap::default();
        updates.insert(address, update);

        mock_db
            .update_state(&updates, new_block)
            .expect("State update should succeed");

        assert_eq!(
            mock_db
                .get_storage(&address, &new_storage_value_index)
                .unwrap(),
            new_storage_value_index
        );
        let account_info = mock_db
            .basic_ref(address)
            .unwrap()
            .unwrap();
        assert_eq!(account_info.balance, new_balance);
        let block = mock_db
            .inner
            .read()
            .unwrap()
            .block
            .clone()
            .expect("block is Some");
        assert_eq!(block.number, 1);

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    async fn test_block_number_getter(mut mock_db: PreCachedDB) -> Result<(), Box<dyn Error>> {
        let address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")?;
        mock_db
            .init_account(address, AccountInfo::default(), None, false)
            .expect("Account init should succeed");

        let new_block = BlockHeader {
            number: 1,
            hash: Bytes::from_str(
                "0xc6b994ec855fb2b31013c7ae65074406fac46679b5b963469104e0bfeddd66d9",
            )
            .unwrap(),
            timestamp: 123,
            ..Default::default()
        };
        let updates = HashMap::default();

        mock_db
            .update_state(&updates, new_block)
            .expect("State update should succeed");

        let block_number = mock_db.block_number();
        assert_eq!(block_number.unwrap().unwrap(), 1);

        Ok(())
    }

    #[rstest]
    #[tokio::test]
    async fn test_update() {
        let mock_db = PreCachedDB {
            inner: Arc::new(RwLock::new(PreCachedDBInner {
                accounts: AccountStorage::new(),
                block: None,
            })),
        };

        let account_update = AccountUpdate::new(
            Address::from_str("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D").unwrap(),
            Chain::Ethereum,
            HashMap::new(),
            Some(U256::from(500)),
            Some(Vec::<u8>::new()),
            ChangeType::Creation,
        );

        let new_block = BlockHeader {
            number: 1,
            hash: Bytes::from_str(
                "0xc6b994ec855fb2b31013c7ae65074406fac46679b5b963469104e0bfeddd66d9",
            )
            .unwrap(),
            timestamp: 123,
            ..Default::default()
        };

        mock_db
            .update(vec![account_update], Some(new_block))
            .unwrap();

        let account_info = mock_db
            .basic_ref(Address::from_str("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D").unwrap())
            .unwrap()
            .unwrap();

        assert_eq!(
            account_info,
            AccountInfo {
                nonce: 0,
                balance: U256::from(500),
                code_hash: B256::from_str(
                    "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
                )
                .unwrap(),
                code: Some(Bytecode::default()),
            }
        );

        assert_eq!(
            mock_db
                .inner
                .read()
                .unwrap()
                .block
                .clone()
                .expect("block is Some")
                .number,
            1
        );
    }

    #[rstest]
    fn test_force_update_accounts_overwrites_existing(mock_db: PreCachedDB) {
        let address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc").unwrap();

        // Simulate a placeholder proxy inserted by another decoder's snapshot loop.
        mock_db
            .init_account(address, AccountInfo::default(), None, true)
            .expect("placeholder init should succeed");

        // Now force-overwrite with the real proxy (authoritative vm_storage data).
        let storage_slot = U256::from(1);
        let storage_value = U256::from(42);
        let mut slots = HashMap::new();
        slots.insert(storage_slot, storage_value);
        let update = AccountUpdate::new(
            address,
            Chain::Ethereum,
            slots,
            Some(U256::from(100)),
            Some(Vec::<u8>::new()),
            ChangeType::Creation,
        );
        mock_db
            .force_update_accounts(vec![update])
            .expect("force update should succeed");

        let info = mock_db
            .basic_ref(address)
            .unwrap()
            .unwrap();
        assert_eq!(info.balance, U256::from(100));
        assert_eq!(
            mock_db
                .get_storage(&address, &storage_slot)
                .unwrap(),
            storage_value
        );
    }

    #[rstest]
    fn test_force_update_accounts_non_creation_ignored(mock_db: PreCachedDB) {
        let address = Address::from_str("0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc").unwrap();
        let original_balance = U256::from(10);

        mock_db
            .init_account(
                address,
                AccountInfo { balance: original_balance, ..Default::default() },
                None,
                true,
            )
            .expect("init should succeed");

        // A non-Creation update should be ignored by force_update_accounts.
        let update = AccountUpdate::new(
            address,
            Chain::Ethereum,
            HashMap::new(),
            Some(U256::from(999)),
            None,
            ChangeType::Update,
        );
        mock_db
            .force_update_accounts(vec![update])
            .expect("force update should succeed");

        // Balance must remain unchanged.
        let info = mock_db
            .basic_ref(address)
            .unwrap()
            .unwrap();
        assert_eq!(info.balance, original_balance);
    }

    /// This test requires a running TychoDB instance.
    ///
    /// To run this test, start TychoDB with the following command:
    /// ```bash
    /// cargo run --release -- \
    //     --endpoint https://mainnet.eth.streamingfast.io:443 \
    //     --module map_changes \
    //     --spkg substreams/ethereum-ambient/substreams-ethereum-ambient-v0.3.0.spkg
    /// ```
    /// 
    /// Then run the test with:
    /// ```bash
    /// cargo test --package src --lib -- --ignored --exact --nocapture
    /// evm::engine_db::tycho_db::tests::test_tycho_db_connection
    /// ```
    #[ignore]
    #[rstest]
    fn test_tycho_db_connection() {
        tracing_subscriber::fmt()
            .with_env_filter("debug")
            .init();

        let ambient_contract =
            Address::from_str("0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688").unwrap();

        let db = PreCachedDB::new().expect("db should initialize");

        let acc_info = db
            .basic_ref(ambient_contract)
            .unwrap()
            .unwrap();

        debug!(?acc_info, "Account info");
    }
}
