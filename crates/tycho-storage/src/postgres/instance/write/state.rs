//! Protocol state of an instance: one value per component and attribute name, versioned.
use std::collections::{BTreeMap, BTreeSet};

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
use tycho_common::{
    models::{protocol::ProtocolComponentStateDelta, TxHash},
    storage::StorageError,
};

use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew,
        origins::BatchOrigins,
        orm::{NewProtocolState, NewProtocolStateHistory},
        schema::tycho::{protocol_state, protocol_state_history},
        write::versioning::{VersionedChange, VersionedRowNew},
        InstanceId,
    },
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Writes the state deltas of a batch as new versions: an updated attribute starts a version,
    /// a deleted attribute ends one. Each change takes the block timestamp and index of its
    /// transaction from `origins`.
    ///
    /// `NotFound` for a component that is not in the instance or a transaction that is not in the
    /// batch. `Unexpected` for a change that does not start after the stored version of its key,
    /// and for a deleted attribute with no stored version.
    pub(crate) async fn upsert_protocol_states(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        deltas: &[(TxHash, ProtocolComponentStateDelta)],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        // A batch spans many blocks, so the same components come back many times. Look up each
        // one once.
        let external_ids: Vec<&str> = deltas
            .iter()
            .map(|(_, delta)| delta.component_id.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let component_ids = self
            .component_ids(instance, &external_ids, conn)
            .await?;

        let mut changes = Vec::new();
        for (tx, delta) in deltas {
            let origin = origins.change(tx)?;
            let component = component_ids[&delta.component_id];
            // Sorted, so that the key a failure names does not depend on the hash order.
            let updated: BTreeMap<_, _> = delta
                .updated_attributes
                .iter()
                .collect();
            for (name, value) in updated {
                changes.push(VersionedChange::Update(NewProtocolState {
                    instance_id: instance,
                    protocol_component_id: component,
                    attribute_name: name.clone(),
                    attribute_value: value.clone(),
                    // write_versions links every later version to the one it replaces.
                    previous_value: None,
                    valid_from: origin.valid_from,
                    tx_index: origin.tx_index,
                }));
            }
            let deleted: BTreeSet<_> = delta
                .deleted_attributes
                .iter()
                .collect();
            for name in deleted {
                changes.push(VersionedChange::Delete { key: (component, name.clone()), origin });
            }
        }
        self.write_versions(instance, changes, conn)
            .await
    }
}

#[async_trait]
impl VersionedRowNew for NewProtocolState {
    const ENTITY: &'static str = "ProtocolState";
    /// Component id and attribute name.
    type Key = (i64, String);
    type History = NewProtocolStateHistory;

    fn key(&self) -> Self::Key {
        (self.protocol_component_id, self.attribute_name.clone())
    }

