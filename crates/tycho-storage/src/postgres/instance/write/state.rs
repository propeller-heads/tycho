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

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, Timelike, Utc};
    use tycho_common::Bytes;

    use super::*;
    use crate::postgres::{
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew, origins::ChangeOrigin, orm::ProtocolStateHistory,
            write::versioning::VersionedChange,
        },
        testing::instance_test_connection,
    };

    struct Setup {
        conn: AsyncPgConnection,
        gateway: PostgresGatewayNew,
        instance: InstanceId,
        component: i64,
    }

    /// An instance with one component, whose `reserve` attribute is 1 since 2020-01-01 at
    /// transaction index 0.
    async fn setup() -> Setup {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let component =
            db_fixtures::insert_instance_component(&mut conn, instance, "pool", &[]).await;
        db_fixtures::insert_instance_state(&mut conn, instance, component, "reserve", value(1))
            .await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        Setup { conn, gateway, instance, component }
    }

    fn value(value: u64) -> Bytes {
        Bytes::from(value)
    }

    fn stored_since() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    /// A time `minutes` before now, in whole seconds, so it compares equal after a round trip.
    fn minutes_ago(minutes: i64) -> NaiveDateTime {
        let ts = Utc::now().naive_utc() - Duration::minutes(minutes);
        ts.with_nanosecond(0)
            .expect("zero nanoseconds is valid")
    }

    fn origin(valid_from: NaiveDateTime, tx_index: i32) -> ChangeOrigin {
        ChangeOrigin { block_number: 2, tx_index, valid_from }
    }

    fn update(
        setup: &Setup,
        name: &str,
        new: u64,
        origin: ChangeOrigin,
    ) -> VersionedChange<NewProtocolState> {
        VersionedChange::Update(NewProtocolState {
            instance_id: setup.instance,
            protocol_component_id: setup.component,
            attribute_name: name.to_string(),
            attribute_value: value(new),
            previous_value: None,
            valid_from: origin.valid_from,
            tx_index: origin.tx_index,
        })
    }

    fn delete(
        setup: &Setup,
        name: &str,
        origin: ChangeOrigin,
    ) -> VersionedChange<NewProtocolState> {
        VersionedChange::Delete { key: (setup.component, name.to_string()), origin }
    }

    async fn write(
        setup: &mut Setup,
        changes: Vec<VersionedChange<NewProtocolState>>,
    ) -> Result<(), StorageError> {
        setup
            .gateway
            .write_versions(setup.instance, changes, &mut setup.conn)
            .await
    }

    async fn current(setup: &mut Setup) -> Vec<ProtocolState> {
        protocol_state::table
            .filter(protocol_state::instance_id.eq(setup.instance))
            .order_by(protocol_state::attribute_name)
            .select(ProtocolState::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the current state failed")
    }

    async fn history(setup: &mut Setup) -> Vec<ProtocolStateHistory> {
        protocol_state_history::table
            .filter(protocol_state_history::instance_id.eq(setup.instance))
            .select(ProtocolStateHistory::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the state history failed")
    }

    #[tokio::test]
    async fn test_new_attribute_is_current_without_history() {
        let mut setup = setup().await;
        let at = origin(minutes_ago(1), 3);

        let change = update(&setup, "fee", 30, at);

        write(&mut setup, vec![change])
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 2);
        let fee = &current[0];
        assert_eq!(
            (&fee.attribute_name, &fee.attribute_value, &fee.previous_value),
            (&"fee".to_string(), &value(30), &None)
        );
        assert_eq!((fee.valid_from, fee.tx_index), (at.valid_from, 3));
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_update_moves_the_stored_value_to_history() {
        let mut setup = setup().await;
        let at = origin(minutes_ago(1), 3);

        let change = update(&setup, "reserve", 2, at);

        write(&mut setup, vec![change])
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 1);
        let reserve = &current[0];
        assert_eq!(
            (&reserve.attribute_value, &reserve.previous_value),
            (&value(2), &Some(value(1)))
        );
        assert_eq!((reserve.valid_from, reserve.tx_index), (at.valid_from, 3));
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        let archived = &history[0];
        assert_eq!(archived.attribute_value, value(1));
        assert_eq!(
            (archived.valid_from, archived.valid_to, archived.tx_index),
            (stored_since(), at.valid_from, 0)
        );
    }

    #[tokio::test]
    async fn test_deletion_moves_the_stored_value_to_history() {
        let mut setup = setup().await;
        let at = origin(minutes_ago(1), 0);

        let change = delete(&setup, "reserve", at);

        write(&mut setup, vec![change])
            .await
            .unwrap();

        assert!(current(&mut setup).await.is_empty());
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!((history[0].valid_from, history[0].valid_to), (stored_since(), at.valid_from));
    }

    #[tokio::test]
    async fn test_change_before_the_stored_version_fails_and_keeps_it() {
        let mut setup = setup().await;
        let before = origin(stored_since() - Duration::days(1), 0);

        let change = update(&setup, "reserve", 2, before);

        let result = write(&mut setup, vec![change]).await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))));
        let current = current(&mut setup).await;
        assert_eq!(current[0].attribute_value, value(1));
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_version_ending_before_the_horizon_is_not_archived() {
        let mut setup = setup().await;
        setup.gateway.retention_horizon = minutes_ago(0);

        let change = update(&setup, "reserve", 2, origin(minutes_ago(1), 0));

        write(&mut setup, vec![change])
            .await
            .unwrap();

        assert_eq!(current(&mut setup).await[0].attribute_value, value(2));
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_version_without_history_day_fails() {
        let mut setup = setup().await;
        let far_ahead = origin(minutes_ago(0) + Duration::days(60), 0);

        let change = update(&setup, "reserve", 2, far_ahead);

        let result = write(&mut setup, vec![change]).await;

        assert!(result.is_err());
    }
}
