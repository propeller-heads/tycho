use std::collections::{HashMap, HashSet};

use diesel::{prelude::*, upsert::excluded};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{
        blockchain::{
            EntryPoint, EntryPointWithTracingParams, TracedEntryPoint, TracingParams, TracingResult,
        },
        Chain, ComponentId, EntryPointId, PaginationParams,
    },
    storage::{EntryPointFilter, StorageError, WithTotal},
    Bytes,
};

use super::{
    orm::{
        self, EntryPointTracingType, NewEntryPoint, NewEntryPointTracingParams,
        NewEntryPointTracingParamsCallsAccount, NewEntryPointTracingResult,
        NewProtocolComponentHasEntryPointTracingParams, NewProtocolComponentUsesEntryPoint,
    },
    schema::{self},
    storage_error_from_diesel, PostgresError, PostgresGateway,
};

impl PostgresGateway {
    /// Insert new entry points into the database. This function ignores conflicts on inserts.
    ///
    /// # Arguments
    ///
    /// * `new_data` - A map of protocol component external ids to a list of entry points.
    /// * `chain` - The chain to insert the entry points for.
    /// * `conn` - The database connection to use.
    pub(crate) async fn insert_entry_points(
        &self,
        new_data: &HashMap<ComponentId, HashSet<EntryPoint>>,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use schema::{entry_point::dsl::*, protocol_component_uses_entry_point::dsl::*};

        let chain_id = self.get_chain_id(chain)?;

        let pc_ids = orm::ProtocolComponent::ids_by_external_ids(
            new_data.keys().map(AsRef::as_ref),
            chain_id,
            conn,
        )
        .await
        .map_err(PostgresError::from)?
        .into_iter()
        .map(|(id_, ext_id)| (ext_id, id_))
        .collect::<HashMap<_, _>>();

        let new_entry_points = new_data
            .values()
            .flat_map(|ep| {
                ep.iter()
                    .map(|ep| NewEntryPoint {
                        external_id: ep.external_id.clone(),
                        target: ep.target.clone(),
                        signature: ep.signature.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        diesel::insert_into(entry_point)
            .values(&new_entry_points)
            .on_conflict_do_nothing()
            .execute(conn)
            .await
            .map_err(|e| storage_error_from_diesel(e, "EntryPoint", "Batch upsert", None))?;

        // Fetch entry points by their external_ids, we can't use .returning() on the insert above
        // because it doesn't return the ids on conflicts.
        let input_external_ids: Vec<EntryPointId> = new_data
            .values()
            .flat_map(|ep| {
                ep.iter()
                    .map(|ep| ep.external_id.clone())
            })
            .collect();

        let entry_point_ids =
            orm::EntryPoint::ids_by_external_ids(&input_external_ids, conn).await?;

        let mut pc_entry_point_links = Vec::new();

        for (pc_ext_id, eps) in new_data.iter() {
            let pc_id = match pc_ids.get(pc_ext_id) {
                Some(_id) => _id,
                None => {
                    return Err(StorageError::NotFound(
                        "ProtocolComponent".to_string(),
                        pc_ext_id.to_string(),
                    ));
                }
            };

            for ep in eps.iter() {
                let ep_id = match entry_point_ids.get(&ep.external_id) {
                    Some(_id) => _id,
                    None => {
                        return Err(StorageError::NotFound(
                            "EntryPoint".to_string(),
                            ep.external_id.clone(),
                        ));
                    }
                };

                pc_entry_point_links.push(NewProtocolComponentUsesEntryPoint {
                    protocol_component_id: *pc_id,
                    entry_point_id: *ep_id,
                });
            }
        }

        // Insert links between protocol components and entry points
        // Design choice: we don't want to delete previously inserted links here, they are
        // cumulative
        diesel::insert_into(protocol_component_uses_entry_point)
            .values(&pc_entry_point_links)
            .on_conflict_do_nothing()
            .execute(conn)
            .await
            .map_err(|e| {
                storage_error_from_diesel(
                    e,
                    "ProtocolComponentUsesEntryPoint",
                    "Batch upsert",
                    None,
                )
            })?;
        Ok(())
    }

    /// Insert entry point tracing params into the database. This function ignores conflicts on
    /// inserts.
    ///
    /// # Arguments
    ///
    /// * `new_data` - A map of entry point ids to a list of tracing params and component id related
    ///   to the tracing params.
    /// * `conn` - The database connection to use.
    pub(crate) async fn insert_entry_point_tracing_params(
        &self,
        new_data: &HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use schema::{
            entry_point_tracing_params::dsl::*,
            protocol_component_has_entry_point_tracing_params::dsl::*,
        };

        let input_external_ids: Vec<EntryPointId> = new_data.keys().cloned().collect();
        let entry_point_ids =
            orm::EntryPoint::ids_by_external_ids(&input_external_ids, conn).await?;

        let mut new_tracing_params = Vec::new();
        for (ep_id, ep) in new_data.iter() {
            let entry_point_id_ = entry_point_ids
                .get(ep_id)
                .ok_or_else(|| {
                    StorageError::NotFound("EntryPoint".to_string(), ep_id.to_string())
                })?;
            for (params, _) in ep.iter() {
                let db_params = match params {
                    TracingParams::RPCTracer(rpc_tracer) => serde_json::to_value(rpc_tracer)
                        .map_err(|e| {
                            StorageError::Unexpected(format!(
                                "Failed to serialize RPCTracerEntryPoint: {e}"
                            ))
                        })?,
                };
                new_tracing_params.push(NewEntryPointTracingParams {
                    entry_point_id: *entry_point_id_,
                    tracing_type: EntryPointTracingType::from(params),
                    data: Some(db_params),
                });
            }
        }

        diesel::insert_into(entry_point_tracing_params)
            .values(&new_tracing_params)
            .on_conflict_do_nothing()
            .execute(conn)
            .await
            .map_err(|e| {
                storage_error_from_diesel(e, "EntryPointTracingParams", "Batch upsert", None)
            })?;

        // Insert links between protocol components and tracing params
        let chain_id = self.get_chain_id(chain)?;

        // Fetch entry points tracing params, we can't use .returning() on the insert above
        // because it doesn't return the ids on conflicts.
        let params_ids = orm::EntryPointTracingParams::ids_by_entry_point_with_tracing_params(
            &new_data
                .iter()
                .flat_map(|(ep_id, ep)| {
                    ep.iter()
                        .map(|params| (ep_id.clone(), params.0.clone()))
                })
                .collect::<Vec<_>>(),
            conn,
        )
        .await?;

        let pc_ids = orm::ProtocolComponent::ids_by_external_ids(
            new_data
                .values()
                .flat_map(|set| set.iter())
                .map(|(_, pc_ext_id)| pc_ext_id.as_str()),
            chain_id,
            conn,
        )
        .await
        .map_err(PostgresError::from)?
        .into_iter()
        .map(|(id_, ext_id)| (ext_id, id_))
        .collect::<HashMap<_, _>>();

        let mut pc_tracing_params_links = Vec::new();
        for (ep_id, set) in new_data.iter() {
            for (params, pc_ext_id) in set.iter() {
                let pc_id = match pc_ids.get(pc_ext_id.as_str()) {
                    Some(_id) => _id,
                    None => {
                        return Err(StorageError::NotFound(
                            "ProtocolComponent".to_string(),
                            pc_ext_id.to_string(),
                        ));
                    }
                };

                let params_id = match params_ids.get(&(ep_id.clone(), params.clone())) {
                    Some(_id) => _id,
                    None => {
                        return Err(StorageError::NotFound(
                            "EntryPointTracingParams".to_string(),
                            format!("{ep_id:?}, {params:?}"),
                        ));
                    }
                };

                pc_tracing_params_links.push(NewProtocolComponentHasEntryPointTracingParams {
                    protocol_component_id: *pc_id,
                    entry_point_tracing_params_id: *params_id,
                });
            }
        }

        diesel::insert_into(protocol_component_has_entry_point_tracing_params)
            .values(&pc_tracing_params_links)
            .on_conflict_do_nothing()
            .execute(conn)
            .await
            .map_err(|e| {
                storage_error_from_diesel(
                    e,
                    "ProtocolComponentHasEntryPointTracingParams",
                    "Batch upsert",
                    None,
                )
            })?;

        Ok(())
    }

    /// Get entry points tracing params from the database.
    ///
    /// # Arguments
    ///
    /// * `filter` - The filter to apply to the query.
    /// * `pagination_params` - The pagination parameters to apply to the query, if None, all
    ///   results are returned.
    /// * `conn` - The database connection to use.
    ///
    /// Note: to avoid getting partial results, the pagination is applied to components, not
    /// entry points.
    pub(crate) async fn get_entry_points_tracing_params(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPointWithTracingParams>>>, StorageError>
    {
        use schema::{
            entry_point as ep, entry_point_tracing_params as eptp, protocol_component as pc,
            protocol_component_has_entry_point_tracing_params as pcheptp,
        };

        let ps_id = self.get_protocol_system_id(&filter.protocol_system)?;
        let mut component_query = schema::protocol_component::table
            .filter(pc::protocol_system_id.eq(ps_id))
            .select(pc::id)
            .into_boxed();

        if let Some(component_ids) = filter.component_ids {
            component_query = component_query.filter(pc::external_id.eq_any(component_ids));
        }

        // Apply pagination and fetch total count
        let count: Option<u64> = if let Some(pagination_params) = pagination_params {
            component_query = component_query
                .order_by(pc::id)
                .limit(pagination_params.page_size)
                .offset(pagination_params.offset());

            Some(
                schema::protocol_component::table
                    .filter(pc::protocol_system_id.eq(ps_id))
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .unwrap_or(0) as u64,
            )
        } else {
            None
        };

        // PERF: 4 inner joins is not very efficient.
        // Note: it is important that we use the protocol_component_has_entry_point_tracing_params
        // table to link components to the params they generate. The reasons are:
        // - our retracing logic relies on knowing which component generated which params.
        // - if we instead link components to entrypoints and fetch all the params for that entry
        //   point, we run the risk of returning excessive amounts of duplicate params for every
        //   component that uses it. This is especially evident for entry points that have
        //   auto-generated params, such as the hooks dci. This has potential to exponentially
        //   increase memory usage.
        let results = schema::entry_point::table
            .inner_join(eptp::table.on(ep::id.eq(eptp::entry_point_id)))
            .inner_join(pcheptp::table.on(eptp::id.eq(pcheptp::entry_point_tracing_params_id)))
            .inner_join(pc::table.on(pcheptp::protocol_component_id.eq(pc::id)))
            .filter(pc::id.eq_any(component_query))
            .select((
                pc::external_id,
                orm::EntryPoint::as_select(),
                orm::EntryPointTracingParams::as_select(),
            ))
            .load::<(String, orm::EntryPoint, orm::EntryPointTracingParams)>(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(
                    err,
                    "EntryPointWithTracingParams",
                    "None",
                    Some(format!("protocol: {:?}", filter.protocol_system)),
                )
            })?;

        let res: HashMap<String, HashSet<EntryPointWithTracingParams>> =
            results
                .into_iter()
                .fold(HashMap::new(), |mut acc, (pc_ext_id, ep, params)| {
                    acc.entry(pc_ext_id)
                        .or_default()
                        .insert(EntryPointWithTracingParams::new(ep.into(), (&params).into()));
                    acc
                });

        Ok(WithTotal { entity: res, total: count })
    }

    /// Get entry points from the database.
    ///
    /// # Arguments
    ///
    /// * `filter` - The filter to apply to the query.
    /// * `pagination_params` - The pagination parameters to apply to the query, if None, all
    ///   results are returned.
    /// * `conn` - The database connection to use.
    ///
    /// Note: to avoid getting partial results, the pagination is applied to components, not
    /// entry points.
    pub(crate) async fn get_entry_points(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPoint>>>, StorageError> {
        use schema::{
            entry_point as ep, protocol_component as pc,
            protocol_component_uses_entry_point as pcuep,
        };

        let ps_id = self.get_protocol_system_id(&filter.protocol_system)?;
        let mut component_query = schema::protocol_component::table
            .filter(pc::protocol_system_id.eq(ps_id))
            .select(pc::id)
            .into_boxed();

        if let Some(component_ids) = filter.component_ids {
            component_query = component_query.filter(pc::external_id.eq_any(component_ids));
        }

        // Apply pagination and fetch total count
        let count: Option<u64> = if let Some(pagination_params) = pagination_params {
            component_query = component_query
                .order_by(pc::id)
                .limit(pagination_params.page_size)
                .offset(pagination_params.offset());

            Some(
                schema::protocol_component::table
                    .filter(pc::protocol_system_id.eq(ps_id))
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .unwrap_or(0) as u64,
            )
        } else {
            None
        };

        let results = schema::entry_point::table
            .inner_join(pcuep::table.on(ep::id.eq(pcuep::entry_point_id)))
            .inner_join(pc::table.on(pcuep::protocol_component_id.eq(pc::id)))
            .filter(pc::id.eq_any(component_query))
            .select((pc::external_id, orm::EntryPoint::as_select()))
            .load::<(String, orm::EntryPoint)>(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(
                    err,
                    "EntryPoint",
                    "None",
                    Some(format!("protocol: {:?}", filter.protocol_system)),
                )
            })?;

        let res: HashMap<ComponentId, HashSet<EntryPoint>> =
            results
                .into_iter()
                .fold(HashMap::new(), |mut acc, (pc_ext_id, ep)| {
                    acc.entry(pc_ext_id)
                        .or_default()
                        .insert(ep.into());
                    acc
                });

        Ok(WithTotal { entity: res, total: count })
    }

    /// Upsert traced entry points into the database. Merge the tracing results for the same entry
    /// point and tracing params.
    ///
    /// # Arguments
    ///
    /// * `traced_entry_points` - The traced entry points to upsert.
    /// * `conn` - The database connection to use.
    ///
    /// Note: If we merge with existing data, we keep the latest block id for the
    /// `detection_block` field.
    pub(crate) async fn upsert_traced_entry_points(
        &self,
        traced_entry_points: &[TracedEntryPoint],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use schema::{
            entry_point as ep, entry_point_tracing_params as eptp,
            entry_point_tracing_params_calls_account::dsl::*, entry_point_tracing_result as eptr,
            entry_point_tracing_result::dsl::*,
        };

        let block_hashes: HashSet<_> = traced_entry_points
            .iter()
            .map(|tep| tep.detection_block_hash.clone())
            .collect();
        let blocks = schema::block::table
            .filter(schema::block::hash.eq_any(block_hashes))
            .select((schema::block::hash, schema::block::id))
            .load::<(Bytes, i64)>(conn)
            .await
            .map_err(PostgresError::from)?;
        let block_id_map: HashMap<_, _> = blocks.into_iter().collect();

        let params_ids = orm::EntryPointTracingParams::ids_by_entry_point_with_tracing_params(
            &traced_entry_points
                .iter()
                .map(|tep| {
                    (
                        tep.entry_point_with_params
                            .entry_point
                            .external_id
                            .clone(),
                        tep.entry_point_with_params
                            .params
                            .clone(),
                    )
                })
                .collect::<Vec<_>>(),
            conn,
        )
        .await?;

        // Group input traced entry points by params_id and merge their tracing results
        let mut grouped_traced_points: HashMap<i64, (i64, TracingResult)> = HashMap::new();
        for tep in traced_entry_points {
            let params_id = params_ids
                .get(&(
                    tep.entry_point_with_params
                        .entry_point
                        .external_id
                        .clone(),
                    tep.entry_point_with_params
                        .params
                        .clone(),
                ))
                .ok_or_else(|| {
                    StorageError::NotFound(
                        "EntryPointTracingParams".to_string(),
                        tep.entry_point_with_params
                            .entry_point
                            .external_id
                            .clone(),
                    )
                })?;

            let block_id = block_id_map
                .get(&tep.detection_block_hash)
                .copied()
                .ok_or_else(|| {
                    StorageError::NotFound(
                        "Block".to_string(),
                        tep.detection_block_hash.to_string(),
                    )
                })?;

            // Merge with existing entry for same params_id, if any
            if let Some((b_id, existing_result)) = grouped_traced_points.get_mut(params_id) {
                *b_id = block_id; // We keep the latest block id in the inserts.
                existing_result.merge(tep.tracing_result.clone());
            } else {
                grouped_traced_points.insert(*params_id, (block_id, tep.tracing_result.clone()));
            }
        }

        // Retrieve existing tracing results from database
        let existing_results = schema::entry_point_tracing_result::table
            .inner_join(eptp::table.on(eptr::entry_point_tracing_params_id.eq(eptp::id)))
            .inner_join(ep::table.on(eptp::entry_point_id.eq(ep::id)))
            .filter(
                eptr::entry_point_tracing_params_id.eq_any(grouped_traced_points.keys().cloned()),
            )
            .select((eptr::entry_point_tracing_params_id, eptr::detection_data))
            .load::<(i64, serde_json::Value)>(conn)
            .await
            .map_err(|e| storage_error_from_diesel(e, "TracingResult", "Query", None))?;

        // Create a map of existing results by params_id
        let existing_results_map: HashMap<i64, TracingResult> = existing_results
            .into_iter()
            .map(|(params_id, data)| {
                let tracing_result: TracingResult = serde_json::from_value(data).map_err(|e| {
                    StorageError::DecodeError(format!("Failed to deserialize TracingResult: {e}"))
                })?;
                Ok((params_id, tracing_result))
            })
            .collect::<Result<HashMap<_, _>, StorageError>>()?;

        // Merge existing results with new grouped results
        let mut final_values = Vec::new();
        for (params_id, (block_id, mut new_result)) in grouped_traced_points {
            // If there's an existing result, merge it with the new one
            if let Some(existing_result) = existing_results_map.get(&params_id) {
                new_result.merge(existing_result.clone());
            }

            let tracing_result = serde_json::to_value(&new_result).map_err(|e| {
                StorageError::Unexpected(format!("Failed to serialize TracingResult: {e}"))
            })?;

            final_values.push(NewEntryPointTracingResult {
                entry_point_tracing_params_id: params_id,
                detection_block: block_id,
                detection_data: tracing_result,
            });
        }

        // Insert protocol components in batches to avoid exceeding PostgreSQL parameter limit
        for chunk in final_values.chunks(orm::NewEntryPointTracingResult::MAX_BATCH_SIZE) {
            diesel::insert_into(entry_point_tracing_result)
                .values(chunk)
                .on_conflict(schema::entry_point_tracing_result::entry_point_tracing_params_id)
                .do_update()
                .set((
                    detection_block.eq(excluded(detection_block)),
                    detection_data.eq(excluded(detection_data)),
                    modified_ts.eq(excluded(modified_ts)),
                ))
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "NewEntryPointTracingResult",
                        "Batch upsert",
                        None,
                    )
                })?;
        }

        let all_called_addresses: HashSet<_> = traced_entry_points
            .iter()
            .flat_map(|tep| tep.tracing_result.accessed_slots.keys())
            .cloned()
            .collect();

        let accounts = schema::account::table
            .filter(schema::account::address.eq_any(&all_called_addresses))
            .select((schema::account::address, schema::account::id))
            .load::<(Bytes, i64)>(conn)
            .await
            .map_err(PostgresError::from)?;

        let account_id_map: HashMap<_, _> = accounts.into_iter().collect();

        let mut new_entry_point_calls_account = Vec::new();
        for tep in traced_entry_points {
            let params_id = params_ids
                .get(&(
                    tep.entry_point_with_params
                        .entry_point
                        .external_id
                        .clone(),
                    tep.entry_point_with_params
                        .params
                        .clone(),
                ))
                .ok_or_else(|| {
                    StorageError::NotFound(
                        "EntryPointTracingParams".to_string(),
                        tep.entry_point_with_params
                            .entry_point
                            .external_id
                            .clone(),
                    )
                })?;
            for address in tep.tracing_result.accessed_slots.keys() {
                let acc_id = account_id_map
                    .get(address)
                    .ok_or_else(|| {
                        StorageError::NotFound("Account".to_string(), address.to_string())
                    })?;

                new_entry_point_calls_account.push(NewEntryPointTracingParamsCallsAccount {
                    entry_point_tracing_params_id: *params_id,
                    account_id: *acc_id,
                });
            }
        }

        diesel::insert_into(entry_point_tracing_params_calls_account)
            .values(&new_entry_point_calls_account)
            .on_conflict_do_nothing() // Design choice: we don't want to delete previously inserted links here, they are
            // cumulative
            .execute(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "EntryPointPointsToAccount", "Batch upsert", None)
            })?;

