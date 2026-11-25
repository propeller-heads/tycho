//! Balances of the accounts of an instance: one balance per account and token, versioned. The
//! native balance of an account is its balance of the chain's native token.
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
        orm::{NewAccountBalance, NewAccountBalanceHistory},
        schema::tycho::{account_balance, account_balance_history},
        write::versioning::VersionedRowNew,
        InstanceId,
    },
    storage_error_from_diesel,
};

#[async_trait]
impl VersionedRowNew for NewAccountBalance {
    const ENTITY: &'static str = "AccountBalance";
    /// Account id and token address.
    type Key = (i64, Bytes);
    type History = NewAccountBalanceHistory;

    fn key(&self) -> Self::Key {
        (self.account_id, self.token.clone())
    }

    fn instance(&self) -> InstanceId {
        self.instance_id
    }

    fn version(&self) -> (NaiveDateTime, i32) {
        (self.valid_from, self.tx_index)
    }

    /// Account balances keep nothing of the version they replace.
    fn link_next(&self, _next: &mut Self) {}

    fn into_history(self, valid_to: NaiveDateTime) -> NewAccountBalanceHistory {
        NewAccountBalanceHistory {
            instance_id: self.instance_id,
            account_id: self.account_id,
            token: self.token,
            balance: self.balance,
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
        let (account_ids, tokens): (Vec<i64>, Vec<Bytes>) = keys.iter().cloned().unzip();
        sql_query(
            "SELECT ab.* FROM tycho.account_balance ab \
             JOIN unnest($2::int8[], $3::bytea[]) AS k(account_id, token) \
             ON ab.account_id = k.account_id AND ab.token = k.token \
             WHERE ab.instance_id = $1",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(account_ids)
        .bind::<Array<Binary>, _>(tokens)
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
            diesel::insert_into(account_balance::table)
                .values(chunk)
                .on_conflict((
                    account_balance::instance_id,
                    account_balance::account_id,
                    account_balance::token,
                ))
                .do_update()
                .set((
                    account_balance::balance.eq(excluded(account_balance::balance)),
                    account_balance::valid_from.eq(excluded(account_balance::valid_from)),
                    account_balance::tx_index.eq(excluded(account_balance::tx_index)),
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
        let (account_ids, tokens): (Vec<i64>, Vec<Bytes>) = keys.iter().cloned().unzip();
        sql_query(
            "DELETE FROM tycho.account_balance ab \
             USING unnest($2::int8[], $3::bytea[]) AS k(account_id, token) \
             WHERE ab.instance_id = $1 AND ab.account_id = k.account_id AND ab.token = k.token",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(account_ids)
        .bind::<Array<Binary>, _>(tokens)
        .execute(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None))?;
        Ok(())
    }

    async fn insert_history(
        rows: &[NewAccountBalanceHistory],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(NewAccountBalanceHistory::MAX_BATCH_SIZE) {
            diesel::insert_into(account_balance_history::table)
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
