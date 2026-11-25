use std::collections::HashMap;

use chrono::NaiveDateTime;
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

use super::version::{resolve_version, unique_by_key, ReadAt};
use crate::postgres::{
    instance::{
        active::active_instance_limits,
        gateway::PostgresGatewayNew,
        schema::tycho::{
            component_balance, component_balance_history, protocol_component, protocol_state,
            protocol_state_history,
        },
        InstanceId,
    },
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the token balances of the components of `system`'s active instance, by external id
    /// and token address.
    ///
    /// - `system`: the protocol system, because the answer is keyed by external id and an external
    ///   id can repeat across protocol systems. `NotFound` for an unknown name. A protocol system
    ///   without an active instance has no balances.
    /// - `ids`: only components with these external ids.
    /// - `at`: the version. Before the head of the instance, the history tables answer too.
    ///   `Unsupported` for a version before the history horizon or before the base block of the
    ///   instance.
    ///
    /// Components without balances are absent. `modify_tx` is always 32 zero bytes. `NotFound` for
    /// an unknown chain.
    pub async fn get_component_balances(
        &self,
        chain: &Chain,
        system: &str,
        ids: Option<&[&str]>,
        at: Option<&Version>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<ComponentId, HashMap<Address, ComponentBalance>>, StorageError> {
        self.get_chain_id(chain)?;
        self.get_protocol_system_id(system)?;
        let Some(instance) = active_instance_limits(system, conn).await? else {
            return Ok(HashMap::new());
        };

        let read = resolve_version(&instance, at, conn).await?;

        Ok(component_balances_at(instance.id, ids, read, conn)
            .await?
            .into_iter()
            .map(|(external_id, balances)| {
                let balances = balances
                    .into_iter()
                    .map(|(token, (balance, balance_float))| {
                        let balance = ComponentBalance::new(
                            token.clone(),
                            balance,
                            balance_float,
                            Bytes::zero(32),
                            &external_id,
                        );
                        (token, balance)
                    })
                    .collect();
                (external_id, balances)
            })
            .collect())
    }

    /// Returns the state of the components of `system`'s active instance. The total is always
    /// `None`.
    ///
    /// - `at`: the version. Before the head of the instance, the history tables answer too.
    ///   `Unsupported` for a version before the history horizon or before the base block of the
    ///   instance.
    /// - `system`: the protocol system. Required, because a pool id can repeat across protocol
    ///   systems: `None` is `Unsupported`. `NotFound` for an unknown name. A protocol system
    ///   without an active instance has no states.
    /// - `ids`: only components with these external ids.
    /// - `retrieve_balances`: add the token balances of each component.
    /// - `pagination_params`: `Unsupported` when set. Page the component ids instead.
    ///
    /// A component without state attributes is absent, even with balances. `NotFound` for an
    /// unknown chain.
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
        let Some(system) = system else {
            return Err(StorageError::Unsupported(
                "protocol states need a protocol system: a pool id can repeat across protocols"
                    .to_string(),
            ));
        };
        self.get_protocol_system_id(&system)?;
        if pagination_params.is_some() {
            return Err(StorageError::Unsupported(
                "protocol states are not paged: page the component ids".to_string(),
            ));
        }
        states_of(&system, ids, at.as_ref(), retrieve_balances, conn).await
    }
}