        Ok(())
    }

    /// Get all tracing results for a set of entry points from the database.
    ///
    /// # Arguments
    ///
    /// * `entry_points` - The entry point ids to get tracing results for.
    /// * `conn` - The database connection to use.
    pub(crate) async fn get_tracing_results(
        &self,
        entry_points: &HashSet<EntryPointId>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<EntryPointId, HashMap<TracingParams, TracingResult>>, StorageError> {
        use schema::{
            entry_point as ep, entry_point_tracing_params as eptp,
            entry_point_tracing_result as eptr,
        };
        let entry_point_ids = orm::EntryPoint::ids_by_external_ids(
            &entry_points
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            conn,
        )
        .await?;

        let results = schema::entry_point_tracing_result::table
            .inner_join(eptp::table.on(eptr::entry_point_tracing_params_id.eq(eptp::id)))
            .inner_join(ep::table.on(eptp::entry_point_id.eq(ep::id)))
            .filter(eptp::entry_point_id.eq_any(entry_point_ids.values().cloned()))
            .select((
                ep::external_id,
                orm::EntryPointTracingParams::as_select(),
                eptr::detection_data,
            ))
            .load::<(String, orm::EntryPointTracingParams, serde_json::Value)>(conn)
            .await
            .map_err(|e| storage_error_from_diesel(e, "TracingResult", "Query", None))?;

        let mut results_by_entry_point = HashMap::new();
        for (ep_ext_id, tracing_params, tracing_result) in results {
            let converted_tracing_result = serde_json::from_value(tracing_result).map_err(|e| {
                StorageError::DecodeError(format!("Failed to deserialize TracingResult: {e}"))
            })?;
            results_by_entry_point
                .entry(ep_ext_id.clone())
                .or_insert_with(HashMap::new)
                .insert((&tracing_params).into(), converted_tracing_result);
        }

        Ok(results_by_entry_point)
    }
}

