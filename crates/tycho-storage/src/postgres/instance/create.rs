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
) -> Result<InstanceId, StorageError> {
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
            let instance = match stored {
                Some((instance, InstanceStatus::Ready)) => instance,
                Some((instance, status)) => {
                    return Err(StorageError::Unexpected(format!(
                        "instance {instance} ({protocol_system} {label}) is {status:?}, not ready"
                    ))
                    .into());
                }
                None => create(system_id, label, conn).await?,
            };
            diesel::update(schema::protocol_system::table.find(system_id))
                .set(schema::protocol_system::active_instance_id.eq(instance))
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "ProtocolSystem", protocol_system, None)
                })?;
            Ok::<_, PostgresError>(instance)
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
