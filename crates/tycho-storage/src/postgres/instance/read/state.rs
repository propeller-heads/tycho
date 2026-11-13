use std::collections::HashMap;

use diesel::{pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{protocol::ComponentBalance, Address, Chain, ComponentId},
    storage::{StorageError, Version},
    Bytes,
};

use super::version::check_current;
use crate::postgres::{
    instance::{
        active::active_instance,
        gateway::PostgresGatewayNew,
        orm as iorm,
        schema::tycho::{component_balance, protocol_component},
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
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel_async::AsyncPgConnection;
    use tycho_common::{
        models::{protocol::ComponentBalance, Address, Chain, ComponentId},
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
}
