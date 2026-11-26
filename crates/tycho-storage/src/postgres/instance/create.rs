//! Creation of an instance before an indexer run.
use diesel::{prelude::*, sql_query, sql_types::Int4};
use diesel_async::{
    scoped_futures::ScopedFutureExt, AsyncConnection, AsyncPgConnection, RunQueryDsl,
};
use tycho_common::storage::StorageError;

use crate::postgres::{
    instance::{
        orm::{InstanceStatus, NewProtocolInstance},
        schema::tycho::protocol_instance,
        InstanceId,
    },
    schema, storage_error_from_diesel, PostgresError,
};

/// Returns the instance `label` of `protocol_system`, and makes it the active instance of the
/// protocol. Creates the instance, with its partitions, when the database does not have it yet.
/// The flag is true when this call created the instance.
///
/// The gateway that calls it has run the migrations and stored its protocol systems when it was
/// built.
///
/// Fails with `NotFound` when the protocol system is not stored, and with `Unexpected` when the
/// instance exists but is not ready.
pub async fn ensure_instance(
    protocol_system: &str,
    label: &str,
    conn: &mut AsyncPgConnection,
) -> Result<(InstanceId, bool), StorageError> {
    conn.transaction(|conn| {
        async move {
            let system_id: i64 = schema::protocol_system::table
                .filter(schema::protocol_system::name.eq(protocol_system))
                .select(schema::protocol_system::id)
                .get_result(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "ProtocolSystem", protocol_system, None)
                })?;
            let stored: Option<(InstanceId, InstanceStatus)> = protocol_instance::table
                .filter(protocol_instance::protocol_system_id.eq(system_id))
                .filter(protocol_instance::label.eq(label))
                .select((protocol_instance::id, protocol_instance::status))
                .first(conn)
                .await
                .optional()
                .map_err(|err| storage_error_from_diesel(err, "ProtocolInstance", label, None))?;
            let (instance, created) = match stored {
                Some((instance, InstanceStatus::Ready)) => (instance, false),
                Some((instance, status)) => {
                    return Err(StorageError::Unexpected(format!(
                        "instance {instance} ({protocol_system} {label}) is {status:?}, not ready"
                    ))
                    .into());
                }
                None => (create(system_id, label, conn).await?, true),
            };
            diesel::update(schema::protocol_system::table.find(system_id))
                .set(schema::protocol_system::active_instance_id.eq(instance))
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "ProtocolSystem", protocol_system, None)
                })?;
            Ok::<_, PostgresError>((instance, created))
        }
        .scope_boxed()
    })
    .await
    .map_err(StorageError::from)
}

/// Inserts the instance row, then creates and attaches its partitions.
async fn create(
    protocol_system_id: i64,
    label: &str,
    conn: &mut AsyncPgConnection,
) -> Result<InstanceId, StorageError> {
    let instance: InstanceId = diesel::insert_into(protocol_instance::table)
        .values(NewProtocolInstance {
            protocol_system_id,
            label: label.to_string(),
            status: InstanceStatus::Loading,
            base_block_number: None,
            base_block_hash: None,
            base_block_ts: None,
            source: "extractor".to_string(),
        })
        .returning(protocol_instance::id)
        .get_result(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, "ProtocolInstance", label, None))?;
    for function in ["tycho.create_instance_leaves", "tycho.attach_instance_leaves"] {
        sql_query(format!("SELECT {function}($1)"))
            .bind::<Int4, _>(instance)
            .execute(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ProtocolInstance", &instance.to_string(), None)
            })?;
    }
    diesel::update(protocol_instance::table.find(instance))
        .set(protocol_instance::status.eq(InstanceStatus::Ready))
        .execute(conn)
        .await
        .map_err(|err| {
            storage_error_from_diesel(err, "ProtocolInstance", &instance.to_string(), None)
        })?;
    Ok(instance)
}

#[cfg(test)]
mod tests {
    use diesel::{prelude::*, sql_query, sql_types::Text};
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::storage::StorageError;

    use super::ensure_instance;
    use crate::postgres::{
        db_fixtures,
        instance::{orm::InstanceStatus, schema::tycho::protocol_instance},
        schema,
        testing::instance_test_connection,
    };

    #[derive(QueryableByName)]
    struct Leaf {
        #[diesel(sql_type = Text)]
        tablename: String,
    }

    /// A test connection with the protocol system `vm:curve` stored.
    async fn setup() -> AsyncPgConnection {
        let mut conn = instance_test_connection().await;
        db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        conn
    }

    #[tokio::test]
    async fn test_ensure_instance_creates_an_active_instance_with_its_partitions() {
        let mut conn = setup().await;

        let instance = ensure_instance("vm:curve", "0.0.1", &mut conn)
            .await
            .unwrap();

        let status: InstanceStatus = protocol_instance::table
            .find(instance)
            .select(protocol_instance::status)
            .get_result(&mut conn)
            .await
            .unwrap();
        assert_eq!(status, InstanceStatus::Ready);
        let active: Option<i32> = schema::protocol_system::table
            .filter(schema::protocol_system::name.eq("vm:curve"))
            .select(schema::protocol_system::active_instance_id)
            .get_result(&mut conn)
            .await
            .unwrap();
        assert_eq!(active, Some(instance.0));
        let leaves: Vec<Leaf> = sql_query(format!(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'tycho' \
             AND tablename = 'account_i{instance}'"
        ))
        .load(&mut conn)
        .await
        .unwrap();
        let names: Vec<&str> = leaves
            .iter()
            .map(|leaf| leaf.tablename.as_str())
            .collect();
        assert_eq!(names, vec![format!("account_i{instance}")]);
    }

    #[tokio::test]
    async fn test_ensure_instance_returns_the_stored_instance() {
        let mut conn = setup().await;
        let first = ensure_instance("vm:curve", "0.0.1", &mut conn)
            .await
            .unwrap();

        let second = ensure_instance("vm:curve", "0.0.1", &mut conn)
            .await
            .unwrap();

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn test_ensure_instance_fails_for_a_dropped_instance() {
        let mut conn = setup().await;
        let instance = ensure_instance("vm:curve", "0.0.1", &mut conn)
            .await
            .unwrap();
        diesel::update(protocol_instance::table.find(instance))
            .set(protocol_instance::status.eq(InstanceStatus::Dropped))
            .execute(&mut conn)
            .await
            .unwrap();

        let result = ensure_instance("vm:curve", "0.0.1", &mut conn).await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_ensure_instance_fails_for_an_unknown_protocol_system() {
        let mut conn = instance_test_connection().await;

        let result = ensure_instance("vm:unknown", "0.0.1", &mut conn).await;

        assert!(matches!(result, Err(StorageError::NotFound(..))), "{result:?}");
    }
}
