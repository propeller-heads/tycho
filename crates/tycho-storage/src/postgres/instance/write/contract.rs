//! Contracts of an instance: accounts, and the versions of their code, storage and balances.
//!
//! Each instance stores its own copy of every contract it uses, so the same address can have one
//! account in each instance.
use std::collections::{BTreeSet, HashMap};

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    keccak256,
    models::{
        contract::{Account, AccountDelta},
        ChangeType, TxHash,
    },
    storage::StorageError,
    Bytes,
};

use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew,
        origins::BatchOrigins,
        orm::{NewAccount, NewAccountBalance, NewContractCode, NewContractStorage},
        schema::tycho::account,
        write::{token::check_tokens_stored, versioning::VersionedChange},
        InstanceId,
    },
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Stores the accounts that the instance does not store yet. A stored account keeps its row:
    /// a later creation of the same address changes nothing. Code, storage and balances of a new
    /// account are versions, written by [`Self::update_contracts`].
    ///
    /// Each account takes its creation block, timestamp and transaction from `origins`. `NotFound`
    /// for a creation transaction that is not in the batch, and `Unexpected` for an account with
    /// no creation transaction.
    pub(crate) async fn insert_contracts(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        accounts: &[Account],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let rows = accounts
            .iter()
            .map(|new| {
                let creation_tx = new
                    .creation_tx
                    .as_ref()
                    .ok_or_else(|| {
                        StorageError::Unexpected(format!(
                            "Account {}: an account needs the transaction that created it",
                            new.address
                        ))
                    })?;
                let origin = origins.change(creation_tx)?;
                Ok(NewAccount {
                    instance_id: instance,
                    address: new.address.clone(),
                    title: new.title.clone(),
                    created_at: Some(origin.valid_from),
                    creation_block: Some(origin.block_number),
                    creation_tx_hash: Some(creation_tx.clone()),
                    deleted_at: None,
                    deletion_tx_hash: None,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        for chunk in rows.chunks(NewAccount::MAX_BATCH_SIZE) {
            diesel::insert_into(account::table)
                .values(chunk)
                .on_conflict((account::instance_id, account::address))
                .do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "Account", &instance.to_string(), None)
                })?;
        }
        Ok(())
    }

    /// Writes the code, storage slots and native balance of each account delta as new versions,
    /// at the block timestamp and index of its transaction from `origins`.
    ///
    /// A code with the hash of the version it replaces adds no version. A slot without value is
    /// a version with a NULL value. The native balance is the balance of the chain's native token.
    ///
    /// `NotFound` for an account that is not in the instance, a transaction that is not in the
    /// batch, or a native token that is not stored. `Unexpected` for an account deletion, two
    /// deltas of one account in one transaction, or a change that does not start after the stored
    /// version of its key.
    pub(crate) async fn update_contracts(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        deltas: &[(TxHash, AccountDelta)],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        if let Some((_, deleted)) = deltas
            .iter()
            .find(|(_, delta)| delta.change_type() == ChangeType::Deletion)
        {
            return Err(StorageError::Unexpected(format!(
                "Account {}: the instance writer does not delete accounts",
                deleted.address
            )));
        }
        let addresses: Vec<&Bytes> = deltas
            .iter()
            .map(|(_, delta)| &delta.address)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let account_ids = self
            .account_ids(instance, &addresses, conn)
            .await?;
        let native_tokens = deltas
            .iter()
            .filter(|(_, delta)| delta.balance.is_some())
            .map(|(_, delta)| native_token(delta))
            .collect::<Result<BTreeSet<_>, _>>()?;
        check_tokens_stored(&native_tokens.iter().collect::<Vec<_>>(), conn).await?;

        let mut code = Vec::new();
        let mut storage = Vec::new();
        let mut balances = Vec::new();
        for (tx, delta) in deltas {
            let origin = origins.change(tx)?;
            let account_id = account_ids[&delta.address];
            if let Some(new_code) = delta.code() {
                code.push(VersionedChange::Update(NewContractCode {
                    instance_id: instance,
                    account_id,
                    code: new_code.clone(),
                    hash: keccak256(new_code).into(),
                    modify_tx_hash: tx.clone(),
                    valid_from: origin.valid_from,
                    tx_index: origin.tx_index,
                }));
            }
            for (slot, value) in &delta.slots {
                storage.push(VersionedChange::Update(NewContractStorage {
                    instance_id: instance,
                    account_id,
                    slot: slot.clone(),
                    value: value.clone(),
                    // write_versions links every later version to the one it replaces.
                    previous_value: None,
                    valid_from: origin.valid_from,
                    tx_index: origin.tx_index,
                }));
            }
            if let Some(balance) = &delta.balance {
                balances.push(VersionedChange::Update(NewAccountBalance {
                    instance_id: instance,
                    account_id,
                    token: native_token(delta)?,
                    balance: balance.clone(),
                    valid_from: origin.valid_from,
                    tx_index: origin.tx_index,
                }));
            }
        }
        self.write_versions(instance, code, conn)
            .await?;
        self.write_versions(instance, storage, conn)
            .await?;
        self.write_versions(instance, balances, conn)
            .await
    }

    /// Returns the id of each account of the instance, by address. Deleted accounts are found too.
    ///
    /// `NotFound` for the first address that has no account in the instance.
    pub(crate) async fn account_ids(
        &self,
        instance: InstanceId,
        addresses: &[&Bytes],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Bytes, i64>, StorageError> {
        let ids: HashMap<Bytes, i64> = account::table
            .filter(account::instance_id.eq(instance))
            .filter(account::address.eq_any(addresses))
            .select((account::address, account::id))
            .load::<(Bytes, i64)>(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Account", &instance.to_string(), None))?
            .into_iter()
            .collect();
        if let Some(missing) = addresses
            .iter()
            .find(|address| !ids.contains_key(**address))
        {
            return Err(StorageError::NotFound("Account".to_string(), missing.to_string()));
        }
        Ok(ids)
    }
}

/// Address of the native token of the delta's chain: the token of its native balance.
fn native_token(delta: &AccountDelta) -> Result<Bytes, StorageError> {
    delta
        .chain
        .try_native_token()
        .map(|token| token.address)
        .map_err(|err| StorageError::Unexpected(format!("Account {}: {err}", delta.address)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::{
        keccak256,
        models::{
            blockchain::{Block, Transaction},
            contract::{Account as DomainAccount, AccountDelta},
            Chain, ChangeType,
        },
        storage::StorageError,
        Bytes,
    };

    use crate::postgres::{
        cache::WriteOp,
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            origins::BatchOrigins,
            orm::{Account, AccountBalance, ContractCode, ContractStorage},
            schema::tycho::{account, account_balance, contract_code, contract_storage},
            InstanceId,
        },
        testing::instance_test_connection,
    };

    fn address(id: u8) -> Bytes {
        Bytes::from(vec![id; 20])
    }

    #[tokio::test]
    async fn test_account_ids_finds_only_the_instance_accounts() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let a = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let account_of_a = db_fixtures::insert_instance_account(&mut conn, a, address(1)).await;
        db_fixtures::insert_instance_account(&mut conn, b, address(1)).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .account_ids(a, &[&address(1)], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids.len(), 1);
        assert_eq!(ids[&address(1)], account_of_a);
    }

    #[tokio::test]
    async fn test_account_ids_fails_for_an_unknown_address() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let result = gateway
            .account_ids(instance, &[&address(1), &address(2)], &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("Account".to_string(), address(2).to_string()))
        );
    }

    #[tokio::test]
    async fn test_account_ids_finds_a_deleted_account() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let id = db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let deleted_at = NaiveDate::from_ymd_opt(2021, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");
        diesel::update(account::table)
            .filter(account::id.eq(id))
            .set(account::deleted_at.eq(deleted_at))
            .execute(&mut conn)
            .await
            .expect("deleting the account failed");
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .account_ids(instance, &[&address(1)], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids[&address(1)], id);
    }

    fn ts(second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 25)
            .and_then(|date| date.and_hms_opt(12, 0, second))
            .expect("a valid timestamp")
    }

    /// A batch of block 10 at second 10 with the transaction `0xa1` at index 1, and of block 11 at
    /// second 11 with the transaction `0xb2` at index 2.
    fn origins() -> BatchOrigins {
        let block = |number: u8| {
            Block::new(
                number.into(),
                Chain::Ethereum,
                Bytes::from(vec![number]),
                Bytes::default(),
                ts(number.into()),
            )
        };
        let tx = |hash: u8, block: &Block, index: u64| {
            Transaction::new(
                Bytes::from(vec![hash]),
                block.hash.clone(),
                Bytes::default(),
                None,
                index,
            )
        };
        let (block_10, block_11) = (block(10), block(11));
        let ops = vec![
            WriteOp::UpsertBlock(vec![block_10.clone(), block_11.clone()]),
            WriteOp::UpsertTx(vec![tx(0xa1, &block_10, 1), tx(0xb2, &block_11, 2)]),
        ];
        BatchOrigins::from_ops(&ops).expect("a valid batch")
    }

    fn new_account(id: u8, title: &str, creation_tx: Option<u8>) -> DomainAccount {
        DomainAccount::new(
            Chain::Ethereum,
            address(id),
            title.to_string(),
            HashMap::new(),
            Bytes::from(0u64),
            HashMap::new(),
            Bytes::default(),
            Bytes::default(),
            Bytes::default(),
            Bytes::default(),
            creation_tx.map(|hash| Bytes::from(vec![hash])),
        )
    }

    async fn setup() -> (AsyncPgConnection, PostgresGatewayNew, InstanceId) {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        (conn, gateway, instance)
    }

    async fn stored(conn: &mut AsyncPgConnection, instance: InstanceId) -> Vec<Account> {
        account::table
            .filter(account::instance_id.eq(instance))
            .order_by(account::address)
            .select(Account::as_select())
            .load(conn)
            .await
            .expect("reading the accounts failed")
    }

    #[tokio::test]
    async fn test_insert_contracts_stores_the_creation_of_each_account() {
        let (mut conn, gateway, instance) = setup().await;
        let accounts = [new_account(1, "pool", Some(0xa1)), new_account(2, "vault", Some(0xb2))];

        gateway
            .insert_contracts(instance, &origins(), &accounts, &mut conn)
            .await
            .unwrap();

        let stored = stored(&mut conn, instance).await;
        let creations: Vec<_> = stored
            .iter()
            .map(|row| {
                (
                    row.address.clone(),
                    row.title.as_str(),
                    row.created_at,
                    row.creation_block,
                    row.creation_tx_hash.clone(),
                )
            })
            .collect();
        assert_eq!(
            creations,
            vec![
                (address(1), "pool", Some(ts(10)), Some(10), Some(Bytes::from("0xa1"))),
                (address(2), "vault", Some(ts(11)), Some(11), Some(Bytes::from("0xb2"))),
            ]
        );
    }

    #[tokio::test]
    async fn test_insert_contracts_keeps_the_stored_account() {
        let (mut conn, gateway, instance) = setup().await;
        gateway
            .insert_contracts(
                instance,
                &origins(),
                &[new_account(1, "first", Some(0xa1))],
                &mut conn,
            )
            .await
            .unwrap();

        gateway
            .insert_contracts(
                instance,
                &origins(),
                &[new_account(1, "second", Some(0xb2))],
                &mut conn,
            )
            .await
            .unwrap();

        let stored = stored(&mut conn, instance).await;
        assert_eq!(stored.len(), 1);
        assert_eq!((stored[0].title.as_str(), stored[0].creation_block), ("first", Some(10)));
    }

    #[tokio::test]
    async fn test_insert_contracts_fails_for_a_creation_not_in_the_batch() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_contracts(
                instance,
                &origins(),
                &[new_account(1, "pool", Some(0xff))],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("Transaction".to_string(), Bytes::from("0xff").to_string()))
        );
    }

    #[tokio::test]
    async fn test_insert_contracts_fails_for_an_account_without_creation() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_contracts(instance, &origins(), &[new_account(1, "pool", None)], &mut conn)
            .await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
        assert!(stored(&mut conn, instance)
            .await
            .is_empty());
    }

    /// Address of the native token of Ethereum.
    fn eth() -> Bytes {
        Bytes::from([0u8; 20])
    }

    async fn insert_eth(conn: &mut AsyncPgConnection) {
        db_fixtures::insert_token(conn, &hex::encode(eth()), "ETH", 18, Some(100)).await;
    }

    fn delta(
        id: u8,
        slots: &[(&str, Option<&str>)],
        balance: Option<u64>,
        code: Option<&str>,
        change: ChangeType,
    ) -> AccountDelta {
        AccountDelta::new(
            Chain::Ethereum,
            address(id),
            slots
                .iter()
                .map(|(slot, value)| (Bytes::from(*slot), value.map(Bytes::from)))
                .collect(),
            balance.map(Bytes::from),
            code.map(Bytes::from),
            change,
        )
    }

    fn tx(hash: u8) -> Bytes {
        Bytes::from(vec![hash])
    }

    #[tokio::test]
    async fn test_update_contracts_writes_code_slots_and_native_balance() {
        let (mut conn, gateway, instance) = setup().await;
        insert_eth(&mut conn).await;
        let id = db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let deltas = [(
            tx(0xb2),
            delta(
                1,
                &[("0x01", Some("0x0a")), ("0x02", None)],
                Some(7),
                Some("0x6080"),
                ChangeType::Update,
            ),
        )];

        gateway
            .update_contracts(instance, &origins(), &deltas, &mut conn)
            .await
            .unwrap();

        let code: Vec<ContractCode> = contract_code::table
            .filter(contract_code::instance_id.eq(instance))
            .select(ContractCode::as_select())
            .load(&mut conn)
            .await
            .unwrap();
        assert_eq!(code.len(), 1);
        assert_eq!(
            (code[0].account_id, &code[0].code, &code[0].hash, &code[0].modify_tx_hash),
            (id, &Bytes::from("0x6080"), &Bytes::from(keccak256(Bytes::from("0x6080"))), &tx(0xb2))
        );
        assert_eq!((code[0].valid_from, code[0].tx_index), (ts(11), 2));
        let slots: Vec<(Bytes, Option<Bytes>)> = contract_storage::table
            .filter(contract_storage::instance_id.eq(instance))
            .filter(contract_storage::account_id.eq(id))
            .order_by(contract_storage::slot)
            .select(ContractStorage::as_select())
            .load(&mut conn)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.slot, row.value))
            .collect();
        assert_eq!(
            slots,
            vec![(Bytes::from("0x01"), Some(Bytes::from("0x0a"))), (Bytes::from("0x02"), None)]
        );
        let balances: Vec<AccountBalance> = account_balance::table
            .filter(account_balance::instance_id.eq(instance))
            .select(AccountBalance::as_select())
            .load(&mut conn)
            .await
            .unwrap();
        assert_eq!(balances.len(), 1);
        assert_eq!(
            (balances[0].account_id, &balances[0].token, &balances[0].balance),
            (id, &eth(), &Bytes::from(7u64))
        );
    }

    #[tokio::test]
    async fn test_update_contracts_fails_without_the_native_token() {
        let (mut conn, gateway, instance) = setup().await;
        db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let deltas = [(tx(0xb2), delta(1, &[], Some(7), None, ChangeType::Update))];

        let result = gateway
            .update_contracts(instance, &origins(), &deltas, &mut conn)
            .await;

        assert_eq!(result, Err(StorageError::NotFound("Token".to_string(), eth().to_string())));
    }

    #[tokio::test]
    async fn test_update_contracts_fails_for_an_account_not_in_the_instance() {
        let (mut conn, gateway, instance) = setup().await;
        let deltas =
            [(tx(0xb2), delta(1, &[("0x01", Some("0x0a"))], None, None, ChangeType::Update))];

        let result = gateway
            .update_contracts(instance, &origins(), &deltas, &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("Account".to_string(), address(1).to_string()))
        );
    }

    #[tokio::test]
    async fn test_update_contracts_fails_for_a_deletion() {
        let (mut conn, gateway, instance) = setup().await;
        db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let deltas = [(tx(0xb2), delta(1, &[], None, None, ChangeType::Deletion))];

        let result = gateway
            .update_contracts(instance, &origins(), &deltas, &mut conn)
            .await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_update_contracts_fails_for_two_deltas_of_one_account_in_one_transaction() {
        let (mut conn, gateway, instance) = setup().await;
        db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let deltas = [
            (tx(0xb2), delta(1, &[("0x01", Some("0x0a"))], None, None, ChangeType::Update)),
            (tx(0xb2), delta(1, &[("0x01", Some("0x0b"))], None, None, ChangeType::Update)),
        ];

        let result = gateway
            .update_contracts(instance, &origins(), &deltas, &mut conn)
            .await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
    }
}
