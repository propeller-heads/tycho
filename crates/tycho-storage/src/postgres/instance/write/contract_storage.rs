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

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::Bytes;

    use crate::postgres::{
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            orm::{ContractStorage, ContractStorageHistory, NewContractStorage},
            schema::tycho::{contract_storage, contract_storage_history},
            write::versioning::VersionedChange,
            InstanceId,
        },
        testing::instance_test_connection,
    };

    struct Setup {
        conn: AsyncPgConnection,
        gateway: PostgresGatewayNew,
        instance: InstanceId,
        account: i64,
    }

    /// An instance with one account, whose slot `0x01` holds `0x0a` since 2020-01-01.
    async fn setup() -> Setup {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let account =
            db_fixtures::insert_instance_account(&mut conn, instance, Bytes::from([1u8; 20])).await;
        db_fixtures::insert_instance_slots(
            &mut conn,
            instance,
            account,
            &[(Bytes::from("0x01"), Bytes::from("0x0a"))],
        )
        .await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        Setup { conn, gateway, instance, account }
    }

    /// A time `minutes` before now, in whole seconds, so it compares equal after a round trip.
    fn minutes_ago(minutes: i64) -> NaiveDateTime {
        let ts = Utc::now().naive_utc() - Duration::minutes(minutes);
        ts.with_nanosecond(0)
            .expect("zero nanoseconds is valid")
    }

    fn stored_since() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    fn update(
        setup: &Setup,
        slot: &str,
        value: Option<&str>,
        valid_from: NaiveDateTime,
    ) -> VersionedChange<NewContractStorage> {
        VersionedChange::Update(NewContractStorage {
            instance_id: setup.instance,
            account_id: setup.account,
            slot: Bytes::from(slot),
            value: value.map(Bytes::from),
            previous_value: None,
            valid_from,
            tx_index: 0,
        })
    }

    async fn write(setup: &mut Setup, changes: Vec<VersionedChange<NewContractStorage>>) {
        setup
            .gateway
            .write_versions(setup.instance, changes, &mut setup.conn)
            .await
            .expect("writing the storage failed");
    }

    async fn current(setup: &mut Setup) -> Vec<ContractStorage> {
        contract_storage::table
            .filter(contract_storage::instance_id.eq(setup.instance))
            .order_by(contract_storage::slot)
            .select(ContractStorage::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the current storage failed")
    }

    async fn history(setup: &mut Setup) -> Vec<ContractStorageHistory> {
        contract_storage_history::table
            .filter(contract_storage_history::instance_id.eq(setup.instance))
            .select(ContractStorageHistory::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the storage history failed")
    }

    #[tokio::test]
    async fn test_new_slot_is_current_without_previous_value() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, "0x02", Some("0x0b"), at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 2);
        assert_eq!(
            (
                &current[1].slot,
                &current[1].value,
                &current[1].previous_value,
                current[1].valid_from
            ),
            (&Bytes::from("0x02"), &Some(Bytes::from("0x0b")), &None, at)
        );
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_update_links_the_previous_value_and_moves_it_to_history() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, "0x01", Some("0x0b"), at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!(
            (&current[0].value, &current[0].previous_value),
            (&Some(Bytes::from("0x0b")), &Some(Bytes::from("0x0a")))
        );
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!(
            (&history[0].value, history[0].valid_from, history[0].valid_to),
            (&Some(Bytes::from("0x0a")), stored_since(), at)
        );
    }

    #[tokio::test]
    async fn test_slot_without_value_is_a_version_with_null_value() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, "0x01", None, at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 1);
        assert_eq!(
            (&current[0].value, &current[0].previous_value, current[0].valid_from),
            (&None, &Some(Bytes::from("0x0a")), at)
        );
        assert_eq!(history(&mut setup).await.len(), 1);
    }
}
