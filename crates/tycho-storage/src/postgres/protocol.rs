use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{NaiveDateTime, Utc};
use diesel::{
    prelude::*,
    upsert::{excluded, on_constraint},
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use itertools::Itertools;
use tracing::{debug_span, error, instrument, trace, warn, Instrument, Level};
use tycho_common::{
    models::{
        protocol::{
            ComponentBalance, ProtocolComponent, ProtocolComponentState,
            ProtocolComponentStateDelta, QualityRange,
        },
        token::Token,
        Address, Balance, Chain, ChangeType, ComponentId, FinancialType, ImplementationType,
        PaginationParams, ProtocolType, StoreVal, TxHash,
    },
    storage::{BlockOrTimestamp, StorageError, Version, WithTotal},
    Bytes,
};

use super::{
    maybe_lookup_block_ts, maybe_lookup_version_ts, orm, schema, storage_error_from_diesel,
    truncate_to_byte_limit,
    versioning::{apply_partitioned_versioning, VersioningEntry},
    PostgresError, PostgresGateway, WithOrdinal, WithTxHash, MAX_TS, MAX_VERSION_TS,
};

// Private methods
impl PostgresGateway {
    /// # Decoding ProtocolStates from database results.
    ///
    /// This function takes as input the database result for querying protocol states and their
    /// linked component id and transaction hash.
    ///
    /// ## Assumptions:
    /// - It is assumed that the rows in the result are ordered by component ID
    ///
    /// The function processes these individual `ProtocolState` entities and combines all entities
    /// with matching component IDs into a single `ProtocolState`. The final output is a list
    /// where each element is a `ProtocolState` representing a unique component.
    ///
    /// ## Returns:
    /// - A Result containing a vector of `ProtocolState`, otherwise, it will return a StorageError.
    #[instrument(level = Level::DEBUG, skip(self, balances, states_result))]
    fn _decode_protocol_states(
        &self,
        mut balances: HashMap<ComponentId, HashMap<Address, ComponentBalance>>,
        states_result: Result<Vec<(orm::ProtocolState, ComponentId)>, diesel::result::Error>,
        context: &str,
    ) -> Result<Vec<ProtocolComponentState>, StorageError> {
        let data_vec = states_result
            .map_err(|err| storage_error_from_diesel(err, "ProtocolStates", context, None))?;

        // Decode final state deltas. We can assume result is sorted by component_id.
        // Therefore we can use slices to iterate over the data in groups of
        // component_id.
        let mut protocol_states = Vec::new();

        let mut index = 0;
        while index < data_vec.len() {
            let component_start = index;
            let current_component_id = &data_vec[index].1;

            // Iterate until the component_id changes
            while index < data_vec.len() && &data_vec[index].1 == current_component_id {
                index += 1;
            }

            let states_slice = &data_vec[component_start..index];
            let protocol_balances: HashMap<Address, Balance> = balances
                .remove(current_component_id)
                .unwrap_or_default()
                .into_iter()
                .map(|(key, balance)| (key, balance.balance))
                .collect();

            let protocol_state = ProtocolComponentState::new(
                current_component_id,
                states_slice
                    .iter()
                    .map(|x| (x.0.attribute_name.clone(), x.0.attribute_value.clone()))
                    .collect(),
                protocol_balances,
            );

            protocol_states.push(protocol_state);
        }

        // add remaining balances as states with empty attributes
        for (component_id, balances) in balances.into_iter() {
            protocol_states.push(ProtocolComponentState::new(
                component_id.as_str(),
                HashMap::new(),
                balances
                    .into_iter()
                    .map(|(key, balance)| (key, balance.balance))
                    .collect(),
            ))
        }

        Ok(protocol_states)
    }

    #[instrument(level = Level::DEBUG, skip(self, conn))]
    async fn _get_or_create_protocol_system_id(
        &self,
        new: String,
        conn: &mut AsyncPgConnection,
    ) -> Result<i64, StorageError> {
        use super::schema::protocol_system::dsl::*;

        let existing_entry = protocol_system
            .filter(name.eq(new.to_string().clone()))
            .first::<orm::ProtocolSystem>(conn)
            .await;

        if let Ok(entry) = existing_entry {
            Ok(entry.id)
        } else {
            let new_entry = orm::NewProtocolSystem { name: new.to_string() };

            let inserted_protocol_system = diesel::insert_into(protocol_system)
                .values(&new_entry)
                .get_result::<orm::ProtocolSystem>(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "ProtocolSystem", &new.to_string(), None)
                })?;
            Ok(inserted_protocol_system.id)
        }
    }

    #[instrument(level = Level::DEBUG, skip(self, ids, conn))]
    pub async fn get_protocol_components(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        min_tvl: Option<f64>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<ProtocolComponent>>, StorageError> {
        use super::schema::{protocol_component::dsl::*, transaction::dsl::*};
        let chain_id_value = self.get_chain_id(chain)?;

        let mut count_query = protocol_component
            .left_join(schema::component_tvl::table)
            .into_boxed();

        let mut query = protocol_component
            .inner_join(transaction.on(creation_tx.eq(schema::transaction::id)))
            .left_join(schema::component_tvl::table)
            .select((orm::ProtocolComponent::as_select(), hash))
            .into_boxed();

        match (system, ids) {
            (Some(ps), None) => {
                let protocol_system = self.get_protocol_system_id(&ps)?;
                query = query.filter(
                    chain_id
                        .eq(chain_id_value)
                        .and(protocol_system_id.eq(protocol_system)),
                );
                count_query = count_query.filter(
                    chain_id
                        .eq(chain_id_value)
                        .and(protocol_system_id.eq(protocol_system)),
                );
            }
            (None, Some(external_ids)) => {
                query = query.filter(
                    chain_id
                        .eq(chain_id_value)
                        .and(external_id.eq_any(external_ids)),
                );
                count_query = count_query.filter(
                    chain_id
                        .eq(chain_id_value)
                        .and(external_id.eq_any(external_ids)),
                );
            }
            (Some(ps), Some(external_ids)) => {
                let protocol_system = self.get_protocol_system_id(&ps)?;
                query = query.filter(
                    chain_id.eq(chain_id_value).and(
                        external_id
                            .eq_any(external_ids)
                            .and(protocol_system_id.eq(protocol_system)),
                    ),
                );
                count_query = count_query.filter(
                    chain_id.eq(chain_id_value).and(
                        external_id
                            .eq_any(external_ids)
                            .and(protocol_system_id.eq(protocol_system)),
                    ),
                );
            }
            (_, _) => {
                query = query.filter(chain_id.eq(chain_id_value));
                count_query = count_query.filter(chain_id.eq(chain_id_value));
            }
        }

        if let Some(thr) = min_tvl {
            query = query.filter(schema::component_tvl::tvl.gt(thr));
            count_query = count_query.filter(schema::component_tvl::tvl.gt(thr));
        }

        let count = count_query
            .count()
            .get_result::<i64>(conn)
            .await
            .map_err(PostgresError::from)?;

        // Apply optional pagination when loading protocol components to ensure consistency
        if let Some(pagination) = pagination_params {
            query = query
                .order_by(schema::protocol_component::id)
                .limit(pagination.page_size)
                .offset(pagination.offset());
        }

        let orm_protocol_components = query
            .load::<(orm::ProtocolComponent, TxHash)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .map(|(pc, txh)| (pc, Some(txh)))
            .collect();

        let res = self
            .build_protocol_components(orm_protocol_components, chain, conn)
            .await?;

        Ok(WithTotal { entity: res, total: Some(count) })
    }

    #[instrument(level = Level::DEBUG, skip(self, orm_protocol_components, conn))]
    async fn build_protocol_components(
        &self,
        orm_protocol_components: Vec<(orm::ProtocolComponent, Option<TxHash>)>,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ProtocolComponent>, StorageError> {
        let protocol_component_ids = orm_protocol_components
            .iter()
            .map(|(pc, _)| pc.id)
            .collect::<Vec<i64>>();

        let protocol_component_tokens: Vec<(i64, Address)> =
            schema::protocol_component_holds_token::table
                .inner_join(schema::token::table)
                .inner_join(
                    schema::account::table.on(schema::token::account_id.eq(schema::account::id)),
                )
                .select((
                    schema::protocol_component_holds_token::protocol_component_id,
                    schema::account::address,
                ))
                .filter(
                    schema::protocol_component_holds_token::protocol_component_id
                        .eq_any(protocol_component_ids.clone()),
                )
                .order_by((
                    schema::protocol_component_holds_token::protocol_component_id.asc(),
                    schema::protocol_component_holds_token::token_index.asc(),
                ))
                .load::<(i64, Address)>(conn)
                .await
                .map_err(PostgresError::from)?;

        let protocol_component_contracts: Vec<(i64, Address)> =
            schema::protocol_component_holds_contract::table
                .inner_join(schema::contract_code::table)
                .inner_join(
                    schema::account::table
                        .on(schema::contract_code::account_id.eq(schema::account::id)),
                )
                .select((
                    schema::protocol_component_holds_contract::protocol_component_id,
                    schema::account::address,
                ))
                .filter(
                    schema::protocol_component_holds_contract::protocol_component_id
                        .eq_any(protocol_component_ids),
                )
                .load::<(i64, Address)>(conn)
                .await
                .map_err(PostgresError::from)?;

        let protocol_type_names_by_id: HashMap<i64, String> = schema::protocol_type::table
            .select((schema::protocol_type::id, schema::protocol_type::name))
            .load::<(i64, String)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();

        fn map_addresses_to_protocol_component(
            protocol_component_to_address: Vec<(i64, Address)>,
        ) -> HashMap<i64, Vec<Address>> {
            protocol_component_to_address
                .into_iter()
                .fold(HashMap::new(), |mut acc, (key, address)| {
                    acc.entry(key)
                        .or_default()
                        .push(address);
                    acc
                })
        }
        let protocol_component_tokens =
            map_addresses_to_protocol_component(protocol_component_tokens);
        let protocol_component_contracts =
            map_addresses_to_protocol_component(protocol_component_contracts);

        orm_protocol_components
            .into_iter()
            .map(|(pc, tx_hash)| {
                let ps = self.get_protocol_system(&pc.protocol_system_id)?;
                let tokens_by_pc: Vec<Address> = protocol_component_tokens
                    .get(&pc.id)
                    // We expect all protocol components to have tokens.
                    .expect("Could not find Tokens for Protocol Component.")
                    .clone();
                let contracts_by_pc: Vec<Address> = protocol_component_contracts
                    .get(&pc.id)
                    .cloned()
                    // We expect all protocol components to have contracts.
                    .unwrap_or_default();

                let static_attributes: HashMap<String, StoreVal> = if let Some(v) = pc.attributes {
                    serde_json::from_value(v).map_err(|_| {
                        StorageError::DecodeError("Failed to decode static attributes.".to_string())
                    })?
                } else {
                    Default::default()
                };

                Ok(ProtocolComponent::new(
                    &pc.external_id,
                    &ps,
                    protocol_type_names_by_id
                        .get(&pc.protocol_type_id)
                        .ok_or(PostgresError(StorageError::NotFound(
                            "ProtocolType".into(),
                            pc.protocol_type_id.to_string(),
                        )))?,
                    *chain,
                    tokens_by_pc,
                    contracts_by_pc,
                    static_attributes,
                    ChangeType::Creation,
                    tx_hash.unwrap_or(Bytes::from(&[0; 32])),
                    pc.created_at,
                ))
            })
            .collect()
    }

    #[instrument(level = Level::DEBUG, skip(self, tokens, conn))]
    pub async fn get_token_owners(
        &self,
        chain: &Chain,
        tokens: &[Address],
        min_balance: Option<f64>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, (ComponentId, Bytes)>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let token_ids: HashMap<i64, Address> = schema::token::table
            .inner_join(schema::account::table)
            .select((schema::token::id, schema::account::address))
            .filter(schema::account::address.eq_any(tokens))
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();

        let mut res: HashMap<Address, (String, Bytes)> = HashMap::new();
        schema::protocol_component::table
            .inner_join(schema::protocol_component_holds_token::table)
            .inner_join(schema::component_balance::table)
            .select((
                schema::component_balance::token_id,
                schema::protocol_component::external_id,
                schema::component_balance::new_balance,
            ))
            .filter(schema::protocol_component::chain_id.eq(chain_id))
            .filter(schema::component_balance::balance_float.ge(min_balance.unwrap_or(0f64)))
            .filter(schema::component_balance::valid_to.eq(MAX_TS))
            .filter(schema::component_balance::token_id.eq_any(token_ids.keys()))
            .get_results::<(i64, String, Bytes)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .for_each(|(tid, cid, bal)| {
                if let Some(address) = token_ids.get(&tid) {
                    res.entry(address.clone())
                        .and_modify(|v| {
                            // Bytes uses lexicographical order which in case lengths
                            // are equal, is equivalent to big-endian numerical order.
                            if v.1.len() == bal.len() && v.1 < bal {
                                *v = (cid.clone(), bal.clone());
                            }
                        })
                        .or_insert_with(|| (cid, bal));
                }
            });

        Ok(res)
    }

    pub async fn add_protocol_components(
        &self,
        new: &[ProtocolComponent],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use super::schema::{
            account::dsl::*, protocol_component::dsl::*, protocol_component_holds_contract::dsl::*,
            protocol_component_holds_token::dsl::*, token::dsl::*,
        };
        let mut values: Vec<orm::NewProtocolComponent> = Vec::with_capacity(new.len());
        let tx_hashes: Vec<TxHash> = new
            .iter()
            .map(|pc| pc.creation_tx.clone())
            .collect();
        let tx_hash_id_mapping: HashMap<TxHash, i64> = async {
            orm::Transaction::ids_by_hash(&tx_hashes, conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_tx_ids"))
        .await?;
        let pt_id = orm::ProtocolType::id_by_name(&new[0].protocol_type_name, conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ProtocolType", &new[0].protocol_type_name, None)
            })?;
        for pc in new {
            let txh = tx_hash_id_mapping
                .get::<TxHash>(&pc.creation_tx.clone())
                .ok_or(StorageError::DecodeError("TxHash not found".to_string()))?;

            let new_pc = orm::NewProtocolComponent::new(
                &pc.id,
                self.get_chain_id(&pc.chain)?,
                pt_id,
                self.get_protocol_system_id(&pc.protocol_system.to_string())?,
                *txh,
                pc.created_at,
                &pc.static_attributes,
            );
            values.push(new_pc);
        }

        // Insert protocol components in batches to avoid exceeding PostgreSQL parameter limit
        let component_count = values.len();
        let mut inserted_protocol_components: Vec<(i64, String, i64, i64)> = Vec::new();
        async {
            for chunk in values.chunks(orm::NewProtocolComponent::MAX_BATCH_SIZE) {
                let mut batch_results = diesel::insert_into(protocol_component)
                    .values(chunk)
                    .on_conflict((schema::protocol_component::chain_id, external_id))
                    .do_nothing()
                    .returning((
                        schema::protocol_component::id,
                        schema::protocol_component::external_id,
                        schema::protocol_component::protocol_system_id,
                        schema::protocol_component::chain_id,
                    ))
                    .get_results(conn)
                    .await
                    .map_err(|err| {
                        storage_error_from_diesel(err, "ProtocolComponent", "Batch insert", None)
                    })?;
                inserted_protocol_components.append(&mut batch_results);
            }
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("insert_protocol_components", count = component_count))
        .await?;

        let mut protocol_db_id_map = HashMap::new();
        for (pc_id, ex_id, ps_id, chain_id_db) in inserted_protocol_components {
            protocol_db_id_map.insert(
                (ex_id, self.get_protocol_system(&ps_id)?, self.get_chain(&chain_id_db)?),
                pc_id,
            );
        }

        let filtered_new_protocol_components: Vec<&ProtocolComponent> = new
            .iter()
            .filter(|component| {
                let key =
                    (component.id.clone(), component.protocol_system.clone(), component.chain);

                protocol_db_id_map.contains_key(&key)
            })
            .collect();

        // establish component-token junction
        let token_addresses: HashSet<Address> = filtered_new_protocol_components
            .iter()
            .flat_map(|pc| pc.tokens.iter().cloned())
            .collect();

        let pc_tokens_map = filtered_new_protocol_components
            .iter()
            .flat_map(|pc| {
                let pc_id = protocol_db_id_map
                    .get(&(pc.id.clone(), pc.protocol_system.clone(), pc.chain))
                    .unwrap_or_else(|| {
                        //Because we just inserted the protocol systems, there should not be any missing.
                        // However, trying to handle this via Results is needlessly difficult, because you
                        // can not use flat_map on a Result.
                        panic!(
                            "Could not find Protocol Component with ID: {}, Protocol System: {}, Chain: {}",
                            pc.id, pc.protocol_system, pc.chain
                        )
                    });
                pc.tokens
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(move |(idx, add)| (*pc_id, add, idx as i16))
                    .collect::<Vec<(i64, Address, i16)>>()
            })
            .collect::<Vec<(i64, Address, i16)>>();

        let token_addr_count = token_addresses.len();
        let token_add_by_id: HashMap<Address, i64> = async {
            token
                .inner_join(account)
                .select((schema::account::address, schema::token::id))
                .filter(schema::account::address.eq_any(token_addresses))
                .into_boxed()
                .load::<(Address, i64)>(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Token", "Several Chains", None))
        }
        .instrument(debug_span!("resolve_token_ids", count = token_addr_count))
        .await?
        .into_iter()
        .collect();

        let protocol_component_token_junction: Result<
            Vec<orm::NewProtocolComponentHoldsToken>,
            StorageError,
        > = pc_tokens_map
            .iter()
            .map(|(pc_id, t_address, token_idx)| {
                let t_id = token_add_by_id
                    .get(t_address)
                    .ok_or(StorageError::NotFound("Token".to_string(), t_address.to_string()))?;
                Ok(orm::NewProtocolComponentHoldsToken {
                    protocol_component_id: *pc_id,
                    token_id: *t_id,
                    token_index: *token_idx,
                })
            })
            .collect();

        async {
            diesel::insert_into(protocol_component_holds_token)
                .values(&protocol_component_token_junction?)
                .execute(conn)
                .await
                .map_err(PostgresError::from)?;
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("insert_component_token_junction"))
        .await?;

        // establish component-contract junction
        let contract_addresses: HashSet<Address> = new
            .iter()
            .flat_map(|pc| pc.contract_addresses.clone())
            .collect();

        let pc_contract_map = new
            .iter()
            .flat_map(|pc| {
                let pc_id = protocol_db_id_map
                    .get(&(pc.id.clone(), pc.protocol_system.clone(), pc.chain))
                    .unwrap_or_else(|| {
                        panic!(
                            "Could not find Protocol Component with ID: {}, Protocol System: {}, Chain: {}",
                            pc.id, pc.protocol_system, pc.chain
                        )
                    }); //Because we just inserted the protocol systems, there should not be any missing.
                // However, trying to handle this via Results is needlessly difficult, because you
                // can not use flat_map on a Result.

                pc.contract_addresses
                    .iter()
                    .cloned()
                    .map(move |add| (*pc_id, add))
                    .collect::<Vec<(i64, Address)>>()
            })
            .collect::<Vec<(i64, Address)>>();

        let contract_add_by_id: HashMap<Address, i64> = async {
            schema::contract_code::table
                .inner_join(account)
                .select((schema::account::address, schema::contract_code::id))
                .filter(schema::account::address.eq_any(contract_addresses))
                .into_boxed()
                .load::<(Address, i64)>(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Contract", "Several Chains", None))
        }
        .instrument(debug_span!("resolve_contract_ids"))
        .await?
        .into_iter()
        .collect();

        let protocol_component_contract_junction: Result<
            Vec<orm::NewProtocolComponentHoldsContract>,
            StorageError,
        > = pc_contract_map
            .iter()
            .map(|(pc_id, t_address)| {
                let t_id = contract_add_by_id
                    .get(t_address)
                    .ok_or(StorageError::NotFound("Account".to_string(), t_address.to_string()))?;
                Ok(orm::NewProtocolComponentHoldsContract {
                    protocol_component_id: *pc_id,
                    contract_code_id: *t_id,
                })
            })
            .collect();

        async {
            diesel::insert_into(protocol_component_holds_contract)
                .values(&protocol_component_contract_junction?)
                .execute(conn)
                .await
                .map_err(PostgresError::from)?;
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("insert_component_contract_junction"))
        .await?;

        Ok(())
    }

    pub async fn delete_protocol_components(
        &self,
        to_delete: &[ProtocolComponent],
        block_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use super::schema::protocol_component::dsl::*;

        let ids_to_delete: Vec<String> = to_delete
            .iter()
            .map(|c| c.id.to_string())
            .collect();

        diesel::update(protocol_component.filter(external_id.eq_any(ids_to_delete)))
            .set(deleted_at.eq(block_ts))
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }

    pub async fn add_protocol_types(
        &self,
        new_protocol_types: &[ProtocolType],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use super::schema::protocol_type::dsl::*;
        let values: Vec<orm::NewProtocolType> = new_protocol_types
            .iter()
            .map(|new_protocol_type| {
                let financial_protocol_type: orm::FinancialType =
                    match new_protocol_type.financial_type {
                        FinancialType::Swap => orm::FinancialType::Swap,
                        FinancialType::Psm => orm::FinancialType::Psm,
                        FinancialType::Debt => orm::FinancialType::Debt,
                        FinancialType::Leverage => orm::FinancialType::Leverage,
                    };

                let protocol_implementation_type: orm::ImplementationType =
                    match new_protocol_type.implementation {
                        ImplementationType::Custom => orm::ImplementationType::Custom,
                        ImplementationType::Vm => orm::ImplementationType::Vm,
                    };

                orm::NewProtocolType {
                    name: new_protocol_type.name.clone(),
                    implementation: protocol_implementation_type,
                    attribute_schema: new_protocol_type
                        .attribute_schema
                        .clone(),
                    financial_type: financial_protocol_type,
                }
            })
            .collect();

        diesel::insert_into(protocol_type)
            .values(&values)
            .on_conflict(name)
            .do_nothing()
            .execute(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "ProtocolType", "Batch insert", None))?;

        Ok(())
    }

    // Gets all protocol states from the db filtered by chain, component ids and/or protocol system.
    #[allow(clippy::too_many_arguments)]
    #[instrument(level = Level::DEBUG, skip(self, ids, conn))]
    pub async fn get_protocol_states(
        &self,
        chain: &Chain,
        at: Option<Version>,
        // TODO: change to &str
        system: Option<String>,
        ids: Option<&[&str]>,
        retrieve_balances: bool,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError> {
        let chain_db_id = self.get_chain_id(chain)?;
        let version_ts = match &at {
            Some(version) => Some(maybe_lookup_version_ts(version, conn).await?),
            None => None,
        };

        let balances = if retrieve_balances {
            self.get_component_balances(chain, ids, at.as_ref(), conn)
                .await?
        } else {
            HashMap::new()
        };

        match (ids, system) {
            (maybe_ids, Some(system)) => {
                let state_data = orm::ProtocolState::by_protocol(
                    maybe_ids,
                    &system.to_string(),
                    &chain_db_id,
                    version_ts,
                    pagination_params,
                    conn,
                )
                .await;
                let protocol_states = self._decode_protocol_states(
                    balances,
                    state_data.entity,
                    system.to_string().as_str(),
                )?;
                Ok(WithTotal { entity: protocol_states, total: state_data.total })
            }
            (Some(ids), _) => {
                let state_data = orm::ProtocolState::by_id(
                    ids,
                    &chain_db_id,
                    version_ts,
                    pagination_params,
                    conn,
                )
                .await;
                let protocol_states = self._decode_protocol_states(
                    balances,
                    state_data.entity,
                    ids.join(",").as_str(),
                )?;
                Ok(WithTotal { entity: protocol_states, total: state_data.total })
            }
            _ => {
                let state_data =
                    orm::ProtocolState::by_chain(&chain_db_id, version_ts, pagination_params, conn)
                        .await;
                let protocol_states = self._decode_protocol_states(
                    balances,
                    state_data.entity,
                    chain.to_string().as_str(),
                )?;
                Ok(WithTotal { entity: protocol_states, total: state_data.total })
            }
        }
    }

    pub async fn update_protocol_states(
        &self,
        chain: &Chain,
        new: &[(TxHash, &ProtocolComponentStateDelta)],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let chain_db_id = self.get_chain_id(chain)?;
        let new = new
            .iter()
            .map(|(tx, delta)| WithTxHash { entity: delta, tx: Some(tx.to_owned()) })
            .collect::<Vec<_>>();

        let txns: HashMap<TxHash, (i64, i64, NaiveDateTime)> = async {
            orm::Transaction::ids_and_ts_by_hash(new.iter().filter_map(|u| u.tx.as_ref()), conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_tx_ids"))
        .await?
        .into_iter()
        .map(|(id, hash, index, ts)| (hash, (id, index, ts)))
        .collect();

        let components: HashMap<String, i64> = async {
            orm::ProtocolComponent::ids_by_external_ids(
                new.iter()
                    .map(|state| state.component_id.as_str()),
                chain_db_id,
                conn,
            )
            .await
            .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_component_ids"))
        .await?
        .into_iter()
        .map(|(id, external_id)| (external_id, id))
        .collect();

        let mut state_data = Vec::new();
        for state in new {
            let tx = state
                .tx
                .as_ref()
                .ok_or(StorageError::Unexpected(
                    "Could not reference tx in ProtocolStateDelta object".to_string(),
                ))?;
            let (tx_id, tx_index, tx_ts) = txns
                .get(tx)
                .ok_or(StorageError::NotFound("Tx id".to_string(), tx.to_string()))?;

            let component_db_id = *components
                .get(&state.component_id)
                .ok_or(StorageError::NotFound(
                    "Component id".to_string(),
                    state.component_id.to_string(),
                ))?;

            state_data.extend(
                state
                    .updated_attributes
                    .iter()
                    .map(|(attribute, value)| {
                        WithOrdinal::new(
                            VersioningEntry::Update(orm::NewProtocolState::new(
                                component_db_id,
                                attribute,
                                value,
                                *tx_id,
                                *tx_ts,
                            )),
                            (component_db_id, attribute, tx_ts, tx_index),
                        )
                    }),
            );

            state_data.extend(
                state
                    .deleted_attributes
                    .iter()
                    .map(|attr| {
                        WithOrdinal::new(
                            VersioningEntry::Deletion(((component_db_id, attr.clone()), *tx_ts)),
                            (component_db_id, attr, tx_ts, tx_index),
                        )
                    }),
            );
        }

        // insert the prepared protocol state deltas
        if !state_data.is_empty() {
            state_data.sort_by_cached_key(|b| b.ordinal);
            let sorted = state_data
                .into_iter()
                .map(|b| b.entity)
                .collect::<Vec<_>>();
            trace!(entries=?&sorted, "protocol state entries ready for versioning.");
            let state_count = sorted.len();
            let (latest, to_archive, to_delete) =
                async { apply_partitioned_versioning(&sorted, self.retention_horizon, conn).await }
                    .instrument(debug_span!("apply_versioning", count = state_count))
                    .await?;

            async {
                // Insert to_archive in batches if needed
                if !to_archive.is_empty() {
                    trace!(records=?&to_archive, "Inserting archival records!");
                    for chunk in to_archive.chunks(orm::NewProtocolState::MAX_BATCH_SIZE) {
                        diesel::insert_into(schema::protocol_state::table)
                            .values(chunk)
                            .execute(conn)
                            .await
                            .map_err(PostgresError::from)?;
                    }
                }

                // Insert latest in batches if needed
                let latest: Vec<orm::NewProtocolStateLatest> = latest
                    .into_iter()
                    .map(Into::into)
                    .collect();

                if !latest.is_empty() {
                    trace!(new_state=?&latest, "Updating active state!");
                    for chunk in latest.chunks(orm::NewProtocolStateLatest::MAX_BATCH_SIZE) {
                        diesel::insert_into(schema::protocol_state_default::table)
                            .values(chunk)
                            .on_conflict(on_constraint("protocol_state_default_unique_pk"))
                            .do_update()
                            .set((
                                schema::protocol_state_default::attribute_value
                                    .eq(excluded(schema::protocol_state_default::attribute_value)),
                                schema::protocol_state_default::previous_value
                                    .eq(excluded(schema::protocol_state_default::previous_value)),
                                schema::protocol_state_default::modify_tx
                                    .eq(excluded(schema::protocol_state_default::modify_tx)),
                                schema::protocol_state_default::valid_from
                                    .eq(excluded(schema::protocol_state_default::valid_from)),
                            ))
                            .execute(conn)
                            .await
                            .map_err(PostgresError::from)?;
                    }
                }

                // remove deleted attributes from the default table
                if !to_delete.is_empty() {
                    let mut delete_query =
                        diesel::delete(schema::protocol_state_default::table).into_boxed();
                    for (component_id, attr_name) in to_delete {
                        delete_query = delete_query.or_filter(
                            schema::protocol_state_default::protocol_component_id
                                .eq(component_id)
                                .and(schema::protocol_state_default::attribute_name.eq(attr_name)),
                        );
                    }
                    delete_query
                        .execute(conn)
                        .await
                        .map_err(PostgresError::from)?;
                }
                Ok::<(), StorageError>(())
            }
            .instrument(debug_span!("insert_protocol_states", archive_count = to_archive.len()))
            .await?;
        }
        Ok(())
    }

    #[instrument(level = Level::DEBUG, skip(self, addresses, conn))]
    pub async fn get_tokens(
        &self,
        chain: Chain,
        addresses: Option<&[&Address]>,
        quality_filter: QualityRange,
        last_traded_ts_threshold: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        use super::schema::{account::dsl::*, token::dsl::*};
        let chain_db_id = self.get_chain_id(&chain)?;

        let mut count_query = token
            .inner_join(account)
            .select(token::all_columns())
            .filter(schema::account::chain_id.eq(chain_db_id))
            .into_boxed();

        let mut query = token
            .inner_join(account)
            .select((token::all_columns(), schema::account::address))
            .filter(schema::account::chain_id.eq(chain_db_id))
            .into_boxed();

        if let Some(addrs) = addresses {
            query = query.filter(schema::account::address.eq_any(addrs));
            count_query = count_query.filter(schema::account::address.eq_any(addrs));
        }

        if let Some(min_quality) = quality_filter.min {
            query = query.filter(schema::token::quality.ge(min_quality));
            count_query = count_query.filter(schema::token::quality.ge(min_quality));
        }
        if let Some(max_quality) = quality_filter.max {
            query = query.filter(schema::token::quality.le(max_quality));
            count_query = count_query.filter(schema::token::quality.le(max_quality));
        }

        if let Some(last_traded_ts_threshold) = last_traded_ts_threshold {
            let active_tokens_exists = diesel::dsl::exists(
                schema::component_balance_default::table
                    .filter(
                        schema::component_balance_default::valid_from.gt(last_traded_ts_threshold),
                    )
                    .filter(schema::component_balance_default::token_id.eq(schema::token::id)),
            );

            query = query.filter(active_tokens_exists);
            count_query = count_query.filter(active_tokens_exists);
        }

        // TODO: Improve performance by running as subquery
        let count = count_query
            .count()
            .get_result::<i64>(conn)
            .await
            .map_err(PostgresError::from)?;

        if let Some(pagination) = pagination_params {
            query = query
                .limit(pagination.page_size)
                .offset(pagination.offset());
        }

        let results = query
            .order(schema::token::id.asc())
            .load::<(orm::Token, Address)>(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Token", &chain.to_string(), None))?;

        let tokens: Vec<Token> = results
            .into_iter()
            .map(|(orm_token, address_)| {
                let gas_usage: Vec<_> = orm_token
                    .gas
                    .iter()
                    .map(|u| u.map(|g| g as u64))
                    .collect();
                Token::new(
                    &address_,
                    orm_token.symbol.as_str(),
                    orm_token.decimals as u32,
                    orm_token.tax as u64,
                    gas_usage.as_slice(),
                    chain,
                    orm_token.quality as u32,
                )
            })
            .collect();

        Ok(WithTotal { entity: tokens, total: Some(count) })
    }

    pub async fn add_tokens(
        &self,
        tokens: &[Token],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let titles: Vec<String> = tokens
            .iter()
            .map(|token| {
                let formatted = format!("{:?}_{}", token.chain, token.symbol);
                truncate_to_byte_limit(&formatted, 255)
            })
            .collect();

        let addresses: Vec<_> = tokens
            .iter()
            .map(|token| token.address.clone())
            .collect();

        let new_accounts: Vec<orm::NewAccount> = tokens
            .iter()
            .zip(titles.iter())
            .zip(addresses.iter())
            .map(|((token, title), address)| {
                let chain_id = self.get_chain_id(&token.chain)?;
                Ok(orm::NewAccount {
                    title,
                    address,
                    chain_id,
                    creation_tx: None,
                    created_at: None,
                    deleted_at: None,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;

        // Insert accounts in batches to avoid exceeding PostgreSQL parameter limit
        let account_count = new_accounts.len();
        async {
            for chunk in new_accounts.chunks(orm::NewAccount::MAX_BATCH_SIZE) {
                diesel::insert_into(schema::account::table)
                    .values(chunk)
                    .on_conflict((schema::account::address, schema::account::chain_id))
                    .do_nothing()
                    .execute(conn)
                    .await
                    .map_err(|err| storage_error_from_diesel(err, "Account", "batch", None))?;
            }
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("insert_accounts", count = account_count))
        .await?;

        let accounts: Vec<orm::Account> = async {
            schema::account::table
                .filter(schema::account::address.eq_any(addresses))
                .select(orm::Account::as_select())
                .get_results::<orm::Account>(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Account", "retrieve", None))
        }
        .instrument(debug_span!("resolve_account_ids", count = account_count))
        .await?;

        let account_map: HashMap<(Vec<u8>, i64), i64> = accounts
            .iter()
            .map(|account| ((account.address.clone().to_vec(), account.chain_id), account.id))
            .collect();

        let new_tokens: Vec<orm::NewToken> = tokens
            .iter()
            .map(|token| {
                let token_chain_id = self.get_chain_id(&token.chain)?;
                let account_key = (token.address.to_vec(), token_chain_id);

                let account_id = *account_map
                    .get(&account_key)
                    .expect("Account ID not found");

                let mut new_token = orm::NewToken::from_token(account_id, token);
                new_token.symbol = truncate_to_byte_limit(&token.symbol, 255);
                Ok(new_token)
            })
            .collect::<Result<Vec<_>, StorageError>>()?;

        // Insert tokens in batches to avoid exceeding PostgreSQL parameter limit
        let token_count = new_tokens.len();
        async {
            for chunk in new_tokens.chunks(orm::NewToken::MAX_BATCH_SIZE) {
                diesel::insert_into(schema::token::table)
                    .values(chunk)
                    // .on_conflict(..).do_nothing() is necessary to ignore updating duplicated
                    // entries
                    .on_conflict(schema::token::account_id)
                    .do_nothing()
                    .execute(conn)
                    .await
                    .map_err(|err| storage_error_from_diesel(err, "Token", "batch", None))?;
            }
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("insert_tokens", count = token_count))
        .await?;

        Ok(())
    }

    pub async fn update_tokens(
        &self,
        tokens: &[Token],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        trace!(addresses=?tokens.iter().map(|t| &t.address).collect::<Vec<_>>(), "Updating tokens");
        let address_to_db_id = {
            let token_addresses: HashSet<Address> = tokens
                .iter()
                .map(|t| t.address.clone())
                .collect();
            async {
                schema::account::table
                    .inner_join(schema::token::table)
                    .select((schema::account::address, schema::token::id))
                    .filter(schema::account::address.eq_any(token_addresses))
                    .get_results(conn)
                    .await
                    .map_err(PostgresError::from)
            }
            .instrument(debug_span!("resolve_token_ids"))
            .await?
            .into_iter()
            .collect::<HashMap<Bytes, i64>>()
        };
        use schema::token::dsl::*;
        async {
            for t in tokens.iter() {
                if let Some(db_id) = address_to_db_id.get(&t.address) {
                    let gas_val = t
                        .gas
                        .iter()
                        .map(|v| v.map(|g| g as i64))
                        .collect::<Vec<_>>();
                    diesel::update(schema::token::table)
                        .set((
                            symbol.eq(&t.symbol),
                            decimals.eq(t.decimals as i32),
                            tax.eq(t.tax as i64),
                            quality.eq(t.quality as i32),
                            gas.eq(gas_val),
                        ))
                        .filter(id.eq(db_id))
                        .execute(conn)
                        .await
                        .map_err(PostgresError::from)?;
                } else {
                    warn!(address=?&t.address, "Tried to update non existing token! Consider inserting it first!");
                }
            }
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("update_token_rows"))
        .await?;
        Ok(())
    }

    pub async fn add_component_balances(
        &self,
        component_balances: &[ComponentBalance],
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use super::schema::{account::dsl::*, token::dsl::*};

        let chain_db_id = self.get_chain_id(chain)?;
        let token_addresses: HashSet<Address> = component_balances
            .iter()
            .map(|component_balance| component_balance.token.clone())
            .collect();
        let token_count = token_addresses.len();
        let token_ids: HashMap<Address, i64> = async {
            token
                .inner_join(account)
                .select((schema::account::address, schema::token::id))
                .filter(schema::account::address.eq_any(&token_addresses))
                .load::<(Address, i64)>(conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_token_ids", count = token_count))
        .await?
        .into_iter()
        .collect();

        let modify_txs: HashSet<TxHash> = component_balances
            .iter()
            .map(|component_balance| component_balance.modify_tx.clone())
            .collect();

        let tx_count = modify_txs.len();
        let transaction_ids_and_ts: HashMap<TxHash, (i64, i64, NaiveDateTime)> = async {
            orm::Transaction::ids_and_ts_by_hash(modify_txs.iter(), conn)
                .await
                .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_tx_ids", count = tx_count))
        .await?
        .into_iter()
        .map(|(db_id, hash, index, ts)| (hash, (db_id, index, ts)))
        .collect();

        let protocol_component_ids: HashMap<String, i64> = async {
            orm::ProtocolComponent::ids_by_external_ids(
                component_balances
                    .iter()
                    .map(|component_balance| component_balance.component_id.as_str()),
                chain_db_id,
                conn,
            )
            .await
            .map_err(PostgresError::from)
        }
        .instrument(debug_span!("resolve_component_ids"))
        .await?
        .into_iter()
        .map(|(component_id, external_id)| (external_id, component_id))
        .collect();

        let mut new_component_balances = Vec::new();
        for component_balance in component_balances.iter() {
            let token_id = token_ids
                .get(&component_balance.token)
                .ok_or_else(|| {
                    error!(?chain, ?component_balance.token, ?component_balance, "Token not found");
                    StorageError::NotFound("Token".to_string(), component_balance.token.to_string())
                })?;
            let (transaction_id, transaction_index, transaction_ts) = transaction_ids_and_ts
                .get(&component_balance.modify_tx)
                .ok_or_else(|| {
                    error!(?chain, ?component_balance.modify_tx, ?component_balance, "Transaction not found");
                    StorageError::NotFound("Transaction".to_string(), component_balance.modify_tx.to_string())
                })?;

            let protocol_component_id = protocol_component_ids
                .get(&component_balance.component_id)
                .ok_or_else(|| {
                    error!(?chain, ?component_balance.component_id, ?component_balance, "Protocol component not found");
                    StorageError::NotFound("ProtocolComponent".to_string(), component_balance.component_id.to_string())
                })?;

            let new_component_balance = orm::NewComponentBalance::new(
                *token_id,
                component_balance.balance.clone(),
                component_balance.balance_float,
                None,
                *transaction_id,
                *protocol_component_id,
                *transaction_ts,
            );
            new_component_balances.push(WithOrdinal::new(
                VersioningEntry::Update(new_component_balance),
                (protocol_component_id, *token_id, transaction_ts, transaction_index),
            ));
        }

        if !component_balances.is_empty() {
            new_component_balances.sort_by_cached_key(|b| b.ordinal);
            let sorted = new_component_balances
                .into_iter()
                .map(|b| b.entity)
                .collect::<Vec<_>>();

            let balance_count = sorted.len();
            let (latest, to_archive, _) =
                async { apply_partitioned_versioning(&sorted, self.retention_horizon, conn).await }
                    .instrument(debug_span!("apply_versioning", count = balance_count))
                    .await?;

            // Insert to_archive in batches if needed
            async {
                if !to_archive.is_empty() {
                    for chunk in to_archive.chunks(orm::NewComponentBalance::MAX_BATCH_SIZE) {
                        diesel::insert_into(schema::component_balance::table)
                            .values(chunk)
                            .execute(conn)
                            .await
                            .map_err(|err| {
                                storage_error_from_diesel(err, "ComponentBalance", "batch", None)
                            })?;
                    }
                }

                // Insert latest in batches if needed
                let latest = latest
                    .into_iter()
                    .map(orm::NewComponentBalanceLatest::from)
                    .collect::<Vec<_>>();

                if !latest.is_empty() {
                    for chunk in latest.chunks(orm::NewComponentBalanceLatest::MAX_BATCH_SIZE) {
                        diesel::insert_into(schema::component_balance_default::table)
                            .values(chunk)
                            .on_conflict(on_constraint("component_balance_default_unique_pk"))
                            .do_update()
                            .set((
                                schema::component_balance_default::new_balance
                                    .eq(excluded(schema::component_balance_default::new_balance)),
                                schema::component_balance_default::balance_float
                                    .eq(excluded(schema::component_balance_default::balance_float)),
                                schema::component_balance_default::previous_value.eq(excluded(
                                    schema::component_balance_default::previous_value,
                                )),
                                schema::component_balance_default::modify_tx
                                    .eq(excluded(schema::component_balance_default::modify_tx)),
                                schema::component_balance_default::valid_from
                                    .eq(excluded(schema::component_balance_default::valid_from)),
                            ))
                            .execute(conn)
                            .await
                            .map_err(|err| {
                                storage_error_from_diesel(err, "ComponentBalance", "batch", None)
                            })?;
                    }
                }
                Ok::<(), StorageError>(())
            }
            .instrument(debug_span!("insert_component_balances", archive_count = to_archive.len()))
            .await?;
        }
        Ok(())
    }

    #[instrument(skip(self, conn))]
    pub async fn get_balance_deltas(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        target_version: &BlockOrTimestamp,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentBalance>, StorageError> {
        use schema::component_balance::dsl::*;
        let chain_id = self.get_chain_id(chain)?;

        let start_ts = match start_version {
            Some(version) => maybe_lookup_block_ts(version, conn).await?,
            None => Utc::now().naive_utc(),
        };
        let target_ts = maybe_lookup_block_ts(target_version, conn).await?;

        let res = if start_ts <= target_ts {
            // Going forward
            //                  ]     changes to update   ]
            // -----------------|--------------------------|
            //                start                     target
            // We query for balance updates between start and target version.
            component_balance
                .inner_join(schema::protocol_component::table)
                .inner_join(schema::transaction::table)
                .inner_join(schema::token::table.inner_join(schema::account::table))
                .filter(
                    schema::protocol_component::chain_id
                        .eq(chain_id)
                        .and(valid_from.gt(start_ts))
                        .and(valid_from.le(target_ts))
                        .and(
                            valid_to
                                .gt(target_ts)
                                .or(valid_to.is_null()),
                        ),
                )
                .order_by((
                    protocol_component_id,
                    token_id,
                    valid_from.desc(),
                    schema::transaction::index.desc(),
                ))
                .distinct_on((protocol_component_id, token_id))
                .select((
                    schema::protocol_component::external_id,
                    schema::account::address,
                    new_balance,
                    balance_float,
                    schema::transaction::hash,
                ))
                .get_results::<(String, Address, Balance, f64, TxHash)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .map(|(component_id, address, balance, bal_f64, tx)| {
                    ComponentBalance::new(address, balance, bal_f64, tx, component_id.as_str())
                })
                .collect()
        } else {
            // Going backwards
            //                  ]     changes to revert    ]
            // -----------------|--------------------------|
            //                target                     start
            // We query for the previous values of all (protocol_component, token) pairs updated
            // between start and target version.
            component_balance
                .inner_join(schema::protocol_component::table)
                .inner_join(schema::transaction::table)
                .inner_join(schema::token::table.inner_join(schema::account::table))
                .filter(
                    schema::protocol_component::chain_id
                        .eq(chain_id)
                        .and(valid_from.ge(target_ts))
                        .and(valid_from.lt(start_ts))
                        .and(
                            valid_to
                                .gt(target_ts)
                                .or(valid_to.is_null()),
                        ),
                )
                .order_by((
                    protocol_component_id,
                    token_id,
                    valid_from.asc(),
                    schema::transaction::index.asc(),
                ))
                .distinct_on((protocol_component_id, token_id))
                .select((
                    schema::protocol_component::external_id,
                    schema::account::address,
                    previous_value,
                    schema::transaction::hash,
                ))
                .get_results::<(String, Address, Balance, TxHash)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .map(|(component_id, address, balance, tx)| {
                    ComponentBalance::new(address, balance, f64::NAN, tx, component_id.as_str())
                })
                .collect()
        };
        Ok(res)
    }

    #[instrument(level = Level::DEBUG, skip(self, ids, conn))]
    pub async fn get_component_balances(
        &self,
        chain: &Chain,
        ids: Option<&[&str]>,
        at: Option<&Version>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<ComponentId, HashMap<Address, ComponentBalance>>, StorageError> {
        // NOTE: the returned ComponentBalances have a default value for tx_hash as it is assumed
        // the caller does not need them. It is planned for `modify_tx` to be removed from
        // the ComponentBalance

        let version_ts = match &at {
            Some(version) => Some(maybe_lookup_version_ts(version, conn).await?),
            None => None,
        };
        let chain_id = self.get_chain_id(chain)?;

        // NOTE: the balances query was split into 3 separate queries to avoid excessive table joins
        // and improve performance. The queries are as follows:

        // Query 1: component db ids
        let mut component_query = schema::protocol_component::table
            .filter(schema::protocol_component::chain_id.eq(chain_id))
            .select((schema::protocol_component::id, schema::protocol_component::external_id))
            .into_boxed();
        if let Some(external_ids) = ids {
            component_query =
                component_query.filter(schema::protocol_component::external_id.eq_any(external_ids))
        }
        let protocol_components: HashMap<i64, String> = component_query
            .get_results::<(i64, String)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();

        // Query 2: balances
        let mut balance_query = schema::component_balance::table
            .filter(
                schema::component_balance::protocol_component_id.eq_any(protocol_components.keys()),
            )
            .filter(schema::component_balance::valid_to.gt(version_ts.unwrap_or(*MAX_VERSION_TS)))
            .into_boxed();
        // if a version timestamp is provided, we want to filter by valid_from <= version_ts
        if let Some(ts) = version_ts {
            balance_query = balance_query.filter(schema::component_balance::valid_from.le(ts));
        }
        let balances_map: HashMap<String, HashMap<i64, (Balance, f64)>> = balance_query
            .select((
                schema::component_balance::protocol_component_id,
                schema::component_balance::token_id,
                schema::component_balance::new_balance,
                schema::component_balance::balance_float,
            ))
            .order(schema::component_balance::protocol_component_id.asc())
            .get_results::<(i64, i64, Balance, f64)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .group_by(|e| e.0)
            .into_iter()
            .map(|(cid, group)| {
                (
                    protocol_components
                        .get(&cid)
                        .expect("Component ID not found")
                        .clone(),
                    group
                        .map(|(_, tid, bal, balf)| (tid, (bal, balf)))
                        .collect::<HashMap<i64, (Balance, f64)>>(),
                )
            })
            .collect();

        // Query 3: token addresses
        let tokens: HashMap<i64, Address> = schema::token::table
            .inner_join(schema::account::table)
            .filter(
                schema::token::id.eq_any(
                    balances_map
                        .values()
                        .flat_map(|b| b.keys())
                        .collect::<Vec<_>>(),
                ),
            )
            .select((schema::token::id, schema::account::address))
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();

        // Replace token ids with addresses
        let mut balances = HashMap::with_capacity(balances_map.len());

        for (component_id, balance_map) in balances_map {
            let mut new_balance_map = HashMap::new();

            for (tid, bals) in balance_map {
                match tokens.get(&tid) {
                    Some(address) => {
                        let balance = ComponentBalance::new(
                            address.clone(),
                            bals.0,
                            bals.1,
                            TxHash::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
                            component_id.as_str(),
                        );
                        new_balance_map.insert(address.clone(), balance);
                    }
                    None => {
                        Err(StorageError::NotFound("Token".to_string(), tid.to_string()))?;
                    }
                }
            }

            balances.insert(component_id.clone(), new_balance_map);
        }

        Ok(balances)
    }

    #[instrument(level = Level::DEBUG, skip(self, conn))]
    pub async fn get_protocol_states_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ProtocolComponentStateDelta>, StorageError> {
        let start_ts = match start_version {
            Some(version) => maybe_lookup_block_ts(version, conn).await?,
            None => Utc::now().naive_utc(),
        };
        let end_ts = maybe_lookup_block_ts(end_version, conn).await?;

        if start_ts <= end_ts {
            // Going forward
            //                  ]     changes to update   ]
            // -----------------|--------------------------|
            //                start                     target
            // We query for state updates between start and target version. We also query for
            // deleted states between start and target version. We then merge the two
            // sets of results.

            let chain_db_id = self.get_chain_id(chain)?;

            // fetch updated component attributes
            let state_updates =
                orm::ProtocolState::forward_deltas_by_chain(chain_db_id, start_ts, end_ts, conn)
                    .await
                    .map_err(|err| {
                        storage_error_from_diesel(
                            err,
                            "ProtocolStates",
                            chain.to_string().as_str(),
                            None,
                        )
                    })?;

            // fetch deleted component attributes
            let deleted_attrs = orm::ProtocolState::deleted_attributes_by_chain(
                chain_db_id,
                start_ts,
                end_ts,
                conn,
            )
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ProtocolStates", chain.to_string().as_str(), None)
            })?;

            // Decode final state deltas. We can assume both the deleted_attrs and state_updates
            // are sorted by component_id. Therefore we can use slices to iterate over the data
            // in groups of component_id. To do this we first need to collect an ordered set of
            // the component ids, then we can loop through deleted_attrs and state_updates in
            // parallel, creating a slice for each component_id. It is assumed only 1 delta per
            // component attribute is in these slices.

            // Get sets of component_ids from state_updates and deleted_attrs
            let state_updates_ids: BTreeSet<_> = state_updates
                .iter()
                .map(|item| &item.1)
                .collect();
            let deleted_attrs_ids: BTreeSet<_> = deleted_attrs
                .iter()
                .map(|item| &item.0)
                .collect();
            // Union of two sets gives us a sorted set of all unique component_ids
            let mut all_component_ids = state_updates_ids.clone();
            all_component_ids.append(&mut deleted_attrs_ids.clone());

            let mut protocol_states_delta = Vec::new();

            // index trackers to iterate over the state updates and deleted attributes in parallel
            let (mut updates_index, mut deletes_index) = (0, 0);

            for current_component_id in all_component_ids {
                let component_start = updates_index;

                // Iterate over states until the component_id no longer matches the current
                // component id
                while updates_index < state_updates.len() &&
                    &state_updates[updates_index].1 == current_component_id
                {
                    updates_index += 1;
                }

                let deleted_start = deletes_index;
                // Iterate over deleted attributes until the component_id no longer matches the
                // current component id
                while deletes_index < deleted_attrs.len() &&
                    &deleted_attrs[deletes_index].0 == current_component_id
                {
                    deletes_index += 1;
                }

                let states_slice = &state_updates[component_start..updates_index];
                let deleted_slice = &deleted_attrs[deleted_start..deletes_index];

                let state_delta = ProtocolComponentStateDelta::new(
                    current_component_id,
                    states_slice
                        .iter()
                        .map(|x| (x.0.attribute_name.clone(), x.0.attribute_value.clone()))
                        .collect(),
                    deleted_slice
                        .iter()
                        .map(|x| x.1.clone())
                        .collect(),
                );

                protocol_states_delta.push(state_delta);
            }
            Ok(protocol_states_delta)
        } else {
            // Going backwards
            //                  ]     changes to revert    ]
            // -----------------|--------------------------|
            //                target                     start
            // We query for the previous values of all component attributes updated between
            // start and target version.

            let chain_db_id = self.get_chain_id(chain)?;

            // fetch reverse attribute changes
            let result =
                orm::ProtocolState::reverse_delta_by_chain(chain_db_id, start_ts, end_ts, conn)
                    .await
                    .map_err(|err| {
                        storage_error_from_diesel(
                            err,
                            "ProtocolStates",
                            chain.to_string().as_str(),
                            None,
                        )
                    })?;

            // Decode final state deltas. We can assume result is sorted by component_id. Therefore
            // we can use slices to iterate over the data in groups of component_id.

            let mut deltas = Vec::new();

            let mut index = 0;
            while index < result.len() {
                let component_start = index;
                let current_component_id = &result[index].0;

                // Iterate until the component_id changes
                while index < result.len() && &result[index].0 == current_component_id {
                    index += 1;
                }

                let states_slice = &result[component_start..index];

                // sort through state updates and deletions
                let mut updates = HashMap::new();
                let mut deleted = HashSet::new();
                for (_, attribute, prev_value) in states_slice {
                    if let Some(value) = prev_value {
                        // if prev_value is not null, then the attribute was updated and
                        // must be reverted via a reversed update
                        updates.insert(attribute.clone(), value.clone());
                    } else {
                        // if prev_value is null, then the attribute was created and must be
                        // deleted on revert
                        deleted.insert(attribute.clone());
                    }
                }
                let state_delta =
                    ProtocolComponentStateDelta::new(current_component_id, updates, deleted);

                deltas.push(state_delta);
            }

            Ok(deltas)
        }
    }

    #[instrument(level = Level::DEBUG, skip(self, conn))]
    pub async fn get_token_prices(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, f64>, StorageError> {
        use schema::token_price::dsl::*;
        let chain_id = self.get_chain_id(chain)?;
        Ok(token_price
            .inner_join(schema::token::table.inner_join(schema::account::table))
            .select((schema::account::address, price))
            .filter(schema::account::chain_id.eq(chain_id))
            .get_results::<(Address, f64)>(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "TokenPrice", &chain.to_string(), None))?
            .into_iter()
            .collect::<HashMap<_, _>>())
    }

    pub async fn upsert_component_tvl(
        &self,
        chain: &Chain,
        tvl_values: &HashMap<String, f64>,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let external_db_id_map = orm::ProtocolComponent::ids_by_external_ids(
            tvl_values.keys().map(|s| s.as_str()),
            chain_id,
            conn,
        )
        .await
        .map_err(PostgresError::from)?
        .into_iter()
        .map(|(a, b)| (b, a))
        .collect::<HashMap<_, _>>();

        let upsert_map: HashMap<_, _> = tvl_values
            .iter()
            .filter_map(|(component_id, v)| {
                if let Some(db_id) = external_db_id_map.get(component_id) {
                    Some((*db_id, *v))
                } else {
                    warn!(?component_id, "Tried to upsert tvl for unknown component!");
                    None
                }
            })
            .collect();
        orm::ComponentTVL::upsert_many(&upsert_map)
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }

    pub async fn get_protocol_systems(
        &self,
        chain: &Chain,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<String>>, StorageError> {
        if !self.chain_id_cache.value_exists(chain) {
            return Err(StorageError::NotFound("Chain".to_string(), chain.to_string()));
        }
        let all_protocol_systems: Vec<String> = self
            .protocol_system_id_cache
            .map_enum
            .values()
            .cloned()
            .sorted()
            .collect();

        let total = all_protocol_systems.len() as i64;
        let paginated_protocol_systems = if let Some(params) = pagination_params {
            all_protocol_systems
                .into_iter()
                .skip(params.offset() as usize)
                .take(params.page_size as usize)
                .collect()
        } else {
            all_protocol_systems
        };

        Ok(WithTotal { total: Some(total), entity: paginated_protocol_systems })
    }

    pub async fn get_component_tvls(
        &self,
        chain: &Chain,
        system: Option<String>,
        component_ids: Option<&[&str]>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<HashMap<String, f64>>, StorageError> {
        use schema::{component_tvl::dsl as ct, protocol_component::dsl as pc};

        if !self.chain_id_cache.value_exists(chain) {
            return Err(StorageError::NotFound("Chain".to_string(), chain.to_string()));
        }

        let chain_id_val = self.get_chain_id(chain)?;

        let mut query = ct::component_tvl
            .inner_join(pc::protocol_component)
            .filter(pc::chain_id.eq(chain_id_val))
            .into_boxed();

        let mut count_query = ct::component_tvl
            .inner_join(pc::protocol_component)
            .filter(pc::chain_id.eq(chain_id_val))
            .into_boxed();

        if let Some(ids) = component_ids {
            query = query.filter(pc::external_id.eq_any(ids));
            count_query = count_query.filter(pc::external_id.eq_any(ids));
        }

        if let Some(system) = system {
            let system_id = self.get_protocol_system_id(&system)?;
            query = query.filter(pc::protocol_system_id.eq(system_id));
            count_query = count_query.filter(pc::protocol_system_id.eq(system_id));
        }

        if let Some(pagination) = pagination_params {
            query = query
                .limit(pagination.page_size)
                .offset(pagination.offset());
        }

        let count = count_query
            .count()
            .get_result::<i64>(conn)
            .await
            .map_err(PostgresError::from)?;

        let rows: Vec<(String, f64)> = query
            .select((pc::external_id, ct::tvl))
            .load(conn)
            .await
            .map_err(|err| {
                let id_hint = component_ids
                    .and_then(|ids| ids.first().copied())
                    .unwrap_or_default();
                storage_error_from_diesel(err, "ComponentTVL", id_hint, None)
            })?;

        let result = rows
            .into_iter()
            .collect::<HashMap<_, _>>();

        Ok(WithTotal { entity: result, total: Some(count) })
    }
}

#[cfg(test)]
mod test {
    use std::{slice, str::FromStr};

    use diesel_async::AsyncConnection;
    use rstest::rstest;
    use serde_json::json;
    use tycho_common::storage::BlockIdentifier;

    use super::*;
    use crate::postgres::db_fixtures;

    type EVMGateway = PostgresGateway;

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
    const LUSD: &str = "0x5f98805A4E8be255a32880FDeC7F6728C6568bA0";
    const DAI: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";
    const ZKSYNC_PEPE: &str = "0xFD282F16a64c6D304aC05d1A58Da15bed0467c71";

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

    /// This sets up the data needed to test the gateway. Returns the inserted chain's DB id and the
    /// inserted transaction hashes.
    ///
    /// The setup is structured such that each protocol state's historical changes are kept together
    /// to make it easy to reason about the change a state should have at each version. Please note
    /// that if you change something here, also update the state fixtures right below, which contain
    /// protocol states at each version.
    async fn setup_data(conn: &mut AsyncPgConnection) -> (i64, Vec<String>) {
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
        let chain_id_sn = db_fixtures::insert_chain(conn, "starknet").await;
        db_fixtures::insert_token(
            conn,
            chain_id_sn,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let chain_id_zk = db_fixtures::insert_chain(conn, "zksync").await;
        db_fixtures::insert_token(
            conn,
            chain_id_zk,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        let tx_hashes = [
            "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945".to_string(),
            "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54".to_string(),
            "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7".to_string(),
            "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388".to_string(),
        ];

        let txn = db_fixtures::insert_txns(
            conn,
            &[
                (blk[0], 1i64, &tx_hashes[0]),
                (blk[0], 2i64, &tx_hashes[1]),
                // ----- Block 01 LAST
                (blk[1], 1i64, &tx_hashes[2]),
                (blk[1], 2i64, &tx_hashes[3]),
                // ----- Block 02 LAST
            ],
        )
        .await;

        let protocol_system_id_ambient =
            db_fixtures::insert_protocol_system(conn, "ambient".to_owned()).await;
        let protocol_system_id_zz =
            db_fixtures::insert_protocol_system(conn, "zigzag".to_owned()).await;

        let protocol_type_id = db_fixtures::insert_protocol_type(
            conn,
            "Pool",
            Some(FinancialType::Swap),
            None,
            Some(ImplementationType::Custom),
        )
        .await;

        // insert tokens
        // Ethereum
        let (account_id_weth, weth_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            WETH.trim_start_matches("0x"),
            "WETH",
            18,
            None,
        )
        .await;
        let (_, usdc_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            USDC.trim_start_matches("0x"),
            "USDC",
            6,
            None,
        )
        .await;
        let (_, dai_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            DAI.trim_start_matches("0x"),
            "DAI",
            18,
            Some(100i32),
        )
        .await;
        let (_, lusd_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            LUSD.trim_start_matches("0x"),
            "LUSD",
            18,
            Some(70i32),
        )
        .await;

        // ZK Sync
        db_fixtures::insert_token(
            conn,
            chain_id_zk,
            ZKSYNC_PEPE.trim_start_matches("0x"),
            "PEPE",
            6,
            Some(0i32),
        )
        .await;

        // insert token prices
        db_fixtures::insert_token_prices(&[(weth_id, 1.0), (usdc_id, 0.0005)], conn).await;

        let contract_code_id = db_fixtures::insert_contract_code(
            conn,
            account_id_weth,
            txn[0],
            Bytes::from_str("C0C0C0").unwrap(),
        )
        .await;

        // components and their balances
        // tvl will be 2.0
        let protocol_component_id = db_fixtures::insert_protocol_component(
            conn,
            "state1",
            chain_id,
            protocol_system_id_ambient,
            protocol_type_id,
            txn[0],
            Some(vec![weth_id, usdc_id]),
            Some(vec![contract_code_id]),
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(10u128.pow(18)).lpad(32, 0),
            Bytes::zero(32),
            1e18,
            weth_id,
            txn[0],
            protocol_component_id,
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(2000 * 10u128.pow(6)).lpad(32, 0),
            Bytes::zero(32),
            2000.0 * 1e6,
            usdc_id,
            txn[0],
            protocol_component_id,
            None,
        )
        .await;
        // tvl will be 1.0 cause we miss dai price
        let protocol_component_id2 = db_fixtures::insert_protocol_component(
            conn,
            "state3",
            chain_id,
            protocol_system_id_ambient,
            protocol_type_id,
            txn[0],
            Some(vec![weth_id, dai_id]),
            Some(vec![contract_code_id]),
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(10u128.pow(18)).lpad(32, 0),
            Bytes::zero(32),
            1e18,
            weth_id,
            txn[0],
            protocol_component_id2,
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
            Bytes::zero(32),
            2000.0 * 1e18,
            dai_id,
            txn[0],
            protocol_component_id2,
            None,
        )
        .await;
        // tvl will be 1.0 cause we miss lusd price
        let protocol_component_id3 = db_fixtures::insert_protocol_component(
            conn,
            "state2",
            chain_id_sn,
            protocol_system_id_zz,
            protocol_type_id,
            txn[1],
            Some(vec![lusd_id, usdc_id]),
            Some(vec![contract_code_id]),
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
            Bytes::zero(32),
            1e18,
            lusd_id,
            txn[1],
            protocol_component_id3,
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(2000 * 10u128.pow(6)).lpad(32, 0),
            Bytes::zero(32),
            2000.0 * 1e6,
            usdc_id,
            txn[1],
            protocol_component_id3,
            None,
        )
        .await;
        // component without balances and thus without tvl
        db_fixtures::insert_protocol_component(
            conn,
            "no_tvl",
            chain_id,
            protocol_system_id_ambient,
            protocol_type_id,
            txn[0],
            Some(vec![weth_id, dai_id]),
            Some(vec![contract_code_id]),
        )
        .await;

        // protocol state for state1-reserve1
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[0],
            "reserve1".to_owned(),
            Bytes::from(1100u128).lpad(32, 0),
            None,
            Some(txn[2]),
        )
        .await;

        // protocol state for state1-reserve2
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[0],
            "reserve2".to_owned(),
            Bytes::from(500u128).lpad(32, 0),
            None,
            None,
        )
        .await;

        // protocol state update for state1-reserve1
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[3],
            "reserve1".to_owned(),
            Bytes::from(1000u128).lpad(32, 0),
            Some(Bytes::from(1100u128).lpad(32, 0)),
            None,
        )
        .await;

        db_fixtures::insert_component_balance(
            conn,
            Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
            Bytes::zero(32),
            2100.0 * 1e18,
            dai_id,
            txn[2],
            protocol_component_id3,
            None,
        )
        .await;
        db_fixtures::calculate_component_tvl(conn).await;
        (chain_id, tx_hashes.to_vec())
    }

    fn protocol_state() -> ProtocolComponentState {
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(1000u128).lpad(32, 0)),
            ("reserve2".to_owned(), Bytes::from(500u128).lpad(32, 0)),
        ]
        .into_iter()
        .collect();
        let balances: HashMap<Address, Balance> = vec![
            (
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
                    .parse()
                    .unwrap(),
                Balance::from(10u128.pow(18)).lpad(32, 0),
            ),
            (
                "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
                    .parse()
                    .unwrap(),
                Balance::from(2000 * 10u128.pow(6)).lpad(32, 0),
            ),
        ]
        .into_iter()
        .collect();
        ProtocolComponentState::new("state1", attributes, balances)
    }

    #[rstest]
    #[case::by_chain(None, None)]
    #[case::by_system(Some("ambient".to_string()), None)]
    #[case::by_ids(None, Some(vec ! ["state1"]))]
    #[tokio::test]
    async fn test_get_protocol_states(
        #[case] system: Option<String>,
        #[case] ids: Option<Vec<&str>>,
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let mut protocol_state = protocol_state();
        protocol_state.balances = HashMap::new();
        let expected = vec![protocol_state];

        let gateway = EVMGateway::from_connection(&mut conn).await;

        let result = gateway
            .get_protocol_states(
                &Chain::Ethereum,
                None,
                system,
                ids.as_deref(),
                false,
                None,
                &mut conn,
            )
            .await
            .unwrap()
            .entity;

        assert_eq!(result, expected)
    }

    #[tokio::test]
    async fn test_get_protocol_states_with_pagination() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let mut protocol_state = protocol_state();
        protocol_state.balances = HashMap::new();
        let expected = vec![protocol_state];

        let gateway = EVMGateway::from_connection(&mut conn).await;

        let result = gateway
            .get_protocol_states(
                &Chain::Ethereum,
                None,
                None,
                None,
                false,
                Some(&PaginationParams { page: 0, page_size: 1 }),
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(result.entity, expected);
        assert_eq!(result.total.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_get_protocol_states_at() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let gateway = EVMGateway::from_connection(&mut conn).await;

        let mut protocol_state = protocol_state();
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(1100u128).lpad(32, 0)),
            ("reserve2".to_owned(), Bytes::from(500u128).lpad(32, 0)),
        ]
        .into_iter()
        .collect();
        protocol_state.attributes = attributes;

        let expected = vec![
            protocol_state,
            ProtocolComponentState::new(
                "state3",
                HashMap::new(),
                HashMap::from([
                    (
                        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
                            .parse()
                            .unwrap(),
                        Balance::from(10u128.pow(18)).lpad(32, 0),
                    ),
                    (
                        "0x6b175474e89094c44da98b954eedeac495271d0f"
                            .parse()
                            .unwrap(),
                        Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
                    ),
                ]),
            ),
        ];

        let result = gateway
            .get_protocol_states(
                &Chain::Ethereum,
                Some(Version::from_block_number(Chain::Ethereum, 1)),
                None,
                None,
                true,
                None,
                &mut conn,
            )
            .await
            .unwrap()
            .entity;

        assert_eq!(result, expected)
    }

    fn protocol_state_delta() -> ProtocolComponentStateDelta {
        let attributes: HashMap<String, Bytes> =
            vec![("reserve1".to_owned(), Bytes::from(1000u128).lpad(32, 0))]
                .into_iter()
                .collect();
        ProtocolComponentStateDelta::new("state3", attributes, HashSet::new())
    }

    #[tokio::test]
    async fn test_update_protocol_states() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        let gateway = EVMGateway::from_connection(&mut conn).await;
        let chain = Chain::Ethereum;

        // set up deletable attribute state
        let protocol_component_id = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("state3"))
            .select(schema::protocol_component::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch protocol component id");
        let txn_id = schema::transaction::table
            .filter(
                schema::transaction::hash.eq(Bytes::from_str(
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                )
                .expect("valid txhash")
                .to_vec()),
            )
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");
        db_fixtures::insert_protocol_state(
            &mut conn,
            protocol_component_id,
            txn_id,
            "deletable".to_owned(),
            Bytes::from(1000u128).lpad(32, 0),
            None,
            None,
        )
        .await;

        // update
        let mut new_state1 = protocol_state_delta();
        let attributes1: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(700u128).lpad(32, 0)),
            ("reserve2".to_owned(), Bytes::from(700u128).lpad(32, 0)),
        ]
        .into_iter()
        .collect();
        new_state1
            .updated_attributes
            .clone_from(&attributes1);
        new_state1.deleted_attributes = vec!["deletable".to_owned()]
            .into_iter()
            .collect();
        let tx_1 =
            Bytes::from_str("0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7")
                .unwrap();

        // newer update
        let mut new_state2 = protocol_state_delta();
        let attributes2: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(800u128).lpad(32, 0)),
            ("reserve2".to_owned(), Bytes::from(800u128).lpad(32, 0)),
        ]
        .into_iter()
        .collect();
        new_state2
            .updated_attributes
            .clone_from(&attributes2);
        let tx_2 =
            Bytes::from_str("0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388")
                .unwrap();

        // update the protocol state
        gateway
            .update_protocol_states(
                &chain,
                &[(tx_1.clone(), &new_state1), (tx_2.clone(), &new_state2)],
                &mut conn,
            )
            .await
            .expect("Failed to update protocol states");

        // check the correct state is considered the valid one
        let db_states = gateway
            .get_protocol_states(
                &chain,
                None,
                None,
                Some(&[new_state1.component_id.as_str()]),
                true,
                None,
                &mut conn,
            )
            .await
            .expect("Failed ")
            .entity;
        let mut expected_state = protocol_state();
        expected_state.attributes = attributes2;
        expected_state
            .component_id
            .clone_from(&new_state1.component_id);
        expected_state.balances = vec![
            (
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
                    .parse()
                    .unwrap(),
                Balance::from(10u128.pow(18)).lpad(32, 0),
            ),
            (
                "0x6b175474e89094c44da98b954eedeac495271d0f"
                    .parse()
                    .unwrap(),
                Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
            ),
        ]
        .into_iter()
        .collect();
        assert_eq!(db_states[0], expected_state);

        // fetch the older state from the db and check it's valid_to is set correctly
        let older_state = schema::protocol_state::table
            .inner_join(schema::protocol_component::table)
            .inner_join(schema::transaction::table)
            .filter(schema::transaction::hash.eq(tx_1))
            .filter(schema::protocol_component::external_id.eq(new_state1.component_id.as_str()))
            .select(orm::ProtocolState::as_select())
            .first::<orm::ProtocolState>(&mut conn)
            .await
            .expect("Failed to fetch protocol state");
        assert_eq!(older_state.attribute_value, Bytes::from(700u128).lpad(32, 0));
        // fetch the newer state from the db to compare the valid_from
        let newer_state = schema::protocol_state::table
            .inner_join(schema::protocol_component::table)
            .inner_join(schema::transaction::table)
            .filter(schema::transaction::hash.eq(tx_2))
            .filter(schema::protocol_component::external_id.eq(new_state1.component_id.as_str()))
            .select(orm::ProtocolState::as_select())
            .first::<orm::ProtocolState>(&mut conn)
            .await
            .expect("Failed to fetch protocol state");
        assert_eq!(older_state.valid_to, newer_state.valid_from);

        // check the deleted attribute is deleted (valid_to has been set correctly)
        let deleted_state = schema::protocol_state::table
            .inner_join(schema::protocol_component::table)
            .filter(schema::protocol_component::external_id.eq(new_state2.component_id.as_str()))
            .filter(schema::protocol_state::attribute_name.eq("deletable"))
            .select(orm::ProtocolState::as_select())
            .first::<orm::ProtocolState>(&mut conn)
            .await
            .expect("Failed to fetch protocol state");
        assert_eq!(deleted_state.valid_to, older_state.valid_to);
    }

    #[tokio::test]
    async fn test_get_balance_deltas() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let protocol_external_id = String::from("state1");
        // set up changed balances
        let protocol_component_id = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq(protocol_external_id.clone()))
            .select(schema::protocol_component::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch protocol component id");
        let (token_id, account_id) = schema::token::table
            .filter(schema::token::symbol.eq("WETH"))
            .select((schema::token::id, schema::token::account_id))
            .first::<(i64, i64)>(&mut conn)
            .await
            .expect("Failed to fetch token id and acccount id");
        let token_address = schema::account::table
            .filter(schema::account::id.eq(account_id))
            .select(schema::account::address)
            .first::<Address>(&mut conn)
            .await
            .expect("Failed to fetch token address");

        let from_tx_hash =
            Bytes::from_str("0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54")
                .expect("valid txhash");

        let from_txn_id = schema::transaction::table
            .filter(schema::transaction::hash.eq(from_tx_hash.to_vec()))
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");

        let to_tx_hash =
            Bytes::from("0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388");

        let to_txn_id = schema::transaction::table
            .filter(schema::transaction::hash.eq(&to_tx_hash))
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");

        diesel::update(schema::component_balance::table)
            .filter(
                schema::component_balance::protocol_component_id
                    .eq(protocol_component_id)
                    .and(schema::component_balance::token_id.eq(&token_id)),
            )
            .set(schema::component_balance::valid_to.eq(db_fixtures::yesterday_one_am()))
            .execute(&mut conn)
            .await
            .expect("version update failed");

        db_fixtures::insert_component_balance(
            &mut conn,
            Balance::from(1000u128).lpad(32, 0),
            Balance::zero(32),
            1000.0,
            token_id,
            from_txn_id,
            protocol_component_id,
            Some(to_txn_id),
        )
        .await;
        db_fixtures::insert_component_balance(
            &mut conn,
            Balance::from(2000u128).lpad(32, 0),
            Balance::from(1000u128).lpad(32, 0),
            2000.0,
            token_id,
            to_txn_id,
            protocol_component_id,
            None,
        )
        .await;

        let gateway = EVMGateway::from_connection(&mut conn).await;

        let expected_forward_deltas = vec![ComponentBalance {
            component_id: protocol_external_id.clone(),
            token: token_address.clone(),
            balance: Balance::from(2000u128).lpad(32, 0),
            balance_float: 2000.0,
            modify_tx: to_tx_hash,
        }];

        // test forward case
        let result = gateway
            .get_balance_deltas(
                &Chain::Ethereum,
                Some(&BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1)))),
                &BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 2))),
                &mut conn,
            )
            .await
            .unwrap();
        assert_eq!(result, expected_forward_deltas);

        let expected_txh =
            Bytes::from("bb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945");
        let expected_backward_deltas = vec![
            ComponentBalance {
                token: Bytes::from(DAI),
                balance: Bytes::from(
                    "0x0000000000000000000000000000000000000000000000000000000000000000",
                ),
                balance_float: 0.0,
                modify_tx: expected_txh.clone(),
                component_id: "state3".to_owned(),
            },
            ComponentBalance {
                token: Bytes::from(USDC),
                balance: Bytes::from(
                    "0x0000000000000000000000000000000000000000000000000000000000000000",
                ),
                balance_float: 0.0,
                modify_tx: expected_txh.clone(),
                component_id: "state1".to_owned(),
            },
            ComponentBalance {
                token: Bytes::from(WETH),
                balance: Bytes::from(
                    "0x0000000000000000000000000000000000000000000000000000000000000000",
                ),
                balance_float: 0.0,
                modify_tx: expected_txh.clone(),
                component_id: "state1".to_owned(),
            },
            ComponentBalance {
                token: Bytes::from(WETH),
                balance: Bytes::from(
                    "0x0000000000000000000000000000000000000000000000000000000000000000",
                ),
                balance_float: 0.0,
                modify_tx: expected_txh.clone(),
                component_id: "state3".to_owned(),
            },
        ];

        // test backward case
        let mut result = gateway
            .get_balance_deltas(
                &Chain::Ethereum,
                Some(&BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 2)))),
                &BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1))),
                &mut conn,
            )
            .await
            .unwrap();
        result.sort_unstable_by_key(|e| (e.token.clone(), e.component_id.clone()));
        // fix NaN comparison
        result.iter_mut().for_each(|r| {
            assert!(r.balance_float.is_nan());
            r.balance_float = 0.0;
        });
        assert_eq!(result, expected_backward_deltas);
    }

    #[tokio::test]
    async fn test_get_protocol_states_delta_forward() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        // set up deleted attribute state
        let protocol_component_id = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("state1"))
            .select(schema::protocol_component::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch protocol component id");
        let from_txn_id = schema::transaction::table
            .filter(
                schema::transaction::hash.eq(Bytes::from_str(
                    "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54",
                )
                .expect("valid txhash")
                .to_vec()),
            )
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");
        let to_txn_id = schema::transaction::table
            .filter(
                schema::transaction::hash.eq(Bytes::from_str(
                    "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388",
                )
                .expect("valid txhash")
                .to_vec()),
            )
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");
        db_fixtures::insert_protocol_state(
            &mut conn,
            protocol_component_id,
            from_txn_id,
            "deleted".to_owned(),
            Bytes::from(1000u128).lpad(32, 0),
            None,
            Some(to_txn_id),
        )
        .await;

        // set up deleted attribute different state (one that isn't also updated)
        let protocol_component_id2 = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("state3"))
            .select(schema::protocol_component::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch protocol component id");
        db_fixtures::insert_protocol_state(
            &mut conn,
            protocol_component_id2,
            from_txn_id,
            "deleted2".to_owned(),
            Bytes::from(100u128).lpad(32, 0),
            None,
            Some(to_txn_id),
        )
        .await;

        let gateway = EVMGateway::from_connection(&mut conn).await;

        // expected result
        let mut state_delta = protocol_state_delta();
        state_delta
            .component_id
            .clone_from(&"state1".to_string());
        state_delta.deleted_attributes = vec!["deleted".to_owned()]
            .into_iter()
            .collect();
        let other_state_delta = ProtocolComponentStateDelta {
            component_id: "state3".to_owned(),
            updated_attributes: HashMap::new(),
            deleted_attributes: vec!["deleted2".to_owned()]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let expected = vec![state_delta, other_state_delta];

        // test
        let result = gateway
            .get_protocol_states_delta(
                &Chain::Ethereum,
                Some(&BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1)))),
                &BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 2))),
                &mut conn,
            )
            .await
            .unwrap();

        // asserts
        assert_eq!(result, expected)
    }

    #[tokio::test]
    async fn test_get_protocol_states_delta_backward() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;

        // set up newly added attribute state (to be deleted on revert)
        let protocol_component_id = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("state1"))
            .select(schema::protocol_component::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch protocol component id");
        let txn_id = schema::transaction::table
            .filter(
                schema::transaction::hash.eq(Bytes::from_str(
                    "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7",
                )
                .expect("valid txhash")
                .to_vec()),
            )
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");
        db_fixtures::insert_protocol_state(
            &mut conn,
            protocol_component_id,
            txn_id,
            "to_delete".to_owned(),
            Bytes::from(1000u128).lpad(32, 0),
            None,
            None,
        )
        .await;

        // set up deleted attribute state (to be created on revert)
        let from_txn_id = schema::transaction::table
            .filter(
                schema::transaction::hash.eq(Bytes::from_str(
                    "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54",
                )
                .expect("valid txhash")
                .to_vec()),
            )
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");
        let to_txn_id = schema::transaction::table
            .filter(
                schema::transaction::hash.eq(Bytes::from_str(
                    "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388",
                )
                .expect("valid txhash")
                .to_vec()),
            )
            .select(schema::transaction::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");
        db_fixtures::insert_protocol_state(
            &mut conn,
            protocol_component_id,
            from_txn_id,
            "deleted".to_owned(),
            Bytes::from(1000u128).lpad(32, 0),
            None,
            Some(to_txn_id),
        )
        .await;

        let gateway = EVMGateway::from_connection(&mut conn).await;

        // expected result
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve1".to_owned(), Bytes::from(1100u128).lpad(32, 0)),
            ("deleted".to_owned(), Bytes::from(1000u128).lpad(32, 0)),
        ]
        .into_iter()
        .collect();
        let state_delta = ProtocolComponentStateDelta {
            component_id: "state1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: vec!["to_delete".to_owned()]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let expected = vec![state_delta];

        // test
        let result = gateway
            .get_protocol_states_delta(
                &Chain::Ethereum,
                Some(&BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 2)))),
                &BlockOrTimestamp::Block(BlockIdentifier::Number((Chain::Ethereum, 1))),
                &mut conn,
            )
            .await
            .unwrap();

        // asserts
        assert_eq!(result, expected)
    }

    #[tokio::test]
    async fn test_get_or_create_protocol_system_id() {
        let mut conn = setup_db().await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let first_id = gw
            ._get_or_create_protocol_system_id("ambient".to_string(), &mut conn)
            .await
            .unwrap();

        let second_id = gw
            ._get_or_create_protocol_system_id("ambient".to_string(), &mut conn)
            .await
            .unwrap();
        assert!(first_id > 0);
        assert_eq!(first_id, second_id);
    }

    #[tokio::test]
    async fn test_add_protocol_type() {
        let mut conn = setup_db().await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let protocol_type = ProtocolType {
            name: "Protocol".to_string(),
            financial_type: FinancialType::Debt,
            attribute_schema: Some(json!({"attribute": "schema"})),
            implementation: ImplementationType::Custom,
        };

        gw.add_protocol_types(&[protocol_type], &mut conn)
            .await
            .unwrap();

        let inserted_data = schema::protocol_type::table
            .filter(schema::protocol_type::name.eq("Protocol"))
            .select(schema::protocol_type::all_columns)
            .first::<orm::ProtocolType>(&mut conn)
            .await
            .unwrap();

        assert_eq!(inserted_data.name, "Protocol".to_string());
        assert_eq!(inserted_data.financial_type, orm::FinancialType::Debt);
        assert_eq!(inserted_data.attribute_schema, Some(json!({"attribute": "schema"})));
        assert_eq!(inserted_data.implementation, orm::ImplementationType::Custom);
    }

    #[tokio::test]
    async fn test_get_tokens() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        // get all eth tokens (no address filter)
        let tokens = gw
            .get_tokens(Chain::Ethereum, None, QualityRange::None(), None, None, &mut conn)
            .await
            .unwrap()
            .entity;
        assert_eq!(tokens.len(), 5);

        // get weth and usdc
        let tokens = gw
            .get_tokens(
                Chain::Ethereum,
                Some(&[&WETH.into(), &USDC.into()]),
                QualityRange::None(),
                None,
                None,
                &mut conn,
            )
            .await
            .unwrap()
            .entity;
        assert_eq!(tokens.len(), 2);

        // get weth
        let tokens = gw
            .get_tokens(
                Chain::Ethereum,
                Some(&[&WETH.into()]),
                QualityRange::None(),
                None,
                None,
                &mut conn,
            )
            .await
            .unwrap()
            .entity;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].symbol, "WETH".to_string());
        assert_eq!(tokens[0].decimals, 18);
    }

    #[tokio::test]
    async fn test_get_tokens_with_pagination() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        // get tokens with pagination
        let result = gw
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::None(),
                None,
                Some(&PaginationParams { page: 0, page_size: 1 }),
                &mut conn,
            )
            .await
            .unwrap();
        assert_eq!(result.entity.len(), 1);
        assert_eq!(result.total, Some(5));

        let first_token_symbol = result.entity[0].symbol.clone();

        // get tokens with 0 page_size
        let result = gw
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::None(),
                None,
                Some(&PaginationParams { page: 0, page_size: 0 }),
                &mut conn,
            )
            .await
            .unwrap();
        assert_eq!(result.entity.len(), 0);
        assert_eq!(result.total, Some(5));

        // get tokens skipping page
        let result = gw
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::None(),
                None,
                Some(&PaginationParams { page: 2, page_size: 1 }),
                &mut conn,
            )
            .await
            .unwrap();
        assert_eq!(result.entity.len(), 1);
        assert_eq!(result.total, Some(5));
        assert_ne!(result.entity[0].symbol, first_token_symbol);
    }

    #[tokio::test]
    async fn test_get_tokens_zksync() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let tokens = gw
            .get_tokens(Chain::ZkSync, None, QualityRange::None(), None, None, &mut conn)
            .await
            .unwrap()
            .entity;

        assert_eq!(tokens.len(), 2);
        let expected_token =
            Token::new(&ZKSYNC_PEPE.parse().unwrap(), "PEPE", 6, 10, &[Some(10)], Chain::ZkSync, 0);

        assert_eq!(tokens[1], expected_token);
    }

    #[tokio::test]
    async fn test_get_tokens_with_min_quality() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let tokens = gw
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::min_only(80_i32),
                None,
                None,
                &mut conn,
            )
            .await
            .unwrap()
            .entity;

        assert_eq!(tokens.len(), 2);

        let expected_token =
            Token::new(&DAI.parse().unwrap(), "DAI", 18, 10, &[Some(10)], Chain::Ethereum, 100);

        assert_eq!(tokens[1], expected_token);
    }

    #[tokio::test]
    async fn test_get_tokens_with_quality_range() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let tokens = gw
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::new(60_i32, 70_i32),
                None,
                None,
                &mut conn,
            )
            .await
            .unwrap()
            .entity;

        assert_eq!(tokens.len(), 1);

        let expected_token =
            Token::new(&LUSD.parse().unwrap(), "LUSD", 18, 10, &[Some(10)], Chain::Ethereum, 70);

        assert_eq!(tokens[0], expected_token);
    }

    #[tokio::test]
    async fn test_get_tokens_with_30_day_activity() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let days_cutoff: Option<NaiveDateTime> = Some(db_fixtures::yesterday_midnight());

        let tokens = gw
            .get_tokens(Chain::Ethereum, None, QualityRange::None(), days_cutoff, None, &mut conn)
            .await
            .unwrap()
            .entity;

        assert_eq!(tokens.len(), 1);
        let expected_token =
            Token::new(&DAI.parse().unwrap(), "DAI", 18, 10, &[Some(10)], Chain::Ethereum, 100);

        assert_eq!(tokens[0], expected_token);
    }

    #[tokio::test]
    async fn test_add_tokens() {
        let mut conn = setup_db().await;
        let (chain_id, _) = setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        // Insert one new token (USDT) and an existing token (WETH)
        let weth_symbol = "WETH".to_string();
        let old_token = db_fixtures::get_token_by_symbol(&mut conn, weth_symbol.clone()).await;
        let old_weth_account = &orm::Account::by_address(
            &Bytes::from_str(WETH.trim_start_matches("0x")).expect("address ok"),
            chain_id,
            &mut conn,
        )
        .await
        .unwrap()[0];

        let usdt_symbol = "USDT".to_string();
        let tokens = [
            Token::new(
                &Bytes::from(USDT),
                usdt_symbol.as_str(),
                6,
                0,
                &[Some(64), None],
                Chain::Ethereum,
                100,
            ),
            Token::new(
                &Bytes::from(WETH),
                weth_symbol.as_str(),
                18,
                0,
                &[Some(100), None],
                Chain::Ethereum,
                100,
            ),
        ];

        gw.add_tokens(&tokens, &mut conn)
            .await
            .unwrap();

        let inserted_token = db_fixtures::get_token_by_symbol(&mut conn, usdt_symbol.clone()).await;
        assert_eq!(inserted_token.symbol, usdt_symbol);
        assert_eq!(inserted_token.decimals, 6);
        let inserted_account = &orm::Account::by_address(
            &Bytes::from_str(USDT.trim_start_matches("0x")).expect("address ok"),
            chain_id,
            &mut conn,
        )
        .await
        .unwrap()[0];
        assert_eq!(inserted_account.id, inserted_token.account_id);
        assert_eq!(inserted_account.title, "Ethereum_USDT".to_string());

        // make sure nothing changed on WETH (ids included)
        let new_token = db_fixtures::get_token_by_symbol(&mut conn, weth_symbol.clone()).await;
        assert_eq!(new_token, old_token);
        let updated_weth_account = &orm::Account::by_address(
            &Bytes::from_str(WETH.trim_start_matches("0x")).expect("address ok"),
            chain_id,
            &mut conn,
        )
        .await
        .unwrap()[0];
        assert_eq!(updated_weth_account, old_weth_account);
        assert!(inserted_account.id > updated_weth_account.id);
    }

    #[tokio::test]
    async fn test_update_tokens() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let dai_address = Bytes::from(DAI);
        let mut prev = gw
            .get_tokens(
                Chain::Ethereum,
                Some(&[&dai_address]),
                QualityRange::None(),
                None,
                None,
                &mut conn,
            )
            .await
            .expect("failed to get old token")
            .entity
            .remove(0);
        prev.gas = vec![Some(20000)];

        gw.update_tokens(&[prev.clone()], &mut conn)
            .await
            .expect("failed to update tokens");
        let updated = gw
            .get_tokens(
                Chain::Ethereum,
                Some(&[&dai_address]),
                QualityRange::None(),
                None,
                None,
                &mut conn,
            )
            .await
            .expect("failed to get updated token")
            .entity
            .remove(0);

        assert_eq!(updated, prev);
    }

    #[tokio::test]
    async fn test_add_component_balances() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let tx_hash =
            Bytes::from("0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945");
        let component_external_id = "state2".to_owned();
        let base_token = Bytes::from(WETH);
        // Test the case where a previous balance doesn't exist
        let component_balance = ComponentBalance {
            token: base_token.clone(),
            balance: Bytes::from(
                "0x000000000000000000000000000000000000000000000000000000000000000c",
            ),
            balance_float: 12.0,
            modify_tx: tx_hash.clone(),
            component_id: component_external_id.clone(),
        };

        gw.add_component_balances(&[component_balance], &Chain::Starknet, &mut conn)
            .await
            .unwrap();

        let inserted_data = schema::component_balance::table
            .select(orm::ComponentBalance::as_select())
            .filter(schema::component_balance::new_balance.eq(Bytes::from(
                "0x000000000000000000000000000000000000000000000000000000000000000c",
            )))
            .first::<orm::ComponentBalance>(&mut conn)
            .await
            .expect("retrieving inserted balance failed!");

        assert_eq!(inserted_data.new_balance, Balance::from(12u128).lpad(32, 0));
        assert_eq!(inserted_data.previous_value, Balance::from("0x00"),);

        let referenced_token = schema::token::table
            .filter(schema::token::id.eq(inserted_data.token_id))
            .select(orm::Token::as_select())
            .first::<orm::Token>(&mut conn)
            .await
            .expect("failed to get associated token");
        assert_eq!(referenced_token.symbol, "WETH");

        let referenced_component = schema::protocol_component::table
            .filter(schema::protocol_component::id.eq(inserted_data.protocol_component_id))
            .select(orm::ProtocolComponent::as_select())
            .first::<orm::ProtocolComponent>(&mut conn)
            .await
            .expect("failed to get associated component");
        assert_eq!(referenced_component.external_id, "state2");

        // Test the case where there was a previous balance
        let new_tx_hash =
            Bytes::from("0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7");
        let updated_component_balance = ComponentBalance {
            token: base_token.clone(),
            balance: Balance::from(2000u128).lpad(32, 0),
            balance_float: 2000.0,
            modify_tx: new_tx_hash,
            component_id: component_external_id.clone(),
        };

        let updated_component_balances = vec![updated_component_balance.clone()];

        dbg!(&updated_component_balances);
        gw.add_component_balances(&updated_component_balances, &Chain::Starknet, &mut conn)
            .await
            .unwrap();

        // Obtain newest inserted value
        let new_inserted_data = schema::component_balance::table
            .inner_join(schema::protocol_component::table)
            .select(orm::ComponentBalance::as_select())
            .filter(
                schema::component_balance::valid_to
                    .eq(MAX_TS)
                    .and(schema::protocol_component::external_id.eq(&component_external_id))
                    .and(schema::component_balance::token_id.eq(referenced_token.id)),
            )
            .first::<orm::ComponentBalance>(&mut conn)
            .await
            .expect("retrieving inserted balance failed!");

        assert_eq!(new_inserted_data.new_balance, Balance::from(2000u128).lpad(32, 0));
        assert_eq!(new_inserted_data.previous_value, Balance::from(12u128).lpad(32, 0));
    }

    #[tokio::test]
    async fn test_add_protocol_components() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let protocol_type_name_1 = String::from("Test_Type_1");
        let protocol_type_id_1 =
            db_fixtures::insert_protocol_type(&mut conn, &protocol_type_name_1, None, None, None)
                .await;
        db_fixtures::insert_protocol_type(&mut conn, "Test_Type_2", None, None, None).await;
        let protocol_system = "ambient".to_string();
        let chain = Chain::Ethereum;
        let original_component = ProtocolComponent::new(
            "test_contract_id",
            &protocol_system,
            &protocol_type_name_1,
            chain,
            vec![Bytes::from(WETH)],
            vec![Bytes::from(WETH)],
            HashMap::new(),
            ChangeType::Creation,
            Bytes::from("0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945"),
            Default::default(),
        );

        gw.add_protocol_components(slice::from_ref(&original_component), &mut conn)
            .await
            .expect("adding components failed");

        let inserted_data = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("test_contract_id".to_string()))
            .select(orm::ProtocolComponent::as_select())
            .first::<orm::ProtocolComponent>(&mut conn)
            .await
            .expect("failed to get inserted data");

        assert_eq!(inserted_data.protocol_type_id, protocol_type_id_1);
        assert_eq!(
            gw.get_protocol_system_id(
                &original_component
                    .protocol_system
                    .to_string()
            )
            .unwrap(),
            inserted_data.protocol_system_id
        );
        assert_eq!(
            gw.get_chain_id(&original_component.chain)
                .unwrap(),
            inserted_data.chain_id
        );
        assert_eq!(original_component.id, inserted_data.external_id);

        // assert junction table
        let component_token_junction = schema::protocol_component_holds_token::table
            .select((
                schema::protocol_component_holds_token::protocol_component_id,
                schema::protocol_component_holds_token::token_id,
            ))
            .filter(
                schema::protocol_component_holds_token::protocol_component_id.eq(inserted_data.id),
            )
            .first::<(i64, i64)>(&mut conn)
            .await
            .unwrap();

        assert_eq!(component_token_junction.0, inserted_data.id);

        let token = schema::token::table
            .select(schema::token::all_columns)
            .filter(schema::token::id.eq(component_token_junction.1))
            .load::<orm::Token>(&mut conn)
            .await;

        assert!(token.is_ok());

        // assert component-contract junction table
        let component_contract_junction = schema::protocol_component_holds_contract::table
            .select((
                schema::protocol_component_holds_contract::protocol_component_id,
                schema::protocol_component_holds_contract::contract_code_id,
            ))
            .filter(
                schema::protocol_component_holds_contract::protocol_component_id
                    .eq(inserted_data.id),
            )
            .first::<(i64, i64)>(&mut conn)
            .await
            .unwrap();

        assert_eq!(component_contract_junction.0, inserted_data.id);

        let contract = schema::contract_code::table
            .select(schema::contract_code::all_columns)
            .filter(schema::contract_code::id.eq(component_contract_junction.1))
            .load::<orm::ContractCode>(&mut conn)
            .await;

        assert!(contract.is_ok())
    }

    fn create_test_protocol_component(id: &str) -> ProtocolComponent {
        ProtocolComponent::new(
            id,
            "ambient",
            "type_id_1",
            Chain::Ethereum,
            vec![],
            vec![],
            HashMap::new(),
            ChangeType::Creation,
            Bytes::from("0x0000000000000000000000000000000000000000000000000000000011121314"),
            chrono::DateTime::from_timestamp(1000, 0)
                .unwrap()
                .naive_utc(),
        )
    }

    #[tokio::test]
    async fn test_delete_protocol_components() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let test_components = vec![
            create_test_protocol_component("state1"),
            create_test_protocol_component("state2"),
        ];

        gw.delete_protocol_components(&test_components, Utc::now().naive_utc(), &mut conn)
            .await
            .expect("failed to delete protocol components");

        let pc_ids: Vec<String> = test_components
            .iter()
            .map(|test_pc| test_pc.id.to_string())
            .collect();

        let updated_timestamps = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq_any(pc_ids))
            .select(schema::protocol_component::deleted_at)
            .load::<Option<NaiveDateTime>>(&mut conn)
            .await
            .unwrap();

        assert_eq!(updated_timestamps.len(), 2);
        updated_timestamps
            .into_iter()
            .for_each(|ts| assert!(ts.is_some(), "Found None in updated_ts"));
    }

    #[tokio::test]
    async fn test_get_protocol_components_with_pagination() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let result = gw
            .get_protocol_components(
                &Chain::Ethereum,
                None,
                None,
                None,
                // Without pagination should return 3 components
                Some(&PaginationParams { page: 0, page_size: 2 }),
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(result.entity.len(), 2);
        assert_eq!(result.total, Some(3));
    }

    #[rstest]
    #[case::get_one(Some("zigzag".to_string()))]
    #[case::get_none(Some("ambient".to_string()))]
    #[tokio::test]
    async fn test_get_protocol_components_with_system_only(#[case] system: Option<String>) {
        let mut conn = setup_db().await;
        let (_, tx_hashes) = setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let chain = Chain::Starknet;

        let result = gw
            .get_protocol_components(&chain, system.clone(), None, None, None, &mut conn)
            .await;

        assert!(result.is_ok());

        match system.unwrap().as_str() {
            "zigzag" => {
                let components = result.unwrap().entity;
                assert_eq!(components.len(), 1);

                let pc = &components[0];
                assert_eq!(pc.id, "state2".to_string());
                assert_eq!(pc.protocol_system, "zigzag");
                assert_eq!(pc.chain, Chain::Starknet);
                assert_eq!(pc.creation_tx, Bytes::from(tx_hashes.get(1).unwrap().as_str()));
            }
            "ambient" => {
                let components = result.unwrap().entity;
                assert_eq!(components.len(), 0)
            }
            _ => {}
        }
    }

    #[rstest]
    #[case::get_one("state1".to_string())]
    #[case::get_none("state2".to_string())]
    #[tokio::test]
    async fn test_get_protocol_components_with_external_id_only(#[case] external_id: String) {
        let mut conn = setup_db().await;
        let (_, tx_hashes) = setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let temp_ids_array = [external_id.as_str()];
        let ids = Some(temp_ids_array.as_slice());
        let chain = Chain::Ethereum;

        let result = gw
            .get_protocol_components(&chain, None, ids, None, None, &mut conn)
            .await
            .unwrap()
            .entity;

        match external_id.as_str() {
            "state1" => {
                assert_eq!(result.len(), 1);

                let pc = &result[0];
                assert_eq!(pc.id, external_id.to_string());
                assert_eq!(pc.protocol_system, "ambient");
                assert_eq!(pc.chain, Chain::Ethereum);
                assert_eq!(pc.creation_tx, Bytes::from(tx_hashes[0].as_str()));
            }
            "state2" => {
                assert_eq!(result.len(), 0)
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn test_get_protocol_components_with_system_and_ids() {
        let mut conn = setup_db().await;
        let (_, tx_hashes) = setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let system = "ambient".to_string();
        let ids = Some(["state1", "state2"].as_slice());
        let chain = Chain::Ethereum;
        let result = gw
            .get_protocol_components(&chain, Some(system), ids, None, None, &mut conn)
            .await;

        let components = result.unwrap().entity;
        assert_eq!(components.len(), 1);

        let pc = &components[0];
        assert_eq!(pc.id, "state1".to_string());
        assert_eq!(pc.protocol_system, "ambient");
        assert_eq!(pc.chain, Chain::Ethereum);
        assert_eq!(pc.creation_tx, Bytes::from(tx_hashes[0].as_str()));
    }

    #[rstest]
    #[case::ethereum(Chain::Ethereum, & ["state1", "state3", "no_tvl"])]
    #[case::starknet(Chain::Starknet, & ["state2"])]
    #[tokio::test]
    async fn test_get_protocol_components_with_chain_filter(
        #[case] chain: Chain,
        #[case] exp_ids: &[&str],
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = exp_ids
            .iter()
            .map(|&s| s.to_owned())
            .collect::<HashSet<_>>();

        let components = gw
            .get_protocol_components(&chain, None, None, None, None, &mut conn)
            .await
            .expect("failed retrieving components")
            .entity
            .into_iter()
            .map(|c| c.id)
            .collect::<HashSet<_>>();

        assert_eq!(components, exp);
    }

    #[rstest]
    #[case::empty(Some(10.0), & [])]
    #[case::all(None, & ["state1", "state3", "no_tvl"])]
    #[case::with_tvl(Some(0.0), & ["state1", "state3"])]
    #[case::with_tvl(Some(1.0), & ["state1"])]
    #[tokio::test]
    async fn test_get_protocol_components_with_min_tvl(
        #[case] min_tvl: Option<f64>,
        #[case] exp_ids: &[&str],
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let exp = exp_ids
            .iter()
            .map(|&s| s.to_owned())
            .collect::<HashSet<_>>();
        let gw = EVMGateway::from_connection(&mut conn).await;

        let res = gw
            .get_protocol_components(&Chain::Ethereum, None, None, min_tvl, None, &mut conn)
            .await
            .expect("failed retrieving components")
            .entity
            .into_iter()
            .map(|comp| comp.id)
            .collect::<HashSet<_>>();

        assert_eq!(res, exp);
    }

    #[rstest]
    #[case::dai(&[DAI], HashMap::from([
        (Bytes::from("0x6b175474e89094c44da98b954eedeac495271d0f"), (
            "state3".to_string(),
            Bytes::from("0x00000000000000000000000000000000000000000000006c6b935b8bbd400000")
        ))]))]
    #[case::weth(&[WETH], HashMap::from([
        (Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"), (
            "state1".to_string(),
            Bytes::from("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")
        ))]))]
    #[tokio::test]
    async fn test_get_token_owners(
        #[case] tokens: &[&str],
        #[case] exp: HashMap<Address, (String, Bytes)>,
    ) {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let tokens = tokens
            .iter()
            .map(|s| Bytes::from(*s))
            .collect::<Vec<_>>();

        let res = gw
            .get_token_owners(&Chain::Ethereum, &tokens, Some(1.0), &mut conn)
            .await
            .expect("retrieving components failed");

        assert_eq!(res, exp);
    }

    #[tokio::test]
    async fn test_get_token_prices() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = [(Bytes::from(WETH), 1.0), (Bytes::from(USDC), 0.0005)]
            .into_iter()
            .collect::<HashMap<_, _>>();

        let prices = gw
            .get_token_prices(&Chain::Ethereum, &mut conn)
            .await
            .expect("retrieving token prices failed!");

        assert_eq!(prices, exp);
    }

    #[tokio::test]
    async fn test_get_component_balances() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp: HashMap<_, _> = [
            (
                "state1",
                Bytes::from(WETH),
                ComponentBalance::new(
                    Bytes::from(WETH),
                    Balance::from(10u128.pow(18)).lpad(32, 0),
                    1e18,
                    Bytes::zero(32),
                    "state1",
                ),
            ),
            (
                "state1",
                Bytes::from(USDC),
                ComponentBalance::new(
                    Bytes::from(USDC),
                    Balance::from(2000 * 10u128.pow(6)).lpad(32, 0),
                    2000000000.0,
                    Bytes::zero(32),
                    "state1",
                ),
            ),
        ]
        .into_iter()
        .group_by(|e| e.0)
        .into_iter()
        .map(|(cid, group)| {
            (
                cid.to_owned(),
                group
                    .map(|(_, addr, bal)| (addr, bal))
                    .collect::<HashMap<_, _>>(),
            )
        })
        .collect();

        let res = gw
            .get_component_balances(&Chain::Ethereum, Some(&["state1"]), None, &mut conn)
            .await
            .expect("retrieving balances failed!");

        assert_eq!(res, exp);
    }

    #[tokio::test]
    async fn test_get_balances_at() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        // set up changed balances
        let protocol_component_id = schema::protocol_component::table
            .filter(schema::protocol_component::external_id.eq("state3"))
            .select(schema::protocol_component::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch protocol component id");
        let weth_id = schema::token::table
            .filter(schema::token::symbol.eq("WETH"))
            .select(schema::token::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch token id");
        let dai_id = schema::token::table
            .filter(schema::token::symbol.eq("DAI"))
            .select(schema::token::id)
            .first::<i64>(&mut conn)
            .await
            .expect("Failed to fetch token id");

        let tx_hash =
            Bytes::from_str("0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7")
                .expect("valid txhash");

        let (txn_id, ts) = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::hash.eq(tx_hash.to_vec()))
            .select((schema::transaction::id, schema::block::ts))
            .first::<(i64, NaiveDateTime)>(&mut conn)
            .await
            .expect("Failed to fetch transaction id");

        diesel::update(
            schema::component_balance::table
                .filter(schema::component_balance::protocol_component_id.eq(protocol_component_id)),
        )
        .set(schema::component_balance::valid_to.eq(ts))
        .execute(&mut conn)
        .await
        .expect("updating valid_to failed");

        db_fixtures::insert_component_balance(
            &mut conn,
            Balance::from(2 * 10u128.pow(18)).lpad(32, 0),
            Balance::from(10u128.pow(18)).lpad(32, 0),
            2e18,
            weth_id,
            txn_id,
            protocol_component_id,
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            &mut conn,
            Balance::from(3000 * 10u128.pow(6)).lpad(32, 0),
            Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
            3000.0 * 1e6,
            dai_id,
            txn_id,
            protocol_component_id,
            None,
        )
        .await;

        let exp: HashMap<_, _> = [
            (
                "state3",
                Bytes::from(WETH),
                ComponentBalance::new(
                    Bytes::from(WETH),
                    Balance::from(10u128.pow(18)).lpad(32, 0),
                    1e18,
                    Bytes::zero(32),
                    "state3",
                ),
            ),
            (
                "state3",
                Bytes::from(DAI),
                ComponentBalance::new(
                    Bytes::from(DAI),
                    Balance::from(2000 * 10u128.pow(18)).lpad(32, 0),
                    2000e18,
                    Bytes::zero(32),
                    "state3",
                ),
            ),
        ]
        .into_iter()
        .group_by(|e| e.0)
        .into_iter()
        .map(|(cid, group)| {
            (
                cid.to_owned(),
                group
                    .map(|(_, addr, bal)| (addr, bal))
                    .collect::<HashMap<_, _>>(),
            )
        })
        .collect();

        let res = gw
            .get_component_balances(
                &Chain::Ethereum,
                Some(&["state3"]),
                Some(&Version::from_block_number(Chain::Ethereum, 1)),
                &mut conn,
            )
            .await
            .expect("retrieving balances failed!");

        assert_eq!(res, exp);
    }

    #[tokio::test]
    async fn test_upsert_component_tvl() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let chain_id = gw
            .get_chain_id(&Chain::Ethereum)
            .unwrap();
        let exp = [("state1", 100.0), ("no_tvl", 1.0), ("state3", 1.0)]
            .into_iter()
            .map(|(id, tvl)| (id.to_owned(), tvl))
            .collect::<HashMap<_, _>>();

        let new_tvl = [("state1".to_owned(), 100.0), ("no_tvl".to_owned(), 1.0)]
            .into_iter()
            .collect::<HashMap<_, _>>();
        gw.upsert_component_tvl(&Chain::Ethereum, &new_tvl, &mut conn)
            .await
            .expect("upsert failed!");

        let tvl_values = schema::component_tvl::table
            .inner_join(schema::protocol_component::table)
            .select((schema::protocol_component::external_id, schema::component_tvl::tvl))
            .filter(schema::protocol_component::chain_id.eq(chain_id))
            .get_results::<(String, f64)>(&mut conn)
            .await
            .unwrap()
            .into_iter()
            .collect::<HashMap<_, _>>();

        assert_eq!(tvl_values, exp);
    }

    #[tokio::test]
    async fn test_get_protocol_systems() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = ["ambient", "zigzag"]
            .iter()
            .map(|s| s.to_string())
            .collect::<HashSet<_>>();

        let res = gw
            .get_protocol_systems(&Chain::Ethereum, None)
            .await
            .expect("retrieving protocol systems failed!");

        assert_eq!(res.total, Some(exp.len() as i64));
        assert_eq!(
            res.entity
                .into_iter()
                .collect::<HashSet<_>>(),
            exp
        );
    }

    #[tokio::test]
    async fn test_get_protocol_systems_with_pagination() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let all = ["ambient", "zigzag"];
        let exp = vec![all[0]];

        let res = gw
            .get_protocol_systems(
                &Chain::Ethereum,
                Some(&PaginationParams { page: 0, page_size: 1 }),
            )
            .await
            .expect("retrieving protocol systems failed!");

        assert_eq!(res.total, Some(all.len() as i64));
        assert_eq!(res.entity.iter().collect_vec(), exp);
    }

    #[tokio::test]
    async fn test_get_protocol_systems_chain_not_exist() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let res = gw
            .get_protocol_systems(&Chain::Arbitrum, None)
            .await;

        match res {
            Err(StorageError::NotFound(entity, value)) => {
                assert_eq!(entity, "Chain");
                assert_eq!(value, Chain::Arbitrum.to_string());
            }
            _ => panic!("Expected StorageError::NotFound, but got {res:?}"),
        }
    }

    #[tokio::test]
    async fn test_truncate_token_title() {
        let mut conn = setup_db().await;
        let (chain_id, _) = setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;

        let too_long_symbol = "🐶🐱🐰🦊🐻🐼🐨🐯🦁🐮🐷🐽🐸🐵🐔🐧🐦🐤🦅🦉🦇🐺🐗🐴🦄🐝🐛🪱🦋🐌🐞🐜🪰🪲🕷🦂🐢🐍🦎🦖🦕🐙🦑🦐🦞🦀🐡🐠🐟🐬🐳🐋🐊🐅🐆🦓🦍🦧🦣🐘🦛🦏🐪🐫🦒🦘🦬🐃🐂🐄🐎🐖🐏🐑🦙🐐🦌🐕🐩🦮🐕\u{200d}🦺🐈🐈\u{200d}⬛🐓🦤🦚🦜🦢🦩🕊🐇🦝🦨🦡🦫🦦🦥🐁🐀🐿🦔🐾🐉🐲🐶🐱🐰🦊🐻🐼🐨🐯🦁🐮🐷🐽🐸🐵🐔🐧🐦🐤🦅🦉🦇🐺🐗🐴🦄🐝🐛🪱🦋🐌🐞🐜🪰🪲🕷🦂🐢🐍🦎🦖🦕🐙🦑🦐🦞🦀🐡🐠🐟🐬🐳🐋🐊🐅🐆🦓🦍🦧🦣🐘🦛🦏🐪🐫🦒🦘🦬🐃🐂🐄🐎🐖🐏🐑🦙🐐🦌🐕🐩🦮🐕\u{200d}🦺🐈🐈\u{200d}⬛🐓🦤🦚🦜🦢🦩🕊🐇🦝🦨🦡🦫🦦🦥🐁🐀🐿🦔🐾🐉🐲🐶🐱🐰🦊🐻🐼🐨🐯🦁🐮🐷🐽🐸🐵🐔🐧🐦🐤🦅🦉🦇🐺🐗🐴🦄🐝🐛🪱🦋🐌🐞🐜🪰🪲🕷🦂🐢🐍🦎🦖🦕🐙🦑🦐🦞🦀".to_string();
        let tokens = [Token::new(
            &Bytes::from("0x052313a7af625b5a08fd3816ea0da1912ced8c8b"),
            &too_long_symbol,
            6,
            0,
            &[Some(64), None],
            Chain::Ethereum,
            100,
        )];

        gw.add_tokens(&tokens, &mut conn)
            .await
            .unwrap();

        let inserted_account = &orm::Account::by_address(
            &Bytes::from_str("0x052313a7af625b5a08fd3816ea0da1912ced8c8b".trim_start_matches("0x"))
                .expect("address ok"),
            chain_id,
            &mut conn,
        )
        .await
        .unwrap()[0];
        assert_eq!(inserted_account.title, "Ethereum_🐶🐱🐰🦊🐻🐼🐨🐯🦁🐮🐷🐽🐸🐵🐔🐧🐦🐤🦅🦉🦇🐺🐗🐴🦄🐝🐛🪱🦋🐌🐞🐜🪰🪲🕷🦂🐢🐍🦎🦖🦕🐙🦑🦐🦞🦀🐡🐠🐟🐬🐳🐋🐊🐅🐆🦓🦍🦧🦣🐘🦛".to_string());
    }

    #[tokio::test]
    async fn test_get_component_tvls() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let protocol_system = "ambient".to_string();
        let tvls = gw
            .get_component_tvls(
                &Chain::Ethereum,
                Some(protocol_system),
                Some(&["state1", "no_tvl", "not_exist", "state2"]),
                Some(&PaginationParams { page: 0, page_size: 10 }),
                &mut conn,
            )
            .await
            .expect("failed retrieving component tvl");

        assert_eq!(tvls.total.unwrap(), 1);
        assert_eq!(tvls.entity.get("state1"), Some(&2.0));
        assert!(!tvls.entity.contains_key("no_tvl")); // component not requested
        assert!(!tvls.entity.contains_key("not_exist")); // component does not exist
        assert!(!tvls.entity.contains_key("state2")); // component not in the requested
    }

    #[tokio::test]
    async fn test_get_component_tvls_with_system_only() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let protocol_system = "ambient".to_string();
        let tvls = gw
            .get_component_tvls(
                &Chain::Ethereum,
                Some(protocol_system),
                None,
                Some(&PaginationParams { page: 0, page_size: 10 }),
                &mut conn,
            )
            .await
            .expect("failed retrieving component tvl");

        assert_eq!(tvls.total.unwrap(), 2);
        assert_eq!(tvls.entity.get("state1"), Some(&2.0));
        assert_eq!(tvls.entity.get("state3"), Some(&1.0));
        assert!(!tvls.entity.contains_key("no_tvl"));
    }

    #[tokio::test]
    async fn test_get_component_tvls_with_component_only() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let tvls = gw
            .get_component_tvls(
                &Chain::Ethereum,
                None,
                Some(&["state1", "state3"]),
                Some(&PaginationParams { page: 0, page_size: 10 }),
                &mut conn,
            )
            .await
            .expect("failed retrieving component tvl");

        assert_eq!(tvls.total.unwrap(), 2);
        assert_eq!(tvls.entity.get("state1"), Some(&2.0));
        assert_eq!(tvls.entity.get("state3"), Some(&1.0));
    }

    #[tokio::test]
    async fn test_get_component_tvls_with_pagination() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let protocol_system = "ambient".to_string();
        let tvls = gw
            .get_component_tvls(
                &Chain::Ethereum,
                Some(protocol_system),
                Some(&["state1", "state3"]),
                Some(&PaginationParams { page: 0, page_size: 1 }),
                &mut conn,
            )
            .await
            .expect("failed retrieving component tvl");

        assert_eq!(tvls.total.unwrap(), 2);
        assert_eq!(tvls.entity.get("state1"), Some(&2.0));
        assert!(!tvls.entity.contains_key("state3"));
    }

    #[tokio::test]
    async fn test_ensure_token_with_price() {
        let mut conn = setup_db().await;
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;

        let native = Chain::Ethereum.native_token();
        let wrapped = Chain::Ethereum.wrapped_native_token();
        crate::postgres::ensure_token_with_price(chain_id, &native, &mut conn).await;
        crate::postgres::ensure_token_with_price(chain_id, &wrapped, &mut conn).await;

        let gw = EVMGateway::from_connection(&mut conn).await;
        let prices = gw
            .get_token_prices(&Chain::Ethereum, &mut conn)
            .await
            .expect("get prices failed");

        let expected_price = 10.0_f64.powi(18);
        assert_eq!(prices.get(&native.address).copied(), Some(expected_price));
        assert_eq!(prices.get(&wrapped.address).copied(), Some(expected_price));
    }

    #[tokio::test]
    async fn test_ensure_token_with_price_idempotent() {
        let mut conn = setup_db().await;
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;

        let native = Chain::Ethereum.native_token();
        crate::postgres::ensure_token_with_price(chain_id, &native, &mut conn).await;
        crate::postgres::ensure_token_with_price(chain_id, &native, &mut conn).await;

        let gw = EVMGateway::from_connection(&mut conn).await;
        let prices = gw
            .get_token_prices(&Chain::Ethereum, &mut conn)
            .await
            .expect("get prices failed");

        let expected_price = 10.0_f64.powi(18);
        assert_eq!(prices.get(&native.address).copied(), Some(expected_price));
    }
}
