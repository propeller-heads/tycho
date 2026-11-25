use std::collections::HashMap;

use chrono::NaiveDateTime;
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

use super::version::{resolve_version, unique_by_key, unique_by_key_nested, ReadAt};
use crate::postgres::{
    count_to_i64,
    instance::{
        active::active_instance_limits,
        gateway::PostgresGatewayNew,
        schema::tycho::{
            account, account_balance, account_balance_history, contract_code,
            contract_code_history, contract_storage, contract_storage_history,
        },
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
    /// - `at`: the version. Before the head of the instance, the history tables answer too.
    ///   `Unsupported` for a version before the history horizon or before the base block of the
    ///   instance. Accounts created after it or deleted at or before it are left out.
    /// - `include_slots`: add the storage slots. A slot without a value has empty bytes.
    /// - `pagination_params`: the page of accounts to return, in account id order. The total is
    ///   then the number of matching accounts on all pages. With `addresses`, accounts without code
    ///   count too. Without it, all matching contracts are returned, and the total is their number.
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
        let Some(active) = active_instance_limits(protocol_system, conn).await? else {
            return Ok(WithTotal { entity: Vec::new(), total: Some(0) });
        };
        let read = resolve_version(&active, at, conn).await?;
        let instance = active.id;

        let filtered = || {
            let mut query = account::table
                .filter(account::instance_id.eq(instance))
                .into_boxed::<Pg>();
            query = match read {
                // No account was created or deleted after the head.
                ReadAt::Current => query.filter(account::deleted_at.is_null()),
                ReadAt::Past { ts, head: _ } => query
                    .filter(
                        account::created_at
                            .is_null()
                            .or(account::created_at.le(ts)),
                    )
                    .filter(
                        account::deleted_at
                            .is_null()
                            .or(account::deleted_at.gt(ts)),
                    ),
            };
            match addresses {
                Some(addresses) => query = query.filter(account::address.eq_any(addresses)),
                None => {
                    let current = contract_code::table
                        .filter(contract_code::instance_id.eq(instance))
                        .filter(contract_code::account_id.eq(account::id));
                    query = match read {
                        ReadAt::Current => query.filter(exists(current)),
                        // The writer can move the code of a deleted account to history, so an
                        // account deleted after `ts` can have no current code row.
                        ReadAt::Past { ts, head } => query.filter(
                            exists(current.filter(contract_code::valid_from.le(ts))).or(exists(
                                contract_code_history::table
                                    .filter(contract_code_history::instance_id.eq(instance))
                                    .filter(contract_code_history::account_id.eq(account::id))
                                    .filter(contract_code_history::valid_from.le(ts))
                                    .filter(contract_code_history::valid_to.gt(ts))
                                    .filter(contract_code_history::valid_to.le(head)),
                            )),
                        ),
                    };
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

        let mut codes = codes_at(instance, &ids, read, conn).await?;
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
            load_account_balances(chain, instance, Some(&page_addresses), true, read, conn).await?;
        let mut slots = if include_slots {
            slots_at(instance, &ids, read, conn).await?
        } else {
            HashMap::new()
        };

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
    /// - `at`: the version. Before the head of the instance, the history tables answer too.
    ///   `Unsupported` for a version before the history horizon or before the base block of the
    ///   instance.
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
        let Some(active) = active_instance_limits(protocol_system, conn).await? else {
            return Ok(HashMap::new());
        };
        let read = resolve_version(&active, at, conn).await?;
        load_account_balances(chain, active.id, accounts, include_native, read, conn).await
    }
}

/// Returns the token balances valid at `read` of the accounts of `instance`, by account address and
/// token address. `accounts`: only these accounts; `None` for all of them.
///
/// A `Past` read adds the history rows to the current rows. `Unexpected` when two rows of one
/// account and token are valid at the version.
async fn load_account_balances(
    chain: &Chain,
    instance: InstanceId,
    accounts: Option<&[Address]>,
    include_native: bool,
    read: ReadAt,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
    let native = chain.native_token().address;
    let skip = (!include_native).then_some(&native);
    let mut rows =
        current_account_balance_rows(instance, accounts, skip, read.current_until(), conn).await?;
    if let ReadAt::Past { ts, head } = read {
        rows.extend(history_account_balance_rows(instance, accounts, skip, ts, head, conn).await?);
    }
    Ok(unique_by_key_nested(
        "account_balance",
        instance,
        read,
        rows.into_iter()
            .map(|(address, token, balance)| ((address, token), balance)),
    )?
    .into_iter()
    .map(|(address, balances)| {
        let balances = balances
            .into_iter()
            .map(|(token, balance)| {
                let balance =
                    AccountBalance::new(address.clone(), token.clone(), balance, Bytes::zero(32));
                (token, balance)
            })
            .collect();
        (address, balances)
    })
    .collect())
}

/// A code row: account id, and code, code hash and the hash of the transaction that set it.
type CodeRow = (i64, (Bytes, Bytes, Bytes));

/// Returns the code valid at `read` of the accounts `ids` of `instance`, by account id, as code,
/// code hash and the hash of the transaction that set it.
///
/// A `Past` read adds the history rows to the current rows. `Unexpected` when two code rows of one
/// account are valid at the version.
async fn codes_at(
    instance: InstanceId,
    ids: &[i64],
    read: ReadAt,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<i64, (Bytes, Bytes, Bytes)>, StorageError> {
    let mut rows = current_code_rows(instance, ids, read.current_until(), conn).await?;
    if let ReadAt::Past { ts, head } = read {
        rows.extend(history_code_rows(instance, ids, ts, head, conn).await?);
    }
    unique_by_key("contract_code", instance, read, rows)
}

/// Loads the current code rows of the accounts `ids` of `instance`.
/// `ts`: only rows with `valid_from <= ts`.
async fn current_code_rows(
    instance: InstanceId,
    ids: &[i64],
    ts: Option<NaiveDateTime>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<CodeRow>, StorageError> {
    let mut query = contract_code::table
        .filter(contract_code::instance_id.eq(instance))
        .filter(contract_code::account_id.eq_any(ids))
        .select((
            contract_code::account_id,
            (contract_code::code, contract_code::hash, contract_code::modify_tx_hash),
        ))
        .into_boxed::<Pg>();
    if let Some(ts) = ts {
        query = query.filter(contract_code::valid_from.le(ts));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// Loads the history code rows of the accounts `ids` of `instance` that are valid at `ts`:
/// `valid_from <= ts < valid_to`, with `valid_to <= head` (see [`ReadAt::Past`]).
async fn history_code_rows(
    instance: InstanceId,
    ids: &[i64],
    ts: NaiveDateTime,
    head: NaiveDateTime,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<CodeRow>, StorageError> {
    Ok(contract_code_history::table
        .filter(contract_code_history::instance_id.eq(instance))
        .filter(contract_code_history::account_id.eq_any(ids))
        .filter(contract_code_history::valid_from.le(ts))
        .filter(contract_code_history::valid_to.gt(ts))
        .filter(contract_code_history::valid_to.le(head))
        .select((
            contract_code_history::account_id,
            (
                contract_code_history::code,
                contract_code_history::hash,
                contract_code_history::modify_tx_hash,
            ),
        ))
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// A slot row: account id, slot and value. A deleted slot has no value.
type SlotRow = (i64, Bytes, Option<Bytes>);

/// Returns the storage slots valid at `read` of the accounts `ids` of `instance`, by account id and
/// slot. A slot without a value has empty bytes.
///
/// A `Past` read adds the history rows to the current rows. `Unexpected` when two rows of one
/// account and slot are valid at the version.
async fn slots_at(
    instance: InstanceId,
    ids: &[i64],
    read: ReadAt,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<i64, HashMap<Bytes, Bytes>>, StorageError> {
    let mut rows = current_slot_rows(instance, ids, read.current_until(), conn).await?;
    if let ReadAt::Past { ts, head } = read {
        rows.extend(history_slot_rows(instance, ids, ts, head, conn).await?);
    }
    unique_by_key_nested(
        "contract_storage",
        instance,
        read,
        rows.into_iter()
            .map(|(account_id, slot, value)| ((account_id, slot), value.unwrap_or_default())),
    )
}

/// Loads the current slot rows of the accounts `ids` of `instance`.
/// `ts`: only rows with `valid_from <= ts`.
async fn current_slot_rows(
    instance: InstanceId,
    ids: &[i64],
    ts: Option<NaiveDateTime>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<SlotRow>, StorageError> {
    let mut query = contract_storage::table
        .filter(contract_storage::instance_id.eq(instance))
        .filter(contract_storage::account_id.eq_any(ids))
        .select((contract_storage::account_id, contract_storage::slot, contract_storage::value))
        .into_boxed::<Pg>();
    if let Some(ts) = ts {
        query = query.filter(contract_storage::valid_from.le(ts));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// Loads the history slot rows of the accounts `ids` of `instance` that are valid at `ts`:
/// `valid_from <= ts < valid_to`, with `valid_to <= head` (see [`ReadAt::Past`]).
async fn history_slot_rows(
    instance: InstanceId,
    ids: &[i64],
    ts: NaiveDateTime,
    head: NaiveDateTime,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<SlotRow>, StorageError> {
    Ok(contract_storage_history::table
        .filter(contract_storage_history::instance_id.eq(instance))
        .filter(contract_storage_history::account_id.eq_any(ids))
        .filter(contract_storage_history::valid_from.le(ts))
        .filter(contract_storage_history::valid_to.gt(ts))
        .filter(contract_storage_history::valid_to.le(head))
        .select((
            contract_storage_history::account_id,
            contract_storage_history::slot,
            contract_storage_history::value,
        ))
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// An account balance row: account address, token and balance.
type AccountBalanceRow = (Address, Address, Balance);

/// Loads the current balance rows of the accounts of `instance`.
/// `accounts`: only these accounts; `None` for all of them. `skip`: leave out the balances of this
/// token. `ts`: only rows with `valid_from <= ts`.
async fn current_account_balance_rows(
    instance: InstanceId,
    accounts: Option<&[Address]>,
    skip: Option<&Address>,
    ts: Option<NaiveDateTime>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<AccountBalanceRow>, StorageError> {
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
    if let Some(ts) = ts {
        query = query.filter(account_balance::valid_from.le(ts));
    }
    if let Some(accounts) = accounts {
        query = query.filter(account::address.eq_any(accounts));
    }
    if let Some(skip) = skip {
        query = query.filter(account_balance::token.ne(skip));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

/// Loads the history balance rows of the accounts of `instance` that are valid at `ts`:
/// `valid_from <= ts < valid_to`, with `valid_to <= head` (see [`ReadAt::Past`]).
/// `accounts`: only these accounts; `None` for all of them. `skip`: leave out the balances of this
/// token.
async fn history_account_balance_rows(
    instance: InstanceId,
    accounts: Option<&[Address]>,
    skip: Option<&Address>,
    ts: NaiveDateTime,
    head: NaiveDateTime,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<AccountBalanceRow>, StorageError> {
    let mut query = account_balance_history::table
        .inner_join(
            account::table.on(account::instance_id
                .eq(account_balance_history::instance_id)
                .and(account::id.eq(account_balance_history::account_id))),
        )
        .filter(account_balance_history::instance_id.eq(instance))
        .filter(account::instance_id.eq(instance))
        .filter(account_balance_history::valid_from.le(ts))
        .filter(account_balance_history::valid_to.gt(ts))
        .filter(account_balance_history::valid_to.le(head))
        .select((
            account::address,
            account_balance_history::token,
            account_balance_history::balance,
        ))
        .into_boxed::<Pg>();
    if let Some(accounts) = accounts {
        query = query.filter(account::address.eq_any(accounts));
    }
    if let Some(skip) = skip {
        query = query.filter(account_balance_history::token.ne(skip));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
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
        storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
        Bytes,
    };

    use crate::postgres::{
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            read::testing::{before_retention, recent},
            InstanceId,
        },
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
    async fn test_account_balances_before_retention_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = balances(
            &s.gateway,
            &mut conn,
            "vm:curve",
            true,
            Some(&at(before_retention(recent(5)))),
        )
        .await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("retention"), "{msg}");
    }

    /// Inserts a version of the balance of `token` in an account of curve.
    async fn balance_version(
        conn: &mut AsyncPgConnection,
        s: &Setup,
        account: i64,
        token: Address,
        value: &str,
        valid_from: NaiveDateTime,
        valid_to: Option<NaiveDateTime>,
    ) {
        db_fixtures::insert_instance_account_balance_version(
            conn,
            s.curve,
            account,
            token,
            b(value),
            valid_from,
            valid_to,
        )
        .await;
    }

    #[tokio::test]
    async fn test_account_balances_at_a_past_version() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = db_fixtures::insert_instance_account(&mut conn, s.curve, b("0xaa")).await;
        db_fixtures::insert_instance_account_balance(&mut conn, s.curve, id, eth(), b("0x01"))
            .await;
        balance_version(&mut conn, &s, id, b("0x02"), "0x04", recent(1), Some(recent(3))).await;
        balance_version(&mut conn, &s, id, b("0x02"), "0x05", recent(3), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, token_balance) in [(recent(2), "0x04"), (recent(4), "0x05")] {
            let result = balances(&s.gateway, &mut conn, "vm:curve", true, Some(&at(ts)))
                .await
                .unwrap();

            assert_eq!(
                result,
                [(
                    b("0xaa"),
                    [balance("0xaa", eth(), "0x01"), balance("0xaa", b("0x02"), token_balance)]
                        .into()
                )]
                .into(),
                "{ts}"
            );
        }
    }

    #[tokio::test]
    async fn test_account_balances_at_a_past_version_skip_other_instances() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = db_fixtures::insert_instance_account(&mut conn, s.curve, b("0xaa")).await;
        balance_version(&mut conn, &s, id, b("0x02"), "0x04", recent(1), Some(recent(3))).await;
        db_fixtures::insert_instance_account_balance_version(
            &mut conn,
            s.uniswap_v4,
            id,
            b("0x02"),
            b("0x06"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = balances(&s.gateway, &mut conn, "vm:curve", true, Some(&at(recent(2))))
            .await
            .unwrap();

        assert_eq!(result, [(b("0xaa"), [balance("0xaa", b("0x02"), "0x04")].into())].into());
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
    async fn test_contracts_before_retention_are_unsupported() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = contracts(
            &s.gateway,
            &mut conn,
            "vm:curve",
            None,
            None,
            Some(&at(before_retention(recent(5)))),
        )
        .await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("retention"), "{msg}");
    }

    #[tokio::test]
    async fn test_contracts_before_the_base_block_are_unsupported() {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        use crate::postgres::instance::schema::tycho::protocol_instance;

        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;
        diesel::update(protocol_instance::table.find(s.curve))
            .set(protocol_instance::base_block_ts.eq(recent(2)))
            .execute(&mut conn)
            .await
            .unwrap();

        let result =
            contracts(&s.gateway, &mut conn, "vm:curve", None, None, Some(&at(recent(1)))).await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("base block"), "{msg}");
    }

    /// Inserts a version of the code of an account of curve.
    async fn code_version(
        conn: &mut AsyncPgConnection,
        s: &Setup,
        account: i64,
        code: &str,
        valid_from: NaiveDateTime,
        valid_to: Option<NaiveDateTime>,
    ) {
        db_fixtures::insert_instance_code_version(
            conn,
            s.curve,
            account,
            b(code),
            valid_from,
            valid_to,
        )
        .await;
    }

    /// Inserts a version of a storage slot of an account of curve. `value` `None`: a deleted slot.
    async fn slot_version(
        conn: &mut AsyncPgConnection,
        s: &Setup,
        account: i64,
        slot: &str,
        value: Option<&str>,
        valid_from: NaiveDateTime,
        valid_to: Option<NaiveDateTime>,
    ) {
        db_fixtures::insert_instance_slot_version(
            conn,
            s.curve,
            account,
            b(slot),
            value.map(b),
            valid_from,
            valid_to,
        )
        .await;
    }

    /// Sets when an account of curve was created and deleted.
    async fn set_lifetime(
        conn: &mut AsyncPgConnection,
        s: &Setup,
        id: i64,
        created_at: Option<NaiveDateTime>,
        deleted_at: Option<NaiveDateTime>,
    ) {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        use crate::postgres::instance::schema::tycho::account;

        diesel::update(account::table.find((s.curve, id)))
            .set((account::created_at.eq(created_at), account::deleted_at.eq(deleted_at)))
            .execute(conn)
            .await
            .unwrap();
    }

    /// Reads the contracts of curve at `ts`, without addresses, on the page `pagination` if set.
    async fn curve_contracts_at(
        s: &Setup,
        conn: &mut AsyncPgConnection,
        pagination: Option<&PaginationParams>,
        ts: NaiveDateTime,
    ) -> Result<(Vec<Account>, Option<i64>), StorageError> {
        contracts(&s.gateway, conn, "vm:curve", None, pagination, Some(&at(ts))).await
    }

    #[tokio::test]
    async fn test_contracts_at_a_past_version_read_history() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = db_fixtures::insert_instance_account(&mut conn, s.curve, b("0xaa")).await;
        code_version(&mut conn, &s, id, "0xc0de01", recent(1), Some(recent(3))).await;
        code_version(&mut conn, &s, id, "0xc0de02", recent(3), None).await;
        slot_version(&mut conn, &s, id, "0x01", Some("0x0a"), recent(1), Some(recent(3))).await;
        slot_version(&mut conn, &s, id, "0x01", Some("0x0b"), recent(3), None).await;
        db_fixtures::insert_instance_slots(&mut conn, s.curve, id, &[(b("0x02"), b("0x0c"))]).await;
        balance_version(&mut conn, &s, id, eth(), "0x01", recent(1), Some(recent(3))).await;
        balance_version(&mut conn, &s, id, eth(), "0x02", recent(3), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, code, slot, native) in
            [(recent(2), "0xc0de01", "0x0a", "0x01"), (recent(4), "0xc0de02", "0x0b", "0x02")]
        {
            let (result, _) = curve_contracts_at(&s, &mut conn, None, ts)
                .await
                .unwrap();

            assert_eq!(addresses(&result), vec![b("0xaa")], "{ts}");
            assert_eq!(result[0].code, b(code), "{ts}");
            assert_eq!(
                result[0].slots,
                [(b("0x01"), b(slot)), (b("0x02"), b("0x0c"))].into(),
                "{ts}"
            );
            assert_eq!(result[0].native_balance, b(native), "{ts}");
        }
    }

    #[tokio::test]
    async fn test_contracts_at_a_past_version_read_a_deleted_slot_as_empty() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        slot_version(&mut conn, &s, id, "0x01", Some("0x0a"), recent(1), Some(recent(3))).await;
        slot_version(&mut conn, &s, id, "0x01", None, recent(3), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, value) in [(recent(4), Bytes::new()), (recent(2), b("0x0a"))] {
            let (result, _) = curve_contracts_at(&s, &mut conn, None, ts)
                .await
                .unwrap();

            assert_eq!(result[0].slots, [(b("0x01"), value)].into(), "{ts}");
        }
    }

    #[tokio::test]
    async fn test_contracts_at_a_past_version_skip_deleted_and_new_accounts() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let deleted = contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        set_lifetime(&mut conn, &s, deleted, None, Some(recent(2))).await;
        let created = contract(&mut conn, s.curve, "0xbb", "0xc0de", &[]).await;
        set_lifetime(&mut conn, &s, created, Some(recent(3)), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        for (ts, expected) in
            [(recent(1), vec![b("0xaa")]), (recent(2), vec![]), (recent(3), vec![b("0xbb")])]
        {
            let (result, _) = curve_contracts_at(&s, &mut conn, None, ts)
                .await
                .unwrap();

            assert_eq!(addresses(&result), expected, "{ts}");
        }
    }

    #[tokio::test]
    async fn test_contracts_without_addresses_at_a_past_version_list_deleted_accounts() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = account_with_balances(&mut conn, s.curve, "0xbb", "0x01", "0x05").await;
        code_version(&mut conn, &s, id, "0xc0de", day(1), Some(recent(3))).await;
        set_lifetime(&mut conn, &s, id, None, Some(recent(3))).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;
        let page = PaginationParams::new(0, 10);

        let (before, _) = curve_contracts_at(&s, &mut conn, None, recent(2))
            .await
            .unwrap();
        let (paged, paged_total) = curve_contracts_at(&s, &mut conn, Some(&page), recent(2))
            .await
            .unwrap();
        let (after, after_total) = curve_contracts_at(&s, &mut conn, Some(&page), recent(4))
            .await
            .unwrap();

        assert_eq!(addresses(&before), vec![b("0xbb")]);
        assert_eq!(before[0].code, b("0xc0de"));
        assert_eq!(addresses(&paged), vec![b("0xbb")]);
        assert_eq!(paged_total, Some(1));
        assert!(after.is_empty(), "{after:?}");
        assert_eq!(after_total, Some(0));
    }

    #[tokio::test]
    async fn test_contracts_without_addresses_at_a_past_version_skip_code_from_later() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        code_version(&mut conn, &s, id, "0xc0de", recent(3), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;
        let page = PaginationParams::new(0, 10);

        let (before, before_total) = curve_contracts_at(&s, &mut conn, Some(&page), recent(2))
            .await
            .unwrap();
        let (after, after_total) = curve_contracts_at(&s, &mut conn, Some(&page), recent(4))
            .await
            .unwrap();

        assert!(before.is_empty(), "{before:?}");
        assert_eq!(before_total, Some(0));
        assert_eq!(addresses(&after), vec![b("0xaa")]);
        assert_eq!(after_total, Some(1));
    }

    #[tokio::test]
    async fn test_contracts_at_a_past_version_find_unchanged_code() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        contract(&mut conn, s.curve, "0xaa", "0xc0de", &[("0x01", "0x0a")]).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;
        let id = ContractId::new(Chain::Ethereum, b("0xaa"));

        for ts in [recent(1), recent(4)] {
            let (all, _) = curve_contracts_at(&s, &mut conn, None, ts)
                .await
                .unwrap();
            let one = s
                .gateway
                .get_contract(&id, "vm:curve", Some(&at(ts)), true, &mut conn)
                .await
                .unwrap();

            assert_eq!(addresses(&all), vec![b("0xaa")], "{ts}");
            assert_eq!(one.code, b("0xc0de"), "{ts}");
            assert_eq!(one.slots, [(b("0x01"), b("0x0a"))].into(), "{ts}");
        }
    }

    #[tokio::test]
    async fn test_contracts_at_a_past_version_skip_other_instances() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        code_version(&mut conn, &s, id, "0xc0de01", recent(1), Some(recent(3))).await;
        code_version(&mut conn, &s, id, "0xc0de02", recent(3), None).await;
        slot_version(&mut conn, &s, id, "0x01", Some("0x0a"), recent(1), Some(recent(3))).await;
        slot_version(&mut conn, &s, id, "0x01", Some("0x0b"), recent(3), None).await;
        db_fixtures::insert_instance_code_version(
            &mut conn,
            s.uniswap_v4,
            id,
            b("0xdead"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::insert_instance_slot_version(
            &mut conn,
            s.uniswap_v4,
            id,
            b("0x01"),
            Some(b("0xbb")),
            recent(1),
            Some(recent(3)),
        )
        .await;
        let later = account_with_balances(&mut conn, s.curve, "0xbb", "0x01", "0x05").await;
        code_version(&mut conn, &s, later, "0xc0de03", recent(3), None).await;
        db_fixtures::insert_instance_code_version(
            &mut conn,
            s.uniswap_v4,
            later,
            b("0xdead"),
            recent(1),
            Some(recent(3)),
        )
        .await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let page = PaginationParams::new(0, 10);

        let (result, total) = curve_contracts_at(&s, &mut conn, Some(&page), recent(2))
            .await
            .unwrap();

        assert_eq!(addresses(&result), vec![b("0xaa")]);
        assert_eq!(total, Some(1));
        assert_eq!(result[0].code, b("0xc0de01"));
        assert_eq!(result[0].slots, [(b("0x01"), b("0x0a"))].into());
    }

    #[tokio::test]
    async fn test_contracts_with_two_valid_code_rows_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let id = account_with_balances(&mut conn, s.curve, "0xaa", "0x01", "0x05").await;
        code_version(&mut conn, &s, id, "0xc0de01", recent(1), Some(recent(3))).await;
        code_version(&mut conn, &s, id, "0xc0de02", recent(1), None).await;
        db_fixtures::set_instance_head(&mut conn, s.curve, recent(5)).await;

        let result = curve_contracts_at(&s, &mut conn, None, recent(2)).await;

        assert!(matches!(result, Err(StorageError::Unexpected(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_contracts_at_latest_read_up_to_the_head() {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        use crate::postgres::instance::schema::tycho::account;

        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        // A head ahead of this machine's clock, as with a block timestamp past the local time.
        let head = chrono::Utc::now().naive_utc() + chrono::Duration::hours(1);
        let created = contract(&mut conn, s.curve, "0xaa", "0xc0de", &[]).await;
        diesel::update(account::table.find((s.curve, created)))
            .set(account::created_at.eq(Some(head)))
            .execute(&mut conn)
            .await
            .unwrap();
        db_fixtures::set_instance_head(&mut conn, s.curve, head).await;
        let latest = Version(
            BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)),
            VersionKind::Last,
        );

        let (result, _) = contracts(&s.gateway, &mut conn, "vm:curve", None, None, Some(&latest))
            .await
            .unwrap();

        assert_eq!(addresses(&result), vec![b("0xaa")]);
    }
}
