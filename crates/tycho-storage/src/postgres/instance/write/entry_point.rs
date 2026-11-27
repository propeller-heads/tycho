//! Entry points of an instance, with their tracing params and traced results.
//!
//! Every lookup and write stays inside one instance. Two instances that use the same entry point
//! each store their own copy.
use std::collections::{HashMap, HashSet};

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{blockchain::EntryPoint, ComponentId},
    storage::StorageError,
};

use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew,
        orm::{NewEntryPoint, ProtocolComponentUsesEntryPoint},
        schema::tycho::{entry_point, protocol_component_uses_entry_point},
        InstanceId,
    },
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Stores the entry points of each component in the instance, and links each component to its
    /// entry points.
    ///
    /// An entry point that is stored already is kept, with no error, and the new links are
    /// added. Links are never removed.
    ///
    /// Fails with `NotFound` for a component that the instance does not have, and with
    /// `Unexpected` when one external id names two entry points of the instance.
    pub(crate) async fn insert_entry_points(
        &self,
        instance: InstanceId,
        new_data: &HashMap<ComponentId, HashSet<EntryPoint>>,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        if new_data.is_empty() {
            return Ok(());
        }
        let unique: HashSet<&EntryPoint> = new_data.values().flatten().collect();
        let rows: Vec<NewEntryPoint> = unique
            .iter()
            .map(|ep| NewEntryPoint {
                instance_id: instance,
                external_id: ep.external_id.clone(),
                target: ep.target.clone(),
                signature: ep.signature.clone(),
            })
            .collect();
        for chunk in rows.chunks(NewEntryPoint::MAX_BATCH_SIZE) {
            diesel::insert_into(entry_point::table)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "EntryPoint", &instance.to_string(), None)
                })?;
        }

        let external_ids: Vec<&str> = unique
            .iter()
            .map(|ep| ep.external_id.as_str())
            .collect();
        let entry_point_ids = entry_point_ids(instance, &external_ids, conn).await?;
        let component_external_ids: Vec<&str> = new_data
            .keys()
            .map(String::as_str)
            .collect();
        let component_ids = self
            .component_ids(instance, &component_external_ids, conn)
            .await?;

        let links: Vec<ProtocolComponentUsesEntryPoint> = new_data
            .iter()
            .flat_map(|(component, eps)| {
                eps.iter()
                    .map(|ep| ProtocolComponentUsesEntryPoint {
                        instance_id: instance,
                        protocol_component_id: component_ids[component],
                        entry_point_id: entry_point_ids[&ep.external_id],
                    })
            })
            .collect();
        for chunk in links.chunks(ProtocolComponentUsesEntryPoint::MAX_BATCH_SIZE) {
            diesel::insert_into(protocol_component_uses_entry_point::table)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "ProtocolComponentUsesEntryPoint",
                        &instance.to_string(),
                        None,
                    )
                })?;
        }
        Ok(())
    }
}