#[cfg(test)]
mod test {
    use std::{slice, str::FromStr};

    use diesel_async::AsyncConnection;
    use tycho_common::{
        keccak256,
        models::{
            blockchain::{
                AddressStorageLocation, RPCTracerParams, TracedEntryPoint, TracingParams,
                TracingResult,
            },
            FinancialType, ImplementationType, StoreKey,
        },
        Bytes,
    };

    use super::*;
    use crate::postgres::db_fixtures;

    async fn setup_db() -> AsyncPgConnection {
        let db_url = std::env::var("DATABASE_URL").unwrap();
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .unwrap();
        conn.begin_test_transaction()
            .await
            .unwrap();
        conn
    }

    async fn setup_data(conn: &mut AsyncPgConnection) -> i64 {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;

        let blk = db_fixtures::insert_blocks(conn, chain_id).await;

        let txn = db_fixtures::insert_txns(
            conn,
            &[(blk[0], 1i64, "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945")],
        )
        .await;

        db_fixtures::insert_account(
            conn,
            "6b175474e89094c44da98b954eedeac495271d0f",
            "test_account",
            chain_id,
            Some(txn[0]),
        )
        .await;

        let ps_id = db_fixtures::insert_protocol_system(conn, "test_protocol".to_string()).await;
        let unknown_ps_id = db_fixtures::insert_protocol_system(conn, "unknown".to_string()).await;

        let protocol_type_id = db_fixtures::insert_protocol_type(
            conn,
            "Pool",
            Some(FinancialType::Swap),
            None,
            Some(ImplementationType::Custom),
        )
        .await;
        db_fixtures::insert_protocol_component(
            conn,
            "pc_0",
            chain_id,
            ps_id,
            protocol_type_id,
            txn[0],
            None,
            None,
        )
        .await;

        db_fixtures::insert_protocol_component(
            conn,
            "pc_1",
            chain_id,
            ps_id,
            protocol_type_id,
            txn[0],
            None,
            None,
        )
        .await;
        db_fixtures::insert_protocol_component(
            conn,
            "pc_2",
            chain_id,
            ps_id,
            protocol_type_id,
            txn[0],
            None,
            None,
        )
        .await;

        db_fixtures::insert_protocol_component(
            conn,
            "unknown_pc",
            chain_id,
            unknown_ps_id,
            protocol_type_id,
            txn[0],
            None,
            None,
        )
        .await;

        chain_id
    }

