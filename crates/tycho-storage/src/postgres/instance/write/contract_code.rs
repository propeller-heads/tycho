//! Contract code of an instance: one code per account, versioned. A code with the hash of the
//! version it replaces adds no version.
use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{upsert::excluded, ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::storage::StorageError;

use crate::postgres::{
    instance::{
        orm::{ContractCode, NewContractCode, NewContractCodeHistory},
        schema::tycho::{contract_code, contract_code_history},
        write::versioning::VersionedRowNew,
        InstanceId,
    },
    storage_error_from_diesel,
};

#[async_trait]
impl VersionedRowNew for NewContractCode {
    const ENTITY: &'static str = "ContractCode";
    /// Account id.
    type Key = i64;
    type History = NewContractCodeHistory;

    fn key(&self) -> i64 {
        self.account_id
    }

    fn instance(&self) -> InstanceId {
        self.instance_id
    }

    fn version(&self) -> (NaiveDateTime, i32) {
        (self.valid_from, self.tx_index)
    }

    /// Code keeps nothing of the version it replaces.
    fn link_next(&self, _next: &mut Self) {}

    /// Extractors send the same code again; only a new hash is a new version.
    fn repeats(&self, replaced: &Self) -> bool {
        self.hash == replaced.hash
    }

    fn into_history(self, valid_to: NaiveDateTime) -> NewContractCodeHistory {
        NewContractCodeHistory {
            instance_id: self.instance_id,
            account_id: self.account_id,
            code: self.code,
            hash: self.hash,
            modify_tx_hash: self.modify_tx_hash,
            valid_from: self.valid_from,
            valid_to,
            tx_index: self.tx_index,
        }
    }

    async fn load_current(
        instance: InstanceId,
        keys: &[i64],
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError> {
        let rows: Vec<ContractCode> = contract_code::table
            .filter(contract_code::instance_id.eq(instance))
            .filter(contract_code::account_id.eq_any(keys))
            .select(ContractCode::as_select())
            .load(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None)
            })?;
        Ok(rows
            .into_iter()
            .map(|row| NewContractCode {
                instance_id: row.instance_id,
                account_id: row.account_id,
                code: row.code,
                hash: row.hash,
                modify_tx_hash: row.modify_tx_hash,
                valid_from: row.valid_from,
                tx_index: row.tx_index,
            })
            .collect())
    }

    async fn upsert_current(
        rows: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(Self::MAX_BATCH_SIZE) {
            diesel::insert_into(contract_code::table)
                .values(chunk)
                .on_conflict((contract_code::instance_id, contract_code::account_id))
                .do_update()
                .set((
                    contract_code::code.eq(excluded(contract_code::code)),
                    contract_code::hash.eq(excluded(contract_code::hash)),
                    contract_code::modify_tx_hash.eq(excluded(contract_code::modify_tx_hash)),
                    contract_code::valid_from.eq(excluded(contract_code::valid_from)),
                    contract_code::tx_index.eq(excluded(contract_code::tx_index)),
                ))
                .execute(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, "current", None))?;
        }
        Ok(())
    }

    async fn delete_current(
        instance: InstanceId,
        keys: &[i64],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        diesel::delete(contract_code::table)
            .filter(contract_code::instance_id.eq(instance))
            .filter(contract_code::account_id.eq_any(keys))
            .execute(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None)
            })?;
        Ok(())
    }

    async fn insert_history(
        rows: &[NewContractCodeHistory],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(NewContractCodeHistory::MAX_BATCH_SIZE) {
            diesel::insert_into(contract_code_history::table)
                .values(chunk)
                .execute(conn)
                .await
                .map_err(|err| {
                    // The detail names the failing row, for example the `valid_to` that has no
                    // history day.
                    let detail = if let diesel::result::Error::DatabaseError(_, info) = &err {
                        info.details().unwrap_or_default()
                    } else {
                        ""
                    };
                    StorageError::Unexpected(format!(
                        "{} history of instance {}: {err} {detail}",
                        Self::ENTITY,
                        chunk[0].instance_id
                    ))
                })?;
        }
        Ok(())
    }
}
