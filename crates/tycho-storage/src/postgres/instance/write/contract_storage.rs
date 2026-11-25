//! Contract storage of an instance: one value per account and slot, versioned.
//!
//! A slot with no value is stored as a row with a NULL value: it is a version, not a deletion.
use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{
    sql_query,
    sql_types::{Array, BigInt, Binary, Int4},
    upsert::excluded,
    ExpressionMethods,
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{storage::StorageError, Bytes};

use crate::postgres::{
    instance::{
        orm::{NewContractStorage, NewContractStorageHistory},
        schema::tycho::{contract_storage, contract_storage_history},
        write::versioning::VersionedRowNew,
        InstanceId,
    },
    storage_error_from_diesel,
};

#[async_trait]
impl VersionedRowNew for NewContractStorage {
    const ENTITY: &'static str = "ContractStorage";
    /// Account id and slot.
    type Key = (i64, Bytes);
    type History = NewContractStorageHistory;

    fn key(&self) -> Self::Key {
        (self.account_id, self.slot.clone())
    }

    fn instance(&self) -> InstanceId {
        self.instance_id
    }

    fn version(&self) -> (NaiveDateTime, i32) {
        (self.valid_from, self.tx_index)
    }

    fn link_next(&self, next: &mut Self) {
        next.previous_value = self.value.clone();
    }

    fn into_history(self, valid_to: NaiveDateTime) -> NewContractStorageHistory {
        NewContractStorageHistory {
            instance_id: self.instance_id,
            account_id: self.account_id,
            slot: self.slot,
            value: self.value,
            previous_value: self.previous_value,
            valid_from: self.valid_from,
            valid_to,
            tx_index: self.tx_index,
        }
    }

    async fn load_current(
        instance: InstanceId,
        keys: &[Self::Key],
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError> {
        let (account_ids, slots): (Vec<i64>, Vec<Bytes>) = keys.iter().cloned().unzip();
        sql_query(
            "SELECT cs.* FROM tycho.contract_storage cs \
             JOIN unnest($2::int8[], $3::bytea[]) AS k(account_id, slot) \
             ON cs.account_id = k.account_id AND cs.slot = k.slot \
             WHERE cs.instance_id = $1",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(account_ids)
        .bind::<Array<Binary>, _>(slots)
        .load(conn)
        .await
        .map_err(|err| {
            storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None).into()
        })
    }

    async fn upsert_current(
        rows: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(Self::MAX_BATCH_SIZE) {
            diesel::insert_into(contract_storage::table)
                .values(chunk)
                .on_conflict((
                    contract_storage::instance_id,
                    contract_storage::account_id,
                    contract_storage::slot,
                ))
                .do_update()
                .set((
                    contract_storage::value.eq(excluded(contract_storage::value)),
                    contract_storage::previous_value.eq(excluded(contract_storage::previous_value)),
                    contract_storage::valid_from.eq(excluded(contract_storage::valid_from)),
                    contract_storage::tx_index.eq(excluded(contract_storage::tx_index)),
                ))
                .execute(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, "current", None))?;
        }
        Ok(())
    }

    async fn delete_current(
        instance: InstanceId,
        keys: &[Self::Key],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let (account_ids, slots): (Vec<i64>, Vec<Bytes>) = keys.iter().cloned().unzip();
        sql_query(
            "DELETE FROM tycho.contract_storage cs \
             USING unnest($2::int8[], $3::bytea[]) AS k(account_id, slot) \
             WHERE cs.instance_id = $1 AND cs.account_id = k.account_id AND cs.slot = k.slot",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(account_ids)
        .bind::<Array<Binary>, _>(slots)
        .execute(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None))?;
        Ok(())
    }

    async fn insert_history(
        rows: &[NewContractStorageHistory],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(NewContractStorageHistory::MAX_BATCH_SIZE) {
            diesel::insert_into(contract_storage_history::table)
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
