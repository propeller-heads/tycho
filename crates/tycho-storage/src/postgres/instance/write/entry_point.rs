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