    fn instance(&self) -> InstanceId {
        self.instance_id
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
        sql_query(
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
        .map_err(|err| {
            storage_error_from_diesel(err, Self::ENTITY, &instance.to_string(), None).into()
        })
    }

    async fn upsert_current(
        rows: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        for chunk in rows.chunks(Self::MAX_BATCH_SIZE) {
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
        for chunk in rows.chunks(NewProtocolStateHistory::MAX_BATCH_SIZE) {
            diesel::insert_into(protocol_state_history::table)
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

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use tycho_common::{
        models::{
            blockchain::{Block, Transaction},
            protocol::ProtocolComponentStateDelta,
            Chain, TxHash,
        },
        Bytes,
    };

    use super::*;
    use crate::postgres::{
        cache::WriteOp,
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            origins::{BatchOrigins, ChangeOrigin},
            orm::{ProtocolState, ProtocolStateHistory},
            schema::tycho::protocol_component,
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

    /// An instance with one component, whose `reserve` attribute is 1 since
    /// [`db_fixtures::INSTANCE_VALID_FROM`] at transaction index 0.
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
            .order_by((protocol_state_history::valid_from, protocol_state_history::tx_index))
            .select(ProtocolStateHistory::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the state history failed")
    }

    #[tokio::test]
    async fn test_new_attribute_is_current_without_history() {
        let mut setup = setup().await;
        let at = origin(db_fixtures::minutes_ago(1), 3);

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
        let at = origin(db_fixtures::minutes_ago(1), 3);

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
            (db_fixtures::INSTANCE_VALID_FROM, at.valid_from, 0)
        );
    }

    #[tokio::test]
    async fn test_two_updates_in_one_batch_link_the_history() {
        let mut setup = setup().await;
        let first = origin(db_fixtures::minutes_ago(2), 0);
        let second = origin(db_fixtures::minutes_ago(1), 0);

        let changes =
            vec![update(&setup, "reserve", 2, first), update(&setup, "reserve", 3, second)];

        write(&mut setup, changes)
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(
            (&current[0].attribute_value, &current[0].previous_value),
            (&value(3), &Some(value(2)))
        );
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 2);
        assert_eq!((&history[0].attribute_value, &history[0].previous_value), (&value(1), &None));
        assert_eq!(
            (&history[1].attribute_value, &history[1].previous_value),
            (&value(2), &Some(value(1)))
        );
    }

    #[tokio::test]
    async fn test_deletion_moves_the_stored_value_to_history() {
        let mut setup = setup().await;
        let at = origin(db_fixtures::minutes_ago(1), 0);

        let change = delete(&setup, "reserve", at);

        write(&mut setup, vec![change])
            .await
            .unwrap();

        assert!(current(&mut setup).await.is_empty());
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!(
            (history[0].valid_from, history[0].valid_to),
            (db_fixtures::INSTANCE_VALID_FROM, at.valid_from)
        );
    }

    #[tokio::test]
    async fn test_batch_changes_only_its_keys_of_its_instance() {
        let mut setup = setup().await;
        let c1 = setup.component;
        let c2 =
            db_fixtures::insert_instance_component(&mut setup.conn, setup.instance, "pool-2", &[])
                .await;
        let system =
            db_fixtures::insert_protocol_system(&mut setup.conn, "uniswap_v4".to_string()).await;
        let other = db_fixtures::insert_instance(&mut setup.conn, system, "0.0.1").await;
        // An import keeps the source database's ids, so the other instance holds the same
        // component ids.
        for (component, external_id) in [(c1, "pool"), (c2, "pool-2")] {
            sql_query(
                "INSERT INTO tycho.protocol_component
                     (instance_id, id, external_id, protocol_type, created_at, creation_block,
                      creation_tx_hash)
                 VALUES ($1, $2, $3, 'pool', '2020-01-01', 1, '\\x01')",
            )
            .bind::<Int4, _>(other)
            .bind::<BigInt, _>(component)
            .bind::<Text, _>(external_id)
            .execute(&mut setup.conn)
            .await
            .expect("inserting the imported component failed");
        }
        db_fixtures::insert_instance_state(&mut setup.conn, setup.instance, c1, "fee", value(30))
            .await;
        db_fixtures::insert_instance_state(&mut setup.conn, setup.instance, c2, "fee", value(5))
            .await;
        // `price` exists only in the other instance.
        for (component, name) in [(c1, "reserve"), (c1, "fee"), (c1, "price"), (c2, "fee")] {
            db_fixtures::insert_instance_state(&mut setup.conn, other, component, name, value(9))
                .await;
        }
        let at = origin(db_fixtures::minutes_ago(1), 0);

        let changes = vec![
            delete(&setup, "reserve", at),
            VersionedChange::Delete { key: (c2, "fee".to_string()), origin: at },
            update(&setup, "price", 7, at),
        ];

        write(&mut setup, changes)
            .await
            .unwrap();

        let instances = [setup.instance, other];
        let current_keys: Vec<(InstanceId, i64, String)> = protocol_state::table
            .filter(protocol_state::instance_id.eq_any(instances))
            .order_by((
                protocol_state::instance_id,
                protocol_state::protocol_component_id,
                protocol_state::attribute_name,
            ))
            .select((
                protocol_state::instance_id,
                protocol_state::protocol_component_id,
                protocol_state::attribute_name,
            ))
            .load(&mut setup.conn)
            .await
            .expect("reading the current state failed");
        let archived_keys: Vec<(InstanceId, i64, String)> = protocol_state_history::table
            .filter(protocol_state_history::instance_id.eq_any(instances))
            .order_by((
                protocol_state_history::instance_id,
                protocol_state_history::protocol_component_id,
                protocol_state_history::attribute_name,
            ))
            .select((
                protocol_state_history::instance_id,
                protocol_state_history::protocol_component_id,
                protocol_state_history::attribute_name,
            ))
            .load(&mut setup.conn)
            .await
            .expect("reading the state history failed");
        let key = |instance, component, name: &str| (instance, component, name.to_string());
        assert_eq!(
            current_keys,
            vec![
                key(setup.instance, c1, "fee"),
                key(setup.instance, c1, "price"),
                key(other, c1, "fee"),
                key(other, c1, "price"),
                key(other, c1, "reserve"),
                key(other, c2, "fee"),
            ]
        );
        assert_eq!(
            archived_keys,
            vec![key(setup.instance, c1, "reserve"), key(setup.instance, c2, "fee")]
        );
        let price = &current(&mut setup).await[1];
        assert_eq!((&price.attribute_name, &price.previous_value), (&"price".to_string(), &None));
    }

    #[tokio::test]
    async fn test_change_before_the_stored_version_fails_and_keeps_it() {
        let mut setup = setup().await;
        let before = origin(db_fixtures::INSTANCE_VALID_FROM - Duration::days(1), 0);

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
        setup.gateway.retention_horizon = db_fixtures::minutes_ago(0);

        let change = update(&setup, "reserve", 2, origin(db_fixtures::minutes_ago(1), 0));

        write(&mut setup, vec![change])
            .await
            .unwrap();

        assert_eq!(current(&mut setup).await[0].attribute_value, value(2));
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_version_without_history_day_fails() {
        let mut setup = setup().await;
        let far_ahead = origin(db_fixtures::minutes_ago(0) + Duration::days(60), 0);

        let change = update(&setup, "reserve", 2, far_ahead);

        let result = write(&mut setup, vec![change]).await;

        let day = far_ahead.valid_from.date().to_string();
        let instance = format!("instance {}:", setup.instance);
        assert!(
            matches!(
                &result,
                Err(StorageError::Unexpected(msg))
                    if msg.contains("no partition") && msg.contains(&instance) && msg.contains(&day)
            ),
            "{result:?}"
        );
    }

    /// A batch of block 2 at `at`, with the transaction `0xa1` at index 1 and `0xb2` at index 3.
    fn origins(at: NaiveDateTime) -> BatchOrigins {
        let block = Block::new(2, Chain::Ethereum, Bytes::from(vec![2]), Bytes::default(), at);
        let tx = |hash: u8, index: u64| {
            Transaction::new(
                Bytes::from(vec![hash]),
                block.hash.clone(),
                Bytes::default(),
                None,
                index,
            )
        };
        let ops = vec![
            WriteOp::UpsertBlock(vec![block.clone()]),
            WriteOp::UpsertTx(vec![tx(0xa1, 1), tx(0xb2, 3)]),
        ];
        BatchOrigins::from_ops(&ops).expect("a valid batch")
    }

    fn delta(
        tx: u8,
        component: &str,
        updated: &[(&str, u64)],
        deleted: &[&str],
    ) -> (TxHash, ProtocolComponentStateDelta) {
        let updated = updated
            .iter()
            .map(|(name, new)| (name.to_string(), value(*new)))
            .collect();
        let deleted = deleted
            .iter()
            .map(|name| name.to_string())
            .collect();
        (Bytes::from(vec![tx]), ProtocolComponentStateDelta::new(component, updated, deleted))
    }

    async fn upsert(
        setup: &mut Setup,
        at: NaiveDateTime,
        deltas: &[(TxHash, ProtocolComponentStateDelta)],
    ) -> Result<(), StorageError> {
        setup
            .gateway
            .upsert_protocol_states(setup.instance, &origins(at), deltas, &mut setup.conn)
            .await
    }

    #[tokio::test]
    async fn test_state_deltas_update_and_delete_attributes() {
        let mut setup = setup().await;
        db_fixtures::insert_instance_state(
            &mut setup.conn,
            setup.instance,
            setup.component,
            "fee",
            value(5),
        )
        .await;
        let at = db_fixtures::minutes_ago(1);

        upsert(&mut setup, at, &[delta(0xa1, "pool", &[("reserve", 2)], &["fee"])])
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 1);
        assert_eq!(
            (&current[0].attribute_name, &current[0].attribute_value, &current[0].previous_value),
            (&"reserve".to_string(), &value(2), &Some(value(1)))
        );
        assert_eq!((current[0].valid_from, current[0].tx_index), (at, 1));
        let mut archived: Vec<(String, Bytes)> = history(&mut setup)
            .await
            .into_iter()
            .map(|row| (row.attribute_name, row.attribute_value))
            .collect();
        archived.sort();
        assert_eq!(
            archived,
            vec![("fee".to_string(), value(5)), ("reserve".to_string(), value(1))]
        );
    }

    #[tokio::test]
    async fn test_state_deltas_order_two_txs_of_one_block_by_tx_index() {
        let mut setup = setup().await;
        let at = db_fixtures::minutes_ago(1);

        upsert(
            &mut setup,
            at,
            &[
                delta(0xb2, "pool", &[("reserve", 3)], &[]),
                delta(0xa1, "pool", &[("reserve", 2)], &[]),
            ],
        )
        .await
        .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(
            (&current[0].attribute_value, &current[0].previous_value, current[0].tx_index),
            (&value(3), &Some(value(2)), 3)
        );
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 2);
        assert_eq!(
            (&history[1].attribute_value, history[1].valid_to, history[1].tx_index),
            (&value(2), at, 1)
        );
    }

    #[tokio::test]
    async fn test_state_deltas_write_a_deleted_component() {
        let mut setup = setup().await;
        diesel::update(protocol_component::table)
            .filter(protocol_component::id.eq(setup.component))
            .set(protocol_component::deleted_at.eq(db_fixtures::minutes_ago(10)))
            .execute(&mut setup.conn)
            .await
            .expect("deleting the component failed");

        upsert(
            &mut setup,
            db_fixtures::minutes_ago(1),
            &[delta(0xa1, "pool", &[("reserve", 2)], &[])],
        )
        .await
        .unwrap();

        assert_eq!(current(&mut setup).await[0].attribute_value, value(2));
    }

    #[tokio::test]
    async fn test_state_deltas_fail_for_an_unknown_component() {
        let mut setup = setup().await;
        let system =
            db_fixtures::insert_protocol_system(&mut setup.conn, "vm:other".to_string()).await;
        let other = db_fixtures::insert_instance(&mut setup.conn, system, "0.0.1").await;
        db_fixtures::insert_instance_component(&mut setup.conn, other, "other", &[]).await;

        let result = upsert(
            &mut setup,
            db_fixtures::minutes_ago(1),
            &[delta(0xa1, "other", &[("reserve", 2)], &[])],
        )
        .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolComponent".to_string(), "other".to_string()))
        );
    }

    #[tokio::test]
    async fn test_state_deltas_fail_for_a_deleted_attribute_that_is_not_stored() {
        let mut setup = setup().await;

        let result = upsert(
            &mut setup,
            db_fixtures::minutes_ago(1),
            &[delta(0xa1, "pool", &[], &["missing"])],
        )
        .await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
    }
}