/// Reads the states of the matching components of the active instance of `system`.
async fn states_of(
    system: &str,
    ids: Option<&[&str]>,
    at: Option<&Version>,
    retrieve_balances: bool,
    conn: &mut AsyncPgConnection,
) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError> {
    let Some(active) = active_instance_limits(system, conn).await? else {
        return Ok(WithTotal { entity: Vec::new(), total: None });
    };
    let read = resolve_version(&active, at, conn).await?;
    let instance = active.id;

    let rows = match read {
        ReadAt::Current(_) => current_state_rows(instance, ids, None, conn).await?,
        ReadAt::Past { ts, head } => {
            let mut rows = current_state_rows(instance, ids, Some(ts), conn).await?;
            rows.extend(history_state_rows(instance, ids, ts, head, conn).await?);
            rows
        }
    };
    let attributes = unique_by_key(
        "protocol_state",
        instance,
        read,
        rows.into_iter()
            .map(|(external_id, name, value)| ((external_id, name), value)),
    )?;

    let mut balances: HashMap<ComponentId, HashMap<Address, Balance>> = if retrieve_balances {
        component_balances_at(instance, ids, read, conn)
            .await?
            .into_iter()
            .filter(|(external_id, _)| attributes.contains_key(external_id))
            .map(|(external_id, balances)| {
                let balances = balances
                    .into_iter()
                    .map(|(token, (balance, _))| (token, balance))
                    .collect();
                (external_id, balances)
            })
            .collect()
    } else {
        HashMap::new()
    };

    let states = attributes
        .into_iter()
        .map(|(external_id, attributes)| {
            let balances = balances
                .remove(&external_id)
                .unwrap_or_default();
            ProtocolComponentState::new(&external_id, attributes, balances)
        })
        .collect();
    Ok(WithTotal { entity: states, total: None })
}

/// A state row: external id of the component, attribute name and value.
type StateRow = (ComponentId, String, Bytes);