    fn rpc_tracer_entry_point(version: u8) -> EntryPoint {
        match version {
            0 => EntryPoint::new(
                "0xEdf63cce4bA70cbE74064b7687882E71ebB0e988:getRate()".to_string(),
                Bytes::from_str("0xEdf63cce4bA70cbE74064b7687882E71ebB0e988").unwrap(),
                "getRate()".to_string(),
            ),
            1 => EntryPoint::new(
                "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2:totalSupply()".to_string(),
                Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                "totalSupply()".to_string(),
            ),
            _ => panic!("Invalid rpc_tracer_entry_point version"),
        }
    }

    fn tracing_params(version: u8) -> TracingParams {
        match version {
            0 => TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("getRate()")[0..4]),
            )),
            1 => TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("totalSupply()")[0..4]),
            )),
            _ => panic!("Invalid tracing_params version"),
        }
    }

    fn traced_entry_point() -> TracedEntryPoint {
        let entry_point_with_tracing_params =
            EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(0));
        TracedEntryPoint::new(
            entry_point_with_tracing_params,
            Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                .unwrap(),
            TracingResult::new(
                vec![(
                    Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
                    AddressStorageLocation::new(
                        StoreKey::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap(),
                        12,
                    ),
                )]
                .into_iter()
                .collect(),
                HashMap::from([(
                    Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
                    HashSet::from([Bytes::from_str(
                        "0x0000000000000000000000000000000000000000000000000000000000000001",
                    )
                    .unwrap()]),
                )]),
            ),
        )
    }

    #[tokio::test]
    async fn test_entry_points_round_trip() {
        let mut conn = setup_db().await;
        let _chain_id = setup_data(&mut conn).await;
        let gw = PostgresGateway::from_connection(&mut conn).await;

        gw.insert_entry_points(
            &HashMap::from([
                ("pc_0".to_string(), HashSet::from([rpc_tracer_entry_point(0)])),
                (
                    "pc_1".to_string(),
                    HashSet::from([rpc_tracer_entry_point(0), rpc_tracer_entry_point(1)]),
                ),
                ("pc_2".to_string(), HashSet::from([rpc_tracer_entry_point(1)])),
                ("unknown_pc".to_string(), HashSet::from([rpc_tracer_entry_point(0)])),
            ]),
            &Chain::Ethereum,
            &mut conn,
        )
        .await
        .unwrap();

        let filter = EntryPointFilter::new("test_protocol".to_string());
        let retrieved_entry_points = gw
            .get_entry_points(filter, None, &mut conn)
            .await
            .unwrap();

        assert_eq!(
            retrieved_entry_points.entity,
            HashMap::from([
                ("pc_0".to_string(), HashSet::from([rpc_tracer_entry_point(0)])),
                (
                    "pc_1".to_string(),
                    HashSet::from([rpc_tracer_entry_point(0), rpc_tracer_entry_point(1)])
                ),
                ("pc_2".to_string(), HashSet::from([rpc_tracer_entry_point(1)])),
            ])
        );

        let filter = EntryPointFilter::new("test_protocol".to_string())
            .with_component_ids(vec!["pc_1".to_string(), "unknown_pc".to_string()]);
        let retrieved_entry_points = gw
            .get_entry_points(filter, None, &mut conn)
            .await
            .unwrap();

        assert_eq!(
            retrieved_entry_points.entity,
            HashMap::from([(
                "pc_1".to_string(),
                HashSet::from([rpc_tracer_entry_point(0), rpc_tracer_entry_point(1)])
            ),])
        );
    }

    #[tokio::test]
    async fn test_entry_points_with_data_round_trip() {
        let mut conn = setup_db().await;
        let _chain_id = setup_data(&mut conn).await;
        let gw = PostgresGateway::from_connection(&mut conn).await;

        let insert_ep_data = &HashMap::from([
            ("pc_0".to_string(), HashSet::from([rpc_tracer_entry_point(0)])),
            (
                "pc_1".to_string(),
                HashSet::from([rpc_tracer_entry_point(0), rpc_tracer_entry_point(1)]),
            ),
            ("pc_2".to_string(), HashSet::from([rpc_tracer_entry_point(1)])),
            ("unknown_pc".to_string(), HashSet::from([rpc_tracer_entry_point(0)])),
        ]);
        gw.insert_entry_points(insert_ep_data, &Chain::Ethereum, &mut conn)
            .await
            .unwrap();

        let insert_eptp_data = &HashMap::from([
            (
                rpc_tracer_entry_point(0)
                    .external_id
                    .clone(),
                HashSet::from([
                    (tracing_params(0), "pc_0".to_string()),
                    (tracing_params(0), "unknown_pc".to_string()),
                    (tracing_params(1), "pc_1".to_string()),
                ]),
            ),
            (
                rpc_tracer_entry_point(1)
                    .external_id
                    .clone(),
                HashSet::from([
                    (tracing_params(0), "pc_1".to_string()),
                    (tracing_params(1), "pc_2".to_string()),
                ]),
            ),
        ]);
        dbg!(&insert_eptp_data);
        gw.insert_entry_point_tracing_params(insert_eptp_data, &Chain::Ethereum, &mut conn)
            .await
            .unwrap();

        let filter = EntryPointFilter::new("test_protocol".to_string());
        let retrieved_entry_points = gw
            .get_entry_points_tracing_params(filter, None, &mut conn)
            .await
            .unwrap();

        assert_eq!(
            retrieved_entry_points.entity,
            HashMap::from([
                (
                    "pc_1".to_string(),
                    HashSet::from([
                        EntryPointWithTracingParams::new(
                            rpc_tracer_entry_point(0),
                            tracing_params(1)
                        ),
                        EntryPointWithTracingParams::new(
                            rpc_tracer_entry_point(1),
                            tracing_params(0)
                        ),
                    ])
                ),
                (
                    "pc_0".to_string(),
                    HashSet::from([EntryPointWithTracingParams::new(
                        rpc_tracer_entry_point(0),
                        tracing_params(0)
                    )])
                ),
                (
                    "pc_2".to_string(),
                    HashSet::from([EntryPointWithTracingParams::new(
                        rpc_tracer_entry_point(1),
                        tracing_params(1)
                    )])
                ),
            ])
        );

        let filter = EntryPointFilter::new("test_protocol".to_string())
            .with_component_ids(vec!["pc_1".to_string()]);
        let retrieved_entry_points = gw
            .get_entry_points_tracing_params(filter, None, &mut conn)
            .await
            .unwrap();

        assert_eq!(
            retrieved_entry_points.entity,
            HashMap::from([(
                "pc_1".to_string(),
                HashSet::from([
                    EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(1)),
                    EntryPointWithTracingParams::new(rpc_tracer_entry_point(1), tracing_params(0)),
                ])
            ),])
        );

        //Test pagination
        let filter = EntryPointFilter::new("test_protocol".to_string());
        let retrieved_entry_points = gw
            .get_entry_points_tracing_params(filter, Some(&PaginationParams::new(1, 1)), &mut conn)
            .await
            .unwrap();

        assert_eq!(
            retrieved_entry_points.entity,
            HashMap::from([(
                "pc_1".to_string(),
                HashSet::from([
                    EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(1)),
                    EntryPointWithTracingParams::new(rpc_tracer_entry_point(1), tracing_params(0)),
                ])
            ),])
        );
    }

    #[tokio::test]
    async fn test_traced_entry_points_round_trip() {
        let mut conn = setup_db().await;
        let _chain_id = setup_data(&mut conn).await;
        let gw = PostgresGateway::from_connection(&mut conn).await;

        let entry_point = rpc_tracer_entry_point(0);
        let traced_entry_point = traced_entry_point();

        gw.insert_entry_points(
            &HashMap::from([("pc_0".to_string(), HashSet::from([entry_point.clone()]))]),
            &Chain::Ethereum,
            &mut conn,
        )
        .await
        .unwrap();

        gw.insert_entry_point_tracing_params(
            &HashMap::from([(
                entry_point.external_id.clone(),
                HashSet::from([(tracing_params(0), "pc_0".to_string())]),
            )]),
            &Chain::Ethereum,
            &mut conn,
        )
        .await
        .unwrap();

        gw.upsert_traced_entry_points(slice::from_ref(&traced_entry_point), &mut conn)
            .await
            .unwrap();

        let retrieved_traced_entry_points = gw
            .get_tracing_results(&HashSet::from([entry_point.external_id.clone()]), &mut conn)
            .await
            .unwrap();

        assert_eq!(
            retrieved_traced_entry_points,
            HashMap::from([(
                entry_point.external_id.clone(),
                HashMap::from([(tracing_params(0), traced_entry_point.tracing_result)])
            )])
        );
    }

    #[tokio::test]
    async fn test_traced_entry_points_merging() {
        let mut conn = setup_db().await;
        let chain_id = setup_data(&mut conn).await;
        let gw = PostgresGateway::from_connection(&mut conn).await;

        let entry_point = rpc_tracer_entry_point(0);

        gw.insert_entry_points(
            &HashMap::from([("pc_0".to_string(), HashSet::from([entry_point.clone()]))]),
            &Chain::Ethereum,
            &mut conn,
        )
        .await
        .unwrap();

        gw.insert_entry_point_tracing_params(
            &HashMap::from([(
                entry_point.external_id.clone(),
                HashSet::from([(tracing_params(0), "pc_0".to_string())]),
            )]),
            &Chain::Ethereum,
            &mut conn,
        )
        .await
        .unwrap();

        // Need to insert additional accounts for different contracts
        db_fixtures::insert_account(
            &mut conn,
            "a0b86a33e6e3a8f0c8a77c5b6a4c8d8e8f8a8b8c",
            "contract_a",
            chain_id,
            None,
        )
        .await;

        db_fixtures::insert_account(
            &mut conn,
            "b1c97b44f7f4b9f1d9b88d6c7b5d9e9f9b9c9d",
            "contract_b",
            chain_id,
            None,
        )
        .await;

        // Insert initial tracing result into database (with Contract A)
        let contract_a_address =
            Bytes::from_str("0xa0b86a33e6e3a8f0c8a77c5b6a4c8d8e8f8a8b8c").unwrap();
        let initial_traced_entry_point =
            TracedEntryPoint::new(
                EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(0)),
                Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                    .unwrap(),
                TracingResult::new(
                    vec![(
                    contract_a_address.clone(),
                    AddressStorageLocation::new(StoreKey::from_str(
                        "0x0000000000000000000000000000000000000000000000000000000000000001",
                    )
                    .unwrap(),
                                                0)
                )]
                    .into_iter()
                    .collect(),
                    HashMap::from([(
                        contract_a_address.clone(),
                        HashSet::from([Bytes::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        )
                        .unwrap()]),
                    )]),
                ),
            );

        gw.upsert_traced_entry_points(slice::from_ref(&initial_traced_entry_point), &mut conn)
            .await
            .unwrap();

        // Call upsert with multiple traced entry points - testing both:
        // A) Merging within the same call (multiple TracedEntryPoint with same params)
        // B) Merging with existing database data
        let contract_b_address =
            Bytes::from_str("0xb1c97b44f7f4b9f1d9b88d6c7b5d9e9f9b9c9d").unwrap();
        let contract_c_address =
            Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();

        let second_traced_entry_point =
            TracedEntryPoint::new(
                EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(0)),
                Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                    .unwrap(),
                TracingResult::new(
                    vec![(
                    contract_b_address.clone(),
                    AddressStorageLocation::new(StoreKey::from_str(
                        "0x0000000000000000000000000000000000000000000000000000000000000002",
                    )
                    .unwrap()
                    ,0),
                )]
                    .into_iter()
                    .collect(),
                    HashMap::from([(
                        contract_b_address.clone(),
                        HashSet::from([Bytes::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000002",
                        )
                        .unwrap()]),
                    )]),
                ),
            );

        let third_traced_entry_point = TracedEntryPoint::new(
            EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(0)),
            Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                .unwrap(),
            TracingResult::new(
                vec![
                    (
                        contract_c_address.clone(),
                        AddressStorageLocation::new(StoreKey::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000003",
                        )
                        .unwrap(), 0),
                    ),
                    (
                        contract_a_address.clone(),
                        AddressStorageLocation::new(StoreKey::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000003",
                        )
                        .unwrap(), 0),
                    ),
                ]
                .into_iter()
                .collect(),
                HashMap::from([
                    (
                        contract_c_address.clone(),
                        HashSet::from([Bytes::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000003",
                        )
                        .unwrap()]),
                    ),
                    (
                        contract_a_address.clone(),
                        HashSet::from([Bytes::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000003",
                        )
                        .unwrap()]),
                    ),
                ]),
            ),
        );

        let fourth_traced_entry_point =
            TracedEntryPoint::new(
                EntryPointWithTracingParams::new(rpc_tracer_entry_point(0), tracing_params(0)),
                Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                    .unwrap(),
                TracingResult::new(
                    vec![(
                    contract_c_address.clone(),
                    AddressStorageLocation::new(StoreKey::from_str(
                        "0x0000000000000000000000000000000000000000000000000000000000000004",
                    )
                    .unwrap(), 0),
                )]
                    .into_iter()
                    .collect(),
                    HashMap::from([(
                        contract_c_address.clone(),
                        HashSet::from([Bytes::from_str(
                            "0x0000000000000000000000000000000000000000000000000000000000000004",
                        )
                        .unwrap()]),
                    )]),
                ),
            );

        // This single call tests merging:
        // 1. Merge third_traced_entry_point and fourth_traced_entry_point (same contract, same
        //    call)
        // 2. Merge second_traced_entry_point (different contract B, same call)
        // 3. Merge all results with initial_traced_entry_point from database (Contract A, DB merge)
        gw.upsert_traced_entry_points(
            &[second_traced_entry_point, third_traced_entry_point, fourth_traced_entry_point],
            &mut conn,
        )
        .await
        .unwrap();

        // SCENARIO 3: Verify comprehensive merging
        let retrieved_traced_entry_points = gw
            .get_tracing_results(&HashSet::from([entry_point.external_id.clone()]), &mut conn)
            .await
            .unwrap();

        let merged_result = retrieved_traced_entry_points
            .get(&entry_point.external_id)
            .unwrap()
            .get(&tracing_params(0))
            .unwrap();

        // Should contain retriggers from all four results:
        // - One from initial DB entry (Contract A, slot 1)
        // - One from second entry in the batch (Contract B, slot 2)
        // - One from third entry in the batch (Contract C, slot 3)
        // - One from third entry in the batch (Contract A, slot 3)
        // - One from fourth entry in the batch (Contract C, slot 4)
        assert_eq!(merged_result.retriggers.len(), 5);
        assert!(merged_result.retriggers.contains(&(
            contract_a_address.clone(),
            AddressStorageLocation::new(
                StoreKey::from_str(
                    "0x0000000000000000000000000000000000000000000000000000000000000001"
                )
                .unwrap(),
                0
            ),
        )));
        assert!(merged_result.retriggers.contains(&(
            contract_a_address.clone(),
            AddressStorageLocation::new(
                StoreKey::from_str(
                    "0x0000000000000000000000000000000000000000000000000000000000000003"
                )
                .unwrap(),
                0
            ),
        )));
        assert!(merged_result.retriggers.contains(&(
            contract_b_address.clone(),
            AddressStorageLocation::new(
                StoreKey::from_str(
                    "0x0000000000000000000000000000000000000000000000000000000000000002"
                )
                .unwrap(),
                0
            ),
        )));
        assert!(merged_result.retriggers.contains(&(
            contract_c_address.clone(),
            AddressStorageLocation::new(
                StoreKey::from_str(
                    "0x0000000000000000000000000000000000000000000000000000000000000003"
                )
                .unwrap(),
                0
            ),
        )));
        assert!(merged_result.retriggers.contains(&(
            contract_c_address.clone(),
            AddressStorageLocation::new(
                StoreKey::from_str(
                    "0x0000000000000000000000000000000000000000000000000000000000000004"
                )
                .unwrap(),
                0
            ),
        )));

        // Should contain accessed_slots for all three different contracts
        assert_eq!(merged_result.accessed_slots.len(), 3);

        // Contract A should have its slot
        let contract_a_slots = &merged_result.accessed_slots[&contract_a_address];
        assert_eq!(contract_a_slots.len(), 2);
        assert!(contract_a_slots.contains(
            &Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap()
        ));
        assert!(contract_a_slots.contains(
            &Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000003")
                .unwrap()
        ));

        // Contract B should have its slot
        let contract_b_slots = &merged_result.accessed_slots[&contract_b_address];
        assert_eq!(contract_b_slots.len(), 1);
        assert!(contract_b_slots.contains(
            &Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap()
        ));

        // Original DAI contract should have both slots from third and fourth entries
        let original_dai_slots = &merged_result.accessed_slots[&contract_c_address];
        assert_eq!(original_dai_slots.len(), 2);
        assert!(original_dai_slots.contains(
            &Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000003")
                .unwrap()
        ));
        assert!(original_dai_slots.contains(
            &Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000004")
                .unwrap()
        ));
    }
}
