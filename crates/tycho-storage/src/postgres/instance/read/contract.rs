use std::collections::HashMap;

use diesel::{pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{contract::AccountBalance, Address, Balance, Chain},
    storage::{StorageError, Version},
    Bytes,
};

use super::version::check_current;
use crate::postgres::{
    instance::{
        active::active_instance,
        gateway::PostgresGatewayNew,
        schema::tycho::{account, account_balance},
        InstanceId,
    },
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the token balances of the accounts of `protocol_system`'s active instance, by
    /// account address and token address.
    ///
    /// - `protocol_system`: the protocol system whose copy of the accounts to read. `NotFound` for
    ///   an unknown name. A protocol system without an active instance has no balances.
    /// - `accounts`: only these accounts.
    /// - `at`: the version. `Unsupported` for a version before the head of the instance.
    /// - `include_native`: add the balance of the chain's native token.
    ///
    /// Accounts without balances are absent. `modify_tx` is always 32 zero bytes. `NotFound` for
    /// an unknown chain.
    pub async fn get_account_balances(
        &self,
        chain: &Chain,
        protocol_system: &str,
        accounts: Option<&[Address]>,
        at: Option<&Version>,
        include_native: bool,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        self.get_chain_id(chain)?;
        self.get_protocol_system_id(protocol_system)?;
        let Some(instance) = active_instance(conn, protocol_system).await? else {
            return Ok(HashMap::new());
        };
        check_current(conn, instance, at).await?;
        load_account_balances(conn, chain, instance, accounts, include_native).await
    }
}

/// Loads the token balances of the accounts of `instance`, by account address and token address.
/// `accounts`: only these accounts; `None` for all of them.
async fn load_account_balances(
    conn: &mut AsyncPgConnection,
    chain: &Chain,
    instance: InstanceId,
    accounts: Option<&[Address]>,
    include_native: bool,
) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
    let mut query = account_balance::table
        .inner_join(
            account::table.on(account::instance_id
                .eq(account_balance::instance_id)
                .and(account::id.eq(account_balance::account_id))),
        )
        .filter(account_balance::instance_id.eq(instance))
        .filter(account::instance_id.eq(instance))
        .select((account::address, account_balance::token, account_balance::balance))
        .into_boxed::<Pg>();
    if let Some(accounts) = accounts {
        query = query.filter(account::address.eq_any(accounts));
    }
    if !include_native {
        query = query.filter(account_balance::token.ne(chain.native_token().address));
    }
    let rows: Vec<(Address, Address, Balance)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

    let mut balances: HashMap<Address, HashMap<Address, AccountBalance>> = HashMap::new();
    for (address, token, balance) in rows {
        let account_balance =
            AccountBalance::new(address.clone(), token.clone(), balance, Bytes::zero(32));
        balances
            .entry(address)
            .or_default()
            .insert(token, account_balance);
    }
    Ok(balances)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel_async::AsyncPgConnection;
    use tycho_common::{
        models::{contract::AccountBalance, Address, Chain},
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

    fn eth() -> Address {
        Chain::Ethereum.native_token().address
    }

    fn day(d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, d)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    fn at(ts: NaiveDateTime) -> Version {
        Version(BlockOrTimestamp::Timestamp(ts), VersionKind::Last)
    }

    /// Inserts an account at `address` with a native balance of `native` and a balance of
    /// `token_balance` in the token `0x02`. Returns the account id.
    async fn account_with_balances(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
        address: &str,
        native: &str,
        token_balance: &str,
    ) -> i64 {
        let id = db_fixtures::insert_instance_account(conn, instance, b(address)).await;
        db_fixtures::insert_instance_account_balance(conn, instance, id, eth(), b(native)).await;
        db_fixtures::insert_instance_account_balance(
            conn,
            instance,
            id,
            b("0x02"),
            b(token_balance),
        )
        .await;
        id
    }

    async fn balances(
        gateway: &PostgresGatewayNew,
        conn: &mut AsyncPgConnection,
        system: &str,
        include_native: bool,
        at: Option<&Version>,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        gateway
            .get_account_balances(&Chain::Ethereum, system, None, at, include_native, conn)
            .await
    }

    fn balance(account: &str, token: Address, value: &str) -> (Address, AccountBalance) {
        (token.clone(), AccountBalance::new(b(account), token, b(value), Bytes::zero(32)))
    }

    #[tokio::test]
    async fn test_balances_of_an_account() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;

        let without_native = balances(&s.gateway, &mut conn, "vm:curve", false, None)
            .await
            .unwrap();
        let with_native = balances(&s.gateway, &mut conn, "vm:curve", true, None)
            .await
            .unwrap();

        assert_eq!(
            without_native,
            [(b("0xaa"), [balance("0xaa", b("0x02"), "0x05")].into())].into()
        );
        assert_eq!(
            with_native,
            [(
                b("0xaa"),
                [balance("0xaa", eth(), "0x01"), balance("0xaa", b("0x02"), "0x05")].into()
            )]
            .into()
        );
    }

    #[tokio::test]
    async fn test_balances_of_some_accounts() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        account_with_balances(&mut conn, s.curve, "0xbb", "0x01", "0x06").await;

        let result = s
            .gateway
            .get_account_balances(
                &Chain::Ethereum,
                "vm:curve",
                Some(&[b("0xbb")]),
                None,
                false,
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(result, [(b("0xbb"), [balance("0xbb", b("0x02"), "0x06")].into())].into());
    }

    #[tokio::test]
    async fn test_shared_account_balances_come_from_the_protocol_copy() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        account_with_balances(&mut conn, s.uniswap_v4, "0xaa", "0x02", "0x06").await;

        for (system, native, token_balance) in
            [("vm:curve", "0x01", "0x05"), ("uniswap_v4", "0x02", "0x06")]
        {
            let result = balances(&s.gateway, &mut conn, system, true, None)
                .await
                .unwrap();

            assert_eq!(
                result,
                [(
                    b("0xaa"),
                    [balance("0xaa", eth(), native), balance("0xaa", b("0x02"), token_balance)]
                        .into()
                )]
                .into(),
                "{system}"
            );
        }
    }

    #[tokio::test]
    async fn test_balances_of_unknown_protocol_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        for system in ["not-a-protocol", ""] {
            let result = balances(&s.gateway, &mut conn, system, true, None).await;

            assert!(matches!(result, Err(StorageError::NotFound(..))), "{system:?}: {result:?}");
        }
    }

    #[tokio::test]
    async fn test_balances_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let next = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        account_with_balances(&mut conn, next, "0xaa", "0x02", "0x06").await;

        let active = balances(&s.gateway, &mut conn, "vm:curve", false, None)
            .await
            .unwrap();
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;
        let cleared = balances(&s.gateway, &mut conn, "vm:curve", false, None)
            .await
            .unwrap();

        assert_eq!(active, [(b("0xaa"), [balance("0xaa", b("0x02"), "0x05")].into())].into());
        assert!(cleared.is_empty());
    }

    #[tokio::test]
    async fn test_account_balances_before_head_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, day(2)).await;

        let result = balances(&s.gateway, &mut conn, "vm:curve", true, Some(&at(day(1)))).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }
}
