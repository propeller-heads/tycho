use std::collections::HashMap;

use diesel::{pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use itertools::Itertools;
use tycho_common::{
    models::{protocol::ProtocolComponent, Address, Chain, ChangeType, PaginationParams, StoreVal},
    storage::{StorageError, WithTotal},
};

use crate::postgres::{
    instance::{
        active::{active_instance, active_instance_ids},
        gateway::PostgresGatewayNew,
        orm as iorm,
        schema::tycho::{
            account, component_tvl, protocol_component, protocol_component_holds_contract,
            protocol_component_holds_token, protocol_instance,
        },
        InstanceId,
    },
    schema::protocol_system,
    storage_error_from_diesel, PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the components that match all given filters, read from the active instance of each
    /// protocol system, and the number of matching components on all pages.
    ///
    /// - `system`: only components of this protocol system. `NotFound` for an unknown name. A
    ///   protocol system without an active instance has no components.
    /// - `ids`: only components with these external ids. Needs `system`, because an external id can
    ///   repeat across protocol systems: `ids` without `system` is `Unsupported`.
    /// - `min_tvl`: only components with a TVL greater than this value. A component without a TVL
    ///   row never matches.
    /// - `pagination_params`: the page to return, ordered by instance and component id. Without it,
    ///   all matching components are returned, in no particular order.
    ///
    /// Soft-deleted components are returned too. `NotFound` for an unknown chain.
    pub async fn get_protocol_components(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        min_tvl: Option<f64>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<ProtocolComponent>>, StorageError> {
        self.get_chain_id(chain)?;
        let (rows, total) = match (system, ids) {
            (Some(system), ids) => {
                self.get_protocol_system_id(&system)?;
                load_components_of(&system, ids, min_tvl, pagination_params, conn).await?
            }
            (None, Some(_)) => {
                return Err(StorageError::Unsupported(
                    "component ids need a protocol system: a pool id can repeat across protocols"
                        .to_string(),
                ));
            }
            (None, None) => load_active_components(min_tvl, pagination_params, conn).await?,
        };
        let entity = self
            .build_components(rows, chain, conn)
            .await?;
        Ok(WithTotal { entity, total: Some(total) })
    }

    /// Returns the TVL of the components of `system`'s active instance, by external id, and the
    /// number of matching components on all pages.
    ///
    /// - `system`: the protocol system. Required, because the answer is keyed by external id and an
    ///   external id can repeat across protocol systems: `None` is `Unsupported`. `NotFound` for an
    ///   unknown name. A protocol system without an active instance has no TVL.
    /// - `component_ids`: only components with these external ids.
    /// - `pagination_params`: the page to return, in component id order.
    ///
    /// Components without a TVL row are absent. `NotFound` for an unknown chain.
    pub async fn get_component_tvls(
        &self,
        chain: &Chain,
        system: Option<String>,
        component_ids: Option<&[&str]>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<HashMap<String, f64>>, StorageError> {
        if !self.chain_id_cache.value_exists(chain) {
            return Err(StorageError::NotFound("Chain".to_string(), chain.to_string()));
        }
        let Some(system) = system else {
            return Err(StorageError::Unsupported(
                "component TVL needs a protocol system: the response is keyed by pool id"
                    .to_string(),
            ));
        };
        self.get_protocol_system_id(&system)?;
        let Some(instance) = active_instance(conn, &system).await? else {
            return Ok(WithTotal { entity: HashMap::new(), total: Some(0) });
        };

        let filtered = || {
            let mut query = component_tvl::table
                .inner_join(
                    protocol_component::table.on(protocol_component::instance_id
                        .eq(component_tvl::instance_id)
                        .and(protocol_component::id.eq(component_tvl::protocol_component_id))),
                )
                .filter(component_tvl::instance_id.eq(instance))
                .into_boxed::<Pg>();
            if let Some(ids) = component_ids {
                query = query.filter(protocol_component::external_id.eq_any(ids));
            }
            query
        };

        let total = filtered()
            .count()
            .get_result::<i64>(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut query = filtered().select((protocol_component::external_id, component_tvl::tvl));
        if let Some(pagination) = pagination_params {
            query = query
                .order_by(component_tvl::protocol_component_id)
                .limit(pagination.page_size)
                .offset(pagination.offset());
        }
        let rows: Vec<(String, f64)> = query.load(conn).await.map_err(|err| {
            let id_hint = component_ids
                .and_then(|ids| ids.first().copied())
                .unwrap_or_default();
            storage_error_from_diesel(err, "ComponentTVL", id_hint, None)
        })?;

        Ok(WithTotal { entity: rows.into_iter().collect(), total: Some(total) })
    }

    /// Builds the models of component rows, each with the name of its protocol system. Tokens and
    /// contracts come from the row's own instance; tokens are in `token_index` order.
    ///
    /// Panics if a component has no token row, as the gateway on the old tables does.
    async fn build_components(
        &self,
        rows: Vec<(iorm::ProtocolComponent, String)>,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ProtocolComponent>, StorageError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let instance_ids: Vec<InstanceId> = rows
            .iter()
            .map(|(row, _)| row.instance_id)
            .unique()
            .collect();
        let component_ids: Vec<i64> = rows
            .iter()
            .map(|(row, _)| row.id)
            .unique()
            .collect();

        let token_rows: Vec<(InstanceId, i64, Address)> = protocol_component_holds_token::table
            .filter(
                protocol_component_holds_token::instance_id
                    .eq_any(instance_ids.clone())
                    .and(
                        protocol_component_holds_token::protocol_component_id
                            .eq_any(component_ids.clone()),
                    ),
            )
            .order_by((
                protocol_component_holds_token::instance_id,
                protocol_component_holds_token::protocol_component_id,
                protocol_component_holds_token::token_index,
            ))
            .select((
                protocol_component_holds_token::instance_id,
                protocol_component_holds_token::protocol_component_id,
                protocol_component_holds_token::token,
            ))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let contract_rows: Vec<(InstanceId, i64, Address)> =
            protocol_component_holds_contract::table
                .inner_join(
                    account::table.on(account::instance_id
                        .eq(protocol_component_holds_contract::instance_id)
                        .and(account::id.eq(protocol_component_holds_contract::account_id))),
                )
                // Postgres does not carry `= ANY` across the join to `account`, so without this
                // filter it plans and locks every account leaf.
                .filter(
                    protocol_component_holds_contract::instance_id
                        .eq_any(instance_ids.clone())
                        .and(
                            protocol_component_holds_contract::protocol_component_id
                                .eq_any(component_ids),
                        )
                        .and(account::instance_id.eq_any(instance_ids)),
                )
                .select((
                    protocol_component_holds_contract::instance_id,
                    protocol_component_holds_contract::protocol_component_id,
                    account::address,
                ))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;

        let mut tokens = group_by_component(token_rows);
        let mut contracts = group_by_component(contract_rows);

        rows.into_iter()
            .map(|(row, system)| {
                let key = (row.instance_id, row.id);
                let tokens = tokens
                    .remove(&key)
                    // We expect all protocol components to have tokens.
                    .expect("Could not find Tokens for Protocol Component.");
                let contracts = contracts
                    .remove(&key)
                    .unwrap_or_default();
                let static_attributes: HashMap<String, StoreVal> = match row.attributes {
                    Some(value) => serde_json::from_value(value).map_err(|e| {
                        StorageError::DecodeError(format!(
                            "Failed to decode static attributes: {e}"
                        ))
                    })?,
                    None => HashMap::new(),
                };
                Ok(ProtocolComponent::new(
                    &row.external_id,
                    &system,
                    &row.protocol_type,
                    *chain,
                    tokens,
                    contracts,
                    static_attributes,
                    ChangeType::Creation,
                    row.creation_tx_hash,
                    row.created_at,
                ))
            })
            .collect()
    }
}

/// Loads the matching component rows of the active instance of `system`, each with `system` as
/// its protocol system name, and counts the matches on all pages.
async fn load_components_of(
    system: &str,
    ids: Option<&[&str]>,
    min_tvl: Option<f64>,
    pagination_params: Option<&PaginationParams>,
    conn: &mut AsyncPgConnection,
) -> Result<(Vec<(iorm::ProtocolComponent, String)>, i64), StorageError> {
    let Some(instance) = active_instance(conn, system).await? else {
        return Ok((Vec::new(), 0));
    };
    let filtered = || {
        let mut query = protocol_component::table
            .left_join(
                component_tvl::table.on(component_tvl::instance_id
                    .eq(protocol_component::instance_id)
                    .and(component_tvl::protocol_component_id.eq(protocol_component::id))),
            )
            .filter(protocol_component::instance_id.eq(instance))
            .into_boxed::<Pg>();
        if let Some(ids) = ids {
            query = query.filter(protocol_component::external_id.eq_any(ids));
        }
        if let Some(min_tvl) = min_tvl {
            query = query.filter(component_tvl::tvl.gt(min_tvl));
        }
        query
    };

    let total = filtered()
        .count()
        .get_result::<i64>(conn)
        .await
        .map_err(PostgresError::from)?;

    let mut query = filtered().select(iorm::ProtocolComponent::as_select());
    if let Some(pagination) = pagination_params {
        query = query
            .order_by((protocol_component::instance_id, protocol_component::id))
            .limit(pagination.page_size)
            .offset(pagination.offset());
    }
    let rows = query
        .load::<iorm::ProtocolComponent>(conn)
        .await
        .map_err(PostgresError::from)?
        .into_iter()
        .map(|row| (row, system.to_string()))
        .collect();
    Ok((rows, total))
}

/// Loads the matching component rows of every active instance, each with the name of its
/// protocol system, and counts the matches on all pages.
async fn load_active_components(
    min_tvl: Option<f64>,
    pagination_params: Option<&PaginationParams>,
    conn: &mut AsyncPgConnection,
) -> Result<(Vec<(iorm::ProtocolComponent, String)>, i64), StorageError> {
    // The filter's subselect reads `protocol_system` too; Diesel needs the outer one aliased.
    let system = diesel::alias!(protocol_system as system);
    let filtered = || {
        let mut query = protocol_component::table
            .inner_join(
                protocol_instance::table
                    .on(protocol_component::instance_id.eq(protocol_instance::id)),
            )
            .inner_join(
                system.on(system
                    .field(protocol_system::id)
                    .eq(protocol_instance::protocol_system_id)),
            )
            .left_join(
                component_tvl::table.on(component_tvl::instance_id
                    .eq(protocol_component::instance_id)
                    .and(component_tvl::protocol_component_id.eq(protocol_component::id))),
            )
            .filter(protocol_component::instance_id.eq_any(active_instance_ids()))
            .into_boxed::<Pg>();
        if let Some(min_tvl) = min_tvl {
            query = query.filter(component_tvl::tvl.gt(min_tvl));
        }
        query
    };

    let total = filtered()
        .count()
        .get_result::<i64>(conn)
        .await
        .map_err(PostgresError::from)?;

    let mut query = filtered()
        .select((iorm::ProtocolComponent::as_select(), system.field(protocol_system::name)));
    if let Some(pagination) = pagination_params {
        query = query
            .order_by((protocol_component::instance_id, protocol_component::id))
            .limit(pagination.page_size)
            .offset(pagination.offset());
    }
    let rows = query
        .load::<(iorm::ProtocolComponent, String)>(conn)
        .await
        .map_err(PostgresError::from)?;
    Ok((rows, total))
}

/// Groups `(instance, component, address)` rows by component, keeping the row order.
fn group_by_component(
    rows: Vec<(InstanceId, i64, Address)>,
) -> HashMap<(InstanceId, i64), Vec<Address>> {
    let mut grouped: HashMap<(InstanceId, i64), Vec<Address>> = HashMap::new();
    for (instance_id, component_id, address) in rows {
        grouped
            .entry((instance_id, component_id))
            .or_default()
            .push(address);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::NaiveDate;
    use diesel::{
        prelude::*,
        sql_query,
        sql_types::{BigInt, Int4},
    };
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::{
        models::{protocol::ProtocolComponent, Chain, ChangeType, PaginationParams},
        storage::StorageError,
        Bytes,
    };

    use crate::postgres::{
        db_fixtures,
        instance::{gateway::PostgresGatewayNew, orm as iorm, schema::tycho, InstanceId},
        testing::instance_test_connection,
    };

    struct Setup {
        gateway: PostgresGatewayNew,
        curve_system_id: i64,
        curve: InstanceId,
        uniswap_v4: InstanceId,
    }

    /// "vm:curve" and "uniswap_v4" on ethereum, each pointing to an instance.
    async fn setup(conn: &mut AsyncPgConnection) -> Setup {
        db_fixtures::insert_chain(conn, "ethereum").await;
        let curve_system_id =
            db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
        let uniswap_v4_system_id =
            db_fixtures::insert_protocol_system(conn, "uniswap_v4".to_string()).await;
        let curve = db_fixtures::insert_instance(conn, curve_system_id, "0.0.1").await;
        let uniswap_v4 = db_fixtures::insert_instance(conn, uniswap_v4_system_id, "0.0.1").await;
        db_fixtures::set_active_instance(conn, curve_system_id, Some(curve)).await;
        db_fixtures::set_active_instance(conn, uniswap_v4_system_id, Some(uniswap_v4)).await;
        let gateway = PostgresGatewayNew::from_connection(conn).await;
        Setup { gateway, curve_system_id, curve, uniswap_v4 }
    }

    fn b(hex: &str) -> Bytes {
        Bytes::from(hex)
    }

    async fn components(
        gateway: &PostgresGatewayNew,
        conn: &mut AsyncPgConnection,
        system: Option<&str>,
        ids: Option<&[&str]>,
        min_tvl: Option<f64>,
        pagination: Option<&PaginationParams>,
    ) -> Result<(Vec<ProtocolComponent>, Option<i64>), StorageError> {
        let page = gateway
            .get_protocol_components(
                &Chain::Ethereum,
                system.map(str::to_string),
                ids,
                min_tvl,
                pagination,
                conn,
            )
            .await?;
        let mut entity = page.entity;
        if pagination.is_none() {
            entity.sort_by(|a, b| a.id.cmp(&b.id));
        }
        Ok((entity, page.total))
    }

    async fn tvls(
        gateway: &PostgresGatewayNew,
        conn: &mut AsyncPgConnection,
        system: Option<&str>,
        ids: Option<&[&str]>,
    ) -> Result<(HashMap<String, f64>, Option<i64>), StorageError> {
        let page = gateway
            .get_component_tvls(&Chain::Ethereum, system.map(str::to_string), ids, None, conn)
            .await?;
        Ok((page.entity, page.total))
    }

    /// Inserts a component with an explicit id, as an import does, and one token.
    async fn insert_component_with_id(
        conn: &mut AsyncPgConnection,
        instance_id: InstanceId,
        id: i64,
        external_id: &str,
    ) {
        sql_query(
            "INSERT INTO tycho.protocol_component
                 (instance_id, id, external_id, protocol_type, created_at, creation_block,
                  creation_tx_hash)
             VALUES ($1, $2, $3, 'pool', '2020-01-01', 1, '\\x01')",
        )
        .bind::<Int4, _>(instance_id)
        .bind::<BigInt, _>(id)
        .bind::<diesel::sql_types::Text, _>(external_id)
        .execute(conn)
        .await
        .expect("inserting the component failed");
        diesel::insert_into(tycho::protocol_component_holds_token::table)
            .values(iorm::NewProtocolComponentHoldsToken {
                instance_id,
                protocol_component_id: id,
                token: b("0x01"),
                token_index: 0,
            })
            .execute(conn)
            .await
            .expect("inserting the token link failed");
    }

    fn external_ids(components: &[ProtocolComponent]) -> Vec<&str> {
        components
            .iter()
            .map(|c| c.id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn test_components_of_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(
            &mut conn,
            s.curve,
            "a",
            &[b("0x02"), b("0x01")],
        )
        .await;
        db_fixtures::insert_instance_contract_link(&mut conn, s.curve, a, b("0xc1")).await;
        let c = db_fixtures::insert_instance_component(
            &mut conn,
            s.curve,
            "b",
            &[b("0x03"), b("0x04")],
        )
        .await;
        db_fixtures::insert_instance_contract_link(&mut conn, s.curve, c, b("0xc2")).await;
        db_fixtures::insert_instance_component(&mut conn, s.uniswap_v4, "u", &[b("0x05")]).await;
        diesel::update(tycho::protocol_component::table.find((s.curve, a)))
            .set(tycho::protocol_component::attributes.eq(serde_json::json!({"fee": "0x64"})))
            .execute(&mut conn)
            .await
            .expect("setting the attributes failed");

        let (found, total) = components(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .expect("reading the components failed");

        let created_at = NaiveDate::from_ymd_opt(2020, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");
        let expected = vec![
            ProtocolComponent::new(
                "a",
                "vm:curve",
                "pool",
                Chain::Ethereum,
                vec![b("0x02"), b("0x01")],
                vec![b("0xc1")],
                HashMap::from([("fee".to_string(), b("0x64"))]),
                ChangeType::Creation,
                b("0x01"),
                created_at,
            ),
            ProtocolComponent::new(
                "b",
                "vm:curve",
                "pool",
                Chain::Ethereum,
                vec![b("0x03"), b("0x04")],
                vec![b("0xc2")],
                HashMap::new(),
                ChangeType::Creation,
                b("0x01"),
                created_at,
            ),
        ];
        assert_eq!(found, expected);
        assert_eq!(total, Some(2));
    }

    #[tokio::test]
    async fn test_components_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        let ready = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        db_fixtures::insert_instance_component(&mut conn, ready, "old", &[b("0x01")]).await;

        let (found, total) = components(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .expect("reading the components failed");

        assert_eq!(external_ids(&found), vec!["a"]);
        assert_eq!(total, Some(1));
    }

    #[tokio::test]
    async fn test_component_links_come_from_its_own_instance() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let component_id =
            db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        let account_id =
            db_fixtures::insert_instance_contract_link(&mut conn, s.curve, component_id, b("0xc1"))
                .await;
        // An import keeps the source database's ids, so uniswap_v4 holds the same component and
        // account ids.
        sql_query(
            "INSERT INTO tycho.protocol_component
                 (instance_id, id, external_id, protocol_type, created_at, creation_block,
                  creation_tx_hash)
             VALUES ($1, $2, 'u', 'pool', '2020-01-01', 1, '\\x01')",
        )
        .bind::<Int4, _>(s.uniswap_v4)
        .bind::<BigInt, _>(component_id)
        .execute(&mut conn)
        .await
        .expect("inserting the imported component failed");
        sql_query(
            "INSERT INTO tycho.account (instance_id, id, address, title)
             VALUES ($1, $2, '\\xc2', '')",
        )
        .bind::<Int4, _>(s.uniswap_v4)
        .bind::<BigInt, _>(account_id)
        .execute(&mut conn)
        .await
        .expect("inserting the imported account failed");
        diesel::insert_into(tycho::protocol_component_holds_token::table)
            .values(iorm::NewProtocolComponentHoldsToken {
                instance_id: s.uniswap_v4,
                protocol_component_id: component_id,
                token: b("0x02"),
                token_index: 0,
            })
            .execute(&mut conn)
            .await
            .expect("inserting the imported token link failed");
        diesel::insert_into(tycho::protocol_component_holds_contract::table)
            .values(iorm::NewProtocolComponentHoldsContract {
                instance_id: s.uniswap_v4,
                protocol_component_id: component_id,
                account_id,
            })
            .execute(&mut conn)
            .await
            .expect("inserting the imported contract link failed");

        let (curve, _) = components(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .expect("reading the vm:curve components failed");
        let (uniswap_v4, _) =
            components(&s.gateway, &mut conn, Some("uniswap_v4"), None, None, None)
                .await
                .expect("reading the uniswap_v4 components failed");

        assert_eq!(external_ids(&curve), vec!["a"]);
        assert_eq!(curve[0].tokens, vec![b("0x01")]);
        assert_eq!(curve[0].contract_addresses, vec![b("0xc1")]);
        assert_eq!(external_ids(&uniswap_v4), vec!["u"]);
        assert_eq!(uniswap_v4[0].tokens, vec![b("0x02")]);
        assert_eq!(uniswap_v4[0].contract_addresses, vec![b("0xc2")]);

        // Without a protocol, one batch holds both instances.
        let (all, _) = components(&s.gateway, &mut conn, None, None, None, None)
            .await
            .expect("reading all components failed");
        let links: Vec<(&str, &[Bytes], &[Bytes])> = all
            .iter()
            .map(|c| (c.id.as_str(), c.tokens.as_slice(), c.contract_addresses.as_slice()))
            .collect();
        assert_eq!(
            links,
            vec![
                ("a", &[b("0x01")][..], &[b("0xc1")][..]),
                ("u", &[b("0x02")][..], &[b("0xc2")][..]),
            ]
        );
    }

    #[tokio::test]
    async fn test_components_min_tvl_and_paging() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        // In insert (id) order, the matches are c, e, d: neither external id nor TVL order.
        for (external_id, tvl) in [("c", 4.0), ("a", 1.0), ("e", 3.0), ("b", 2.0), ("d", 5.0)] {
            let id = db_fixtures::insert_instance_component(
                &mut conn,
                s.curve,
                external_id,
                &[b("0x01")],
            )
            .await;
            db_fixtures::insert_instance_tvl(&mut conn, s.curve, id, tvl).await;
        }

        let first = PaginationParams::new(0, 2);
        let (page, total) =
            components(&s.gateway, &mut conn, Some("vm:curve"), None, Some(2.5), Some(&first))
                .await
                .expect("reading the first page failed");
        assert_eq!(external_ids(&page), vec!["c", "e"]);
        assert_eq!(total, Some(3));

        let second = PaginationParams::new(1, 2);
        let (page, total) =
            components(&s.gateway, &mut conn, Some("vm:curve"), None, Some(2.5), Some(&second))
                .await
                .expect("reading the second page failed");
        assert_eq!(external_ids(&page), vec!["d"]);
        assert_eq!(total, Some(3));
    }

    #[tokio::test]
    async fn test_components_with_ids_and_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "b", &[b("0x01")]).await;

        let (found, total) = components(
            &s.gateway,
            &mut conn,
            Some("vm:curve"),
            Some(&["a", "missing"]),
            None,
            None,
        )
        .await
        .expect("reading the components failed");

        assert_eq!(external_ids(&found), vec!["a"]);
        assert_eq!(total, Some(1));
    }

    #[tokio::test]
    async fn test_components_of_unknown_protocol_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        let result = components(&s.gateway, &mut conn, Some("unknown"), None, None, None).await;

        assert!(matches!(result, Err(StorageError::NotFound(..))), "got {result:?}");
    }

    #[tokio::test]
    async fn test_components_of_protocol_without_active_instance_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;

        let (found, total) = components(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .expect("reading the components failed");

        assert!(found.is_empty());
        assert_eq!(total, Some(0));
    }

    #[tokio::test]
    async fn test_deleted_components_are_returned() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id =
            db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        diesel::update(tycho::protocol_component::table.find((s.curve, id)))
            .set((
                tycho::protocol_component::deleted_at.eq(chrono::Utc::now().naive_utc()),
                tycho::protocol_component::deletion_tx_hash.eq(b("0x02")),
            ))
            .execute(&mut conn)
            .await
            .expect("deleting the component failed");

        let (found, _) = components(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .expect("reading the components failed");

        assert_eq!(external_ids(&found), vec!["a"]);
    }

    #[tokio::test]
    async fn test_components_without_filters_read_active_instances_only() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_component(&mut conn, s.uniswap_v4, "u", &[b("0x02")]).await;
        let ready = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        db_fixtures::insert_instance_component(&mut conn, ready, "old", &[b("0x03")]).await;

        let (found, total) = components(&s.gateway, &mut conn, None, None, None, None)
            .await
            .expect("reading the components failed");

        let found: Vec<(&str, &str, &[Bytes])> = found
            .iter()
            .map(|c| (c.id.as_str(), c.protocol_system.as_str(), c.tokens.as_slice()))
            .collect();
        assert_eq!(
            found,
            vec![("a", "vm:curve", &[b("0x01")][..]), ("u", "uniswap_v4", &[b("0x02")][..])]
        );
        assert_eq!(total, Some(2));
    }

    #[tokio::test]
    async fn test_components_with_same_pool_id_stay_apart() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "0xpool", &[b("0x01")]).await;
        db_fixtures::insert_instance_component(&mut conn, s.uniswap_v4, "0xpool", &[b("0x02")])
            .await;
        let ids: &[&str] = &["0xpool"];

        for (system, token) in [("vm:curve", b("0x01")), ("uniswap_v4", b("0x02"))] {
            let (found, total) =
                components(&s.gateway, &mut conn, Some(system), Some(ids), None, None)
                    .await
                    .expect("reading the components failed");
            assert_eq!(found.len(), 1, "{system}");
            assert_eq!(found[0].protocol_system, system);
            assert_eq!(found[0].tokens, vec![token]);
            assert_eq!(total, Some(1));
        }

        let result = components(&s.gateway, &mut conn, None, Some(ids), None, None).await;
        assert!(matches!(result, Err(StorageError::Unsupported(_))), "got {result:?}");
    }

    #[tokio::test]
    async fn test_tvls_of_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        for (instance, external_id, tvl) in
            [(s.curve, "a", 1.0), (s.curve, "b", 2.0), (s.uniswap_v4, "u", 3.0)]
        {
            let id = db_fixtures::insert_instance_component(
                &mut conn,
                instance,
                external_id,
                &[b("0x01")],
            )
            .await;
            db_fixtures::insert_instance_tvl(&mut conn, instance, id, tvl).await;
        }

        let (found, total) = tvls(&s.gateway, &mut conn, Some("vm:curve"), None)
            .await
            .expect("reading the TVLs failed");

        assert_eq!(found, HashMap::from([("a".to_string(), 1.0), ("b".to_string(), 2.0)]));
        assert_eq!(total, Some(2));
    }

    #[tokio::test]
    async fn test_tvls_with_same_pool_id_stay_apart() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        for (instance, tvl) in [(s.curve, 1.0), (s.uniswap_v4, 2.0)] {
            let id =
                db_fixtures::insert_instance_component(&mut conn, instance, "0xpool", &[b("0x01")])
                    .await;
            db_fixtures::insert_instance_tvl(&mut conn, instance, id, tvl).await;
        }

        for (system, tvl) in [("vm:curve", 1.0), ("uniswap_v4", 2.0)] {
            for ids in [None, Some(&["0xpool"][..])] {
                let (found, total) = tvls(&s.gateway, &mut conn, Some(system), ids)
                    .await
                    .expect("reading the TVLs failed");
                assert_eq!(found, HashMap::from([("0xpool".to_string(), tvl)]), "{system}");
                assert_eq!(total, Some(1));
            }
        }
    }

    #[tokio::test]
    async fn test_tvls_need_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        let result = tvls(&s.gateway, &mut conn, None, None).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "got {result:?}");
    }

    #[tokio::test]
    async fn test_tvls_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id =
            db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_tvl(&mut conn, s.curve, id, 1.0).await;
        let ready = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        let id =
            db_fixtures::insert_instance_component(&mut conn, ready, "old", &[b("0x01")]).await;
        db_fixtures::insert_instance_tvl(&mut conn, ready, id, 5.0).await;

        let (found, total) = tvls(&s.gateway, &mut conn, Some("vm:curve"), None)
            .await
            .expect("reading the TVLs failed");

        assert_eq!(found, HashMap::from([("a".to_string(), 1.0)]));
        assert_eq!(total, Some(1));
    }

    #[tokio::test]
    async fn test_tvls_of_protocol_without_active_instance_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id =
            db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_tvl(&mut conn, s.curve, id, 1.0).await;
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;

        let (found, total) = tvls(&s.gateway, &mut conn, Some("vm:curve"), None)
            .await
            .expect("reading the TVLs failed");

        assert!(found.is_empty());
        assert_eq!(total, Some(0));
    }

    #[tokio::test]
    async fn test_components_without_filters_page_by_instance_then_id() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        assert!(s.curve < s.uniswap_v4);
        // Ids interleave across the instances, and external ids sort differently, so only
        // (instance_id, id) order gives p, q | m, n. "r" is below the minimum TVL.
        for (instance, id, external_id, tvl) in [
            (s.curve, 9_000_000_001, "p", 10.0),
            (s.uniswap_v4, 9_000_000_002, "m", 10.0),
            (s.curve, 9_000_000_003, "q", 10.0),
            (s.uniswap_v4, 9_000_000_004, "n", 10.0),
            (s.curve, 9_000_000_005, "r", 0.1),
        ] {
            insert_component_with_id(&mut conn, instance, id, external_id).await;
            db_fixtures::insert_instance_tvl(&mut conn, instance, id, tvl).await;
        }

        for (page, expected) in [(0, vec!["p", "q"]), (1, vec!["m", "n"])] {
            let pagination = PaginationParams::new(page, 2);
            let (found, total) =
                components(&s.gateway, &mut conn, None, None, Some(1.0), Some(&pagination))
                    .await
                    .expect("reading the page failed");
            assert_eq!(external_ids(&found), expected, "page {page}");
            assert_eq!(total, Some(4));
        }
    }
}
