//! Component balances of an instance: one balance per component and token, versioned.
use std::collections::{BTreeSet, HashSet};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{
    sql_query,
    sql_types::{Array, BigInt, Binary, Int4},
    upsert::excluded,
    ExpressionMethods, QueryDsl,
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::protocol, storage::StorageError, Bytes};

use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew,
        origins::BatchOrigins,
        orm::{ComponentBalance, NewComponentBalance, NewComponentBalanceHistory},
        schema::tycho::{component_balance, component_balance_history},
        write::{
            versioning::{VersionedChange, VersionedRow},
            ROWS_PER_INSERT,
        },
        InstanceId,
    },
    schema::token,
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Writes the balances of a batch as new versions. Each balance takes the block timestamp and
    /// index of its transaction from `origins`.
    ///
    /// `NotFound` for a component that is not in the instance, a token that is not stored, or a
    /// transaction that is not in the batch. `Unexpected` for a balance that does not start after
    /// the stored version of its key.
    pub(crate) async fn insert_component_balances(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        balances: &[protocol::ComponentBalance],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        // A batch spans many blocks, so the same components and tokens come back many times. Look
        // up each one once.
        let external_ids: Vec<&str> = balances
            .iter()
            .map(|balance| balance.component_id.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let component_ids = self
            .component_ids(instance, &external_ids, conn)
            .await?;
        let addresses: HashSet<&Bytes> = balances
            .iter()
            .map(|balance| &balance.token)
            .collect();
        let stored_tokens: HashSet<Bytes> = token::table
            .filter(token::address.eq_any(&addresses))
            .select(token::address)
            .load::<Bytes>(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Token", "batch", None))?
            .into_iter()
            .collect();
        if let Some(missing) = balances
            .iter()
            .find(|balance| !stored_tokens.contains(&balance.token))
        {
            return Err(StorageError::NotFound("Token".to_string(), missing.token.to_string()));
        }

        let changes = balances
            .iter()
            .map(|balance| {
                let origin = origins.change(&balance.modify_tx)?;
                Ok(VersionedChange::Update(NewComponentBalance {
                    instance_id: instance,
                    protocol_component_id: component_ids[&balance.component_id],
                    token: balance.token.clone(),
                    new_balance: balance.balance.clone(),
                    // write_versions links every later version to the one it replaces.
                    previous_value: no_previous_balance(),
                    balance_float: balance.balance_float,
                    valid_from: origin.valid_from,
                    tx_index: origin.tx_index,
                }))
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        self.write_versions(instance, changes, conn)
            .await
    }
}

/// The previous value of the first version of a key. No balance comes before it, but the column
/// is NOT NULL, so it holds one zero byte, as on the old path.
fn no_previous_balance() -> Bytes {
    Bytes::from("0x00")
}

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

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::{storage::StorageError, Bytes};

    use crate::postgres::{
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            orm::{ComponentBalance, ComponentBalanceHistory, NewComponentBalance},
            schema::tycho::{component_balance, component_balance_history},
            write::versioning::VersionedChange,
            InstanceId,
        },
        testing::instance_test_connection,
    };

    struct Setup {
        conn: AsyncPgConnection,
        gateway: PostgresGatewayNew,
        instance: InstanceId,
        component: i64,
    }

    /// An instance with one component, whose balance of token `0x01` is 1 since 2020-01-01 at
    /// transaction index 0.
    async fn setup() -> Setup {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let component =
            db_fixtures::insert_instance_component(&mut conn, instance, "pool", &[]).await;
        db_fixtures::insert_instance_component_balance(
            &mut conn,
            instance,
            component,
            token(1),
            value(1),
            1.0,
        )
        .await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        Setup { conn, gateway, instance, component }
    }

    fn token(id: u8) -> Bytes {
        Bytes::from(vec![id])
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

    fn update(
        setup: &Setup,
        token_id: u8,
        new: u64,
        valid_from: NaiveDateTime,
        tx_index: i32,
    ) -> VersionedChange<NewComponentBalance> {
        VersionedChange::Update(NewComponentBalance {
            instance_id: setup.instance,
            protocol_component_id: setup.component,
            token: token(token_id),
            new_balance: value(new),
            previous_value: Bytes::from("0x00"),
            balance_float: new as f64,
            valid_from,
            tx_index,
        })
    }

    async fn write(
        setup: &mut Setup,
        changes: Vec<VersionedChange<NewComponentBalance>>,
    ) -> Result<(), StorageError> {
        setup
            .gateway
            .write_versions(setup.instance, changes, &mut setup.conn)
            .await
    }

    async fn current(setup: &mut Setup) -> Vec<ComponentBalance> {
        component_balance::table
            .filter(component_balance::instance_id.eq(setup.instance))
            .order_by(component_balance::token)
            .select(ComponentBalance::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the current balances failed")
    }

    async fn history(setup: &mut Setup) -> Vec<ComponentBalanceHistory> {
        component_balance_history::table
            .filter(component_balance_history::instance_id.eq(setup.instance))
            .order_by(component_balance_history::valid_to)
            .select(ComponentBalanceHistory::as_select())
            .load(&mut setup.conn)
            .await
            .expect("reading the balance history failed")
    }

    #[tokio::test]
    async fn test_new_balance_is_current_without_history() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, 2, 5, at, 3);

        write(&mut setup, vec![change])
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 2);
        let new = &current[1];
        assert_eq!(
            (&new.token, &new.new_balance, &new.previous_value),
            (&token(2), &value(5), &Bytes::from("0x00"))
        );
        assert_eq!((new.valid_from, new.tx_index), (at, 3));
        assert!(history(&mut setup).await.is_empty());
    }

    #[tokio::test]
    async fn test_update_moves_the_stored_balance_to_history() {
        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = update(&setup, 1, 2, at, 3);

        write(&mut setup, vec![change])
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!(current.len(), 1);
        assert_eq!((&current[0].new_balance, &current[0].previous_value), (&value(2), &value(1)));
        assert_eq!((current[0].valid_from, current[0].tx_index), (at, 3));
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].new_balance, value(1));
        assert_eq!(
            (history[0].valid_from, history[0].valid_to, history[0].tx_index),
            (stored_since(), at, 0)
        );
    }

    #[tokio::test]
    async fn test_deletion_moves_the_stored_balance_to_history() {
        use crate::postgres::instance::origins::ChangeOrigin;

        let mut setup = setup().await;
        let at = minutes_ago(1);

        let change = VersionedChange::Delete {
            key: (setup.component, token(1)),
            origin: ChangeOrigin { block_number: 2, tx_index: 0, valid_from: at },
        };

        write(&mut setup, vec![change])
            .await
            .unwrap();

        assert!(current(&mut setup).await.is_empty());
        let history = history(&mut setup).await;
        assert_eq!(history.len(), 1);
        assert_eq!(
            (&history[0].new_balance, history[0].valid_from, history[0].valid_to),
            (&value(1), stored_since(), at)
        );
    }

    #[tokio::test]
    async fn test_two_changes_of_one_key_chain_in_order() {
        let mut setup = setup().await;
        let block_ts = minutes_ago(1);

        let later = update(&setup, 1, 5, block_ts, 5);
        let earlier = update(&setup, 1, 2, block_ts, 2);

        write(&mut setup, vec![later, earlier])
            .await
            .unwrap();

        let current = current(&mut setup).await;
        assert_eq!((&current[0].new_balance, &current[0].previous_value), (&value(5), &value(2)));
        assert_eq!(current[0].tx_index, 5);
        let history = history(&mut setup).await;
        let archived: Vec<_> = history
            .iter()
            .map(|row| (row.new_balance.clone(), row.tx_index))
            .collect();
        assert_eq!(archived, vec![(value(1), 0), (value(2), 2)]);
    }

    #[tokio::test]
    async fn test_change_before_the_stored_version_fails_and_keeps_it() {
        let mut setup = setup().await;

        let change = update(&setup, 1, 2, stored_since() - Duration::days(1), 0);

        let result = write(&mut setup, vec![change]).await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
        assert_eq!(current(&mut setup).await[0].new_balance, value(1));
        assert!(history(&mut setup).await.is_empty());
    }
}
