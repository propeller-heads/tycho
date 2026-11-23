//! Component balances of an instance: one balance per component and token, versioned.
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
        orm::{ComponentBalance, NewComponentBalance, NewComponentBalanceHistory},
        schema::tycho::{component_balance, component_balance_history},
        write::{versioning::VersionedRow, ROWS_PER_INSERT},
        InstanceId,
    },
    storage_error_from_diesel,
};

#[async_trait]
impl VersionedRow for NewComponentBalance {
    const ENTITY: &'static str = "ComponentBalance";
    /// Component id and token address.
    type Key = (i64, Bytes);
    type History = NewComponentBalanceHistory;

    fn key(&self) -> Self::Key {
        (self.protocol_component_id, self.token.clone())
    }

    fn instance(&self) -> InstanceId {
        self.instance_id
    }

    fn version(&self) -> (NaiveDateTime, i32) {
        (self.valid_from, self.tx_index)
    }

    fn link_next(&self, next: &mut Self) {
        next.previous_value = self.new_balance.clone();
    }

    fn into_history(self, valid_to: NaiveDateTime) -> NewComponentBalanceHistory {
        NewComponentBalanceHistory {
            instance_id: self.instance_id,
            protocol_component_id: self.protocol_component_id,
            token: self.token,
            new_balance: self.new_balance,
            previous_value: self.previous_value,
            balance_float: self.balance_float,
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
        let (component_ids, tokens): (Vec<i64>, Vec<Bytes>) = keys.iter().cloned().unzip();
        let rows: Vec<ComponentBalance> = sql_query(
            "SELECT cb.* FROM tycho.component_balance cb \
             JOIN unnest($2::int8[], $3::bytea[]) AS k(component_id, token) \
             ON cb.protocol_component_id = k.component_id AND cb.token = k.token \
             WHERE cb.instance_id = $1",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(component_ids)
        .bind::<Array<Binary>, _>(tokens)
        .load(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None))?;
        Ok(rows
            .into_iter()
            .map(|row| NewComponentBalance {
                instance_id: row.instance_id,
                protocol_component_id: row.protocol_component_id,
                token: row.token,
                new_balance: row.new_balance,
                previous_value: row.previous_value,
                balance_float: row.balance_float,
                valid_from: row.valid_from,
                tx_index: row.tx_index,
            })
            .collect())
    }

    async fn upsert_current(
        rows: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            diesel::insert_into(component_balance::table)
                .values(chunk)
                .on_conflict((
                    component_balance::instance_id,
                    component_balance::protocol_component_id,
                    component_balance::token,
                ))
                .do_update()
                .set((
                    component_balance::new_balance.eq(excluded(component_balance::new_balance)),
                    component_balance::previous_value
                        .eq(excluded(component_balance::previous_value)),
                    component_balance::balance_float.eq(excluded(component_balance::balance_float)),
                    component_balance::valid_from.eq(excluded(component_balance::valid_from)),
                    component_balance::tx_index.eq(excluded(component_balance::tx_index)),
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
        let (component_ids, tokens): (Vec<i64>, Vec<Bytes>) = keys.iter().cloned().unzip();
        sql_query(
            "DELETE FROM tycho.component_balance cb \
             USING unnest($2::int8[], $3::bytea[]) AS k(component_id, token) \
             WHERE cb.instance_id = $1 \
             AND cb.protocol_component_id = k.component_id AND cb.token = k.token",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(component_ids)
        .bind::<Array<Binary>, _>(tokens)
        .execute(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None))?;
        Ok(())
    }

    async fn insert_history(
        rows: &[NewComponentBalanceHistory],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            diesel::insert_into(component_balance_history::table)
                .values(chunk)
                .execute(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, "history", None))?;
        }
        Ok(())
    }
}