/// Returns the id of each entry point of the instance, by external id.
///
/// `NotFound` for the first external id that has no entry point in the instance. An external id
/// alone is not a key: the key is (external id, target, signature). So `Unexpected` when one
/// external id names two entry points of the instance. The lookup before the instance tables
/// picked one of them.
async fn entry_point_ids(
    instance: InstanceId,
    external_ids: &[&str],
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<String, i64>, StorageError> {
    let rows: Vec<(String, i64)> = entry_point::table
        .filter(entry_point::instance_id.eq(instance))
        .filter(entry_point::external_id.eq_any(external_ids))
        .select((entry_point::external_id, entry_point::id))
        .load(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, "EntryPoint", &instance.to_string(), None))?;
    let mut ids = HashMap::with_capacity(rows.len());
    for (external_id, id) in rows {
        if ids
            .insert(external_id.clone(), id)
            .is_some()
        {
            return Err(StorageError::Unexpected(format!(
                "External id {external_id} names more than one entry point of instance {instance}"
            )));
        }
    }
    if let Some(missing) = external_ids
        .iter()
        .find(|id| !ids.contains_key(**id))
    {
        return Err(StorageError::NotFound("EntryPoint".to_string(), (*missing).to_string()));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use diesel_async::AsyncPgConnection;
    use tycho_common::Bytes;

    use super::*;
    use crate::postgres::{db_fixtures, testing::instance_test_connection};

    fn target(id: u8) -> Bytes {
        Bytes::from(vec![0xe0, id])
    }

    fn ep(external_id: &str, target_id: u8) -> EntryPoint {
        EntryPoint::new(external_id.to_string(), target(target_id), "getRate()".to_string())
    }

    /// An instance of `vm:curve` with the components `pool_a` and `pool_b`.
    async fn setup() -> (AsyncPgConnection, PostgresGatewayNew, InstanceId) {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        for component in ["pool_a", "pool_b"] {
            db_fixtures::insert_instance_component(&mut conn, instance, component, &[]).await;
        }
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        (conn, gateway, instance)
    }

    /// Entry points of the instance as (external id, target), by external id.
    async fn stored_entry_points(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<(String, Bytes)> {
        entry_point::table
            .filter(entry_point::instance_id.eq(instance))
            .order_by(entry_point::external_id)
            .select((entry_point::external_id, entry_point::target))
            .load(conn)
            .await
            .expect("loading the entry points failed")
    }

    /// Links of the instance as (component external id, entry point external id), sorted.
    async fn stored_links(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<(String, String)> {
        use crate::postgres::instance::schema::tycho::protocol_component;

        protocol_component_uses_entry_point::table
            .inner_join(
                protocol_component::table.on(protocol_component::id
                    .eq(protocol_component_uses_entry_point::protocol_component_id)
                    .and(
                        protocol_component::instance_id
                            .eq(protocol_component_uses_entry_point::instance_id),
                    )),
            )
            .inner_join(
                entry_point::table.on(entry_point::id
                    .eq(protocol_component_uses_entry_point::entry_point_id)
                    .and(
                        entry_point::instance_id
                            .eq(protocol_component_uses_entry_point::instance_id),
                    )),
            )
            .filter(protocol_component_uses_entry_point::instance_id.eq(instance))
            .order_by((protocol_component::external_id, entry_point::external_id))
            .select((protocol_component::external_id, entry_point::external_id))
            .load(conn)
            .await
            .expect("loading the links failed")
    }

    #[tokio::test]
    async fn test_insert_entry_points_stores_entry_points_and_links() {
        let (mut conn, gateway, instance) = setup().await;
        let new_data = HashMap::from([
            ("pool_a".to_string(), HashSet::from([ep("ep_a", 1), ep("ep_shared", 3)])),
            ("pool_b".to_string(), HashSet::from([ep("ep_b", 2), ep("ep_shared", 3)])),
        ]);

        gateway
            .insert_entry_points(instance, &new_data, &mut conn)
            .await
            .expect("inserting the entry points failed");

        assert_eq!(
            stored_entry_points(&mut conn, instance).await,
            vec![
                ("ep_a".to_string(), target(1)),
                ("ep_b".to_string(), target(2)),
                ("ep_shared".to_string(), target(3)),
            ]
        );
        assert_eq!(
            stored_links(&mut conn, instance).await,
            vec![
                ("pool_a".to_string(), "ep_a".to_string()),
                ("pool_a".to_string(), "ep_shared".to_string()),
                ("pool_b".to_string(), "ep_b".to_string()),
                ("pool_b".to_string(), "ep_shared".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_insert_entry_points_resent_entry_point_adds_a_link_only() {
        let (mut conn, gateway, instance) = setup().await;

        for component in ["pool_a", "pool_b"] {
            let new_data = HashMap::from([(component.to_string(), HashSet::from([ep("ep", 1)]))]);
            gateway
                .insert_entry_points(instance, &new_data, &mut conn)
                .await
                .expect("inserting the entry points failed");
        }

        assert_eq!(
            stored_entry_points(&mut conn, instance).await,
            vec![("ep".to_string(), target(1))]
        );
        assert_eq!(
            stored_links(&mut conn, instance).await,
            vec![
                ("pool_a".to_string(), "ep".to_string()),
                ("pool_b".to_string(), "ep".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_insert_entry_points_stores_one_copy_per_instance() {
        let (mut conn, gateway, a) = setup().await;
        let system =
            db_fixtures::insert_protocol_system(&mut conn, "vm:balancer".to_string()).await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_component(&mut conn, b, "pool_a", &[]).await;
        let new_data = HashMap::from([("pool_a".to_string(), HashSet::from([ep("ep", 1)]))]);

        for instance in [a, b] {
            gateway
                .insert_entry_points(instance, &new_data, &mut conn)
                .await
                .expect("inserting the entry points failed");
        }

        for instance in [a, b] {
            assert_eq!(
                stored_entry_points(&mut conn, instance).await,
                vec![("ep".to_string(), target(1))]
            );
            assert_eq!(
                stored_links(&mut conn, instance).await,
                vec![("pool_a".to_string(), "ep".to_string())]
            );
        }
    }

    #[tokio::test]
    async fn test_insert_entry_points_fails_for_a_component_of_another_instance() {
        let (mut conn, gateway, a) = setup().await;
        let system =
            db_fixtures::insert_protocol_system(&mut conn, "vm:balancer".to_string()).await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_component(&mut conn, b, "pool_of_b", &[]).await;
        let new_data = HashMap::from([("pool_of_b".to_string(), HashSet::from([ep("ep", 1)]))]);

        let result = gateway
            .insert_entry_points(a, &new_data, &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolComponent".to_string(), "pool_of_b".to_string()))
        );
    }

    #[tokio::test]
    async fn test_insert_entry_points_empty_is_ok() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_entry_points(instance, &HashMap::new(), &mut conn)
            .await;

        assert_eq!(result, Ok(()));
        assert!(stored_entry_points(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_entry_point_ids_fails_for_a_shared_external_id() {
        let (mut conn, gateway, instance) = setup().await;
        let new_data = HashMap::from([
            ("pool_a".to_string(), HashSet::from([ep("ep", 1)])),
            ("pool_b".to_string(), HashSet::from([ep("ep", 2)])),
        ]);

        let result = gateway
            .insert_entry_points(instance, &new_data, &mut conn)
            .await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
        assert_eq!(
            entry_point_ids(instance, &["ep"], &mut conn)
                .await
                .map(|ids| ids.len()),
            Err(StorageError::Unexpected(format!(
                "External id ep names more than one entry point of instance {instance}"
            )))
        );
    }

    #[tokio::test]
    async fn test_entry_point_ids_fails_for_an_unknown_external_id() {
        let (mut conn, _gateway, instance) = setup().await;

        let result = entry_point_ids(instance, &["missing"], &mut conn).await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("EntryPoint".to_string(), "missing".to_string()))
        );
    }
}
