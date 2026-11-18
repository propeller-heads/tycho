//! Protocol state of an instance: one value per component and attribute name, versioned.
use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{
    dsl::now,
    prelude::*,
    sql_query,
    sql_types::{Array, BigInt, Int4, Text},
    upsert::excluded,
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::storage::StorageError;

use crate::postgres::{
    instance::{
        orm::{NewProtocolState, NewProtocolStateHistory, ProtocolState},
        schema::tycho::{protocol_state, protocol_state_history},
        write::versioning::VersionedRow,
        InstanceId,
    },
    storage_error_from_diesel,
};

/// Rows per insert statement, far below the bind parameter limit of Postgres.
const ROWS_PER_INSERT: usize = 1000;

#[async_trait]
impl VersionedRow for NewProtocolState {
    const ENTITY: &'static str = "ProtocolState";
    /// Component id and attribute name.
    type Key = (i64, String);
    type History = NewProtocolStateHistory;

    fn key(&self) -> Self::Key {
        (self.protocol_component_id, self.attribute_name.clone())
    }

    fn version(&self) -> (NaiveDateTime, i32) {
        (self.valid_from, self.tx_index)
    }

    fn link_next(&self, next: &mut Self) {
        next.previous_value = Some(self.attribute_value.clone());
    }

    fn into_history(self, valid_to: NaiveDateTime) -> NewProtocolStateHistory {
        NewProtocolStateHistory {
            instance_id: self.instance_id,
            protocol_component_id: self.protocol_component_id,
            attribute_name: self.attribute_name,
            attribute_value: self.attribute_value,
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
        let (component_ids, names): (Vec<i64>, Vec<String>) = keys.iter().cloned().unzip();
        let rows: Vec<ProtocolState> = sql_query(
            "SELECT ps.* FROM tycho.protocol_state ps \
             JOIN unnest($2::int8[], $3::text[]) AS k(component_id, attribute_name) \
             ON ps.protocol_component_id = k.component_id \
             AND ps.attribute_name = k.attribute_name \
             WHERE ps.instance_id = $1",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(component_ids)
        .bind::<Array<Text>, _>(names)
        .load(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None))?;
        Ok(rows
            .into_iter()
            .map(|row| NewProtocolState {
                instance_id: row.instance_id,
                protocol_component_id: row.protocol_component_id,
                attribute_name: row.attribute_name,
                attribute_value: row.attribute_value,
                previous_value: row.previous_value,
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
            diesel::insert_into(protocol_state::table)
                .values(chunk)
                .on_conflict((
                    protocol_state::instance_id,
                    protocol_state::protocol_component_id,
                    protocol_state::attribute_name,
                ))
                .do_update()
                .set((
                    protocol_state::attribute_value.eq(excluded(protocol_state::attribute_value)),
                    protocol_state::previous_value.eq(excluded(protocol_state::previous_value)),
                    protocol_state::valid_from.eq(excluded(protocol_state::valid_from)),
                    protocol_state::tx_index.eq(excluded(protocol_state::tx_index)),
                    protocol_state::modified_ts.eq(now),
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
        let (component_ids, names): (Vec<i64>, Vec<String>) = keys.iter().cloned().unzip();
        sql_query(
            "DELETE FROM tycho.protocol_state ps \
             USING unnest($2::int8[], $3::text[]) AS k(component_id, attribute_name) \
             WHERE ps.instance_id = $1 \
             AND ps.protocol_component_id = k.component_id \
             AND ps.attribute_name = k.attribute_name",
        )
        .bind::<Int4, _>(instance)
        .bind::<Array<BigInt>, _>(component_ids)
        .bind::<Array<Text>, _>(names)
        .execute(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None))?;
        Ok(())
    }

    async fn insert_history(
        rows: &[NewProtocolStateHistory],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            diesel::insert_into(protocol_state_history::table)
                .values(chunk)
                .execute(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, Self::ENTITY, "history", None))?;
        }
        Ok(())
    }
}
