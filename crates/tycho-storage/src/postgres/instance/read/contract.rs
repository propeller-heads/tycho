use std::collections::HashMap;

use chrono::Utc;
use diesel::{dsl::exists, pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tracing::warn;
use tycho_common::{
    models::{
        contract::{Account, AccountBalance},
        Address, Balance, Chain, ContractId, PaginationParams,
    },
    storage::{StorageError, Version, WithTotal},
    Bytes,
};

use super::version::check_current;
use crate::postgres::{
    count_to_i64,
    instance::{
        active::active_instance,
        gateway::PostgresGatewayNew,
        schema::tycho::{account, account_balance, contract_code, contract_storage},
        InstanceId,
    },
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the contract `id` from `protocol_system`'s active instance.
    ///
    /// The same as [`Self::get_contracts`] with the one address. `NotFound` if the instance has no
    /// contract at that address, also if the protocol system has no active instance.
    pub async fn get_contract(
        &self,
        id: &ContractId,
        protocol_system: &str,
        at: Option<&Version>,
        include_slots: bool,
        conn: &mut AsyncPgConnection,
    ) -> Result<Account, StorageError> {
        let address = std::slice::from_ref(&id.address);
        self.get_contracts(&id.chain, protocol_system, Some(address), at, include_slots, None, conn)
            .await?
            .entity
            .pop()
            .ok_or_else(|| StorageError::NotFound("Account".to_string(), hex::encode(&id.address)))
    }

    /// Returns the contracts of `protocol_system`'s active instance, with their code, balances and,
    /// optionally, storage slots.
    ///
    /// - `protocol_system`: the protocol system whose copy of the contracts to read. `NotFound` for
    ///   an unknown name. A protocol system without an active instance has no contracts.
    /// - `addresses`: only the accounts at these addresses. An account without code is left out
    ///   with a warning. Without addresses, all accounts with code.
    /// - `at`: the version. `Unsupported` for a version before the head of the instance. Accounts
    ///   created after it or deleted at or before it are left out.
    /// - `include_slots`: add the storage slots. A slot without a value has empty bytes.
    /// - `pagination_params`: the page of accounts to return, in account id order. The total is
    ///   then the number of matching accounts on all pages. With `addresses`, accounts without code
    ///   count too. Without it, all matching contracts are returned, and the total is `None`.
    ///
    /// `NotFound` for an unknown chain, and for a contract without balances or without a balance
    /// of the chain's native token. `balance_modify_tx` is always 32 zero bytes and `creation_tx`
    /// is always `None`.
    #[expect(clippy::too_many_arguments, reason = "one argument per query filter")]
    pub async fn get_contracts(
        &self,
        chain: &Chain,
        protocol_system: &str,
        addresses: Option<&[Address]>,
        at: Option<&Version>,
        include_slots: bool,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<Account>>, StorageError> {
        self.get_chain_id(chain)?;
        self.get_protocol_system_id(protocol_system)?;
        let Some(instance) = active_instance(conn, protocol_system).await? else {
            return Ok(WithTotal { entity: Vec::new(), total: Some(0) });
        };
        let ts = check_current(conn, instance, at)
            .await?
            .unwrap_or_else(|| Utc::now().naive_utc());

        let filtered = || {
            let mut query = account::table
                .filter(account::instance_id.eq(instance))
                .filter(
                    account::created_at
                        .is_null()
                        .or(account::created_at.le(ts)),
                )
                .filter(
                    account::deleted_at
                        .is_null()
                        .or(account::deleted_at.gt(ts)),
                )
                .into_boxed::<Pg>();
            match addresses {
                Some(addresses) => query = query.filter(account::address.eq_any(addresses)),
                None => {
                    query = query.filter(exists(
                        contract_code::table
                            .filter(contract_code::instance_id.eq(instance))
                            .filter(contract_code::account_id.eq(account::id)),
                    ))
                }
            }
            query
        };
        let columns = (account::id, account::address, account::title);
        let (accounts, total) = match pagination_params {
            Some(pagination) => {
                let total = filtered()
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .map_err(PostgresError::from)?;
                let accounts: Vec<(i64, Address, String)> = filtered()
                    .order_by(account::id)
                    .limit(pagination.page_size)
                    .offset(pagination.offset())
                    .select(columns)
                    .load(conn)
                    .await
                    .map_err(PostgresError::from)?;
                (accounts, Some(total))
            }
            None => {
                let accounts: Vec<(i64, Address, String)> = filtered()
                    .order_by(account::id)
                    .select(columns)
                    .load(conn)
                    .await
                    .map_err(PostgresError::from)?;
                (accounts, None)
            }
        };
        let ids: Vec<i64> = accounts
            .iter()
            .map(|(id, _, _)| *id)
            .collect();

        let mut codes: HashMap<i64, (Bytes, Bytes, Bytes)> = contract_code::table
            .filter(contract_code::instance_id.eq(instance))
            .filter(contract_code::account_id.eq_any(&ids))
            .select((
                contract_code::account_id,
                (contract_code::code, contract_code::hash, contract_code::modify_tx_hash),
            ))
            .load::<(i64, (Bytes, Bytes, Bytes))>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();
        let missing: Vec<String> = accounts
            .iter()
            .filter(|(id, _, _)| !codes.contains_key(id))
            .map(|(_, address, _)| hex::encode(address))
            .collect();
        if !missing.is_empty() {
            warn!(
                missing_count = missing.len(),
                ?missing,
                "Accounts missing contract_code at this version; skipping them"
            );
        }
        let page_addresses: Vec<Address> = accounts
            .iter()
            .map(|(_, address, _)| address.clone())
            .collect();
        let mut balances =
            load_account_balances(conn, chain, instance, Some(&page_addresses), true).await?;

        let mut slots: HashMap<i64, HashMap<Bytes, Bytes>> = HashMap::new();
        if include_slots {
            let rows: Vec<(i64, Bytes, Option<Bytes>)> = contract_storage::table
                .filter(contract_storage::instance_id.eq(instance))
                .filter(contract_storage::account_id.eq_any(&ids))
                .select((
                    contract_storage::account_id,
                    contract_storage::slot,
                    contract_storage::value,
                ))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;
            for (account_id, slot, value) in rows {
                slots
                    .entry(account_id)
                    .or_default()
                    .insert(slot, value.unwrap_or_default());
            }
        }

        let mut contracts = Vec::with_capacity(accounts.len());
        for (id, address, title) in accounts {
            let Some((code, hash, modify_tx_hash)) = codes.remove(&id) else {
                continue;
            };
            let mut token_balances = balances
                .remove(&address)
                .ok_or_else(|| {
                    StorageError::NotFound("account_balances".to_string(), address.to_string())
                })?;
            let native_balance = token_balances
                .remove(&chain.native_token().address)
                .ok_or_else(|| {
                    StorageError::NotFound("native_balance".to_string(), address.to_string())
                })?;
            contracts.push(Account::new(
                *chain,
                address,
                title,
                slots.remove(&id).unwrap_or_default(),
                native_balance.balance,
                token_balances,
                code,
                hash,
                Bytes::zero(32),
                modify_tx_hash,
                None,
            ));
        }
        let total = total.unwrap_or_else(|| count_to_i64(contracts.len()));
        Ok(WithTotal { entity: contracts, total: Some(total) })
    }

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
        models::{
            contract::{Account, AccountBalance},
            Address, Chain, ContractId, PaginationParams,
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

    /// Inserts an account at `address` with the code `code`, code hash `0xc1`, code tx `0xc2`, the
    /// given slots, a native balance of `0x01` and a balance of `0x05` in the token `0x02`.
    /// Returns the account id.
    async fn contract(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
        address: &str,
        code: &str,
        slots: &[(&str, &str)],
    ) -> i64 {
        let id = account_with_balances(conn, instance, address, "0x01", "0x05").await;
        db_fixtures::insert_instance_code(conn, instance, id, b(code), b("0xc1"), b("0xc2")).await;
        let slots: Vec<(Bytes, Bytes)> = slots
            .iter()
            .map(|(slot, value)| (b(slot), b(value)))
            .collect();
        db_fixtures::insert_instance_slots(conn, instance, id, &slots).await;
        id
    }

    async fn contracts(
        gateway: &PostgresGatewayNew,
        conn: &mut AsyncPgConnection,
        system: &str,
        addresses: Option<&[Address]>,
        pagination: Option<&PaginationParams>,
        at: Option<&Version>,
    ) -> Result<(Vec<Account>, Option<i64>), StorageError> {
        let page = gateway
            .get_contracts(&Chain::Ethereum, system, addresses, at, true, pagination, conn)
            .await?;
        let mut entity = page.entity;
        if pagination.is_none() {
            entity.sort_by(|a, b| a.address.cmp(&b.address));
        }
        Ok((entity, page.total))
    }

    fn addresses(accounts: &[Account]) -> Vec<Address> {
        accounts
            .iter()
            .map(|account| account.address.clone())
            .collect()
    }

    #[tokio::test]
    async fn test_contract_of_a_protocol() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[("0x01", "0x0a"), ("0x02", "0x0b")]).await;
        let id = ContractId::new(Chain::Ethereum, b("0xaa"));

        let result = s
            .gateway
            .get_contract(&id, "vm:curve", None, true, &mut conn)
            .await
            .unwrap();

        let expected = Account::new(
            Chain::Ethereum,
            b("0xaa"),
            String::new(),
            [(b("0x01"), b("0x0a")), (b("0x02"), b("0x0b"))].into(),
            b("0x01"),
            [balance("0xaa", b("0x02"), "0x05")].into(),
            b("0xc0de"),
            b("0xc1"),
            Bytes::zero(32),
            b("0xc2"),
            None,
        );
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn test_contract_without_slots() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[("0x01", "0x0a")]).await;
        let id = ContractId::new(Chain::Ethereum, b("0xaa"));

        let result = s
            .gateway
            .get_contract(&id, "vm:curve", None, false, &mut conn)
            .await
            .unwrap();

        assert!(result.slots.is_empty());
        assert_eq!(result.code, b("0xc0de"));
    }

    #[tokio::test]
    async fn test_shared_contract_comes_from_the_protocol_copy() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de01", &[("0x01", "0x0a")]).await;
        contract(&mut conn, s.uniswap_v4, "0xaa", "0xc0de02", &[("0x01", "0x0b")]).await;

        for (system, code, value) in
            [("vm:curve", "0xc0de01", "0x0a"), ("uniswap_v4", "0xc0de02", "0x0b")]
        {
            let (result, _) = contracts(&s.gateway, &mut conn, system, None, None, None)
                .await
                .unwrap();

            assert_eq!(result.len(), 1, "{system}");
            assert_eq!(result[0].code, b(code), "{system}");
            assert_eq!(result[0].slots, [(b("0x01"), b(value))].into(), "{system}");
        }
    }

    #[tokio::test]
    async fn test_contracts_without_addresses_list_accounts_with_code() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        contract(&mut conn, s.curve, "0xbb", "0xc0de", &[]).await;
        account_with_balances(&mut conn, s.curve, "0xcc", "0x01", "0x05").await;

        let (result, total) = contracts(&s.gateway, &mut conn, "vm:curve", None, None, None)
            .await
            .unwrap();
        let (_, paged_total) = contracts(
            &s.gateway,
            &mut conn,
            "vm:curve",
            None,
            Some(&PaginationParams::new(0, 10)),
            None,
        )
        .await
        .unwrap();

        assert_eq!(addresses(&result), vec![b("0xaa"), b("0xbb")]);
        assert_eq!(total, Some(2));
        assert_eq!(paged_total, Some(2));
    }

    #[tokio::test]
    async fn test_contracts_with_addresses_skip_accounts_without_code() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        account_with_balances(&mut conn, s.curve, "0xcc", "0x01", "0x05").await;

        let (result, total) =
            contracts(&s.gateway, &mut conn, "vm:curve", Some(&[b("0xaa"), b("0xcc")]), None, None)
                .await
                .unwrap();

        assert_eq!(addresses(&result), vec![b("0xaa")]);
        assert_eq!(total, Some(1));
    }

    #[tokio::test]
    async fn test_contract_without_native_balance_fails() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = db_fixtures::insert_instance_account(&mut conn, s.curve, b("0xaa")).await;
        db_fixtures::insert_instance_account_balance(&mut conn, s.curve, id, b("0x02"), b("0x05"))
            .await;
        db_fixtures::insert_instance_code(
            &mut conn,
            s.curve,
            id,
            b("0xc0de"),
            b("0xc1"),
            b("0xc2"),
        )
        .await;

        let result = contracts(&s.gateway, &mut conn, "vm:curve", None, None, None).await;

        assert!(
            matches!(&result, Err(StorageError::NotFound(entity, _)) if entity == "native_balance"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn test_contract_without_balances_fails() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = db_fixtures::insert_instance_account(&mut conn, s.curve, b("0xaa")).await;
        db_fixtures::insert_instance_code(
            &mut conn,
            s.curve,
            id,
            b("0xc0de"),
            b("0xc1"),
            b("0xc2"),
        )
        .await;

        let result = contracts(&s.gateway, &mut conn, "vm:curve", None, None, None).await;

        assert!(
            matches!(&result, Err(StorageError::NotFound(entity, _)) if entity == "account_balances"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn test_contracts_paging() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        // Ids rise in insertion order, so the addresses are inserted out of address order.
        contract(&mut conn, s.curve, "0xcc", "0xc0de", &[]).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        contract(&mut conn, s.curve, "0xbb", "0xc0de", &[]).await;

        let (first, first_total) = contracts(
            &s.gateway,
            &mut conn,
            "vm:curve",
            None,
            Some(&PaginationParams::new(0, 2)),
            None,
        )
        .await
        .unwrap();
        let (second, second_total) = contracts(
            &s.gateway,
            &mut conn,
            "vm:curve",
            None,
            Some(&PaginationParams::new(1, 2)),
            None,
        )
        .await
        .unwrap();

        assert_eq!(addresses(&first), vec![b("0xcc"), b("0xaa")]);
        assert_eq!(addresses(&second), vec![b("0xbb")]);
        assert_eq!(first_total, Some(3));
        assert_eq!(second_total, Some(3));
    }

    #[tokio::test]
    async fn test_contracts_skip_deleted_and_inactive() {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        use crate::postgres::instance::schema::tycho::account;

        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let next = db_fixtures::insert_instance(&mut conn, s.curve_system_id, "0.0.2").await;
        let deleted = contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        diesel::update(account::table.find((s.curve, deleted)))
            .set(account::deleted_at.eq(Some(day(1))))
            .execute(&mut conn)
            .await
            .unwrap();
        contract(&mut conn, s.curve, "0xbb", "0xc0de", &[]).await;
        contract(&mut conn, next, "0xaa", "0xc0de", &[]).await;
        contract(&mut conn, next, "0xcc", "0xc0de", &[]).await;

        let (all, _) = contracts(&s.gateway, &mut conn, "vm:curve", None, None, None)
            .await
            .unwrap();
        let (by_address, _) =
            contracts(&s.gateway, &mut conn, "vm:curve", Some(&[b("0xaa")]), None, None)
                .await
                .unwrap();

        assert_eq!(addresses(&all), vec![b("0xbb")]);
        assert!(by_address.is_empty());
    }

    #[tokio::test]
    async fn test_contract_of_unknown_protocol_fails() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        let id = ContractId::new(Chain::Ethereum, b("0xaa"));

        for system in ["not-a-protocol", ""] {
            let many = contracts(&s.gateway, &mut conn, system, None, None, None).await;
            let one = s
                .gateway
                .get_contract(&id, system, None, true, &mut conn)
                .await;

            assert!(matches!(many, Err(StorageError::NotFound(..))), "{system:?}: {many:?}");
            assert!(matches!(one, Err(StorageError::NotFound(..))), "{system:?}: {one:?}");
        }
    }

    #[tokio::test]
    async fn test_contracts_of_protocol_without_active_instance_are_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, None).await;
        let id = ContractId::new(Chain::Ethereum, b("0xaa"));

        let (result, total) = contracts(&s.gateway, &mut conn, "vm:curve", None, None, None)
            .await
            .unwrap();
        let one = s
            .gateway
            .get_contract(&id, "vm:curve", None, true, &mut conn)
            .await;

        assert!(result.is_empty());
        assert_eq!(total, Some(0));
        assert!(
            matches!(&one, Err(StorageError::NotFound(entity, _)) if entity == "Account"),
            "{one:?}"
        );
    }

    #[tokio::test]
    async fn test_contracts_before_head_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, day(2)).await;

        let result =
            contracts(&s.gateway, &mut conn, "vm:curve", None, None, Some(&at(day(1)))).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }
}
