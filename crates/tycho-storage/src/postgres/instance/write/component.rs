//! Protocol components of an instance.
use std::collections::{HashMap, HashSet};

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::storage::StorageError;

use crate::postgres::{
    instance::{gateway::PostgresGatewayNew, schema::tycho::protocol_component, InstanceId},
    schema::protocol_type,
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Returns the id of each component of the instance, by external id. Deleted components are
    /// found too, so a batch still writes the balances of a deleted component, as the writes did
    /// before the instance tables. Do not filter on `deleted_at`.
    ///
    /// `NotFound` for the first external id that has no component in the instance.
    pub(crate) async fn component_ids(
        &self,
        instance: InstanceId,
        external_ids: &[&str],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<String, i64>, StorageError> {
        let ids: HashMap<String, i64> = protocol_component::table
            .filter(protocol_component::instance_id.eq(instance))
            .filter(protocol_component::external_id.eq_any(external_ids))
            .select((protocol_component::external_id, protocol_component::id))
            .load::<(String, i64)>(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ProtocolComponent", &instance.to_string(), None)
            })?
            .into_iter()
            .collect();
        if let Some(missing) = external_ids
            .iter()
            .find(|id| !ids.contains_key(**id))
        {
            return Err(StorageError::NotFound(
                "ProtocolComponent".to_string(),
                (*missing).to_string(),
            ));
        }
        Ok(ids)
    }
}

/// Fails with `NotFound` for the first of `names` that is not a stored protocol type.
///
/// Instance components name their type with no foreign key into `public.protocol_type`, so a
/// writer checks the names first.
pub(super) async fn check_protocol_types_stored(
    names: &[&str],
    conn: &mut AsyncPgConnection,
) -> Result<(), StorageError> {
    let unique: HashSet<&str> = names.iter().copied().collect();
    let stored: HashSet<String> = protocol_type::table
        .filter(protocol_type::name.eq_any(&unique))
        .select(protocol_type::name)
        .load::<String>(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, "ProtocolType", "batch", None))?
        .into_iter()
        .collect();
    if let Some(missing) = names
        .iter()
        .find(|name| !stored.contains(**name))
    {
        return Err(StorageError::NotFound("ProtocolType".to_string(), (*missing).to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use tycho_common::storage::StorageError;

    use super::check_protocol_types_stored;
    use crate::postgres::{
        db_fixtures,
        instance::{gateway::PostgresGatewayNew, schema::tycho::protocol_component},
        testing::instance_test_connection,
    };

    #[tokio::test]
    async fn test_component_ids_finds_only_the_instance_components() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let a = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let pool_of_a = db_fixtures::insert_instance_component(&mut conn, a, "pool", &[]).await;
        db_fixtures::insert_instance_component(&mut conn, b, "pool", &[]).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .component_ids(a, &["pool"], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids.len(), 1);
        assert_eq!(ids["pool"], pool_of_a);
    }

    #[tokio::test]
    async fn test_component_ids_fails_for_an_unknown_id() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_component(&mut conn, instance, "pool", &[]).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let result = gateway
            .component_ids(instance, &["pool", "missing"], &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolComponent".to_string(), "missing".to_string()))
        );
    }

    #[tokio::test]
    async fn test_component_ids_finds_a_deleted_component() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let pool = db_fixtures::insert_instance_component(&mut conn, instance, "pool", &[]).await;
        let deleted_at = NaiveDate::from_ymd_opt(2021, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");
        diesel::update(protocol_component::table)
            .filter(protocol_component::id.eq(pool))
            .set(protocol_component::deleted_at.eq(deleted_at))
            .execute(&mut conn)
            .await
            .expect("deleting the component failed");
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .component_ids(instance, &["pool"], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids["pool"], pool);
    }

    #[tokio::test]
    async fn test_check_protocol_types_stored_accepts_stored_names() {
        let mut conn = instance_test_connection().await;
        db_fixtures::insert_protocol_type(&mut conn, "pool", None, None, None).await;

        let result = check_protocol_types_stored(&["pool", "pool"], &mut conn).await;

        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn test_check_protocol_types_stored_fails_for_an_unknown_name() {
        let mut conn = instance_test_connection().await;
        db_fixtures::insert_protocol_type(&mut conn, "pool", None, None, None).await;

        let result = check_protocol_types_stored(&["pool", "missing"], &mut conn).await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolType".to_string(), "missing".to_string()))
        );
    }
}
