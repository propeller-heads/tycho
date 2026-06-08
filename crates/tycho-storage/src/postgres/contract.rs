use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    slice,
};

use chrono::{NaiveDateTime, Utc};
use diesel::{
    prelude::*,
    upsert::{excluded, on_constraint},
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use itertools::Itertools;
use tracing::{debug, debug_span, error, instrument, warn, Instrument, Level};
use tycho_common::{
    keccak256,
    models::{
        contract::{Account, AccountBalance, AccountDelta},
        AccountToContractStoreDeltas, Address, Balance, Chain, ChangeType, Code, ContractId,
        ContractStoreDeltas, PaginationParams, StoreKey, StoreVal, TxHash,
    },
    storage::{BlockOrTimestamp, StorageError, Version, WithTotal},
    Bytes,
};

use super::{
    maybe_lookup_block_ts, maybe_lookup_version_ts, orm, schema, storage_error_from_diesel,
    versioning::{apply_partitioned_versioning, apply_versioning, VersioningEntry},
    PostgresError, PostgresGateway, WithOrdinal, WithTxHash, MAX_TS, MAX_VERSION_TS,
};

struct CreatedOrDeleted<T> {
    /// Accounts that were created (and deltas are equal to their updates)
    created: HashSet<Address>,
    /// Accounts that were deleted and needed restoring to get correct deltas.
    restored: HashMap<Address, T>,
}

// Private methods
impl PostgresGateway {
    /// Retrieves the changes in balance for all accounts of a chain.
    ///
    /// See [ContractStateGateway::get_accounts_delta] for more information on
    /// the mechanics of this method regarding version timestamps.
    ///
    /// # Returns
    /// This method returns a mapping from each account id to its respective `Balance`. The returned
    /// map indicates the new balance that needs to be applied to reach the desired target
    /// version.
    #[instrument(level = Level::DEBUG, skip(self, conn))]
    async fn get_balance_deltas_internal(
        &self,
        chain: &Chain,
        start_version_ts: &NaiveDateTime,
        target_version_ts: &NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<i64, Balance>, StorageError> {
        use schema::account_balance::dsl::*;
        let chain_id = self.get_chain_id(chain)?;

        let res = if start_version_ts <= target_version_ts {
            // Going forward
            //                  [     changes to update    ]
            // -----------------|--------------------------|
            //                start                     target
            // We query for balance updates between start and target version.
            let changed_account_ids = account_balance
                .inner_join(schema::account::table.inner_join(schema::chain::table))
                .filter(schema::chain::id.eq(chain_id))
                .filter(valid_from.gt(start_version_ts))
                .filter(valid_from.le(target_version_ts))
                .select(account_id)
                .distinct()
                .into_boxed();

            account_balance
                .inner_join(schema::transaction::table)
                .inner_join(schema::token::table)
                .inner_join(
                    schema::account::table.on(schema::account::id.eq(schema::token::account_id)),
                )
                .filter(account_id.eq_any(changed_account_ids))
                .filter(schema::account::address.eq(chain.native_token().address))
                .filter(valid_from.le(target_version_ts))
                .filter(
                    valid_to
                        .gt(target_version_ts)
                        .or(valid_to.is_null()),
                )
                .select((account_id, balance))
                .order_by((account_id, valid_from.desc(), schema::transaction::index.desc()))
                .distinct_on(account_id)
                .get_results::<(i64, Balance)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .collect::<HashMap<i64, Balance>>()
        } else {
            // Going backwards
            //                  [     changes to revert    ]
            // -----------------|--------------------------|
            //                target                     start
            // We query for balances valid AT the target version for accounts that were updated
            // between target and start version.
            let changed_account_ids = account_balance
                .inner_join(schema::account::table.inner_join(schema::chain::table))
                .filter(schema::chain::id.eq(chain_id))
                .filter(valid_from.gt(target_version_ts))
                .filter(valid_from.le(start_version_ts))
                .select(account_id)
                .distinct()
                .into_boxed();

            account_balance
                .inner_join(schema::transaction::table)
                .inner_join(schema::token::table)
                .inner_join(
                    schema::account::table.on(schema::account::id.eq(schema::token::account_id)),
                )
                .filter(account_id.eq_any(changed_account_ids))
                .filter(schema::account::address.eq(chain.native_token().address))
                .filter(valid_from.le(target_version_ts))
                .filter(
                    valid_to
                        .gt(target_version_ts)
                        .or(valid_to.is_null()),
                )
                .select((account_id, balance))
                .order_by((account_id, valid_from.asc(), schema::transaction::index.asc()))
                .distinct_on(account_id)
                .get_results::<(i64, Balance)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .collect::<HashMap<i64, Balance>>()
        };
        Ok(res)
    }

    /// Retrieves the changes in code for all accounts of a chain.
    ///
    /// See [ContractStateGateway::get_accounts_delta] for more information on
    /// the mechanics of this method regarding version timestamps.
    ///
    /// # Returns
    /// This method returns a mapping from each account id to its respective
    /// `Code`. The returned map indicates the new code that needs to be applied
    /// to reach the desired target version.
    #[instrument(level = Level::DEBUG, skip(self, conn))]
    async fn get_code_deltas(
        &self,
        chain_id: i64,
        start_version_ts: &NaiveDateTime,
        target_version_ts: &NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<i64, Code>, StorageError> {
        use schema::contract_code::dsl::*;
        let res = if start_version_ts <= target_version_ts {
            let changed_account_ids = contract_code
                .inner_join(schema::account::table.inner_join(schema::chain::table))
                .filter(schema::chain::id.eq(chain_id))
                .filter(valid_from.gt(start_version_ts))
                .filter(valid_from.le(target_version_ts))
                .select(account_id)
                .distinct()
                .into_boxed();

            contract_code
                .inner_join(schema::transaction::table)
                .filter(account_id.eq_any(changed_account_ids))
                .filter(valid_from.le(target_version_ts))
                .filter(
                    valid_to
                        .gt(target_version_ts)
                        .or(valid_to.is_null()),
                )
                .select((account_id, code))
                .order_by((account_id, valid_from.desc(), schema::transaction::index.desc()))
                .distinct_on(account_id)
                .get_results::<(i64, Code)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .collect::<HashMap<i64, Code>>()
        } else {
            let changed_account_ids = contract_code
                .inner_join(schema::account::table.inner_join(schema::chain::table))
                .filter(schema::chain::id.eq(chain_id))
                .filter(valid_from.gt(target_version_ts))
                .filter(valid_from.le(start_version_ts))
                .select(account_id)
                .distinct()
                .into_boxed();

            contract_code
                .inner_join(schema::transaction::table)
                .filter(account_id.eq_any(changed_account_ids))
                .filter(valid_from.le(target_version_ts))
                .filter(
                    valid_to
                        .gt(target_version_ts)
                        .or(valid_to.is_null()),
                )
                .select((account_id, code))
                .order_by((account_id, valid_from.asc(), schema::transaction::index.asc()))
                .distinct_on(account_id)
                .get_results::<(i64, Code)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .collect::<HashMap<i64, Code>>()
        };
        Ok(res)
    }

    /// Retrieves the changes in slots for all accounts of a chain.
    ///
    /// See [ContractStateGateway::get_accounts_delta] for more information on
    /// the mechanics of this method regarding version timestamps.
    ///
    /// # Returns
    /// This method returns a mapping from each account id to a `ContractStore`.
    /// The returned store entries indicate the updates needed to reach the specified target
    /// version.
    #[instrument(level = Level::DEBUG, skip(self, conn))]
    async fn get_slots_delta(
        &self,
        chain_id: i64,
        start_version_ts: &NaiveDateTime,
        target_version_ts: &NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<i64, ContractStoreDeltas>, StorageError> {
        let changed_values = if start_version_ts <= target_version_ts {
            // Going forward
            //                  ]     changes to forward   ]
            // -----------------|--------------------------|
            //                start                     target
            // We query for changes between start and target version. Then sort
            // these by account and slot by change time in a descending manner
            // (latest change first). Next we deduplicate by account and slot.
            // Finally, we select the value column to give us the latest value
            // within the version range.
            schema::contract_storage::table
                .inner_join(schema::account::table.inner_join(schema::chain::table))
                .filter(schema::chain::id.eq(chain_id))
                .filter(schema::contract_storage::valid_from.gt(start_version_ts))
                .filter(schema::contract_storage::valid_from.le(target_version_ts))
                .order_by((
                    schema::account::id,
                    schema::contract_storage::slot,
                    schema::contract_storage::valid_from.desc(),
                    schema::contract_storage::ordinal.desc(),
                ))
                .select((
                    schema::account::id,
                    schema::contract_storage::slot,
                    schema::contract_storage::value,
                ))
                .distinct_on((schema::account::id, schema::contract_storage::slot))
                .get_results::<(i64, StoreKey, Option<StoreVal>)>(conn)
                .await
                .map_err(PostgresError::from)?
        } else {
            // Going backwards
            //                  ]     changes to revert    ]
            // -----------------|--------------------------|
            //                target                     start
            // We query for changes between target and start version. Then sort
            // these for each account and slot by change time in an ascending
            // manner. Next, we deduplicate by taking the first row for each
            // account and slot. Finally we select the previous_value column to
            // give us the value before this first change within the version
            // range.
            schema::contract_storage::table
                .inner_join(schema::account::table.inner_join(schema::chain::table))
                .filter(schema::chain::id.eq(chain_id))
                .filter(schema::contract_storage::valid_from.gt(target_version_ts))
                .filter(schema::contract_storage::valid_from.le(start_version_ts))
                .order_by((
                    schema::account::id.asc(),
                    schema::contract_storage::slot.asc(),
                    schema::contract_storage::valid_from.asc(),
                    schema::contract_storage::ordinal.asc(),
                ))
                .select((
                    schema::account::id,
                    schema::contract_storage::slot,
                    schema::contract_storage::previous_value,
                ))
                .distinct_on((schema::account::id, schema::contract_storage::slot))
                .get_results::<(i64, Bytes, Option<Bytes>)>(conn)
                .await
                .map_err(PostgresError::from)?
        };

        let mut result: HashMap<i64, ContractStoreDeltas> = HashMap::new();
        for (cid, raw_key, raw_val) in changed_values.into_iter() {
            match result.entry(cid) {
                Entry::Occupied(mut e) => {
                    e.get_mut().insert(raw_key, raw_val);
                }
                Entry::Vacant(e) => {
                    let mut contract_storage = HashMap::new();
                    contract_storage.insert(raw_key, raw_val);
                    e.insert(contract_storage);
                }
            }
        }
        Ok(result)
    }

    /// Fetch deleted or created account deltas
    ///
    /// # Operations
    ///
    /// 1. Going Forward (`start < target`):
    ///     - a) If an account was deleted, emit an empty delta with [ChangeType::Deletion].
    ///     - b) If an account was created, emit an already collected update delta but with
    ///       [ChangeType::Creation].. No need to fetch the state at the `target_version` as by
    ///       design we emit newly created contracts and their components when going forward.
    ///
    /// 2. Going Backward (`target < start`):
    ///     - a) If an account was deleted, it restores the account. Therefore, it needs to retrieve
    ///       the account state at `target` and emits the account with [ChangeType::Creation].
    ///     - b) If an account was created, emit an empty delta with [ChangeType::Deletion].
    ///
    /// We boil down the rules above into two simple operations:
    ///     - The cases from 1b, are simple we just need to record which
    ///     accounts are affected by 1b and then mark existing deltas as created
    ///     - The remaining cases will require new deltas to be constructed,
    ///     those are either restores (2a) or empty deltas indicating deletions
    ///     (1a & 2b)
    ///
    /// # Returns
    ///
    /// The CreatedOrDeleted struct. It contains accounts that fall under 1a within its created
    /// attribute. We can use this attribute to satisfy the first operation. It also contains new
    /// restored / deleted delta structs within the `restored` attribute with which we can satisfy
    /// the second rule.
    #[instrument(level = Level::DEBUG, skip(self, conn))]
    async fn get_created_or_deleted_accounts(
        &self,
        chain: &Chain,
        start_version_ts: &NaiveDateTime,
        target_version_ts: &NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> Result<CreatedOrDeleted<AccountDelta>, StorageError> {
        // Find created or deleted Accounts
        let cod_accounts: Vec<orm::Account> = {
            use schema::account::dsl::*;
            let (start, end) = if start_version_ts < target_version_ts {
                (start_version_ts, target_version_ts)
            } else {
                (target_version_ts, start_version_ts)
            };
            account
                .filter(
                    (deleted_at
                        .gt(start)
                        .and(deleted_at.le(end)))
                    .or(created_at
                        .gt(start)
                        .and(created_at.le(end))),
                )
                .select(orm::Account::as_select())
                .get_results::<orm::Account>(conn)
                .await
                .map_err(PostgresError::from)?
        };
        // Handle creation and deletion of same account within range. Currently
        // not properly supported. We only check for the existence of such a
        // problematic situation see (PR#30).
        // - forward
        //      1. c + d -> current: not present, target: not present => noop
        //      2. c + d + c -> current: present, target: present => emit created, merge with
        //         updates after creation
        //      3. d + c -> current: present, target: present     => emit created, merge with
        //         updates after creation
        //      4. d + c + d -> current: present, target: not present => emit deletion
        // - backward
        //      5. c + d -> current: not present, target: not present => noop
        //      6. c + d + c -> current: present, target: not present => emit delete
        //      7. d + c -> current: present, target: present => restore at target
        //      8. d + c + d -> current: not present, target: present => restore at target
        //
        // The easiest will be to check for these situations, then load the complete state at
        // target.
        if cod_accounts.iter().any(|acc| {
            if start_version_ts < target_version_ts {
                let one = acc
                    .created_at
                    .map(|ts| start_version_ts < &ts && &ts <= target_version_ts)
                    .unwrap_or(false);
                let two = acc
                    .deleted_at
                    .map(|ts| start_version_ts < &ts && &ts <= target_version_ts)
                    .unwrap_or(false);
                one && two
            } else {
                let one = acc
                    .created_at
                    .map(|ts| target_version_ts < &ts && &ts <= start_version_ts)
                    .unwrap_or(false);
                let two = acc
                    .deleted_at
                    .map(|ts| target_version_ts < &ts && &ts <= start_version_ts)
                    .unwrap_or(false);
                one && two
            }
        }) {
            return Err(StorageError::Unexpected(format!(
                "Found account that was deleted and created within range {start_version_ts} - {target_version_ts}!"
            )));
        }

        // The code below assumes that no account is deleted or created more
        // than once within the given version range.
        let (mark_created, cod_deltas) = if start_version_ts > target_version_ts {
            // going backwards
            let deleted_addresses: Vec<Address> = cod_accounts
                .iter()
                .filter(|a| a.deleted_at.is_some())
                .map(|a| a.address.clone())
                .collect::<Vec<_>>();

            // Restore full state delta at from target version for accounts that were deleted
            let version = Some(Version::from_ts(*target_version_ts));
            let restored: HashMap<Address, AccountDelta> = self
                .get_contracts(chain, Some(&deleted_addresses), version.as_ref(), true, None, conn)
                .await
                .map_err(PostgresError::from)?
                .entity
                .into_iter()
                .map(|acc| (acc.address.clone(), acc.into()))
                .collect();

            let deltas = cod_accounts
                .iter()
                .map(|acc| -> Result<_, StorageError> {
                    let update = if let Some(update) = restored.get(&acc.address) {
                        // assuming these have ChangeType created
                        update.clone()
                    } else {
                        AccountDelta::deleted(chain, &acc.address)
                    };
                    Ok((acc.address.clone(), update))
                })
                .collect::<Result<HashMap<_, _>, _>>()?;

            // ensure all accounts have a restore delta
            cod_accounts
                .iter()
                .map(|v| {
                    deltas
                        .contains_key(&v.address)
                        .then_some(v)
                        .ok_or_else(|| {
                            StorageError::Unexpected(format!(
                                "Fatal: Missing account 0x{} detected in delta changes!",
                                hex::encode(&v.address),
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            (HashSet::<Bytes>::new(), deltas)
        } else {
            let (deleted, created): (Vec<_>, Vec<_>) = cod_accounts
                .into_iter()
                .partition(|acc| acc.deleted_at.is_some());
            let created: HashSet<_> = created
                .into_iter()
                .map(|acc| acc.address)
                .collect();
            let deltas = deleted
                .iter()
                .map(|acc| {
                    let update = AccountDelta::deleted(chain, &acc.address);
                    Ok((acc.address.clone(), update))
                })
                .collect::<Result<HashMap<_, _>, StorageError>>()?;
            (created, deltas)
        };
        Ok(CreatedOrDeleted { created: mark_created, restored: cod_deltas })
    }

    /// Upserts slots for a given contract.
    ///
    /// Upserts slots from multiple accounts. To correctly track changes, it is necessary that each
    /// slot modification is associated with the transaction that actually changed the slots.
    ///
    /// # Parameters
    /// - `slots` A hashmap containing only the changed slots. Grouped first by the transaction
    ///   database id that contained the changes, then by account address. Slots that were changed
    ///   to 0 are expected to be included here.
    ///
    /// # Returns
    /// An empty `Ok(())` if the operation succeeded. Will raise an error if any
    /// of the related entities can not be found: e.g. one of the referenced
    /// transactions or accounts is not or not yet persisted.
    #[instrument(level = Level::DEBUG, skip_all)]
    async fn upsert_slots(
        &self,
        slots: HashMap<i64, AccountToContractStoreDeltas>,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let txns: HashSet<_> = slots.keys().copied().collect();
        let tx_ids: HashMap<i64, (i64, NaiveDateTime)> = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq_any(txns))
            .select((schema::transaction::id, (schema::transaction::index, schema::block::ts)))
            .get_results::<(i64, (i64, NaiveDateTime))>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();
        #[allow(clippy::mutable_key_type)]
        let accounts: HashSet<_> = slots
            .values()
            .flat_map(|contract_slots| contract_slots.keys())
            .collect();
        let account_ids: HashMap<Bytes, i64> = schema::account::table
            .filter(schema::account::address.eq_any(accounts))
            .select((schema::account::address, schema::account::id))
            .get_results::<(Bytes, i64)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();

        let mut new_entries = Vec::new();
        for (modify_tx, contract_storage) in slots.iter() {
            let (tx_index, block_ts) = tx_ids.get(modify_tx).ok_or_else(|| {
                StorageError::NoRelatedEntity(
                    "Transaction".into(),
                    "ContractStorage".into(),
                    format!("{modify_tx}"),
                )
            })?;
            for (address, storage) in contract_storage.iter() {
                let account_id = account_ids
                    .get(address)
                    .ok_or_else(|| {
                        StorageError::NoRelatedEntity(
                            "Account".into(),
                            "ContractStorage".into(),
                            hex::encode(address),
                        )
                    })?;
                for (slot, value) in storage.iter() {
                    new_entries.push(WithOrdinal::new(
                        VersioningEntry::Update(orm::NewSlot {
                            slot: slot.clone(),
                            value: value.clone(),
                            previous_value: None,
                            account_id: *account_id,
                            modify_tx: *modify_tx,
                            // this is still required for delta queries
                            ordinal: *tx_index,
                            valid_from: *block_ts,
                            valid_to: MAX_TS,
                        }),
                        (*account_id, slot, *block_ts, *tx_index),
                    ))
                }
            }
        }

        debug!(n = new_entries.len(), "Inserting slots");
        new_entries.sort_by_cached_key(|b| b.ordinal);
        let sorted = new_entries
            .into_iter()
            .map(|b| b.entity)
            .collect::<Vec<_>>();
        let (latest, to_archive, _) =
            apply_partitioned_versioning(&sorted, self.retention_horizon, conn).await?;
        let latest = latest
            .into_iter()
            .map(orm::NewSlotLatest::from)
            .collect::<Vec<_>>();

        for chunk in latest.chunks(1_000) {
            diesel::insert_into(schema::contract_storage_default::table)
                .values(chunk)
                .on_conflict(on_constraint("contract_storage_default_unique_pk"))
                .do_update()
                .set((
                    schema::contract_storage_default::slot
                        .eq(excluded(schema::contract_storage_default::slot)),
                    schema::contract_storage_default::value
                        .eq(excluded(schema::contract_storage_default::value)),
                    schema::contract_storage_default::previous_value
                        .eq(excluded(schema::contract_storage_default::previous_value)),
                    schema::contract_storage_default::account_id
                        .eq(excluded(schema::contract_storage_default::account_id)),
                    schema::contract_storage_default::modify_tx
                        .eq(excluded(schema::contract_storage_default::modify_tx)),
                    schema::contract_storage_default::ordinal
                        .eq(excluded(schema::contract_storage_default::ordinal)),
                    schema::contract_storage_default::valid_from
                        .eq(excluded(schema::contract_storage_default::valid_from)),
                ))
                .execute(conn)
                .await
                .map_err(PostgresError::from)?;
        }

        for chunk in to_archive.chunks(1_000) {
            diesel::insert_into(schema::contract_storage::table)
                .values(chunk)
                .execute(conn)
                .await
                .map_err(PostgresError::from)?;
        }

        Ok(())
    }

    /// Retrieve contract slots.
    ///
    /// Retrieve the storage slots of contracts at a given time/version.
    ///
    /// Will return the slots state after the given block/timestamp. Later we
    /// might change to use VersionResult, but for now we keep it simple. Using
    /// anything else then Version::Last is currently not supported.
    ///
    /// # Parameters
    /// - `chain` The chain for which to retrieve slots for.
    /// - `contracts` Optionally allows filtering by contract address.
    /// - `at` The version at which to retrieve slots. None retrieves the latest
    /// - `conn` The database handle or connection. state.
    #[instrument(level = Level::DEBUG, skip(self, contracts, conn))]
    async fn get_contract_slots(
        &self,
        chain: &Chain,
        contracts: Option<&[Address]>,
        at: Option<&Version>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, ContractStoreDeltas>, StorageError> {
        let version_ts = match &at {
            Some(version) => maybe_lookup_version_ts(version, conn).await?,
            None => Utc::now().naive_utc(),
        };

        let slots = {
            use schema::{account, contract_storage::dsl::*};

            let chain_id = self.get_chain_id(chain)?;
            let mut q = contract_storage
                .inner_join(account::table)
                .filter(account::chain_id.eq(chain_id))
                .filter(
                    valid_from
                        .le(version_ts)
                        .and(valid_to.gt(version_ts)),
                )
                .order_by((account::id, slot, valid_from.desc(), ordinal.desc()))
                .select((account::id, slot, value))
                .distinct_on((account::id, slot))
                .into_boxed();
            if let Some(addresses) = contracts {
                #[allow(clippy::mutable_key_type)]
                let filter_val: HashSet<_> = addresses.iter().collect();
                q = q.filter(account::address.eq_any(filter_val));
            }
            q.get_results::<(i64, Bytes, Option<Bytes>)>(conn)
                .await
                .map_err(PostgresError::from)?
        };
        let accounts = orm::Account::get_addresses_by_id(slots.iter().map(|(cid, _, _)| cid), conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect::<HashMap<i64, Bytes>>();
        Self::construct_account_to_contract_store(slots.into_iter(), accounts)
    }

    /// Constructs a mapping from address to contract slots
    fn construct_account_to_contract_store(
        slot_values: impl Iterator<Item = (i64, Bytes, Option<Bytes>)>,
        addresses: HashMap<i64, Bytes>,
    ) -> Result<AccountToContractStoreDeltas, StorageError> {
        let mut result: AccountToContractStoreDeltas = HashMap::with_capacity(addresses.len());
        for (cid, raw_key, raw_val) in slot_values.into_iter() {
            // note this can theoretically happen (only if there is some really
            // bad database inconsistency) because the call above simply filters
            // for account ids, but won't error or give any inidication of a
            // missing contract id.
            let account_address = addresses.get(&cid).ok_or_else(|| {
                StorageError::DecodeError(format!("Failed to find contract address for id {cid}"))
            })?;

            match result.entry(account_address.clone()) {
                Entry::Occupied(mut e) => {
                    e.get_mut().insert(raw_key, raw_val);
                }
                Entry::Vacant(e) => {
                    let mut contract_storage = HashMap::new();
                    contract_storage.insert(raw_key, raw_val);
                    e.insert(contract_storage);
                }
            }
        }
        Ok(result)
    }

    #[instrument(level = Level::DEBUG, skip(self, conn))]
    pub async fn get_contract(
        &self,
        id: &ContractId,
        version: Option<&Version>,
        include_slots: bool,
        conn: &mut AsyncPgConnection,
    ) -> Result<Account, StorageError> {
        let account_orm: orm::Account = orm::Account::by_id(id, conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "Account", &hex::encode(&id.address), None)
            })?;
        let version_ts = match &version {
            Some(version) => maybe_lookup_version_ts(version, conn).await?,
            None => Utc::now().naive_utc(),
        };
        let chain = id.chain;

        let mut all_balances = self
            .get_account_balances(&chain, Some(slice::from_ref(&id.address)), version, true, conn)
            .await?;
        let account_balances = all_balances
            .get_mut(&id.address)
            .ok_or_else(|| {
                StorageError::NotFound("account_balances".to_string(), id.address.to_string())
            })?;
        let native_balance = account_balances
            .remove(&chain.native_token().address)
            .ok_or_else(|| {
                StorageError::NotFound("native_balance".to_string(), id.address.to_string())
            })?;

        let (code_tx, code_orm) = schema::contract_code::table
            .inner_join(schema::transaction::table)
            .filter(schema::contract_code::account_id.eq(account_orm.id))
            .filter(schema::contract_code::valid_from.le(version_ts))
            .filter(
                schema::contract_code::valid_to
                    .gt(Some(version_ts))
                    .or(schema::contract_code::valid_to.is_null()),
            )
            .select((schema::transaction::hash, orm::ContractCode::as_select()))
            .order_by((
                schema::contract_code::account_id,
                schema::contract_code::valid_from.desc(),
                schema::transaction::index.desc(),
            ))
            .first::<(Bytes, orm::ContractCode)>(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(
                    err,
                    "ContractCode",
                    &hex::encode(&id.address),
                    Some("Account".to_owned()),
                )
            })?;

        let mut account = Account::new(
            chain,
            account_orm.address,
            account_orm.title,
            HashMap::new(),
            native_balance.balance,
            account_balances.clone(),
            code_orm.code,
            code_orm.hash,
            // TODO: remove balance_modify_tx from Account
            Bytes::zero(32),
            code_tx,
            None,
        );

        if include_slots {
            account.slots = self
                .get_contract_slots(&id.chain, Some(&[account.address.clone()]), version, conn)
                .await?
                .remove(&id.address)
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default()))
                .collect();
        }

        Ok(account)
    }

    #[instrument(level = Level::DEBUG, skip(self, ids, conn))]
    pub async fn get_contracts(
        &self,
        chain: &Chain,
        ids: Option<&[Address]>,
        version: Option<&Version>,
        include_slots: bool,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<Account>>, StorageError> {
        let chain_db_id = self.get_chain_id(chain)?;
        let version_ts = match &version {
            Some(version) => maybe_lookup_version_ts(version, conn).await?,
            None => Utc::now().naive_utc(),
        };

        let accounts = {
            use schema::account::dsl::*;
            let mut q = account
                .filter(chain_id.eq(chain_db_id))
                .filter(
                    created_at
                        .is_null() // can be null if the account was initially added as a token
                        .or(created_at.le(version_ts)),
                )
                .filter(
                    deleted_at
                        .is_null()
                        .or(deleted_at.gt(version_ts)),
                )
                .order_by(id)
                .select(orm::Account::as_select())
                .into_boxed();

            // if user passed any contract ids filter by those
            // else get all contracts
            if let Some(contract_ids) = ids {
                q = q.filter(address.eq_any(contract_ids));
            } else {
                // If no specific IDs requested, only get accounts that have code
                // This ensures pagination works correctly with the filtered results
                q = q.filter(
                    id.eq_any(
                        schema::contract_code::table
                            .filter(schema::contract_code::valid_from.le(version_ts))
                            .filter(
                                schema::contract_code::valid_to
                                    .is_null()
                                    .or(schema::contract_code::valid_to.gt(version_ts)),
                            )
                            .select(schema::contract_code::account_id),
                    ),
                );
            }

            // Apply pagination if provided
            if let Some(pagination) = pagination_params {
                q = q
                    .limit(pagination.page_size)
                    .offset(pagination.offset());
            }

            q.get_results::<orm::Account>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .collect::<Vec<_>>()
        };

        let mut all_balances = self
            .get_account_balances(chain, ids, version, true, conn)
            .await?;

        // take all ids and query both code and storage
        let account_ids = accounts
            .iter()
            .map(|a| a.id)
            .collect::<HashSet<_>>();

        let codes = {
            use schema::contract_code::dsl::*;
            contract_code
                .inner_join(schema::transaction::table)
                .filter(account_id.eq_any(&account_ids))
                .filter(valid_from.le(version_ts))
                .filter(
                    valid_to
                        .is_null()
                        .or(valid_to.gt(version_ts)),
                )
                .order_by((account_id, schema::transaction::index.desc()))
                .select((orm::ContractCode::as_select(), schema::transaction::hash))
                .distinct_on(account_id)
                .get_results::<(orm::ContractCode, Bytes)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .map(|(entity, tx)| WithTxHash { entity, tx: Some(tx) })
                .collect::<Vec<_>>()
        };

        // Create a map of account_id to code for efficient lookup
        let code_map: HashMap<i64, WithTxHash<orm::ContractCode>> = codes
            .into_iter()
            .map(|code| (code.entity.account_id, code))
            .collect();

        // Accounts are pre-filtered to have code in the all-IDs path. For the specific-IDs
        // path we filter here and warn about any gaps, which indicate a DB inconsistency
        // (e.g. an account inserted via the token path that never received a contract_code row).
        let filtered_accounts = if ids.is_some() && accounts.len() != code_map.len() {
            let missing: Vec<String> = accounts
                .iter()
                .filter(|a| !code_map.contains_key(&a.id))
                .map(|a| hex::encode(&a.address))
                .collect();
            warn!(
                missing_count = accounts.len() - code_map.len(),
                ?missing,
                "Accounts missing contract_code at this version; skipping them"
            );
            accounts
                .into_iter()
                .filter(|a| code_map.contains_key(&a.id))
                .collect()
        } else {
            accounts
        };

        let slots = if include_slots {
            Some(
                self.get_contract_slots(chain, ids, version, conn)
                    .await?,
            )
        } else {
            None
        };

        let res = filtered_accounts
            .into_iter()
            .map(|account| -> Result<Account, StorageError> {
                let code = code_map
                    .get(&account.id)
                    .ok_or_else(|| {
                        StorageError::Unexpected(format!(
                            "Code not found for account id: {}",
                            account.id
                        ))
                    })?;

                // Note: it is safe to call unwrap here since above we always wrap it into Some
                let code_tx = code.tx.clone().unwrap();

                let balances = all_balances
                    .get_mut(&account.address)
                    .ok_or_else(|| {
                        StorageError::NotFound(
                            "account_balances".to_string(),
                            account.address.to_string(),
                        )
                    })?;
                let native_balance = balances
                    .remove(&chain.native_token().address)
                    .ok_or_else(|| {
                        StorageError::NotFound(
                            "native_balance".to_string(),
                            account.address.to_string(),
                        )
                    })?;

                let mut contract = Account::new(
                    *chain,
                    account.address.clone(),
                    account.title.clone(),
                    HashMap::new(),
                    native_balance.balance,
                    balances.clone(),
                    code.entity.code.clone(),
                    code.entity.hash.clone(),
                    // TODO: remove balance_modify_tx from Account
                    Bytes::zero(32),
                    code_tx,
                    None,
                );

                if let Some(storage) = &slots {
                    if let Some(contract_slots) = storage.get(&contract.address) {
                        contract.slots = contract_slots
                            .clone()
                            .into_iter()
                            .map(|(k, v)| (k, v.unwrap_or_default()))
                            .collect();
                    }
                }

                Ok(contract)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // TODO: Try to reduce the duplication
        // Calculate total count based on whether pagination was used
        let total_count = if pagination_params.is_some() {
            // If pagination was used, we need to query the total count
            use schema::account::dsl::*;
            let mut count_q = account
                .filter(chain_id.eq(chain_db_id))
                .filter(
                    created_at
                        .is_null() // can be null if the account was initially added as a token
                        .or(created_at.le(version_ts)),
                )
                .filter(
                    deleted_at
                        .is_null()
                        .or(deleted_at.gt(version_ts)),
                )
                .select(diesel::dsl::count(id))
                .into_boxed();

            // Apply contract id filters if provided
            if let Some(contract_ids) = ids {
                count_q = count_q.filter(address.eq_any(contract_ids));
            } else {
                // If no specific IDs requested, only count accounts that have code
                // This matches the filtering logic applied to the main query
                count_q = count_q.filter(
                    id.eq_any(
                        schema::contract_code::table
                            .filter(schema::contract_code::valid_from.le(version_ts))
                            .filter(
                                schema::contract_code::valid_to
                                    .is_null()
                                    .or(schema::contract_code::valid_to.gt(version_ts)),
                            )
                            .select(schema::contract_code::account_id),
                    ),
                );
            }

            count_q
                .get_result::<i64>(conn)
                .await
                .map_err(PostgresError::from)?
        } else {
            // If no pagination, use the length of the result
            res.len() as i64
        };

        Ok(WithTotal { entity: res, total: Some(total_count) })
    }

    /// Insert contract
    ///
    /// Inserts a contract. It will not insert contract code, slots or balance since a separate
    /// method exists for updating these related components.
    pub async fn insert_contract(
        &self,
        new: &Account,
        db: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let (creation_tx_id, created_ts) = if let Some(h) = &new.creation_tx {
            let (tx_id, ts) = schema::transaction::table
                .inner_join(schema::block::table)
                .filter(schema::transaction::hash.eq(h.clone()))
                .select((schema::transaction::id, schema::block::ts))
                .first::<(i64, NaiveDateTime)>(db)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "Transaction",
                        &hex::encode(h),
                        Some("Account".into()),
                    )
                })?;

            (Some(tx_id), ts)
        } else {
            (None, chrono::Utc::now().naive_utc())
        };

        let chain_id = self.get_chain_id(&new.chain)?;
        let new_contract = orm::NewContract {
            title: new.title.clone(),
            address: new.address.clone(),
            chain_id,
            creation_tx: creation_tx_id,
            created_at: Some(created_ts),
            deleted_at: None,
        };
        let hex_addr = hex::encode(&new.address);

        diesel::insert_into(schema::account::table)
            .values(new_contract.new_account())
            .on_conflict_do_nothing()
            .execute(db)
            .await
            .map_err(|err| {
                error!("Failed inserting account");
                storage_error_from_diesel(err, "Account", &hex_addr, None)
            })?;

        Ok(())
    }

    pub async fn update_contracts(
        &self,
        chain: &Chain,
        new: &[(Address, &AccountDelta)],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let new = new
            .iter()
            .map(|(tx, delta)| WithTxHash { entity: delta, tx: Some(tx.to_owned()) })
            .collect::<Vec<_>>();

        let txns: HashMap<Bytes, (i64, NaiveDateTime, i64)> = async {
            schema::transaction::table
                .inner_join(schema::block::table)
                .filter(schema::transaction::hash.eq_any(new.iter().filter_map(|u| u.tx.as_ref())))
                .select((
                    schema::transaction::hash,
                    (schema::transaction::id, schema::block::ts, schema::transaction::index),
                ))
                .get_results::<(Bytes, (i64, NaiveDateTime, i64))>(conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_tx_ids"))
        .await?
        .into_iter()
        .collect();

        let addresses: Result<Vec<Bytes>, StorageError> = new
            .iter()
            .map(|u| {
                let id = u.contract_id();
                if id.chain != *chain {
                    return Err(StorageError::Unsupported(format!(
                        "Updating contracts of different chains with a single query  \
                    is not supported. Expected: {}, got: {}!",
                        chain, id.chain
                    )));
                }
                Ok(id.address)
            })
            .collect();

        let accounts: HashMap<_, _> = async {
            schema::account::table
                .filter(schema::account::chain_id.eq(chain_id))
                .filter(schema::account::address.eq_any(addresses?))
                .select((schema::account::address, schema::account::id))
                .get_results::<(Bytes, i64)>(conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_account_ids"))
        .await?
        .into_iter()
        .collect();

        let mut balance_data = Vec::new();
        let mut code_data = Vec::new();
        let mut slot_data: HashMap<i64, AccountToContractStoreDeltas> = HashMap::new();

        for delta in new.iter() {
            let contract_id = delta.contract_id();
            let account_id = *accounts
                .get(&contract_id.address)
                .ok_or_else(|| {
                    error!("Failed to get account while updating slots!");
                    StorageError::NotFound("Account".to_owned(), hex::encode(&contract_id.address))
                })?;

            let tx_hash = delta.tx.as_ref().unwrap();
            let (tx_id, ts, index) = *txns.get(tx_hash).ok_or_else(|| {
                StorageError::NoRelatedEntity(
                    "Transaction".to_owned(),
                    "Account".to_owned(),
                    hex::encode(tx_hash),
                )
            })?;

            if let Some(new_balance) = delta.balance.clone() {
                let new = orm::NewAccountBalance {
                    balance: new_balance,
                    account_id,
                    token_id: self.get_native_token_id(chain)?,
                    modify_tx: tx_id,
                    valid_from: ts,
                    valid_to: None,
                };
                balance_data.push(WithOrdinal::new(new, (account_id, ts, index)));
            }

            if let Some(new_code) = delta.code() {
                let hash = keccak256(new_code.clone());
                let new = orm::NewContractCode {
                    code: new_code,
                    hash: hash.into(),
                    account_id,
                    modify_tx: tx_id,
                    valid_from: ts,
                    valid_to: None,
                };
                code_data.push(WithOrdinal::new(new, (account_id, ts, index)));
            }

            let slots = delta.slots.clone();
            if !slots.is_empty() {
                match slot_data.entry(tx_id) {
                    Entry::Occupied(mut e) => {
                        let v = e.get_mut();
                        if v.contains_key(&contract_id.address) {
                            return Err(StorageError::Unexpected(format!("Ambiguous update! Contract 0x{} received different updates in same tx!", hex::encode(&contract_id.address))));
                        }
                        v.extend([(contract_id.address.clone(), slots)]);
                    }

                    Entry::Vacant(e) => {
                        let v = [(contract_id.address.clone(), slots)]
                            .into_iter()
                            .collect();
                        e.insert(v);
                    }
                }
            }
        }

        if !balance_data.is_empty() {
            async {
                balance_data.sort_by_cached_key(|b| b.ordinal);
                let mut sorted = balance_data
                    .into_iter()
                    .map(|b| b.entity)
                    .collect::<Vec<_>>();
                apply_versioning::<_, orm::AccountBalance>(&mut sorted, conn).await?;
                diesel::insert_into(schema::account_balance::table)
                    .values(&sorted)
                    .execute(conn)
                    .await
                    .map_err(PostgresError::from)?;
                Ok::<(), StorageError>(())
            }
            .instrument(debug_span!("insert_native_balances"))
            .await?;
        }
        if !code_data.is_empty() {
            async {
                code_data.sort_by_cached_key(|b| b.ordinal);
                let mut sorted = code_data
                    .into_iter()
                    .map(|b| b.entity)
                    .collect::<Vec<_>>();
                apply_versioning::<_, orm::ContractCode>(&mut sorted, conn).await?;
                diesel::insert_into(schema::contract_code::table)
                    .values(&sorted)
                    .execute(conn)
                    .await
                    .map_err(PostgresError::from)?;
                Ok::<(), StorageError>(())
            }
            .instrument(debug_span!("insert_contract_code"))
            .await?;
        }

        if !slot_data.is_empty() {
            self.upsert_slots(slot_data, conn)
                .instrument(debug_span!("upsert_slots"))
                .await?;
        }
        Ok(())
    }

    pub async fn delete_contract(
        &self,
        id: &ContractId,
        at_tx: &TxHash,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let account = orm::Account::by_id(id, conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Account", &id.to_string(), None))?;
        let tx = orm::Transaction::by_hash(at_tx, conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(
                    err,
                    "Account",
                    &hex::encode(at_tx),
                    Some("Transaction".to_owned()),
                )
            })?;
        let block_ts = schema::block::table
            .filter(schema::block::id.eq(tx.block_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .map_err(PostgresError::from)?;
        if let Some(tx_id) = account.deletion_tx {
            if tx.id != tx_id {
                return Err(StorageError::Unexpected(format!(
                    "Account {} was already deleted at {:?}!",
                    hex::encode(account.address),
                    account.deleted_at,
                )));
            }
            // Noop if called twice on deleted contract
            return Ok(());
        };
        diesel::update(schema::account::table.filter(schema::account::id.eq(account.id)))
            .set((schema::account::deletion_tx.eq(tx.id), schema::account::deleted_at.eq(block_ts)))
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;
        diesel::update(
            schema::contract_storage::table
                .filter(schema::contract_storage::account_id.eq(account.id)),
        )
        .set(schema::contract_storage::valid_to.eq(block_ts))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        diesel::update(
            schema::account_balance::table
                .filter(schema::account_balance::account_id.eq(account.id)),
        )
        .set(schema::account_balance::valid_to.eq(block_ts))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        diesel::update(
            schema::contract_code::table.filter(schema::contract_code::account_id.eq(account.id)),
        )
        .set(schema::contract_code::valid_to.eq(block_ts))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;
        Ok(())
    }

    #[instrument(level = Level::DEBUG, skip(self, conn))]
    pub async fn get_accounts_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        target_version: &BlockOrTimestamp,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<AccountDelta>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        // To support blocks as versions, we need to ingest all blocks, else the
        // below method can error for any blocks that are not present.
        let start_version_ts = match start_version {
            Some(version) => maybe_lookup_block_ts(version, conn).await?,
            None => Utc::now().naive_utc(),
        };
        let target_version_ts = maybe_lookup_block_ts(target_version, conn).await?;

        let balance_deltas = self
            .get_balance_deltas_internal(chain, &start_version_ts, &target_version_ts, conn)
            .await?;
        let code_deltas = self
            .get_code_deltas(chain_id, &start_version_ts, &target_version_ts, conn)
            .await?;
        let slot_deltas = self
            .get_slots_delta(chain_id, &start_version_ts, &target_version_ts, conn)
            .await?;
        let account_deltas = self
            .get_created_or_deleted_accounts(chain, &start_version_ts, &target_version_ts, conn)
            .await?;

        // We retrieve account addresses separately because this is more
        // efficient for the most common cases. In the most common case, only a
        // handful of accounts that we are interested in will have had changes
        // that need to be reverted. The previous query only returns duplicated
        // account ids, which are lighweight (8 byte vs 20 for addresses), once
        // deduplicated we only fetch the associated addresses. These addresses
        // are considered immutable, so if necessary we could event cache these
        // locally.
        // In the worst case each changed slot is changed on a different
        // account. On mainnet that would be at max 300 contracts/slots, which
        // although not ideal is still bearable.
        let account_addresses = schema::account::table
            .filter(
                schema::account::id.eq_any(
                    balance_deltas
                        .keys()
                        .chain(code_deltas.keys())
                        .chain(slot_deltas.keys())
                        .collect::<HashSet<_>>(),
                ),
            )
            .select((schema::account::id, schema::account::address))
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?;

        let deltas = account_addresses
            .into_iter()
            .map(|(id, address)| -> Result<_, StorageError> {
                let slots = slot_deltas.get(&id);
                let state = if account_deltas
                    .created
                    .contains(&address)
                {
                    ChangeType::Creation
                } else {
                    ChangeType::Update
                };

                let update = AccountDelta::new(
                    *chain,
                    address.clone(),
                    slots.cloned().unwrap_or_default(),
                    balance_deltas.get(&id).cloned(),
                    code_deltas.get(&id).cloned(),
                    state,
                );
                Ok((address, update))
            })
            .chain(
                account_deltas
                    .restored
                    .into_iter()
                    .map(Ok),
            )
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(deltas.into_values().collect())
    }

    pub async fn add_account_balances(
        &self,
        account_balances: &[AccountBalance],
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let chain_id = self.get_chain_id(chain)?;

        // fetch linked tokens
        let token_addresses = account_balances
            .iter()
            .map(|b| b.token.clone())
            .collect::<Vec<_>>();
        let token_count = token_addresses.len();
        let token_ids: HashMap<Address, i64> = async {
            schema::token::table
                .inner_join(schema::account::table)
                .select((schema::account::address, schema::token::id))
                .filter(schema::account::address.eq_any(&token_addresses))
                .load::<(Address, i64)>(conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_token_ids", count = token_count))
        .await?
        .into_iter()
        .collect();

        // fetch linked transactions
        let modify_txs = account_balances
            .iter()
            .map(|b| b.modify_tx.clone())
            .collect::<Vec<_>>();
        let transaction_ids_and_ts = async {
            orm::Transaction::ids_and_ts_by_hash(modify_txs.iter(), conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_tx_ids"))
        .await?
        .into_iter()
        .map(|(db_id, hash, index, ts)| (hash, (db_id, index, ts)))
        .collect::<HashMap<TxHash, (i64, i64, NaiveDateTime)>>();

        // fetch linked accounts
        let account_addresses = account_balances
            .iter()
            .map(|b| b.account.clone())
            .collect::<Vec<_>>();
        let account_ids = async {
            orm::Account::ids_by_addresses(account_addresses.iter(), chain_id, conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_account_ids"))
        .await?
        .into_iter()
        .map(|(id, address)| (address, id))
        .collect::<HashMap<Address, i64>>();

        // collect account balance updates
        let mut new_account_balances = Vec::new();
        for balance in account_balances.iter() {
            let token_id = *token_ids
                .get(&balance.token)
                .ok_or_else(|| {
                    StorageError::NotFound("Token".to_owned(), hex::encode(&balance.token))
                })?;
            let account_id = *account_ids
                .get(&balance.account)
                .ok_or_else(|| {
                    StorageError::NotFound("Account".to_owned(), hex::encode(&balance.account))
                })?;
            let (tx_id, tx_index, tx_ts) = *transaction_ids_and_ts
                .get(&balance.modify_tx)
                .ok_or_else(|| {
                    StorageError::NotFound(
                        "Transaction".to_owned(),
                        hex::encode(&balance.modify_tx),
                    )
                })?;
            let new_account_balance = orm::NewAccountBalance {
                balance: balance.balance.clone(),
                account_id,
                token_id,
                modify_tx: tx_id,
                valid_from: tx_ts,
                valid_to: None,
            };
            new_account_balances.push(WithOrdinal::new(
                new_account_balance,
                (account_id, token_id, tx_ts, tx_index),
            ));
        }

        // insert account balances
        if !account_balances.is_empty() {
            async {
                new_account_balances.sort_by_cached_key(|b| b.ordinal);
                let mut sorted = new_account_balances
                    .into_iter()
                    .map(|b| b.entity)
                    .collect::<Vec<_>>();
                apply_versioning::<_, orm::AccountBalance>(&mut sorted, conn).await?;

                // Insert in batches to avoid exceeding PostgreSQL parameter limit
                for chunk in sorted.chunks(orm::NewAccountBalance::MAX_BATCH_SIZE) {
                    diesel::insert_into(schema::account_balance::table)
                        .values(chunk)
                        .execute(conn)
                        .await
                        .map_err(|err| {
                            storage_error_from_diesel(err, "AccountBalance", "batch", None)
                        })?;
                }
                Ok::<(), StorageError>(())
            }
            .instrument(debug_span!("insert_account_balances"))
            .await?;
        }

        Ok(())
    }

    pub async fn get_account_balances(
        &self,
        chain: &Chain,
        accounts: Option<&[Address]>,
        at: Option<&Version>,
        include_native: bool,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        // NOTE: the returned AccountBalances have a default value for tx_hash as it is assumed
        // the caller does not need them and we get a large performance boost by skipping them.

        let version_ts = match &at {
            Some(version) => Some(maybe_lookup_version_ts(version, conn).await?),
            None => None,
        };
        let chain_id = self.get_chain_id(chain)?;

        // NOTE: the balances query is split into 3 separate queries to avoid excessive table joins
        // and improve performance. The queries are as follows:

        // Query 1: account db ids
        let mut account_query = schema::account::table
            .filter(schema::account::chain_id.eq(chain_id))
            .select((schema::account::id, schema::account::address))
            .into_boxed();
        if let Some(accounts) = accounts {
            account_query = account_query.filter(schema::account::address.eq_any(accounts));
        }
        let account_ids = account_query
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect::<HashMap<_, _>>();

        // Query 2: balances
        let mut balance_query = schema::account_balance::table
            .filter(schema::account_balance::account_id.eq_any(account_ids.keys()))
            .filter(
                schema::account_balance::valid_to
                    .gt(version_ts.unwrap_or(*MAX_VERSION_TS))
                    .or(schema::account_balance::valid_to.is_null()),
            )
            .into_boxed();
        // if a version timestamp is provided, we want to filter by valid_from <= version_ts
        if let Some(ts) = version_ts {
            balance_query = balance_query.filter(schema::account_balance::valid_from.le(ts));
        }
        let balances_map = balance_query
            .select((
                schema::account_balance::account_id,
                schema::account_balance::token_id,
                schema::account_balance::balance,
            ))
            .order(schema::account_balance::account_id.asc())
            .get_results::<(i64, i64, Balance)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .group_by(|e| e.0)
            .into_iter()
            .map(|(acc_id, balances)| {
                account_ids
                    .get(&acc_id)
                    .cloned() // Avoid needing .clone() later
                    .ok_or_else(|| StorageError::NotFound("Account".to_owned(), acc_id.to_string()))
                    .map(|account| {
                        (
                            account,
                            balances
                                .map(|(_, tid, bal)| (tid, bal))
                                .collect::<HashMap<_, _>>(),
                        )
                    })
            })
            .collect::<Result<HashMap<_, _>, StorageError>>()?;

        // Query 3: token addresses
        let tokens = schema::token::table
            .inner_join(schema::account::table)
            .filter(
                schema::token::id.eq_any(
                    balances_map
                        .values()
                        .flat_map(|b| b.keys())
                        .collect::<Vec<_>>(),
                ),
            )
            .select((schema::token::id, schema::account::address))
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect::<HashMap<_, _>>();

        // Replace token ids with addresses
        let mut balances = HashMap::with_capacity(balances_map.len());
        for (account, balance_map) in balances_map {
            let mut new_balance_map = HashMap::new();
            for (token_id, balance) in balance_map {
                let token_address = tokens
                    .get(&token_id)
                    .ok_or_else(|| {
                        StorageError::NotFound("Token".to_owned(), token_id.to_string())
                    })?
                    .clone();
                if !include_native && token_address == chain.native_token().address {
                    // Skip native token balances if not requested
                    continue;
                }
                let account_balance = AccountBalance::new(
                    account.clone(),
                    token_address.clone(),
                    balance,
                    TxHash::from(
                        "0x0000000000000000000000000000000000000000000000000000000000000000",
                    ),
                );
                new_balance_map.insert(token_address, account_balance);
            }
            balances.insert(account, new_balance_map);
        }

        Ok(balances)
    }
}

/// Tests for PostgresGateway's ContractStateGateway methods
///
/// The tests below test the functionality using the concrete EVM types.
#[cfg(test)]
mod test {
    use std::{str::FromStr, time::Duration};

    use diesel_async::AsyncConnection;
    use rstest::rstest;
    use tycho_common::{
        models::{FinancialType, ImplementationType},
        storage::{BlockIdentifier, VersionKind},
    };

    use super::*;
    use crate::postgres::db_fixtures;

    type EVMGateway = PostgresGateway;
    type MaybeTS = Option<NaiveDateTime>;

    async fn setup_db() -> AsyncPgConnection {
        let db_url = std::env::var("DATABASE_URL").unwrap();
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .unwrap();
        conn.begin_test_transaction()
            .await
            .unwrap();
        conn
    }

    /// This sets up the data needed to test the gateway and returns the DB id of the inserted
    /// chain.
    ///
    /// The setup is structured such that each accounts historical changes are kept together this
    /// makes it easy to reason about that change an account should have at each version. Please
    /// note that if you change something here, also update the account fixtures right below, which
    /// contain account states at each version.
    async fn setup_data(conn: &mut AsyncPgConnection) -> i64 {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        // add block 3 with no linked data (to be used to test updates)
        diesel::insert_into(schema::block::table)
            .values((
                schema::block::hash.eq(Vec::from(
                    Bytes::from_str(
                        "f2d7c8b6e3a1905f4c8d26b7e9513a0d7f8e2c9b1a6d5e4f3c2b1a0e9d8c7f61",
                    )
                    .unwrap(),
                )),
                schema::block::parent_hash.eq(Vec::from(
                    Bytes::from_str(
                        "b495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9",
                    )
                    .unwrap(),
                )),
                schema::block::number.eq(3),
                schema::block::ts.eq(db_fixtures::yesterday_one_am()),
                schema::block::chain_id.eq(chain_id),
            ))
            .execute(conn)
            .await
            .unwrap();
        let ts = db_fixtures::yesterday_midnight();
        let ts_p1 = db_fixtures::yesterday_half_past_midnight();
        let tx_hashes = [
            "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945".to_string(),
            "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54".to_string(),
            "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7".to_string(),
            "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388".to_string(),
        ];
        let txn = db_fixtures::insert_txns(
            conn,
            &[
                // deploy c0
                (blk[0], 1i64, &tx_hashes[0]),
                // change c0 state, deploy c2
                (blk[0], 2i64, &tx_hashes[1]),
                // ----- Block 01 LAST
                // deploy c1, delete c2
                (blk[1], 1i64, &tx_hashes[2]),
                // change c0 and c1 state
                (blk[1], 2i64, &tx_hashes[3]),
                // ----- Block 02 LAST
            ],
        )
        .await;
        let (_, native_token) = db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;

        // Account C0
        let c0 = db_fixtures::insert_account(
            conn,
            "6B175474E89094C44Da98b954EedeAC495271d0F",
            "account0",
            chain_id,
            Some(txn[0]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 0, native_token, txn[0], Some(&ts), c0).await;
        let c0_code =
            db_fixtures::insert_contract_code(conn, c0, txn[0], Bytes::from("C0C0C0")).await;
        db_fixtures::insert_account_balance(conn, 100, native_token, txn[1], Some(&ts_p1), c0)
            .await;
        // Slot 2 is never modified again
        db_fixtures::insert_slots(conn, c0, txn[1], &ts, None, &[(2, 1, None)]).await;
        // First version for slots 0 and 1.
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[1],
            &ts,
            Some(&ts_p1),
            &[(0, 1, None), (1, 5, None)],
        )
        .await;
        db_fixtures::insert_account_balance(conn, 101, native_token, txn[3], None, c0).await;
        // Second and final version for 0 and 1, new slots 5 and 6
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[3],
            &ts_p1,
            None,
            &[(0, 2, Some(1)), (1, 3, Some(5)), (5, 25, None), (6, 30, None)],
        )
        .await;
        let (_, usdc_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "USDC",
            18,
            Some(100),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 1000, usdc_id, txn[0], None, c0).await;
        let (_, weth_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            "WETH",
            18,
            Some(100),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 2000, weth_id, txn[0], None, c0).await;

        // Account C1
        let c1 = db_fixtures::insert_account(
            conn,
            "73BcE791c239c8010Cd3C857d96580037CCdd0EE",
            "c1",
            chain_id,
            Some(txn[2]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 50, native_token, txn[2], None, c1).await;
        let c1_code =
            db_fixtures::insert_contract_code(conn, c1, txn[2], Bytes::from("C1C1C1")).await;
        db_fixtures::insert_slots(
            conn,
            c1,
            txn[3],
            &ts_p1,
            None,
            &[(0, 128, None), (1, 255, None)],
        )
        .await;

        // Account C2
        let c2 = db_fixtures::insert_account(
            conn,
            "94a3F312366b8D0a32A00986194053C0ed0CdDb1",
            "c2",
            chain_id,
            Some(txn[1]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 25, native_token, txn[1], None, c2).await;
        let c2_code =
            db_fixtures::insert_contract_code(conn, c2, txn[1], Bytes::from("C2C2C2")).await;
        db_fixtures::insert_slots(conn, c2, txn[1], &ts, None, &[(1, 2, None), (2, 4, None)]).await;
        db_fixtures::delete_account(conn, c2, &ts_p1).await;

        // linked protocol components
        let protocol_system_id =
            db_fixtures::insert_protocol_system(conn, "ambient".to_owned()).await;
        let protocol_type_id = db_fixtures::insert_protocol_type(
            conn,
            "Pool",
            Some(FinancialType::Swap),
            None,
            Some(ImplementationType::Custom),
        )
        .await;
        let _ = db_fixtures::insert_protocol_component(
            conn,
            "component1",
            chain_id,
            protocol_system_id,
            protocol_type_id,
            txn[0],
            None,
            Some(vec![c0_code, c1_code, c2_code]),
        )
        .await;

        chain_id
    }

    fn account_c0(version: u64) -> Account {
        match version {
            1 => Account::new(
                Chain::Ethereum,
                "0x6b175474e89094c44da98b954eedeac495271d0f"
                    .parse()
                    .unwrap(),
                "account0".to_owned(),
                contract_slots([(1, 5), (2, 1), (0, 1)])
                    .into_iter()
                    .map(|(k, v)| (k, v.unwrap_or(Bytes::from("0x00"))))
                    .collect(),
                Bytes::from(100u64).lpad(32, 0),
                vec![
                    (
                        "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                            .parse()
                            .unwrap(),
                        AccountBalance::new(
                            "0x6b175474e89094c44da98b954eedeac495271d0f"
                                .parse()
                                .unwrap(),
                            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                                .parse()
                                .unwrap(),
                            Bytes::from(1000_i32.to_be_bytes()).lpad(32, 0),
                            Bytes::zero(32),
                        ),
                    ),
                    (
                        "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                            .parse()
                            .unwrap(),
                        AccountBalance::new(
                            "0x6b175474e89094c44da98b954eedeac495271d0f"
                                .parse()
                                .unwrap(),
                            "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                                .parse()
                                .unwrap(),
                            Bytes::from(2000_i32.to_be_bytes()).lpad(32, 0),
                            Bytes::zero(32),
                        ),
                    ),
                ]
                .into_iter()
                .collect(),
                Bytes::from("C0C0C0"),
                "0x106781541fd1c596ade97569d584baf47e3347d3ac67ce7757d633202061bdc4"
                    .parse()
                    .unwrap(),
                Bytes::zero(32),
                "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945"
                    .parse()
                    .unwrap(),
                None,
            ),
            2 => Account::new(
                Chain::Ethereum,
                "0x6b175474e89094c44da98b954eedeac495271d0f"
                    .parse()
                    .unwrap(),
                "account0".to_owned(),
                contract_slots([(6, 30), (5, 25), (1, 3), (2, 1), (0, 2)])
                    .into_iter()
                    .map(|(k, v)| (k, v.unwrap_or(Bytes::from("0x00"))))
                    .collect(),
                Bytes::from(101u64).lpad(32, 0),
                vec![
                    (
                        "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                            .parse()
                            .unwrap(),
                        AccountBalance::new(
                            "0x6b175474e89094c44da98b954eedeac495271d0f"
                                .parse()
                                .unwrap(),
                            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                                .parse()
                                .unwrap(),
                            Bytes::from(1000_i32.to_be_bytes()).lpad(32, 0),
                            Bytes::zero(32),
                        ),
                    ),
                    (
                        "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                            .parse()
                            .unwrap(),
                        AccountBalance::new(
                            "0x6b175474e89094c44da98b954eedeac495271d0f"
                                .parse()
                                .unwrap(),
                            "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                                .parse()
                                .unwrap(),
                            Bytes::from(2000_i32.to_be_bytes()).lpad(32, 0),
                            Bytes::zero(32),
                        ),
                    ),
                ]
                .into_iter()
                .collect(),
                Bytes::from("C0C0C0"),
                "0x106781541fd1c596ade97569d584baf47e3347d3ac67ce7757d633202061bdc4"
                    .parse()
                    .unwrap(),
                Bytes::zero(32),
                "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945"
                    .parse()
                    .unwrap(),
                None,
            ),
            _ => panic!("No version found"),
        }
    }

    fn contract_slots(data: impl IntoIterator<Item = (i32, i32)>) -> HashMap<Bytes, Option<Bytes>> {
        data.into_iter()
            .map(|(s, v)| {
                let val = if v == 0 {
                    None
                } else {
                    Some(Bytes::from(u32::try_from(v).unwrap()).lpad(32, 0))
                };
                (Bytes::from(u32::try_from(s).unwrap()).lpad(32, 0), val)
            })
            .collect()
    }

    fn account_c1(version: u64) -> Account {
        match version {
            2 => Account::new(
                Chain::Ethereum,
                "0x73bce791c239c8010cd3c857d96580037ccdd0ee"
                    .parse()
                    .unwrap(),
                "c1".to_owned(),
                contract_slots([(1, 255), (0, 128)])
                    .into_iter()
                    .map(|(k, v)| (k, v.unwrap_or(Bytes::from("0x00"))))
                    .collect(),
                Bytes::from(50u64).lpad(32, 0),
                HashMap::new(),
                Bytes::from("C1C1C1"),
                "0xa04b84acdf586a694085997f32c4aa11c2726a7f7e0b677a27d44d180c08e07f"
                    .parse()
                    .unwrap(),
                Bytes::zero(32),
                "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7"
                    .parse()
                    .unwrap(),
                None,
            ),
            _ => panic!("No version found"),
        }
    }

    fn account_c2(version: u64) -> Account {
        match version {
            1 => Account::new(
                Chain::Ethereum,
                "0x94a3f312366b8d0a32a00986194053c0ed0cddb1"
                    .parse()
                    .unwrap(),
                "c2".to_owned(),
                contract_slots([(1, 2), (2, 4)])
                    .into_iter()
                    .map(|(k, v)| (k, v.unwrap_or(Bytes::from("0x00"))))
                    .collect(),
                Bytes::from(25u64).lpad(32, 0),
                HashMap::new(),
                Bytes::from("C2C2C2"),
                "0x7eb1e0ed9d018991eed6077f5be45b52347f6e5870728809d368ead5b96a1e96"
                    .parse()
                    .unwrap(),
                Bytes::zero(32),
                "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54"
                    .parse()
                    .unwrap(),
                None,
            ),
            _ => panic!("No version found"),
        }
    }

    #[rstest]
    #[case::with_slots(true)]
    #[case::without_slots(false)]
    #[tokio::test]
    async fn test_get_contract(#[case] include_slots: bool) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let acc_address = "6B175474E89094C44Da98b954EedeAC495271d0F";
        let mut expected = account_c0(2);
        if !include_slots {
            expected.slots.clear();
        }

        let gateway = EVMGateway::from_connection(&mut conn).await;
        let id = ContractId::new(Chain::Ethereum, Bytes::from(acc_address));
        let result = gateway
            .get_contract(&id, None, include_slots, &mut conn)
            .await
            .unwrap();

        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::empty(
    None,
    Some(Version::from_ts("2019-01-01T00:00:00".parse().unwrap())),
    vec ! [],
    )]
    #[case::only_c2_block_1(
    Some(vec ! [Bytes::from("94a3f312366b8d0a32a00986194053c0ed0cddb1")]),
    Some(Version::from_block_number(Chain::Ethereum, 1)),
    vec ! [
    account_c2(1)
    ],
    )]
    #[case::all_ids_block_1(
    None,
    Some(Version::from_block_number(Chain::Ethereum, 1)),
    vec ! [
    account_c0(1),
    account_c2(1)
    ],
    )]
    #[case::only_c0_latest(
    Some(vec ! [Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F")]),
    None,
    vec ! [
    account_c0(2)
    ],
    )]
    #[case::all_ids_latest(
    None,
    None,
    vec ! [
    account_c0(2),
    account_c1(2)
    ],
    )]
    #[tokio::test]
    async fn test_get_contracts(
        #[case] ids: Option<Vec<Bytes>>,
        #[case] version: Option<Version>,
        #[case] exp: Vec<Account>,
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let addresses = ids.as_deref();

        let results = gw
            .get_contracts(&Chain::Ethereum, addresses, version.as_ref(), true, None, &mut conn)
            .await
            .unwrap()
            .entity;

        assert_eq!(results, exp);
    }

    #[rstest]
    #[case::empty(
    None,
    Some(Version::from_ts("2019-01-01T00:00:00".parse().unwrap())),
    vec ! [],
    0
    )]
    #[case::only_c2_block_1(
    Some(vec ! [Bytes::from("94a3f312366b8d0a32a00986194053c0ed0cddb1")]),
    Some(Version::from_block_number(Chain::Ethereum, 1)),
    vec ! [
    account_c2(1)
    ],
    1
    )]
    #[case::all_ids_block_1(
    None,
    Some(Version::from_block_number(Chain::Ethereum, 1)),
    vec ! [
    account_c0(1),
    ],
    2
    )]
    #[case::only_c0_latest(
    Some(vec ! [Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F")]),
    None,
    vec ! [
    account_c0(2)
    ],
    1
    )]
    #[case::all_ids_latest(
    None,
    None,
    vec ! [
    account_c0(2),
    ],
    2
    )]
    #[tokio::test]
    async fn test_get_contracts_with_pagination(
        #[case] ids: Option<Vec<Bytes>>,
        #[case] version: Option<Version>,
        #[case] exp: Vec<Account>,
        #[case] exp_total: i64,
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let addresses = ids.as_deref();

        let result = gw
            .get_contracts(
                &Chain::Ethereum,
                addresses,
                version.as_ref(),
                true,
                Some(&PaginationParams { page: 0, page_size: 1 }),
                &mut conn,
            )
            .await
            .unwrap();

        assert!(result.entity.len() <= 1);
        assert_eq!(result.total, Some(exp_total));
        assert_eq!(result.entity, exp);
    }

    #[tokio::test]
    async fn test_get_missing_account() {
        let mut conn = setup_db().await;
        let gateway = EVMGateway::from_connection(&mut conn).await;
        let contract_id = ContractId::new(
            Chain::Ethereum,
            Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F"),
        );
        let result = gateway
            .get_contract(&contract_id, None, false, &mut conn)
            .await;
        if let Err(StorageError::NotFound(entity, id)) = result {
            assert_eq!(entity, "Account");
            assert_eq!(id, hex::encode(contract_id.address));
        } else {
            panic!("Expected NotFound error");
        }
    }

    #[tokio::test]
    async fn test_insert_contract() {
        let mut conn = setup_db().await;
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
        db_fixtures::insert_token(
            &mut conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let gateway = EVMGateway::from_connection(&mut conn).await;
        let blk = db_fixtures::insert_blocks(&mut conn, chain_id).await;
        db_fixtures::insert_txns(
            &mut conn,
            &[
                (
                    blk[0],
                    1i64,
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                (
                    blk[1],
                    1i64,
                    "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7",
                ),
            ],
        )
        .await;
        let code = Bytes::from("1234");
        let code_hash = Bytes::from(&keccak256(&code));
        let expected = Account::new(
            Chain::Ethereum,
            "6B175474E89094C44Da98b954EedeAC495271d0F"
                .parse()
                .expect("address ok"),
            "NewAccount".to_owned(),
            HashMap::new(),
            Bytes::from("0x64"),
            HashMap::new(),
            code,
            code_hash,
            Bytes::zero(32),
            "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7"
                .parse()
                .expect("txhash ok"),
            None,
        );
        gateway
            .insert_contract(&expected, &mut conn)
            .await
            .unwrap();
        let update: AccountDelta = expected.clone().into();
        gateway
            .update_contracts(
                &expected.chain,
                &[(expected.code_modify_tx.clone(), &update)],
                &mut conn,
            )
            .await
            .unwrap();

        let contract_id = ContractId::new(
            Chain::Ethereum,
            Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F"),
        );
        let actual = gateway
            .get_contract(&contract_id, None, true, &mut conn)
            .await
            .unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_update_contracts() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let modify_txhash = "62f4d4f29d10db8722cb66a2adb0049478b11988c8b43cd446b755afb8954678";
        let tx_hash_bytes = Bytes::from(modify_txhash);
        let block = orm::Block::by_number(Chain::Ethereum, 3, &mut conn)
            .await
            .expect("block found");
        db_fixtures::insert_txns(&mut conn, &[(block.id, 100, modify_txhash)]).await;
        let mut account = account_c1(2);
        account.set_balance(&Bytes::from("0x2710"), &Bytes::zero(32));
        let update = AccountDelta::new(
            account.chain,
            account.address.clone(),
            HashMap::new(),
            Some(Bytes::from("0x2710")),
            None,
            ChangeType::Update,
        );

        let contract_id = ContractId::new(Chain::Ethereum, account.address.clone());

        gw.update_contracts(&Chain::Ethereum, &[(tx_hash_bytes, &update)], &mut conn)
            .await
            .expect("upsert success");

        // ensure we did not modify code and the current version remains valid
        let code_versions = orm::ContractCode::all_versions(&contract_id.address, &mut conn)
            .await
            .expect("fetch code versions ok");
        assert_eq!(code_versions.len(), 1);
        assert_eq!(
            code_versions
                .iter()
                .filter(|v| v.valid_to.is_none())
                .count(),
            1
        );
        // ensure we modified balance and there is 1 currently valid version
        let balance_versions = orm::AccountBalance::all_versions(&contract_id.address, &mut conn)
            .await
            .expect("fetch balance version ok");
        assert_eq!(balance_versions.len(), 2);
        assert_eq!(
            balance_versions
                .iter()
                .filter(|v| v.valid_to.is_none())
                .count(),
            1
        );
        // ensure we can still get the contract as usual
        // get contract used below to compare does not include slots
        account.slots = HashMap::new();
        let updated = gw
            .get_contract(&contract_id, None, false, &mut conn)
            .await
            .expect("updated in db");
        assert_eq!(updated, account);
    }

    #[tokio::test]
    async fn test_delete_contract() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let address = "6B175474E89094C44Da98b954EedeAC495271d0F";
        let deletion_tx = "36984d97c02a98614086c0f9e9c4e97f7e0911f6f136b3c8a76d37d6d524d1e5";
        let address_bytes = Bytes::from(address);
        let id = ContractId::new(Chain::Ethereum, address_bytes.clone());
        let gw = EVMGateway::from_connection(&mut conn).await;
        let tx_hash = Bytes::from(deletion_tx);
        let (block_id, block_ts) = schema::block::table
            .select((schema::block::id, schema::block::ts))
            .first::<(i64, NaiveDateTime)>(&mut conn)
            .await
            .expect("blockquery succeeded");
        db_fixtures::insert_txns(&mut conn, &[(block_id, 12, deletion_tx)]).await;

        gw.delete_contract(&id, &tx_hash, &mut conn)
            .await
            .unwrap();

        let res = schema::account::table
            .inner_join(schema::account_balance::table)
            .inner_join(schema::contract_code::table)
            .filter(schema::account::address.eq(address_bytes))
            .select((
                schema::account::deleted_at,
                schema::account_balance::valid_to,
                schema::contract_code::valid_to,
            ))
            .first::<(MaybeTS, MaybeTS, MaybeTS)>(&mut conn)
            .await
            .expect("retrieval query ok");
        assert_eq!(res, (Some(block_ts), Some(block_ts), Some(block_ts)));
    }

    fn bytes32(v: u8) -> Bytes {
        let mut arr = [0; 32];
        arr[31] = v;
        arr.into()
    }

    #[rstest]
    #[case::latest(
    None,
    None,
    [(
    Bytes::from("73bce791c239c8010cd3c857d96580037ccdd0ee"),
    vec ! [
    (bytes32(1u8), Some(bytes32(255u8))),
    (bytes32(0u8), Some(bytes32(128u8))),
    ]
    .into_iter()
    .collect(),
    ),
    (
    Bytes::from("6b175474e89094c44da98b954eedeac495271d0f"),
    vec ! [
    (bytes32(1u8), Some(bytes32(3u8))),
    (bytes32(5u8), Some(bytes32(25u8))),
    (bytes32(2u8), Some(bytes32(1u8))),
    (bytes32(6u8), Some(bytes32(30u8))),
    (bytes32(0u8), Some(bytes32(2u8))),
    ]
    .into_iter()
    .collect(),
    )]
    .into_iter()
    .collect())
    ]
    #[case::latest_only_c0(
    None,
    Some(vec ! [Bytes::from("73bce791c239c8010cd3c857d96580037ccdd0ee")]),
    [(
    Bytes::from("73bce791c239c8010cd3c857d96580037ccdd0ee"),
    vec ! [
    (bytes32(1u8), Some(bytes32(255u8))),
    (bytes32(0u8), Some(bytes32(128u8))),
    ]
    .into_iter()
    .collect(),
    )]
    .into_iter()
    .collect())
    ]
    #[case::at_block_one(
    Some(Version(BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1))), VersionKind::Last)),
    None,
    [(
    Bytes::from("6b175474e89094c44da98b954eedeac495271d0f"),
    vec ! [
    (bytes32(1u8), Some(bytes32(5u8))),
    (bytes32(2u8), Some(bytes32(1u8))),
    (bytes32(0u8), Some(bytes32(1u8))),
    ],
    ), (
    Bytes::from("94a3F312366b8D0a32A00986194053C0ed0CdDb1"),
    vec ! [
    (bytes32(1u8), Some(bytes32(2u8))),
    (bytes32(2u8), Some(bytes32(4u8)))
    ],
    )]
    .into_iter()
    .map(| (k, v) | (k, v.into_iter().collect::< HashMap < _, _ >> ()))
    .collect::< HashMap < _, _ >> ()
    )]
    #[case::before_block_one(
        Some(Version(
            BlockOrTimestamp::Timestamp("2019-01-01T00:00:00".parse().unwrap()),
            VersionKind::Last
        )),
        None,
        HashMap::new())
    ]
    #[tokio::test]
    async fn test_get_slots(
        #[case] version: Option<Version>,
        #[case] addresses: Option<Vec<Address>>,
        #[case] exp: AccountToContractStoreDeltas,
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let addresses: Option<&[Address]> = addresses.as_deref();

        let res = gw
            .get_contract_slots(&Chain::Ethereum, addresses, version.as_ref(), &mut conn)
            .await
            .unwrap();

        assert_eq!(res, exp);
    }

    #[tokio::test]
    async fn test_upsert_slots_against_empty_db() {
        let mut conn = setup_db().await;
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
        db_fixtures::insert_token(
            &mut conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let blk = db_fixtures::insert_blocks(&mut conn, chain_id).await;
        let txn = db_fixtures::insert_txns(
            &mut conn,
            &[
                (
                    blk[0],
                    1i64,
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                (
                    blk[0],
                    2i64,
                    "0xcb8e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130946",
                ),
            ],
        )
        .await;
        db_fixtures::insert_account(
            &mut conn,
            "6B175474E89094C44Da98b954EedeAC495271d0F",
            "Account1",
            chain_id,
            Some(txn[0]),
        )
        .await;
        let slot_data_tx_0 = vec![
            (vec![1u8].into(), Some(vec![10u8].into())),
            (vec![2u8].into(), Some(vec![20u8].into())),
            (vec![3u8].into(), Some(vec![30u8].into())),
        ]
        .into_iter()
        .collect::<ContractStoreDeltas>();
        let slot_data_tx_1 = vec![
            (vec![1u8].into(), Some(vec![11u8].into())),
            (vec![2u8].into(), Some(vec![21u8].into())),
            (vec![3u8].into(), Some(vec![31u8].into())),
        ]
        .into_iter()
        .collect::<ContractStoreDeltas>();
        let input_slots = [
            (
                txn[0],
                vec![(
                    Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F"),
                    slot_data_tx_0.clone(),
                )]
                .into_iter()
                .collect(),
            ),
            (
                txn[1],
                vec![(
                    Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F"),
                    slot_data_tx_1.clone(),
                )]
                .into_iter()
                .collect(),
            ),
        ]
        .into_iter()
        .collect();
        let gw = EVMGateway::from_connection(&mut conn).await;

        gw.upsert_slots(input_slots, &mut conn)
            .await
            .unwrap();

        // Query the stored slots from the database
        let fetched_slot_data = schema::contract_storage::table
            .select((schema::contract_storage::slot, schema::contract_storage::value))
            .filter(schema::contract_storage::valid_to.eq(MAX_TS))
            .get_results(&mut conn)
            .await
            .unwrap()
            .into_iter()
            .collect::<ContractStoreDeltas>();
        assert_eq!(fetched_slot_data, slot_data_tx_1);
    }

    #[tokio::test]
    async fn test_upsert_slots_invalidate_db_side_records() {
        let mut conn = setup_db().await;
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
        db_fixtures::insert_token(
            &mut conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let blk = db_fixtures::insert_blocks(&mut conn, chain_id).await;
        let ts = db_fixtures::yesterday_midnight();
        let txn = db_fixtures::insert_txns(
            &mut conn,
            &[
                (
                    blk[0],
                    1i64,
                    "0x93132c0221f4c45de9c667297dbb982753405978c94367ff074c3edd3c93e22f",
                ),
                (
                    blk[1],
                    1i64,
                    "0xcb8e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130946",
                ),
            ],
        )
        .await;
        let c0 = db_fixtures::insert_account(
            &mut conn,
            "6B175474E89094C44Da98b954EedeAC495271d0F",
            "Account1",
            chain_id,
            Some(txn[0]),
        )
        .await;
        db_fixtures::insert_slots(
            &mut conn,
            c0,
            txn[0],
            &ts,
            None,
            &[(1, 10, None), (2, 20, None), (3, 30, None)],
        )
        .await;

        let slot_data_tx_1 = vec![(1, 11), (2, 12), (3, 13)]
            .into_iter()
            .map(|(s, v)| (int_to_b256(s), Some(int_to_b256(v))))
            .collect::<ContractStoreDeltas>();
        let input_slots = [(
            txn[1],
            vec![(Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F"), slot_data_tx_1.clone())]
                .into_iter()
                .collect(),
        )]
        .into_iter()
        .collect();
        let gw = EVMGateway::from_connection(&mut conn).await;

        gw.upsert_slots(input_slots, &mut conn)
            .await
            .unwrap();

        // Query the stored slots from the database
        let fetched_slot_data = schema::contract_storage::table
            .select((schema::contract_storage::slot, schema::contract_storage::value))
            .filter(schema::contract_storage::valid_to.eq(MAX_TS))
            .get_results(&mut conn)
            .await
            .unwrap()
            .into_iter()
            .collect::<ContractStoreDeltas>();
        assert_eq!(fetched_slot_data, slot_data_tx_1);
    }

    fn int_to_b256(s: u64) -> Bytes {
        Bytes::from(s).lpad(32, 0)
    }

    async fn setup_slots_delta(conn: &mut AsyncPgConnection) {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        let ts = db_fixtures::yesterday_midnight();
        let ts_p1 = db_fixtures::yesterday_one_am();
        let txn = db_fixtures::insert_txns(
            conn,
            &[
                (
                    blk[0],
                    1i64,
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                (
                    blk[1],
                    1i64,
                    "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7",
                ),
            ],
        )
        .await;
        let c0 = db_fixtures::insert_account(
            conn,
            "6B175474E89094C44Da98b954EedeAC495271d0F",
            "c0",
            chain_id,
            Some(txn[0]),
        )
        .await;
        db_fixtures::insert_slots(conn, c0, txn[0], &ts, None, &[(2, 1, None)]).await;
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[0],
            &ts,
            Some(&ts_p1),
            &[(0, 1, None), (1, 5, None)],
        )
        .await;
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[1],
            &ts_p1,
            None,
            &[(0, 2, Some(1)), (1, 3, Some(5)), (5, 25, None), (6, 30, None)],
        )
        .await;
    }

    async fn get_account(
        address: &Address,
        conn: &mut AsyncPgConnection,
    ) -> Result<i64, StorageError> {
        Ok(schema::account::table
            .filter(schema::account::address.eq(address))
            .select(schema::account::id)
            .first::<i64>(conn)
            .await
            .map_err(PostgresError::from)?)
    }

    #[tokio::test]
    async fn get_slots_delta_forward() {
        let mut conn = setup_db().await;
        setup_slots_delta(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let chain_id = gw
            .get_chain_id(&Chain::Ethereum)
            .unwrap();
        let storage = vec![(0u8, 2u8), (1u8, 3u8), (5u8, 25u8), (6u8, 30u8)]
            .into_iter()
            .map(|(k, v)| if v > 0 { (bytes32(k), Some(bytes32(v))) } else { (bytes32(k), None) })
            .collect::<ContractStoreDeltas>();
        let mut exp = HashMap::new();
        let addr = Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F");
        let account_id = get_account(&addr, &mut conn)
            .await
            .unwrap();
        exp.insert(account_id, storage);
        let end_ts = db_fixtures::yesterday_one_am() + Duration::from_secs(3600);
        let start_ts = db_fixtures::yesterday_midnight();

        let res = gw
            .get_slots_delta(chain_id, &start_ts, &end_ts, &mut conn)
            .await
            .unwrap();

        assert_eq!(res, exp);
    }

    #[tokio::test]
    async fn get_slots_delta_backward() {
        let mut conn = setup_db().await;
        setup_slots_delta(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let chain_id = gw
            .get_chain_id(&Chain::Ethereum)
            .unwrap();
        let storage = vec![(0u8, 1u8), (1u8, 5u8), (5u8, 0u8), (6u8, 0u8)]
            .into_iter()
            .map(|(k, v)| if v > 0 { (bytes32(k), Some(bytes32(v))) } else { (bytes32(k), None) })
            .collect::<ContractStoreDeltas>();
        let mut exp = HashMap::new();
        let addr = Bytes::from("6B175474E89094C44Da98b954EedeAC495271d0F");
        let account_id = get_account(&addr, &mut conn)
            .await
            .unwrap();
        exp.insert(account_id, storage);
        let start_ts = db_fixtures::yesterday_one_am() + Duration::from_secs(3600);
        let end_ts = db_fixtures::yesterday_midnight();

        let res = gw
            .get_slots_delta(chain_id, &start_ts, &end_ts, &mut conn)
            .await
            .unwrap();

        assert_eq!(res, exp);
    }

    #[rstest]
    #[case::with_start_version(
        Some(BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 2))))
    )]
    #[case::no_start_version(None)]
    #[tokio::test]
    async fn get_accounts_delta_backward(#[case] start_version: Option<BlockOrTimestamp>) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = vec![
            // c0 had some changes which need to be reverted
            AccountDelta::new(
                Chain::Ethereum,
                Bytes::from_str("6b175474e89094c44da98b954eedeac495271d0f").expect("addr ok"),
                contract_slots([(6, 0), (0, 1), (1, 5), (5, 0)]),
                Some(Bytes::from(100u64).lpad(32, 0)),
                None,
                ChangeType::Update,
            ),
            // c1 which was deployed on block 2 is deleted
            AccountDelta::new(
                Chain::Ethereum,
                Bytes::from_str("73bce791c239c8010cd3c857d96580037ccdd0ee").expect("addr ok"),
                HashMap::new(),
                None,
                None,
                ChangeType::Deletion,
            ),
            // c2 is recreated
            AccountDelta::new(
                Chain::Ethereum,
                Bytes::from_str("94a3f312366b8d0a32a00986194053c0ed0cddb1").expect("addr ok"),
                contract_slots([(1, 2), (2, 4)]),
                Some(Bytes::from(25u64).lpad(32, 0)),
                Some(Bytes::from("C2C2C2")),
                ChangeType::Creation,
            ),
        ];

        let mut changes = gw
            .get_accounts_delta(
                &Chain::Ethereum,
                start_version.as_ref(),
                &BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1))),
                &mut conn,
            )
            .await
            .unwrap();
        changes.sort_unstable_by_key(|u| u.address.clone());

        assert_eq!(changes, exp);
    }

    #[tokio::test]
    async fn get_accounts_delta_forward() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = vec![
            // c0 updates some slots and balances
            AccountDelta::new(
                Chain::Ethereum,
                Bytes::from_str("6b175474e89094c44da98b954eedeac495271d0f").expect("addr ok"),
                contract_slots([(6, 30), (0, 2), (1, 3), (5, 25)]),
                Some(Bytes::from(101u64).lpad(32, 0)),
                None,
                ChangeType::Update,
            ),
            // c1 was deployed
            AccountDelta::new(
                Chain::Ethereum,
                Bytes::from_str("73bce791c239c8010cd3c857d96580037ccdd0ee").expect("addr ok"),
                contract_slots([(0, 128), (1, 255)]),
                Some(Bytes::from(50u64).lpad(32, 0)),
                Some(Bytes::from("C1C1C1")),
                ChangeType::Creation,
            ),
            // c2 is deleted
            AccountDelta::new(
                Chain::Ethereum,
                Bytes::from_str("94a3f312366b8d0a32a00986194053c0ed0cddb1").expect("addr ok"),
                HashMap::new(),
                None,
                None,
                ChangeType::Deletion,
            ),
        ];

        let mut changes = gw
            .get_accounts_delta(
                &Chain::Ethereum,
                Some(&BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1)))),
                &BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 2))),
                &mut conn,
            )
            .await
            .unwrap();
        changes.sort_unstable_by_key(|u| u.address.clone());

        assert_eq!(changes, exp);
    }

    #[rstest]
    #[case::forward("forward")]
    #[case::backward("backward")]
    #[tokio::test]
    async fn get_accounts_delta_fail(#[case] direction: &str) {
        let mut conn = setup_db().await;
        let ts = db_fixtures::yesterday_midnight();
        let ts_p1 = db_fixtures::yesterday_one_am();

        let (start_ts, end_ts) = match direction {
            "forward" => (ts, ts_p1),
            "backward" => (ts_p1, ts),
            _ => {
                panic!("Must use forward or backward")
            }
        };
        let chain_db_id = setup_data(&mut conn).await;
        let c1 = &orm::Account::by_address(
            &Bytes::from("73BcE791c239c8010Cd3C857d96580037CCdd0EE"),
            chain_db_id,
            &mut conn,
        )
        .await
        .unwrap()[0];
        db_fixtures::delete_account(&mut conn, c1.id, &ts_p1).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let start_version = BlockOrTimestamp::Timestamp(start_ts);
        let end_version = BlockOrTimestamp::Timestamp(end_ts);
        let exp = Err(StorageError::Unexpected(format!(
            "Found account that was deleted and created within range {start_ts} - {end_ts}!"
        )));

        let res = gw
            .get_accounts_delta(&Chain::Ethereum, Some(&start_version), &end_version, &mut conn)
            .await;

        assert_eq!(res, exp);
    }

    #[tokio::test]
    async fn test_add_account_balances() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let tx_hash =
            Bytes::from("50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388");

        let account_balance = AccountBalance {
            account: Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
            token: Bytes::from_str("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            balance: Bytes::from(2000_i32.to_be_bytes()),
            modify_tx: tx_hash,
        };

        gw.add_account_balances(&[account_balance], &Chain::Ethereum, &mut conn)
            .await
            .unwrap();

        let inserted_data = schema::account_balance::table
            .inner_join(
                schema::token::table.on(schema::token::id.eq(schema::account_balance::token_id)),
            )
            .inner_join(
                schema::account::table.on(schema::account::id.eq(schema::token::account_id)),
            )
            .filter(
                schema::account::address
                    .eq(Bytes::from_str("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()),
            )
            .filter(schema::account_balance::valid_to.is_null())
            .select(orm::AccountBalance::as_select())
            .first::<orm::AccountBalance>(&mut conn)
            .await
            .unwrap();

        assert_eq!(inserted_data.balance, Bytes::from(2000_i32.to_be_bytes()));
    }

    #[tokio::test]
    async fn test_get_account_balances() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let weth_addr = Bytes::from_str("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc_addr = Bytes::from_str("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let account_addr = Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let exp: HashMap<_, _> = [
            (
                account_addr.clone(),
                weth_addr.clone(),
                AccountBalance::new(
                    account_addr.clone(),
                    weth_addr,
                    Balance::from(2000_i32.to_be_bytes()).lpad(32, 0),
                    Bytes::zero(32),
                ),
            ),
            (
                account_addr.clone(),
                usdc_addr.clone(),
                AccountBalance::new(
                    account_addr.clone(),
                    usdc_addr,
                    Balance::from(1000_i32.to_be_bytes()).lpad(32, 0),
                    Bytes::zero(32),
                ),
            ),
        ]
        .into_iter()
        .group_by(|e| e.0.clone())
        .into_iter()
        .map(|(account, group)| {
            (
                account,
                group
                    .map(|(_, token, bal)| (token, bal))
                    .collect::<HashMap<_, _>>(),
            )
        })
        .collect();

        let res = gw
            .get_account_balances(&Chain::Ethereum, Some(&[account_addr]), None, false, &mut conn)
            .await
            .expect("retrieving balances failed!");

        assert_eq!(res, exp);
    }
}