/// Loads the current state rows of the components of `instance`.
/// `ids`: only components with these external ids. `ts`: only rows with `valid_from <= ts`.
async fn current_state_rows(
    instance: InstanceId,
    ids: Option<&[&str]>,
    ts: Option<NaiveDateTime>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<StateRow>, StorageError> {
    let mut query = protocol_state::table
        .inner_join(
            protocol_component::table.on(protocol_component::instance_id
                .eq(protocol_state::instance_id)
                .and(protocol_component::id.eq(protocol_state::protocol_component_id))),
        )
        .filter(protocol_state::instance_id.eq(instance))
        .filter(protocol_component::instance_id.eq(instance))
        .select((
            protocol_component::external_id,
            protocol_state::attribute_name,
            protocol_state::attribute_value,
        ))
        .into_boxed::<Pg>();
    if let Some(ts) = ts {
        query = query.filter(protocol_state::valid_from.le(ts));
    }
    if let Some(ids) = ids {
        query = query.filter(protocol_component::external_id.eq_any(ids));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// Loads the history state rows of the components of `instance` that are valid at `ts`:
/// `valid_from <= ts < valid_to`, with `valid_to <= head` (see [`ReadAt::Past`]).
/// `ids`: only components with these external ids.
async fn history_state_rows(
    instance: InstanceId,
    ids: Option<&[&str]>,
    ts: NaiveDateTime,
    head: NaiveDateTime,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<StateRow>, StorageError> {
    let mut query = protocol_state_history::table
        .inner_join(
            protocol_component::table.on(protocol_component::instance_id
                .eq(protocol_state_history::instance_id)
                .and(protocol_component::id.eq(protocol_state_history::protocol_component_id))),
        )
        .filter(protocol_state_history::instance_id.eq(instance))
        .filter(protocol_component::instance_id.eq(instance))
        .filter(protocol_state_history::valid_from.le(ts))
        .filter(protocol_state_history::valid_to.gt(ts))
        .filter(protocol_state_history::valid_to.le(head))
        .select((
            protocol_component::external_id,
            protocol_state_history::attribute_name,
            protocol_state_history::attribute_value,
        ))
        .into_boxed::<Pg>();
    if let Some(ids) = ids {
        query = query.filter(protocol_component::external_id.eq_any(ids));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// A balance row: external id of the component, token, balance and its float value.
type BalanceRow = (ComponentId, Address, Balance, f64);

/// Loads the current balance rows of the components of `instance`.
/// `ids`: only components with these external ids. `ts`: only rows with `valid_from <= ts`.
async fn current_balance_rows(
    instance: InstanceId,
    ids: Option<&[&str]>,
    ts: Option<NaiveDateTime>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<BalanceRow>, StorageError> {
    let mut query = component_balance::table
        .inner_join(
            protocol_component::table.on(protocol_component::instance_id
                .eq(component_balance::instance_id)
                .and(protocol_component::id.eq(component_balance::protocol_component_id))),
        )
        .filter(component_balance::instance_id.eq(instance))
        .filter(protocol_component::instance_id.eq(instance))
        .select((
            protocol_component::external_id,
            component_balance::token,
            component_balance::new_balance,
            component_balance::balance_float,
        ))
        .into_boxed::<Pg>();
    if let Some(ts) = ts {
        query = query.filter(component_balance::valid_from.le(ts));
    }
    if let Some(ids) = ids {
        query = query.filter(protocol_component::external_id.eq_any(ids));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// Loads the history balance rows of the components of `instance` that are valid at `ts`:
/// `valid_from <= ts < valid_to`, with `valid_to <= head` (see [`ReadAt::Past`]).
/// `ids`: only components with these external ids.
async fn history_balance_rows(
    instance: InstanceId,
    ids: Option<&[&str]>,
    ts: NaiveDateTime,
    head: NaiveDateTime,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<BalanceRow>, StorageError> {
    let mut query = component_balance_history::table
        .inner_join(
            protocol_component::table.on(protocol_component::instance_id
                .eq(component_balance_history::instance_id)
                .and(protocol_component::id.eq(component_balance_history::protocol_component_id))),
        )
        .filter(component_balance_history::instance_id.eq(instance))
        .filter(protocol_component::instance_id.eq(instance))
        .filter(component_balance_history::valid_from.le(ts))
        .filter(component_balance_history::valid_to.gt(ts))
        .filter(component_balance_history::valid_to.le(head))
        .select((
            protocol_component::external_id,
            component_balance_history::token,
            component_balance_history::new_balance,
            component_balance_history::balance_float,
        ))
        .into_boxed::<Pg>();
    if let Some(ids) = ids {
        query = query.filter(protocol_component::external_id.eq_any(ids));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// Returns the balances valid at `read`, as balance and float value by external id and token.
/// `ids`: only components with these external ids.
///
/// A `Past` read adds the history rows to the current rows. `Unexpected` when two rows of one
/// component and token are valid at the version.
async fn component_balances_at(
    instance: InstanceId,
    ids: Option<&[&str]>,
    read: ReadAt,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<ComponentId, HashMap<Address, (Balance, f64)>>, StorageError> {
    let rows = match read {
        ReadAt::Current(_) => current_balance_rows(instance, ids, None, conn).await?,
        ReadAt::Past { ts, head } => {
            let mut rows = current_balance_rows(instance, ids, Some(ts), conn).await?;
            rows.extend(history_balance_rows(instance, ids, ts, head, conn).await?);
            rows
        }
    };
    unique_by_key(
        "component_balance",
        instance,
        read,
        rows.into_iter()
            .map(|(id, token, balance, float)| ((id, token), (balance, float))),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
    use diesel::{prelude::*, sql_types::Timestamp};
    use diesel_async::{
        scoped_futures::ScopedFutureExt, AsyncConnection, AsyncPgConnection, RunQueryDsl,
    };
    use tycho_common::{
        models::{
            protocol::{ComponentBalance, ProtocolComponentState},
            Address, Chain, ComponentId, PaginationParams,
        },
        storage::{BlockOrTimestamp, StorageError, Version, VersionKind},
        Bytes,
    };

    use super::{
        active_instance_limits, current_state_rows, history_state_rows, resolve_version, ReadAt,
    };
    use crate::postgres::{
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            orm::NewProtocolStateHistory,
            schema::tycho::{
                extraction_state, protocol_instance, protocol_state, protocol_state_history,
            },
            InstanceId,
        },
        snapshot::snapshot_transaction,
        testing::{instance_test_connection, run_against_db},
        PostgresError,
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

    /// Midnight UTC of the day `d` days after the day 10 days ago: inside the default retention.
    fn recent(d: i64) -> NaiveDateTime {
        (Utc::now().date_naive() - Duration::days(10 - d))
            .and_hms_opt(0, 0, 0)
            .expect("a valid time")
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
        system: &str,
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

        let result = balances(&s.gateway, &mut conn, "vm:curve", None)
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
            let result = balances(&s.gateway, &mut conn, system, None)
                .await
                .unwrap();

            assert_eq!(result.len(), 1, "{system}");
            assert_eq!(result["0xpool"][&b("0x01")].balance, balance, "{system}");
        }
    }

    #[tokio::test]
    async fn test_balances_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let next = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;
        component_with_balance(&mut conn, next, "b", "0x02").await;

        let result = balances(&s.gateway, &mut conn, "vm:curve", None)
            .await
            .unwrap();

        assert_eq!(result.keys().collect::<Vec<_>>(), vec!["a"]);
    }

    #[tokio::test]
    async fn test_balances_of_unknown_protocol_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;

        for system in ["not-a-protocol", ""] {
            let result = balances(&s.gateway, &mut conn, system, None).await;

            assert!(matches!(result, Err(StorageError::NotFound(..))), "{system:?}: {result:?}");
        }
    }

    #[tokio::test]
    async fn test_balances_of_inactive_protocol_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;

        let inactive = balances(&s.gateway, &mut conn, "vm:curve", None)
            .await
            .unwrap();

        assert!(inactive.is_empty());
    }

    #[tokio::test]
    async fn test_balances_before_retention_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_balance(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result =
            balances(&s.gateway, &mut conn, "vm:curve", Some(&at(recent(5) - Duration::days(40))))
                .await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("retention"), "{msg}");
    }

    #[tokio::test]
    async fn test_balances_at_a_past_version_read_history() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_component_balance_version(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x10"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::insert_instance_component_balance_version(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x20"),
            recent(3),
            None,
        )
        .await;
        component_with_balance(&mut conn, s.curve, "c", "0x30").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, expected) in [(recent(2), "0x10"), (recent(4), "0x20")] {
            let result = balances(&s.gateway, &mut conn, "vm:curve", Some(&at(ts)))
                .await
                .unwrap();

            assert_eq!(result["a"].len(), 1, "{ts}");
            assert_eq!(result["a"][&b("0x01")].balance, b(expected), "{ts}");
            assert_eq!(result["c"][&b("0x01")].balance, b("0x30"), "{ts}");
        }
    }

    #[tokio::test]
    async fn test_balances_at_a_past_version_skip_deleted_balances() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        db_fixtures::insert_instance_component_balance_version(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x10"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let before = balances(&s.gateway, &mut conn, "vm:curve", Some(&at(recent(2))))
            .await
            .unwrap();
        let deleted = balances(&s.gateway, &mut conn, "vm:curve", Some(&at(recent(3))))
            .await
            .unwrap();

        assert_eq!(before["a"][&b("0x01")].balance, b("0x10"));
        assert!(deleted.is_empty(), "{deleted:?}");
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
    async fn test_states_skip_components_without_state_rows() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        component_with_balance(&mut conn, s.curve, "balance-only", "0x10").await;
        db_fixtures::insert_instance_component(&mut conn, s.curve, "empty", &[b("0x01")]).await;

        let (result, _) = states(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .unwrap();

        assert_eq!(component_ids(&result), vec!["a"]);
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
    async fn test_states_of_unknown_protocol_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;

        for system in ["not-a-protocol", ""] {
            let result = states(&s.gateway, &mut conn, Some(system), None, None, None).await;

            assert!(matches!(result, Err(StorageError::NotFound(..))), "{system:?}: {result:?}");
        }
    }

    #[tokio::test]
    async fn test_states_of_inactive_protocol_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;

        let (inactive, _) = states(&s.gateway, &mut conn, Some("vm:curve"), None, None, None)
            .await
            .unwrap();

        assert!(inactive.is_empty());
    }

    #[tokio::test]
    async fn test_states_paging_is_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;

        let result = states(
            &s.gateway,
            &mut conn,
            Some("vm:curve"),
            None,
            Some(&PaginationParams::new(0, 2)),
            None,
        )
        .await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_states_before_retention_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = states(
            &s.gateway,
            &mut conn,
            Some("vm:curve"),
            None,
            None,
            Some(at(recent(5) - Duration::days(40))),
        )
        .await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("retention"), "{msg}");
    }

    #[tokio::test]
    async fn test_states_before_the_base_block_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;
        diesel::update(protocol_instance::table.find(s.curve))
            .set(protocol_instance::base_block_ts.eq(recent(2)))
            .execute(&mut conn)
            .await
            .unwrap();

        let result =
            states(&s.gateway, &mut conn, Some("vm:curve"), None, None, Some(at(recent(1)))).await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("base block"), "{msg}");
    }

    /// Inserts a version of the attribute `name` of a component of curve.
    async fn state_version(
        conn: &mut AsyncPgConnection,
        s: &Setup,
        component: i64,
        name: &str,
        value: &str,
        valid_from: NaiveDateTime,
        valid_to: Option<NaiveDateTime>,
    ) {
        db_fixtures::insert_instance_state_version(
            conn,
            s.curve,
            component,
            name,
            b(value),
            valid_from,
            valid_to,
        )
        .await;
    }

    async fn curve_states_at(
        s: &Setup,
        conn: &mut AsyncPgConnection,
        ids: Option<&[&str]>,
        ts: NaiveDateTime,
    ) -> Result<Vec<ProtocolComponentState>, StorageError> {
        states(&s.gateway, conn, Some("vm:curve"), ids, None, Some(at(ts)))
            .await
            .map(|(states, _)| states)
    }

    fn state(external_id: &str, attributes: &[(&str, &str)]) -> ProtocolComponentState {
        ProtocolComponentState::new(
            external_id,
            attributes
                .iter()
                .map(|(name, value)| (name.to_string(), b(value)))
                .collect(),
            HashMap::new(),
        )
    }

    #[tokio::test]
    async fn test_states_at_a_past_version_read_history() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), Some(recent(3))).await;
        state_version(&mut conn, &s, a, "x", "0x02", recent(3), None).await;
        db_fixtures::insert_instance_state(&mut conn, s.curve, a, "y", b("0x0b")).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, x) in [(recent(2), "0x01"), (recent(4), "0x02")] {
            let result = curve_states_at(&s, &mut conn, None, ts)
                .await
                .unwrap();

            assert_eq!(result, vec![state("a", &[("x", x), ("y", "0x0b")])], "{ts}");
        }
    }

    #[tokio::test]
    async fn test_states_at_a_past_version_see_the_last_update_of_a_block() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), Some(recent(2))).await;
        state_version(&mut conn, &s, a, "x", "0x02", recent(2), Some(recent(2))).await;
        state_version(&mut conn, &s, a, "x", "0x03", recent(2), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, x) in [(recent(2), "0x03"), (recent(1), "0x01")] {
            let result = curve_states_at(&s, &mut conn, None, ts)
                .await
                .unwrap();

            assert_eq!(result, vec![state("a", &[("x", x)])], "{ts}");
        }
    }

    #[tokio::test]
    async fn test_states_at_a_past_version_skip_deleted_attributes() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), Some(recent(3))).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let before = curve_states_at(&s, &mut conn, None, recent(2))
            .await
            .unwrap();
        let deleted = curve_states_at(&s, &mut conn, None, recent(3))
            .await
            .unwrap();

        assert_eq!(before, vec![state("a", &[("x", "0x01")])]);
        assert!(deleted.is_empty(), "{deleted:?}");
    }

    #[tokio::test]
    async fn test_states_before_head_read_a_row_replaced_at_the_head() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), Some(recent(5))).await;
        state_version(&mut conn, &s, a, "x", "0x02", recent(5), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = curve_states_at(&s, &mut conn, None, recent(4))
            .await
            .unwrap();

        assert_eq!(result, vec![state("a", &[("x", "0x01")])]);
    }

    #[tokio::test]
    async fn test_states_at_a_past_version_with_ids() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        let c = db_fixtures::insert_instance_component(&mut conn, s.curve, "c", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), Some(recent(3))).await;
        state_version(&mut conn, &s, c, "x", "0x02", recent(1), Some(recent(3))).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = curve_states_at(&s, &mut conn, Some(&["a"]), recent(2))
            .await
            .unwrap();

        assert_eq!(result, vec![state("a", &[("x", "0x01")])]);
    }

    #[tokio::test]
    async fn test_states_at_a_past_version_skip_other_instances() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), Some(recent(3))).await;
        db_fixtures::insert_instance_state_version(
            &mut conn,
            s.uniswap_v4,
            a,
            "x",
            b("0x02"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = curve_states_at(&s, &mut conn, None, recent(2))
            .await
            .unwrap();

        assert_eq!(result, vec![state("a", &[("x", "0x01")])]);
    }

    #[tokio::test]
    async fn test_states_at_a_past_version_have_balances_at_that_version() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = component_with_state(&mut conn, s.curve, "a", "0x01").await;
        db_fixtures::insert_instance_component_balance_version(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x10"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::insert_instance_component_balance_version(
            &mut conn,
            s.curve,
            a,
            b("0x01"),
            b("0x20"),
            recent(3),
            None,
        )
        .await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = curve_states_at(&s, &mut conn, None, recent(2))
            .await
            .unwrap();

        let expected = vec![ProtocolComponentState::new(
            "a",
            [("reserve".to_string(), b("0x01"))].into(),
            [(b("0x01"), b("0x10"))].into(),
        )];
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn test_states_with_two_valid_rows_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[b("0x01")]).await;
        state_version(&mut conn, &s, a, "x", "0x01", recent(1), None).await;
        state_version(&mut conn, &s, a, "x", "0x02", recent(1), Some(recent(3))).await;
        let c = db_fixtures::insert_instance_component(&mut conn, s.curve, "c", &[b("0x01")]).await;
        state_version(&mut conn, &s, c, "x", "0x01", recent(1), Some(recent(3))).await;
        diesel::insert_into(protocol_state_history::table)
            .values(NewProtocolStateHistory {
                instance_id: s.curve,
                protocol_component_id: c,
                attribute_name: "x".to_string(),
                attribute_value: b("0x02"),
                previous_value: None,
                valid_from: recent(1),
                valid_to: recent(3),
                tx_index: 1,
            })
            .execute(&mut conn)
            .await
            .unwrap();
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for id in ["a", "c"] {
            let result = curve_states_at(&s, &mut conn, Some(&[id]), recent(2)).await;

            assert!(matches!(result, Err(StorageError::Unexpected(_))), "{id}: {result:?}");
        }
    }

    #[tokio::test]
    async fn test_states_by_id_need_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        let result = states(&s.gateway, &mut conn, None, Some(&["a"]), None, None).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_states_without_a_protocol_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        component_with_state(&mut conn, s.curve, "a", "0x01").await;

        let latest = states(&s.gateway, &mut conn, None, None, None, None).await;
        let versioned = states(&s.gateway, &mut conn, None, None, None, Some(at(day(1)))).await;

        assert!(matches!(latest, Err(StorageError::Unsupported(_))), "{latest:?}");
        assert!(matches!(versioned, Err(StorageError::Unsupported(_))), "{versioned:?}");
    }

    #[tokio::test]
    async fn test_serial_db_state_read_sees_the_callers_snapshot() {
        run_against_db(|pool| async move {
            let mut reader = pool.get().await.unwrap();
            let mut writer = pool.get().await.unwrap();
            let s = setup(&mut writer).await;
            let a = component_with_state(&mut writer, s.curve, "a", "0x01").await;
            db_fixtures::set_instance_head(&mut writer, s.curve, day(2)).await;

            let (before, during) = snapshot_transaction(&mut reader)
                .run(|reader| {
                    let gateway = &s.gateway;
                    let writer = &mut writer;
                    async move {
                        let before =
                            states(gateway, reader, Some("vm:curve"), None, None, Some(at(day(2))))
                                .await;
                        diesel::update(
                            protocol_state::table
                                .filter(protocol_state::instance_id.eq(s.curve))
                                .filter(protocol_state::protocol_component_id.eq(a)),
                        )
                        .set(protocol_state::attribute_value.eq(b("0x02")))
                        .execute(writer)
                        .await?;
                        diesel::update(extraction_state::table.find(s.curve))
                            .set(extraction_state::block_ts.eq(day(3)))
                            .execute(writer)
                            .await?;
                        let during =
                            states(gateway, reader, Some("vm:curve"), None, None, Some(at(day(2))))
                                .await;
                        Ok::<_, PostgresError>((before, during))
                    }
                    .scope_boxed()
                })
                .await
                .unwrap();

            let expected = vec![ProtocolComponentState::new(
                "a",
                [("reserve".to_string(), b("0x01"))].into(),
                HashMap::new(),
            )];
            assert_eq!(before, Ok((expected.clone(), None)));
            assert_eq!(during, Ok((expected, None)));
        })
        .await;
    }

    /// Where the race test moves the row, between the statements of a past read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Gap {
        BeforeCurrentQuery,
        BetweenQueries,
    }

    /// Moves the attribute "x" of `component` to history at `recent(4)` in one transaction, as the
    /// writer does: a history row with the old value, the new current value and the new head.
    async fn move_x_to_history(conn: &mut AsyncPgConnection, instance: InstanceId, component: i64) {
        let counts = conn
            .transaction::<_, diesel::result::Error, _>(|conn| {
                async move {
                    let inserted = diesel::insert_into(protocol_state_history::table)
                        .values(NewProtocolStateHistory {
                            instance_id: instance,
                            protocol_component_id: component,
                            attribute_name: "x".to_string(),
                            attribute_value: b("0x01"),
                            previous_value: None,
                            valid_from: recent(1),
                            valid_to: recent(4),
                            tx_index: 0,
                        })
                        .execute(conn)
                        .await?;
                    let updated = diesel::update(
                        protocol_state::table
                            .filter(protocol_state::instance_id.eq(instance))
                            .filter(protocol_state::protocol_component_id.eq(component))
                            .filter(protocol_state::attribute_name.eq("x")),
                    )
                    .set((
                        protocol_state::attribute_value.eq(b("0x02")),
                        protocol_state::valid_from.eq(recent(4)),
                    ))
                    .execute(conn)
                    .await?;
                    let head = diesel::update(extraction_state::table.find(instance))
                        .set(extraction_state::block_ts.eq(recent(4)))
                        .execute(conn)
                        .await?;
                    Ok([inserted, updated, head])
                }
                .scope_boxed()
            })
            .await
            .expect("moving the row to history failed");
        assert_eq!(
            counts,
            [1, 1, 1],
            "rows changed by the history insert, state update, head update"
        );
    }

    #[tokio::test]
    async fn test_serial_db_past_state_read_sees_a_moved_row_once() {
        for gap in [Gap::BeforeCurrentQuery, Gap::BetweenQueries] {
            run_against_db(move |pool| async move {
                let mut reader = pool.get().await.unwrap();
                let mut writer = pool.get().await.unwrap();
                let s = setup(&mut writer).await;
                let a =
                    db_fixtures::insert_instance_component(&mut writer, s.curve, "a", &[b("0x01")])
                        .await;
                db_fixtures::insert_instance_state_version(
                    &mut writer,
                    s.curve,
                    a,
                    "x",
                    b("0x01"),
                    recent(1),
                    None,
                )
                .await;
                db_fixtures::set_instance_head(&mut writer, s.curve, recent(3)).await;
                diesel::sql_query(
                    "SELECT partman.create_partition_time('tycho.protocol_state_history', \
                     ARRAY[$1])",
                )
                .bind::<Timestamp, _>(recent(4))
                .execute(&mut writer)
                .await
                .unwrap();

                let rows = snapshot_transaction(&mut reader)
                    .run(|reader| {
                        let writer = &mut writer;
                        async move {
                            let active = active_instance_limits("vm:curve", reader)
                                .await
                                .unwrap()
                                .expect("vm:curve has an active instance");
                            let read = resolve_version(&active, Some(&at(recent(2))), reader)
                                .await
                                .unwrap();
                            let (ts, head) = (recent(2), recent(3));
                            assert_eq!(read, ReadAt::Past { ts, head });
                            if gap == Gap::BeforeCurrentQuery {
                                move_x_to_history(writer, s.curve, a).await;
                            }
                            let mut rows = current_state_rows(active.id, None, Some(ts), reader)
                                .await
                                .unwrap();
                            if gap == Gap::BetweenQueries {
                                move_x_to_history(writer, s.curve, a).await;
                            }
                            rows.extend(
                                history_state_rows(active.id, None, ts, head, reader)
                                    .await
                                    .unwrap(),
                            );
                            Ok::<_, PostgresError>(rows)
                        }
                        .scope_boxed()
                    })
                    .await
                    .unwrap();

                assert_eq!(rows, vec![("a".to_string(), "x".to_string(), b("0x01"))], "{gap:?}");
            })
            .await;
        }
    }
}
