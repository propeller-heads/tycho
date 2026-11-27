//! Entry points of an instance, with their tracing params and traced results.
//!
//! Every lookup and write stays inside one instance. Two instances that use the same entry point
//! each store their own copy.
use std::collections::{hash_map::Entry, HashMap, HashSet};

use diesel::{prelude::*, upsert::excluded};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{
        blockchain::{EntryPoint, TracedEntryPoint, TracingParams, TracingResult},
        BlockHash, ComponentId, EntryPointId,
    },
    storage::StorageError,
    Bytes,
};

use super::block::block_origin;
use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew,
        origins::{BatchOrigins, BlockOrigin},
        orm::{
            EntryPointTracingParamsCallsAccount, EntryPointTracingType, NewEntryPoint,
            NewEntryPointTracingParams, NewEntryPointTracingResult,
            ProtocolComponentHasEntryPointTracingParams, ProtocolComponentUsesEntryPoint,
        },
        schema::tycho::{
            entry_point, entry_point_tracing_params, entry_point_tracing_params_calls_account,
            entry_point_tracing_result, protocol_component_has_entry_point_tracing_params,
            protocol_component_uses_entry_point,
        },
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

    /// Stores the tracing params of each entry point in the instance, and links each params to the
    /// component that it was sent with.
    ///
    /// Params that are stored already are kept, with no error, and the new links are added. Links
    /// are never removed.
    ///
    /// Fails with `NotFound` for an entry point or a component that the instance does not have.
    pub(crate) async fn insert_entry_point_tracing_params(
        &self,
        instance: InstanceId,
        new_data: &HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        if new_data.is_empty() {
            return Ok(());
        }
        let external_ids: Vec<&str> = new_data
            .keys()
            .map(String::as_str)
            .collect();
        let entry_point_ids = entry_point_ids(instance, &external_ids, conn).await?;
        let mut rows = Vec::new();
        for (entry_point, params_set) in new_data {
            for (params, _) in params_set {
                rows.push(NewEntryPointTracingParams {
                    instance_id: instance,
                    entry_point_id: entry_point_ids[entry_point],
                    tracing_type: EntryPointTracingType::from(params),
                    data: Some(tracing_params_data(params)?),
                });
            }
        }
        for chunk in rows.chunks(NewEntryPointTracingParams::MAX_BATCH_SIZE) {
            diesel::insert_into(entry_point_tracing_params::table)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "EntryPointTracingParams",
                        &instance.to_string(),
                        None,
                    )
                })?;
        }

        let wanted: Vec<(EntryPointId, TracingParams)> = new_data
            .iter()
            .flat_map(|(entry_point, params_set)| {
                params_set
                    .iter()
                    .map(|(params, _)| (entry_point.clone(), params.clone()))
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let params_ids = tracing_params_ids(instance, &wanted, conn).await?;
        let component_external_ids: Vec<&str> = new_data
            .values()
            .flatten()
            .map(|(_, component)| component.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let component_ids = self
            .component_ids(instance, &component_external_ids, conn)
            .await?;

        let links: Vec<ProtocolComponentHasEntryPointTracingParams> = new_data
            .iter()
            .flat_map(|(entry_point, params_set)| {
                params_set
                    .iter()
                    .map(|(params, component)| ProtocolComponentHasEntryPointTracingParams {
                        instance_id: instance,
                        protocol_component_id: component_ids[component],
                        entry_point_tracing_params_id: params_ids
                            [&(entry_point.clone(), params.clone())],
                    })
            })
            .collect();
        for chunk in links.chunks(ProtocolComponentHasEntryPointTracingParams::MAX_BATCH_SIZE) {
            diesel::insert_into(protocol_component_has_entry_point_tracing_params::table)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "ProtocolComponentHasEntryPointTracingParams",
                        &instance.to_string(),
                        None,
                    )
                })?;
        }
        Ok(())
    }

    /// Stores the traced result of each entry point and params in the instance, and links the
    /// params to every account that the trace accessed.
    ///
    /// Results of the same params are merged, in input order, then merged with the stored result
    /// of the instance. The detection block of a row is the block of the last result of its
    /// params. It comes from `origins`, or from the shared block table.
    ///
    /// Fails with `NotFound` for params or an account that the instance does not have, and with
    /// `NoRelatedEntity` for a detection block that neither `origins` nor the block table has.
    pub(crate) async fn upsert_traced_entry_points(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        traced: &[TracedEntryPoint],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        if traced.is_empty() {
            return Ok(());
        }
        let wanted: Vec<(EntryPointId, TracingParams)> = traced
            .iter()
            .map(params_key)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let params_ids = tracing_params_ids(instance, &wanted, conn).await?;
        let mut blocks: HashMap<BlockHash, BlockOrigin> = HashMap::new();
        for hash in traced
            .iter()
            .map(|tep| &tep.detection_block_hash)
        {
            if !blocks.contains_key(hash) {
                let block = block_origin(origins, hash, "TracedEntryPoint", conn).await?;
                blocks.insert(hash.clone(), block);
            }
        }

        let mut grouped: HashMap<i64, (BlockHash, TracingResult)> = HashMap::new();
        for tep in traced {
            let params_id = params_ids[&params_key(tep)];
            match grouped.entry(params_id) {
                Entry::Occupied(mut entry) => {
                    let (block, result) = entry.get_mut();
                    *block = tep.detection_block_hash.clone();
                    result.merge(tep.tracing_result.clone());
                }
                Entry::Vacant(entry) => {
                    entry.insert((tep.detection_block_hash.clone(), tep.tracing_result.clone()));
                }
            }
        }

        let mut stored = stored_results(instance, grouped.keys().copied(), conn).await?;
        let mut rows = Vec::with_capacity(grouped.len());
        for (params_id, (block_hash, mut result)) in grouped {
            if let Some(stored_result) = stored.remove(&params_id) {
                result.merge(stored_result);
            }
            let detection_data = serde_json::to_value(&result).map_err(|err| {
                StorageError::Unexpected(format!("Failed to serialize TracingResult: {err}"))
            })?;
            rows.push(NewEntryPointTracingResult {
                instance_id: instance,
                entry_point_tracing_params_id: params_id,
                detection_block_number: blocks[&block_hash].number,
                detection_block_hash: block_hash,
                detection_data,
            });
        }
        for chunk in rows.chunks(NewEntryPointTracingResult::MAX_BATCH_SIZE) {
            diesel::insert_into(entry_point_tracing_result::table)
                .values(chunk)
                .on_conflict((
                    entry_point_tracing_result::instance_id,
                    entry_point_tracing_result::entry_point_tracing_params_id,
                ))
                .do_update()
                .set((
                    entry_point_tracing_result::detection_block_number
                        .eq(excluded(entry_point_tracing_result::detection_block_number)),
                    entry_point_tracing_result::detection_block_hash
                        .eq(excluded(entry_point_tracing_result::detection_block_hash)),
                    entry_point_tracing_result::detection_data
                        .eq(excluded(entry_point_tracing_result::detection_data)),
                    entry_point_tracing_result::modified_ts
                        .eq(excluded(entry_point_tracing_result::modified_ts)),
                ))
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "EntryPointTracingResult",
                        &instance.to_string(),
                        None,
                    )
                })?;
        }

        let addresses: Vec<&Bytes> = traced
            .iter()
            .flat_map(|tep| tep.tracing_result.accessed_slots.keys())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let account_ids = self
            .account_ids(instance, &addresses, conn)
            .await?;
        let links: Vec<EntryPointTracingParamsCallsAccount> = traced
            .iter()
            .flat_map(|tep| {
                let params_id = params_ids[&params_key(tep)];
                tep.tracing_result
                    .accessed_slots
                    .keys()
                    .map(move |address| (params_id, address))
            })
            .map(|(params_id, address)| EntryPointTracingParamsCallsAccount {
                instance_id: instance,
                entry_point_tracing_params_id: params_id,
                account_id: account_ids[address],
            })
            .collect();
        for chunk in links.chunks(EntryPointTracingParamsCallsAccount::MAX_BATCH_SIZE) {
            diesel::insert_into(entry_point_tracing_params_calls_account::table)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "EntryPointTracingParamsCallsAccount",
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

/// Stored tracing params as (data, id), by entry point id and tracing type.
type StoredParams = HashMap<(i64, EntryPointTracingType), Vec<(Option<serde_json::Value>, i64)>>;

/// Returns the id of the tracing params of each (entry point external id, params) pair, in the
/// instance.
///
/// The params are found by their JSON, compared in Rust, because the unique key holds a hash
/// that a trigger computes. `NotFound` for the first pair that the instance does not have.
async fn tracing_params_ids(
    instance: InstanceId,
    wanted: &[(EntryPointId, TracingParams)],
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<(EntryPointId, TracingParams), i64>, StorageError> {
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let external_ids: Vec<&str> = wanted
        .iter()
        .map(|(entry_point, _)| entry_point.as_str())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let entry_point_ids = entry_point_ids(instance, &external_ids, conn).await?;
    let rows: Vec<(i64, EntryPointTracingType, Option<serde_json::Value>, i64)> =
        entry_point_tracing_params::table
            .filter(entry_point_tracing_params::instance_id.eq(instance))
            .filter(
                entry_point_tracing_params::entry_point_id
                    .eq_any(entry_point_ids.values().copied()),
            )
            .select((
                entry_point_tracing_params::entry_point_id,
                entry_point_tracing_params::tracing_type,
                entry_point_tracing_params::data,
                entry_point_tracing_params::id,
            ))
            .load(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(
                    err,
                    "EntryPointTracingParams",
                    &instance.to_string(),
                    None,
                )
            })?;
    let mut stored: StoredParams = HashMap::new();
    for (entry_point_id, tracing_type, data, id) in rows {
        stored
            .entry((entry_point_id, tracing_type))
            .or_default()
            .push((data, id));
    }

    let mut ids = HashMap::with_capacity(wanted.len());
    for (entry_point, params) in wanted {
        let data = Some(tracing_params_data(params)?);
        let key = (entry_point_ids[entry_point], EntryPointTracingType::from(params));
        let Some(id) = stored.get(&key).and_then(|candidates| {
            candidates
                .iter()
                .find(|(stored_data, _)| *stored_data == data)
                .map(|(_, id)| *id)
        }) else {
            return Err(StorageError::NotFound(
                "EntryPointTracingParams".to_string(),
                format!("{entry_point}, {params:?}"),
            ));
        };
        ids.insert((entry_point.clone(), params.clone()), id);
    }
    Ok(ids)
}

/// The (entry point external id, params) pair of a traced entry point.
fn params_key(tep: &TracedEntryPoint) -> (EntryPointId, TracingParams) {
    (
        tep.entry_point_with_params
            .entry_point
            .external_id
            .clone(),
        tep.entry_point_with_params
            .params
            .clone(),
    )
}

/// Returns the stored traced result of each params id in the instance, by params id.
async fn stored_results(
    instance: InstanceId,
    params_ids: impl Iterator<Item = i64>,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<i64, TracingResult>, StorageError> {
    let rows: Vec<(i64, serde_json::Value)> = entry_point_tracing_result::table
        .filter(entry_point_tracing_result::instance_id.eq(instance))
        .filter(entry_point_tracing_result::entry_point_tracing_params_id.eq_any(params_ids))
        .select((
            entry_point_tracing_result::entry_point_tracing_params_id,
            entry_point_tracing_result::detection_data,
        ))
        .load(conn)
        .await
        .map_err(|err| {
            storage_error_from_diesel(err, "EntryPointTracingResult", &instance.to_string(), None)
        })?;
    rows.into_iter()
        .map(|(params_id, data)| {
            let result = serde_json::from_value(data).map_err(|err| {
                StorageError::DecodeError(format!("Failed to deserialize TracingResult: {err}"))
            })?;
            Ok((params_id, result))
        })
        .collect()
}

/// The JSON of tracing params, as the instance stores them in `data`.
fn tracing_params_data(params: &TracingParams) -> Result<serde_json::Value, StorageError> {
    match params {
        TracingParams::RPCTracer(rpc_tracer) => serde_json::to_value(rpc_tracer).map_err(|err| {
            StorageError::Unexpected(format!("Failed to serialize RPCTracerParams: {err}"))
        }),
    }
}

#[cfg(test)]
mod tests {
    use diesel_async::AsyncPgConnection;
    use tycho_common::models::{
        blockchain::{Block, EntryPointWithTracingParams, RPCTracerParams},
        Chain,
    };

    use super::*;
    use crate::postgres::{
        db_fixtures::{self, ts},
        testing::instance_test_connection,
    };

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

    fn params(calldata: u8) -> TracingParams {
        TracingParams::RPCTracer(RPCTracerParams::new(None, Bytes::from(vec![calldata])))
    }

    /// The setup instance with the entry point `ep` on `pool_a`.
    async fn setup_with_entry_point() -> (AsyncPgConnection, PostgresGatewayNew, InstanceId) {
        let (mut conn, gateway, instance) = setup().await;
        let new_data = HashMap::from([("pool_a".to_string(), HashSet::from([ep("ep", 1)]))]);
        gateway
            .insert_entry_points(instance, &new_data, &mut conn)
            .await
            .expect("inserting the entry points failed");
        (conn, gateway, instance)
    }

    /// Tracing params data of the instance, sorted by id.
    async fn stored_params(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<Option<serde_json::Value>> {
        entry_point_tracing_params::table
            .filter(entry_point_tracing_params::instance_id.eq(instance))
            .order_by(entry_point_tracing_params::id)
            .select(entry_point_tracing_params::data)
            .load(conn)
            .await
            .expect("loading the tracing params failed")
    }

    /// Params links of the instance as (component id, params id), sorted.
    async fn stored_params_links(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<(i64, i64)> {
        protocol_component_has_entry_point_tracing_params::table
            .filter(protocol_component_has_entry_point_tracing_params::instance_id.eq(instance))
            .order_by((
                protocol_component_has_entry_point_tracing_params::protocol_component_id,
                protocol_component_has_entry_point_tracing_params::entry_point_tracing_params_id,
            ))
            .select((
                protocol_component_has_entry_point_tracing_params::protocol_component_id,
                protocol_component_has_entry_point_tracing_params::entry_point_tracing_params_id,
            ))
            .load(conn)
            .await
            .expect("loading the params links failed")
    }

    #[tokio::test]
    async fn test_insert_tracing_params_stores_params_and_links() {
        let (mut conn, gateway, instance) = setup_with_entry_point().await;
        let new_data = HashMap::from([(
            "ep".to_string(),
            HashSet::from([(params(1), "pool_a".to_string()), (params(2), "pool_a".to_string())]),
        )]);

        gateway
            .insert_entry_point_tracing_params(instance, &new_data, &mut conn)
            .await
            .expect("inserting the tracing params failed");

        let mut data = stored_params(&mut conn, instance).await;
        data.sort_by_key(|value| value.as_ref().map(ToString::to_string));
        let mut expected = vec![
            Some(tracing_params_data(&params(1)).unwrap()),
            Some(tracing_params_data(&params(2)).unwrap()),
        ];
        expected.sort_by_key(|value| value.as_ref().map(ToString::to_string));
        assert_eq!(data, expected);
        let ids = tracing_params_ids(
            instance,
            &[("ep".to_string(), params(1)), ("ep".to_string(), params(2))],
            &mut conn,
        )
        .await
        .expect("looking the params up failed");
        let pool_a = gateway
            .component_ids(instance, &["pool_a"], &mut conn)
            .await
            .unwrap()["pool_a"];
        let mut expected_links = vec![
            (pool_a, ids[&("ep".to_string(), params(1))]),
            (pool_a, ids[&("ep".to_string(), params(2))]),
        ];
        expected_links.sort();
        assert_eq!(stored_params_links(&mut conn, instance).await, expected_links);
    }

    #[tokio::test]
    async fn test_insert_tracing_params_shared_by_two_components() {
        let (mut conn, gateway, instance) = setup_with_entry_point().await;
        let new_data = HashMap::from([(
            "ep".to_string(),
            HashSet::from([(params(1), "pool_a".to_string()), (params(1), "pool_b".to_string())]),
        )]);

        gateway
            .insert_entry_point_tracing_params(instance, &new_data, &mut conn)
            .await
            .expect("inserting the tracing params failed");

        assert_eq!(
            stored_params(&mut conn, instance)
                .await
                .len(),
            1
        );
        assert_eq!(
            stored_params_links(&mut conn, instance)
                .await
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn test_insert_tracing_params_resent_adds_no_row() {
        let (mut conn, gateway, instance) = setup_with_entry_point().await;
        let new_data =
            HashMap::from([("ep".to_string(), HashSet::from([(params(1), "pool_a".to_string())]))]);

        for _ in 0..2 {
            gateway
                .insert_entry_point_tracing_params(instance, &new_data, &mut conn)
                .await
                .expect("inserting the tracing params failed");
        }

        assert_eq!(
            stored_params(&mut conn, instance)
                .await
                .len(),
            1
        );
        assert_eq!(
            stored_params_links(&mut conn, instance)
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_insert_tracing_params_fails_for_an_unknown_entry_point() {
        let (mut conn, gateway, instance) = setup_with_entry_point().await;
        let new_data = HashMap::from([(
            "missing".to_string(),
            HashSet::from([(params(1), "pool_a".to_string())]),
        )]);

        let result = gateway
            .insert_entry_point_tracing_params(instance, &new_data, &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("EntryPoint".to_string(), "missing".to_string()))
        );
    }

    #[tokio::test]
    async fn test_insert_tracing_params_fails_for_an_unknown_component() {
        let (mut conn, gateway, instance) = setup_with_entry_point().await;
        let new_data = HashMap::from([(
            "ep".to_string(),
            HashSet::from([(params(1), "missing".to_string())]),
        )]);

        let result = gateway
            .insert_entry_point_tracing_params(instance, &new_data, &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolComponent".to_string(), "missing".to_string()))
        );
    }

    #[tokio::test]
    async fn test_insert_tracing_params_empty_is_ok() {
        let (mut conn, gateway, instance) = setup_with_entry_point().await;

        let result = gateway
            .insert_entry_point_tracing_params(instance, &HashMap::new(), &mut conn)
            .await;

        assert_eq!(result, Ok(()));
        assert!(stored_params(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_tracing_params_ids_fails_for_unknown_params() {
        let (mut conn, _gateway, instance) = setup_with_entry_point().await;

        let result =
            tracing_params_ids(instance, &[("ep".to_string(), params(9))], &mut conn).await;

        assert_eq!(
            result,
            Err(StorageError::NotFound(
                "EntryPointTracingParams".to_string(),
                format!("ep, {:?}", params(9))
            ))
        );
    }

    fn account(id: u8) -> Bytes {
        Bytes::from(vec![0xac, id])
    }

    /// A traced result of `ep` with params 1 at block `block`, accessing `slot` of each account.
    fn traced(block: u64, accounts: &[u8], slot: u8) -> TracedEntryPoint {
        let accessed_slots = accounts
            .iter()
            .map(|id| (account(*id), HashSet::from([Bytes::from(vec![slot])])))
            .collect();
        TracedEntryPoint::new(
            EntryPointWithTracingParams::new(ep("ep", 1), params(1)),
            Bytes::from(block),
            TracingResult::new(HashSet::new(), accessed_slots),
        )
    }

    /// The setup instance with the params 1 of `ep` on `pool_a`, and the accounts 1 and 2. The
    /// chain `ethereum` is stored, with no block.
    async fn setup_with_params() -> (AsyncPgConnection, PostgresGatewayNew, InstanceId) {
        let (mut conn, _, instance) = setup_with_entry_point().await;
        db_fixtures::insert_chain(&mut conn, "ethereum").await;
        for id in [1, 2] {
            db_fixtures::insert_instance_account(&mut conn, instance, account(id)).await;
        }
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        let new_data =
            HashMap::from([("ep".to_string(), HashSet::from([(params(1), "pool_a".to_string())]))]);
        gateway
            .insert_entry_point_tracing_params(instance, &new_data, &mut conn)
            .await
            .expect("inserting the tracing params failed");
        (conn, gateway, instance)
    }

    /// A batch with the blocks 10 and 11.
    fn blocks_10_and_11() -> BatchOrigins {
        db_fixtures::batch_origins(&[(10, ts(10)), (11, ts(11))], &[])
    }

    /// Traced results of the instance as (block number, block hash, result).
    async fn stored_traced(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<(i64, Bytes, TracingResult)> {
        entry_point_tracing_result::table
            .filter(entry_point_tracing_result::instance_id.eq(instance))
            .select((
                entry_point_tracing_result::detection_block_number,
                entry_point_tracing_result::detection_block_hash,
                entry_point_tracing_result::detection_data,
            ))
            .load::<(i64, Bytes, serde_json::Value)>(conn)
            .await
            .expect("loading the traced results failed")
            .into_iter()
            .map(|(number, hash, data)| {
                (number, hash, serde_json::from_value(data).expect("a tracing result"))
            })
            .collect()
    }

    /// Accounts linked to tracing params in the instance, sorted.
    async fn stored_called_accounts(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<Bytes> {
        use crate::postgres::instance::schema::tycho::account as account_table;

        entry_point_tracing_params_calls_account::table
            .inner_join(
                account_table::table.on(account_table::id
                    .eq(entry_point_tracing_params_calls_account::account_id)
                    .and(
                        account_table::instance_id
                            .eq(entry_point_tracing_params_calls_account::instance_id),
                    )),
            )
            .filter(entry_point_tracing_params_calls_account::instance_id.eq(instance))
            .order_by(account_table::address)
            .select(account_table::address)
            .load(conn)
            .await
            .expect("loading the called accounts failed")
    }

    #[tokio::test]
    async fn test_upsert_traced_stores_result_with_batch_block() {
        let (mut conn, gateway, instance) = setup_with_params().await;
        let tep = traced(10, &[1], 0x01);

        gateway
            .upsert_traced_entry_points(
                instance,
                &blocks_10_and_11(),
                std::slice::from_ref(&tep),
                &mut conn,
            )
            .await
            .expect("upserting the traced entry points failed");

        assert_eq!(
            stored_traced(&mut conn, instance).await,
            vec![(10, Bytes::from(10u64), tep.tracing_result)]
        );
    }

    #[tokio::test]
    async fn test_upsert_traced_takes_block_from_public_block() {
        let (mut conn, gateway, instance) = setup_with_params().await;
        let block = Block::new(12, Chain::Ethereum, Bytes::from(12u64), Bytes::default(), ts(12));
        gateway
            .upsert_block(&[block], &mut conn)
            .await
            .expect("storing the block failed");

        gateway
            .upsert_traced_entry_points(
                instance,
                &BatchOrigins::default(),
                &[traced(12, &[1], 0x01)],
                &mut conn,
            )
            .await
            .expect("upserting the traced entry points failed");

        let rows = stored_traced(&mut conn, instance).await;
        assert_eq!((rows[0].0, &rows[0].1), (12, &Bytes::from(12u64)));
    }

    #[tokio::test]
    async fn test_upsert_traced_fails_for_an_unknown_block() {
        let (mut conn, gateway, instance) = setup_with_params().await;

        let result = gateway
            .upsert_traced_entry_points(
                instance,
                &blocks_10_and_11(),
                &[traced(99, &[1], 0x01)],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::NoRelatedEntity(
                "Block".to_string(),
                "TracedEntryPoint".to_string(),
                Bytes::from(99u64).to_string()
            ))
        );
    }

    #[tokio::test]
    async fn test_upsert_traced_merges_results_of_one_batch() {
        let (mut conn, gateway, instance) = setup_with_params().await;

        gateway
            .upsert_traced_entry_points(
                instance,
                &blocks_10_and_11(),
                &[traced(10, &[1], 0x01), traced(11, &[2], 0x02)],
                &mut conn,
            )
            .await
            .expect("upserting the traced entry points failed");

        let mut expected = traced(10, &[1], 0x01).tracing_result;
        expected.merge(traced(11, &[2], 0x02).tracing_result);
        assert_eq!(
            stored_traced(&mut conn, instance).await,
            vec![(11, Bytes::from(11u64), expected)]
        );
    }

    #[tokio::test]
    async fn test_upsert_traced_merges_with_stored_result() {
        let (mut conn, gateway, instance) = setup_with_params().await;

        for tep in [traced(10, &[1], 0x01), traced(11, &[1], 0x02)] {
            gateway
                .upsert_traced_entry_points(instance, &blocks_10_and_11(), &[tep], &mut conn)
                .await
                .expect("upserting the traced entry points failed");
        }

        let mut expected = traced(10, &[1], 0x01).tracing_result;
        expected.merge(traced(11, &[1], 0x02).tracing_result);
        assert_eq!(
            stored_traced(&mut conn, instance).await,
            vec![(11, Bytes::from(11u64), expected)]
        );
    }

    #[tokio::test]
    async fn test_upsert_traced_links_accessed_accounts() {
        let (mut conn, gateway, instance) = setup_with_params().await;

        gateway
            .upsert_traced_entry_points(
                instance,
                &blocks_10_and_11(),
                &[traced(10, &[1, 2], 0x01)],
                &mut conn,
            )
            .await
            .expect("upserting the traced entry points failed");

        assert_eq!(stored_called_accounts(&mut conn, instance).await, vec![account(1), account(2)]);
    }

    #[tokio::test]
    async fn test_upsert_traced_without_accessed_slots_adds_no_link() {
        let (mut conn, gateway, instance) = setup_with_params().await;

        gateway
            .upsert_traced_entry_points(
                instance,
                &blocks_10_and_11(),
                &[traced(10, &[], 0x01)],
                &mut conn,
            )
            .await
            .expect("upserting the traced entry points failed");

        assert_eq!(
            stored_traced(&mut conn, instance)
                .await
                .len(),
            1
        );
        assert!(stored_called_accounts(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_upsert_traced_fails_for_an_account_of_another_instance() {
        let (mut conn, gateway, a) = setup_with_params().await;
        let system =
            db_fixtures::insert_protocol_system(&mut conn, "vm:balancer".to_string()).await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_account(&mut conn, b, account(3)).await;

        let result = gateway
            .upsert_traced_entry_points(
                a,
                &blocks_10_and_11(),
                &[traced(10, &[3], 0x01)],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("Account".to_string(), account(3).to_string()))
        );
    }

    #[tokio::test]
    async fn test_upsert_traced_fails_for_unknown_params() {
        let (mut conn, gateway, instance) = setup_with_params().await;
        let mut tep = traced(10, &[1], 0x01);
        tep.entry_point_with_params.params = params(9);

        let result = gateway
            .upsert_traced_entry_points(instance, &blocks_10_and_11(), &[tep], &mut conn)
            .await;

        assert!(
            matches!(result, Err(StorageError::NotFound(ref entity, _)) if entity == "EntryPointTracingParams"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn test_upsert_traced_empty_is_ok() {
        let (mut conn, gateway, instance) = setup_with_params().await;

        let result = gateway
            .upsert_traced_entry_points(instance, &blocks_10_and_11(), &[], &mut conn)
            .await;

        assert_eq!(result, Ok(()));
        assert!(stored_traced(&mut conn, instance)
            .await
            .is_empty());
    }
}
