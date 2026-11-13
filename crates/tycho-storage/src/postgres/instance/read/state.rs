use std::collections::HashMap;

use diesel::{pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{
        protocol::{ComponentBalance, ProtocolComponentState},
        Address, Balance, Chain, ComponentId, PaginationParams,
    },
    storage::{StorageError, Version, WithTotal},
    Bytes,
};

use super::version::check_current;
use crate::postgres::{
    instance::{
        active::active_instance,
        gateway::PostgresGatewayNew,
        orm as iorm,
        schema::tycho::{component_balance, protocol_component, protocol_state},
        InstanceId,
    },
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the token balances of the components of `system`'s active instance, by external id
    /// and token address.
    ///
    /// - `system`: the protocol system. Required, because the answer is keyed by external id and an
    ///   external id can repeat across protocol systems: `None` is `Unsupported`. An unknown name,
    ///   or a protocol system without an active instance, has no balances.
    /// - `ids`: only components with these external ids.
    /// - `at`: the version. `Unsupported` for a version before the head of the instance.
    ///
    /// Components without balances are absent. `modify_tx` is always 32 zero bytes. `NotFound` for
    /// an unknown chain.
    pub async fn get_component_balances(
        &self,
        chain: &Chain,
        system: Option<&str>,
        ids: Option<&[&str]>,
        at: Option<&Version>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<ComponentId, HashMap<Address, ComponentBalance>>, StorageError> {
        self.get_chain_id(chain)?;
        let Some(system) = system else {
            return Err(StorageError::Unsupported(
                "component balances need a protocol system: the answer is keyed by pool id"
                    .to_string(),
            ));
        };
        let Some(instance) = active_instance(conn, system).await? else {
            return Ok(HashMap::new());
        };
        check_current(conn, instance, at).await?;

        let mut query = component_balance::table
            .inner_join(
                protocol_component::table.on(protocol_component::instance_id
                    .eq(component_balance::instance_id)
                    .and(protocol_component::id.eq(component_balance::protocol_component_id))),
            )
            .filter(component_balance::instance_id.eq(instance))
            .filter(protocol_component::instance_id.eq(instance))
            .select((protocol_component::external_id, iorm::ComponentBalance::as_select()))
            .into_boxed::<Pg>();
        if let Some(ids) = ids {
            query = query.filter(protocol_component::external_id.eq_any(ids));
        }
        let rows: Vec<(String, iorm::ComponentBalance)> = query
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut balances: HashMap<ComponentId, HashMap<Address, ComponentBalance>> = HashMap::new();
        for (external_id, row) in rows {
            let balance = ComponentBalance::new(
                row.token.clone(),
                row.new_balance,
                row.balance_float,
                Bytes::zero(32),
                &external_id,
            );
            balances
                .entry(external_id)
                .or_default()
                .insert(row.token, balance);
        }
        Ok(balances)
    }

    /// Returns the state of the components of the active instance of each protocol system, and,
    /// with paging, the number of matching components on all pages.
    ///
    /// - `at`: the version. `Unsupported` for a version before the head of the instance.
    /// - `system`: only components of this protocol system. An unknown name, or a protocol system
    ///   without an active instance, has no states.
    /// - `ids`: only components with these external ids.
    /// - `retrieve_balances`: add the token balances of each component.
    /// - `pagination_params`: the page of components to return, in component id order. Without it,
    ///   all matching components are returned, in no particular order, and the total is `None`.
    ///
    /// A component with balances but no state attributes is returned with empty attributes. A
    /// component with neither is absent. `NotFound` for an unknown chain.
    #[expect(clippy::too_many_arguments, reason = "one argument per query filter")]
    pub async fn get_protocol_states(
        &self,
        chain: &Chain,
        at: Option<Version>,
        system: Option<String>,
        ids: Option<&[&str]>,
        retrieve_balances: bool,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError> {
        self.get_chain_id(chain)?;
        match system {
            Some(system) => {
                states_of(&system, ids, at.as_ref(), retrieve_balances, pagination_params, conn)
                    .await
            }
            None => Err(StorageError::Unsupported(
                "protocol states without a protocol system are not supported yet".to_string(),
            )),
        }
    }
}

/// Reads the states of the matching components of the active instance of `system`.
async fn states_of(
    system: &str,
    ids: Option<&[&str]>,
    at: Option<&Version>,
    retrieve_balances: bool,
    pagination_params: Option<&PaginationParams>,
    conn: &mut AsyncPgConnection,
) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError> {
    let Some(instance) = active_instance(conn, system).await? else {
        return Ok(WithTotal { entity: Vec::new(), total: Some(0) });
    };
    check_current(conn, instance, at).await?;

    let filtered = || {
        let mut query = protocol_component::table
            .filter(protocol_component::instance_id.eq(instance))
            .into_boxed::<Pg>();
        if let Some(ids) = ids {
            query = query.filter(protocol_component::external_id.eq_any(ids));
        }
        query
    };
    let (components, total) = match pagination_params {
        Some(pagination) => {
            let total = filtered()
                .count()
                .get_result::<i64>(conn)
                .await
                .map_err(PostgresError::from)?;
            let components: Vec<(i64, String)> = filtered()
                .order_by(protocol_component::id)
                .limit(pagination.page_size)
                .offset(pagination.offset())
                .select((protocol_component::id, protocol_component::external_id))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;
            (components, Some(total))
        }
        None => {
            let components: Vec<(i64, String)> = filtered()
                .select((protocol_component::id, protocol_component::external_id))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;
            (components, None)
        }
    };
    // Without a page or ids, the read covers the whole instance: filtering on every component id
    // would only make the query larger.
    let page_ids: Option<Vec<i64>> = (pagination_params.is_some() || ids.is_some()).then(|| {
        components
            .iter()
            .map(|(id, _)| *id)
            .collect()
    });

    let mut state_query = protocol_state::table
        .filter(protocol_state::instance_id.eq(instance))
        .select((
            protocol_state::instance_id,
            protocol_state::protocol_component_id,
            protocol_state::attribute_name,
            protocol_state::attribute_value,
        ))
        .into_boxed::<Pg>();
    if let Some(page_ids) = &page_ids {
        state_query = state_query.filter(protocol_state::protocol_component_id.eq_any(page_ids));
    }
    let state_rows: Vec<StateRow> = state_query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

    let balances = if retrieve_balances {
        load_balances(conn, &[instance], page_ids.as_deref()).await?
    } else {
        HashMap::new()
    };

    let components = components
        .into_iter()
        .map(|(id, external_id)| ((instance, id), external_id))
        .collect();
    Ok(WithTotal { entity: build_states(components, state_rows, balances), total })
}

type StateRow = (InstanceId, i64, String, Bytes);

/// Loads the token balances of the components of `instances`, by instance and component id.
/// `component_ids`: only these components; `None` for all of them.
async fn load_balances(
    conn: &mut AsyncPgConnection,
    instances: &[InstanceId],
    component_ids: Option<&[i64]>,
) -> Result<HashMap<(InstanceId, i64), HashMap<Address, Balance>>, StorageError> {
    let mut query = component_balance::table
        .filter(component_balance::instance_id.eq_any(instances))
        .select((
            component_balance::instance_id,
            component_balance::protocol_component_id,
            component_balance::token,
            component_balance::new_balance,
        ))
        .into_boxed::<Pg>();
    if let Some(component_ids) = component_ids {
        query = query.filter(component_balance::protocol_component_id.eq_any(component_ids));
    }
    let rows: Vec<(InstanceId, i64, Address, Balance)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

    let mut balances: HashMap<(InstanceId, i64), HashMap<Address, Balance>> = HashMap::new();
    for (instance_id, component_id, token, balance) in rows {
        balances
            .entry((instance_id, component_id))
            .or_default()
            .insert(token, balance);
    }
    Ok(balances)
}

/// Builds one state per component, in the order of `components`: its attributes from
/// `state_rows` and its balances from `balances`. A component with neither is left out.
fn build_states(
    components: Vec<((InstanceId, i64), String)>,
    state_rows: Vec<StateRow>,
    mut balances: HashMap<(InstanceId, i64), HashMap<Address, Balance>>,
) -> Vec<ProtocolComponentState> {
    let mut attributes: HashMap<(InstanceId, i64), HashMap<String, Bytes>> = HashMap::new();
    for (instance_id, component_id, name, value) in state_rows {
        attributes
            .entry((instance_id, component_id))
            .or_default()
            .insert(name, value);
    }

    let mut states = Vec::new();
    for (key, external_id) in components {
        let component_attributes = attributes
            .remove(&key)
            .unwrap_or_default();
        let component_balances = balances
            .remove(&key)
            .unwrap_or_default();
        if component_attributes.is_empty() && component_balances.is_empty() {
            continue;
        }
        states.push(ProtocolComponentState::new(
            &external_id,
            component_attributes,
            component_balances,
        ));
    }
    states
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel_async::AsyncPgConnection;
    use tycho_common::{
        models::{
            protocol::{ComponentBalance, ProtocolComponentState},
            Address, Chain, ComponentId, PaginationParams,
        },
        storage::{BlockOrTimestamp, StorageError, Version, VersionKind},
        Bytes,
    };

    use crate::postgres::{
        db_fixtures,
        instance::{gateway::PostgresGatewayNew, InstanceId},
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

    fn day(d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, d)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    fn at(ts: NaiveDateTime) -> Version {
        Version(BlockOrTimestamp::Timestamp(ts), VersionKind::Last)
    }

    /// Inserts a component with one token and a balance of `balance` in it.
    async fn component_with_balance(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
        external_id: &str,
        balance: &str,
    ) -> i64 {
        let id =
            db_fixtures::insert_instance_component(conn, instance, external_id, &[b("0x01")]).await;
        db_fixtures::insert_instance_component_balance(
            conn,
            instance,
            id,
            b("0x01"),
            b(balance),
            1.0,
        )
        .await;
        id
    }

    async fn balances(
        gateway: &PostgresGatewayNew,
        conn: &mut AsyncPgConnection,
        system: Option<&str>,
        at: Option<&Version>,
    ) -> Result<HashMap<ComponentId, HashMap<Address, ComponentBalance>>, StorageError> {
        gateway
            .get_component_balances(&Chain::Ethereum, system, None, at, conn)
            .await
    }

    #[tokio::test]
    async fn test_balances_of_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(
            &mut conn,
            s.curve,
            "a",
            &[b("0x01"), b("0x02")],
        )
        .await;
        db_fixtures::insert_instance_component_balance(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x10"),
            16.0,
        )
        .await;
        db_fixtures::insert_instance_component_balance(
            &mut conn,
            s.curve,
            a,
            b("0x02"),
            b("0x20"),
            32.0,
        )
        .await;
        component_with_balance(&mut conn, s.curve, "c", "0x30").await;
        component_with_balance(&mut conn, s.uniswap_v4, "u", "0x40").await;

        let result = balances(&s.gateway, &mut conn, Some("vm:curve"), None)
            .await
            .unwrap();

        let expected: HashMap<ComponentId, HashMap<Address, ComponentBalance>> = [
            (
                "a".to_string(),
                [
                    (
                        b("0x01"),
                        ComponentBalance::new(b("0x01"), b("0x10"), 16.0, Bytes::zero(32), "a"),
                    ),
                    (
                        b("0x02"),
                        ComponentBalance::new(b("0x02"), b("0x20"), 32.0, Bytes::zero(32), "a"),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            (
                "c".to_string(),
                [(
                    b("0x01"),
                    ComponentBalance::new(b("0x01"), b("0x30"), 1.0, Bytes::zero(32), "c"),
                )]
                .into_iter()
                .collect(),
            ),
        ]
        .into_iter()
        .collect();
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn test_balances_with_same_pool_id_stay_apart() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "0xpool", "0x01").await;
        component_with_balance(&mut conn, s.uniswap_v4, "0xpool", "0x02").await;

        for (system, balance) in [("vm:curve", b("0x01")), ("uniswap_v4", b("0x02"))] {
            let result = balances(&s.gateway, &mut conn, Some(system), None)
                .await
                .unwrap();

            assert_eq!(result.len(), 1, "{system}");
            assert_eq!(result["0xpool"][&b("0x01")].balance, balance, "{system}");
        }
    }

    #[tokio::test]
    async fn test_balances_need_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        let result = balances(&s.gateway, &mut conn, None, None).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_balances_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let next = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;
        component_with_balance(&mut conn, next, "b", "0x02").await;

        let result = balances(&s.gateway, &mut conn, Some("vm:curve"), None)
            .await
            .unwrap();

        assert_eq!(result.keys().collect::<Vec<_>>(), vec!["a"]);
    }

    #[tokio::test]
    async fn test_balances_of_unknown_or_inactive_protocol_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;

        let unknown = balances(&s.gateway, &mut conn, Some("not-a-protocol"), None)
            .await
            .unwrap();
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;
        let inactive = balances(&s.gateway, &mut conn, Some("vm:curve"), None)
            .await
            .unwrap();

        assert!(unknown.is_empty());
        assert!(inactive.is_empty());
    }

    #[tokio::test]
    async fn test_balances_before_head_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, day(2)).await;

        let result = balances(&s.gateway, &mut conn, Some("vm:curve"), Some(&at(day(1)))).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }

    async fn states(
        gateway: &PostgresGatewayNew,
        conn: &mut AsyncPgConnection,
        system: Option<&str>,
        ids: Option<&[&str]>,
        pagination: Option<&PaginationParams>,
        at: Option<Version>,
    ) -> Result<(Vec<ProtocolComponentState>, Option<i64>), StorageError> {
        let page = gateway
            .get_protocol_states(
                &Chain::Ethereum,
                at,
                system.map(str::to_string),
                ids,
                true,
                pagination,
                conn,
            )
            .await?;
        let mut entity = page.entity;
        if pagination.is_none() {
            entity.sort_by(|a, b| a.component_id.cmp(&b.component_id));
        }
        Ok((entity, page.total))
    }

    /// Inserts a component with one token and the attribute "reserve" set to `value`.
    async fn component_with_state(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
        external_id: &str,
        value: &str,
    ) -> i64 {
        let id =
            db_fixtures::insert_instance_component(conn, instance, external_id, &[b("0x01")]).await;
        db_fixtures::insert_instance_state(conn, instance, id, "reserve", b(value)).await;
        id
    }

    fn component_ids(states: &[ProtocolComponentState]) -> Vec<&str> {
        states
            .iter()
            .map(|state| state.component_id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn test_states_of_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_state(&mut conn, s.curve, a, "x", b("0x0a")).await;
        db_fixtures::insert_instance_state(&mut conn, s.curve, a, "y", b("0x0b")).await;
        db_fixtures::insert_instance_component_balance(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x10"),
            16.0,
        )
        .await;
        let c = db_fixtures::insert_instance_component(&mut conn, s.curve, "c", &[b("0x01")]).await;
        db_fixtures::insert_instance_state(&mut conn, s.curve, c, "x", b("0x0c")).await;
        db_fixtures::insert_instance_state(&mut conn, s.curve, c, "y", b("0x0d")).await;
        component_with_state(&mut conn, s.uniswap_v4, "u", "0x01").await;

        let (result, total) = states(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .unwrap();

        let expected = vec![
            ProtocolComponentState::new(
                "a",
                [("x".to_string(), b("0x0a")), ("y".to_string(), b("0x0b"))].into(),
                [(b("0x01"), b("0x10"))].into(),
            ),
            ProtocolComponentState::new(
                "c",
                [("x".to_string(), b("0x0c")), ("y".to_string(), b("0x0d"))].into(),
                HashMap::new(),
            ),
        ];
        assert_eq!(result, expected);
        assert_eq!(total, None);
    }

    #[tokio::test]
    async fn test_states_with_ids() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        component_with_state(&mut conn, s.curve, "c", "0x02").await;

        let (result, _) = states(&s.gateway, &mut conn, Some("vm:curve"), Some(&["a"]), None, None)
            .await
            .unwrap();

        assert_eq!(component_ids(&result), vec!["a"]);
    }

    #[tokio::test]
    async fn test_states_with_same_pool_id_stay_apart() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "0xpool", "0x01").await;
        component_with_state(&mut conn, s.uniswap_v4, "0xpool", "0x02").await;

        for (system, value) in [("vm:curve", b("0x01")), ("uniswap_v4", b("0x02"))] {
            let (result, _) = states(&s.gateway, &mut conn, Some(system), None, None, None)
                .await
                .unwrap();

            assert_eq!(result.len(), 1, "{system}");
            assert_eq!(result[0].attributes, [("reserve".to_string(), value)].into(), "{system}");
        }
    }

    #[tokio::test]
    async fn test_states_add_components_with_balances_only() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "a", "0x10").await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "empty", &[b("0x01")]).await;

        let (result, _) = states(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .unwrap();

        assert_eq!(
            result,
            vec![ProtocolComponentState::new("a", HashMap::new(), [(b("0x01"), b("0x10"))].into())]
        );
    }

    #[tokio::test]
    async fn test_states_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let next = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        component_with_state(&mut conn, next, "b", "0x02").await;

        let (result, _) = states(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .unwrap();

        assert_eq!(component_ids(&result), vec!["a"]);
    }

    #[tokio::test]
    async fn test_states_of_unknown_or_inactive_protocol_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;

        let (unknown, _) = states(&s.gateway, &mut conn, Some("not-a-protocol"), None, None, None)
            .await
            .unwrap();
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;
        let (inactive, _) = states(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .unwrap();

        assert!(unknown.is_empty());
        assert!(inactive.is_empty());
    }

    #[tokio::test]
    async fn test_states_paging_counts_components() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        for id in ["a", "c", "d"] {
            component_with_state(&mut conn, s.curve, id, "0x01").await;
        }

        let (result, total) = states(
            &s.gateway,
            &mut conn,
            Some("vm:curve"),
            None,
            Some(&PaginationParams::new(0, 2)),
            None,
        )
        .await
        .unwrap();

        assert_eq!(component_ids(&result), vec!["a", "c"]);
        assert_eq!(total, Some(3));
    }

    #[tokio::test]
    async fn test_states_before_head_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, day(2)).await;

        let result =
            states(&s.gateway, &mut conn, Some("vm:curve"), None, None, Some(at(day(1)))).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }
}
