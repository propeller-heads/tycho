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
            orm::{ContractCode, ContractCodeHistory, NewContractCode},
            schema::tycho::{contract_code, contract_code_history},
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

    /// An instance with one account, with no code.
    async fn setup() -> Setup {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let account =
            db_fixtures::insert_instance_account(&mut conn, instance, Bytes::from([1u8; 20])).await;
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

    /// Code `code` with hash `hash`, set by transaction `tx`.
    fn update(
        setup: &Setup,
        code: u8,
        hash: u8,
        tx: u8,
        valid_from: NaiveDateTime,
    ) -> VersionedChange<NewContractCode> {
        VersionedChange::Update(NewContractCode {
            instance_id: setup.instance,
            account_id: setup.account,
            code: Bytes::from(vec![code]),
            hash: Bytes::from(vec![hash]),
            modify_tx_hash: Bytes::from(vec![tx]),
            valid_from,
            tx_index: 0,
        })
    }

    async fn store_code(setup: &mut Setup, code: u8, hash: u8, tx: u8) {
        db_fixtures::insert_instance_code(
            &mut setup.conn,
            setup.instance,
            setup.account,
            Bytes::from(vec![code]),
            Bytes::from(vec![hash]),
            Bytes::from(vec![tx]),
        )
        .await;
    }

    async fn write(setup: &mut Setup, changes: Vec<VersionedChange<NewContractCode>>) {
        setup
            .gateway
            .write_versions(setup.instance, changes, &mut setup.conn)
            .await
            .expect("writing the code failed");
    }

    async fn current(setup: &mut Setup) -> Vec<ContractCode> {
        contract_code::table
            .filter(contract_code::instance_id.eq(setup.instance))
            .select(ContractCode::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the current code failed")
    }

    async fn history(setup: &mut Setup) -> Vec<ContractCodeHistory> {
        contract_code_history::table
            .filter(contract_code_history::instance_id.eq(setup.instance))
            .select(ContractCodeHistory::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the code history failed")
    }

    /// Physical location of the current code row: an UPDATE moves the row, even to the same values.
    async fn row_location(setup: &mut Setup) -> String {
        contract_code::table
            .filter(contract_code::instance_id.eq(setup.instance))
            .select(diesel::dsl::sql::<diesel::sql_types::Text>("ctid::text"))
            .get_result(&mut setup.conn)
            .await
            .expect("reading the location of the code row failed")
    }

    #[tokio::test]
    async fn test_first_code_is_current_without_history() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, 1, 0x11, 0xa1, at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 1);
        assert_eq!(
            (&current[0].code, &current[0].hash, &current[0].modify_tx_hash, current[0].valid_from),
            (&Bytes::from("0x01"), &Bytes::from("0x11"), &Bytes::from("0xa1"), at)
        );
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_new_hash_moves_the_stored_code_to_history() {
        let mut setup = setup().await;
        store_code(&mut setup, 1, 0x11, 0xa1).await;
        let at = minutes_ago(1);

        let change = update(&setup, 2, 0x22, 0xb2, at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!(
            (&current[0].code, &current[0].modify_tx_hash),
            (&Bytes::from("0x02"), &Bytes::from("0xb2"))
        );
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!(
            (&history[0].code, history[0].valid_from, history[0].valid_to),
            (&Bytes::from("0x01"), stored_since(), at)
        );
    }

    #[tokio::test]
    async fn test_same_hash_adds_no_version() {
        let mut setup = setup().await;
        store_code(&mut setup, 1, 0x11, 0xa1).await;
        let location = row_location(&mut setup).await;

        let change = update(&setup, 1, 0x11, 0xb2, minutes_ago(1));
        write(&mut setup, vec![change]).await;

        assert_eq!(row_location(&mut setup).await, location, "the stored row was rewritten");
        let current = current(&mut setup).await;
        assert_eq!(current.len(), 1);
        assert_eq!(
            (&current[0].modify_tx_hash, current[0].valid_from),
            (&Bytes::from("0xa1"), stored_since())
        );
        assert!(history(&mut setup).await.is_empty());
    }
}
