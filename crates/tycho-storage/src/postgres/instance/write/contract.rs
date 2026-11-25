//! Contracts of an instance: accounts, and the versions of their code, storage and balances.
//!
//! Each instance stores its own copy of every contract it uses, so the same address can have one
//! account in each instance.
use std::collections::HashMap;

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{storage::StorageError, Bytes};

use crate::postgres::{
    instance::{gateway::PostgresGatewayNew, schema::tycho::account, InstanceId},
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Returns the id of each account of the instance, by address. Deleted accounts are found too.
    ///
    /// `NotFound` for the first address that has no account in the instance.
    pub(crate) async fn account_ids(
        &self,
        instance: InstanceId,
        addresses: &[&Bytes],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Bytes, i64>, StorageError> {
        let ids: HashMap<Bytes, i64> = account::table
            .filter(account::instance_id.eq(instance))
            .filter(account::address.eq_any(addresses))
            .select((account::address, account::id))
            .load::<(Bytes, i64)>(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Account", &instance.to_string(), None))?
            .into_iter()
            .collect();
        if let Some(missing) = addresses
            .iter()
            .find(|address| !ids.contains_key(**address))
        {
            return Err(StorageError::NotFound("Account".to_string(), missing.to_string()));
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use tycho_common::{storage::StorageError, Bytes};

    use crate::postgres::{
        db_fixtures,
        instance::{gateway::PostgresGatewayNew, schema::tycho::account},
        testing::instance_test_connection,
    };

    fn address(id: u8) -> Bytes {
        Bytes::from(vec![id; 20])
    }

    #[tokio::test]
    async fn test_account_ids_finds_only_the_instance_accounts() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let a = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let account_of_a = db_fixtures::insert_instance_account(&mut conn, a, address(1)).await;
        db_fixtures::insert_instance_account(&mut conn, b, address(1)).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .account_ids(a, &[&address(1)], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids.len(), 1);
        assert_eq!(ids[&address(1)], account_of_a);
    }

    #[tokio::test]
    async fn test_account_ids_fails_for_an_unknown_address() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let result = gateway
            .account_ids(instance, &[&address(1), &address(2)], &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("Account".to_string(), address(2).to_string()))
        );
    }

    #[tokio::test]
    async fn test_account_ids_finds_a_deleted_account() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let id = db_fixtures::insert_instance_account(&mut conn, instance, address(1)).await;
        let deleted_at = NaiveDate::from_ymd_opt(2021, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");
        diesel::update(account::table)
            .filter(account::id.eq(id))
            .set(account::deleted_at.eq(deleted_at))
            .execute(&mut conn)
            .await
            .expect("deleting the account failed");
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .account_ids(instance, &[&address(1)], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids[&address(1)], id);
    }
}
