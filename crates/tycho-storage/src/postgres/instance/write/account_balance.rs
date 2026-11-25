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
            orm::{AccountBalance, AccountBalanceHistory, NewAccountBalance},
            schema::tycho::{account_balance, account_balance_history},
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

    /// An instance with one account, whose balance of token `0x01` is 1 since 2020-01-01.
    async fn setup() -> Setup {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let account =
            db_fixtures::insert_instance_account(&mut conn, instance, Bytes::from([1u8; 20])).await;
        db_fixtures::insert_instance_account_balance(
            &mut conn,
            instance,
            account,
            Bytes::from("0x01"),
            Bytes::from(1u64),
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
        token: &str,
        balance: u64,
        valid_from: NaiveDateTime,
    ) -> VersionedChange<NewAccountBalance> {
        VersionedChange::Update(NewAccountBalance {
            instance_id: setup.instance,
            account_id: setup.account,
            token: Bytes::from(token),
            balance: Bytes::from(balance),
            valid_from,
            tx_index: 0,
        })
    }

    async fn write(setup: &mut Setup, changes: Vec<VersionedChange<NewAccountBalance>>) {
        setup
            .gateway
            .write_versions(setup.instance, changes, &mut setup.conn)
            .await
            .expect("writing the balances failed");
    }

    async fn current(setup: &mut Setup) -> Vec<AccountBalance> {
        account_balance::table
            .filter(account_balance::instance_id.eq(setup.instance))
            .order_by(account_balance::token)
            .select(AccountBalance::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the current balances failed")
    }

    async fn history(setup: &mut Setup) -> Vec<AccountBalanceHistory> {
        account_balance_history::table
            .filter(account_balance_history::instance_id.eq(setup.instance))
            .select(AccountBalanceHistory::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the balance history failed")
    }

    #[tokio::test]
    async fn test_new_balance_is_current_without_history() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, "0x02", 5, at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 2);
        assert_eq!(
            (&current[1].token, &current[1].balance, current[1].valid_from),
            (&Bytes::from("0x02"), &Bytes::from(5u64), at)
        );
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_update_moves_the_stored_balance_to_history() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, "0x01", 2, at);
        write(&mut setup, vec![change]).await;

        let current = current(&mut setup).await;
        assert_eq!((&current[0].balance, current[0].valid_from), (&Bytes::from(2u64), at));
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!(
            (&history[0].balance, history[0].valid_from, history[0].valid_to),
            (&Bytes::from(1u64), stored_since(), at)
        );
    }
}
