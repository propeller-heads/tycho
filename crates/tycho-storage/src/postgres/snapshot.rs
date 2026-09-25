//! One-shot read of all live state for the entity cache.
//!
//! Every query filters live rows directly. On the partitioned tables a live row has
//! `valid_to = MAX_TS`. On `contract_code` and `account_balance`, and for `deleted_at` on
//! `account` and `protocol_component`, a live row is `NULL` as written or `MAX_TS` as reopened by
//! `revert_state`, so both count. Versioned reads are not needed, because one live row per key is
//! the versioning invariant, and they are much slower on tables this size. Rows are selected by a
//! subquery, never by an id list: Postgres caps bind parameters at 65,535 and a chain can hold
//! more components than that.

use std::collections::HashMap;

use chrono::NaiveDateTime;
use diesel::{
    pg::Pg, sql_types::BigInt, BoolExpressionMethods, ExpressionMethods, JoinOnDsl, QueryDsl,
};
use diesel_async::{
    pg::TransactionBuilder, pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt,
    AsyncPgConnection, RunQueryDsl,
};
use tycho_common::{
    models::{
        contract::{Account, AccountBalance},
        protocol::ProtocolComponentState,
        Address, Chain,
    },
    storage::{AccountSnapshot, ComponentSnapshot, StateSnapshot, StorageError, WriteTimestamp},
    Bytes,
};

use super::{schema, PostgresError, PostgresGateway, MAX_TS};

/// Ids of the live contracts of a chain: accounts that are not deleted and have a live code
/// row.
fn contract_ids(chain_id: i64) -> schema::account::BoxedQuery<'static, Pg, BigInt> {
    schema::account::table
        .filter(schema::account::chain_id.eq(chain_id))
        .filter(
            schema::account::deleted_at
                .is_null()
                .or(schema::account::deleted_at.eq(MAX_TS)),
        )
        .filter(
            schema::account::id.eq_any(
                schema::contract_code::table
                    .filter(
                        schema::contract_code::valid_to
                            .is_null()
                            .or(schema::contract_code::valid_to.eq(MAX_TS)),
                    )
                    .select(schema::contract_code::account_id),
            ),
        )
        .select(schema::account::id)
        .into_boxed()
}

/// Ids of the components of a chain that are not deleted.
fn component_ids(chain_id: i64) -> schema::protocol_component::BoxedQuery<'static, Pg, BigInt> {
    schema::protocol_component::table
        .filter(schema::protocol_component::chain_id.eq(chain_id))
        .filter(
            schema::protocol_component::deleted_at
                .is_null()
                .or(schema::protocol_component::deleted_at.eq(MAX_TS)),
        )
        .select(schema::protocol_component::id)
        .into_boxed()
}

impl PostgresGateway {
    /// Every live contract of `chain` with its code, balances and slots, each value stamped with
    /// the block of its row's `modify_tx`. Order is unspecified.
    ///
    /// # Errors
    ///
    /// `StorageError::NotFound("native_balance", address)` when a contract has no live native
    /// balance row, as the DB read path reports it.
    ///
    /// `StorageError::NotFound("ContractCode", address)` when a contract has no live code row.
    /// This means the live-code invariant is broken: the `contract_ids` subquery requires a live
    /// code row.
    pub(crate) async fn snapshot_accounts(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<AccountSnapshot>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let native_token_id = self.get_native_token_id(chain)?;

        let accounts: Vec<(i64, Address, String)> = schema::account::table
            .filter(schema::account::id.eq_any(contract_ids(chain_id)))
            .select((schema::account::id, schema::account::address, schema::account::title))
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut codes: HashMap<i64, (Bytes, Bytes, WriteTimestamp, Bytes)> =
            schema::contract_code::table
                .inner_join(schema::transaction::table.inner_join(schema::block::table))
                .filter(schema::contract_code::account_id.eq_any(contract_ids(chain_id)))
                .filter(
                    schema::contract_code::valid_to
                        .is_null()
                        .or(schema::contract_code::valid_to.eq(MAX_TS)),
                )
                .select((
                    schema::contract_code::account_id,
                    schema::contract_code::code,
                    schema::contract_code::hash,
                    schema::contract_code::valid_from,
                    schema::block::number,
                    schema::transaction::hash,
                ))
                .get_results::<(i64, Bytes, Bytes, NaiveDateTime, i64, Bytes)>(conn)
                .await
                .map_err(PostgresError::from)?
                .into_iter()
                .map(|(account_id, code, hash, valid_from, number, tx)| {
                    (account_id, (code, hash, WriteTimestamp::new(valid_from, number as u64), tx))
                })
                .collect();

        let mut balances: HashMap<i64, Vec<(i64, Address, Bytes, WriteTimestamp)>> = HashMap::new();
        for (account_id, token_id, token, balance, valid_from, number) in
            schema::account_balance::table
                .inner_join(schema::token::table.inner_join(schema::account::table))
                .inner_join(schema::transaction::table.inner_join(schema::block::table))
                .filter(schema::account_balance::account_id.eq_any(contract_ids(chain_id)))
                .filter(
                    schema::account_balance::valid_to
                        .is_null()
                        .or(schema::account_balance::valid_to.eq(MAX_TS)),
                )
                .select((
                    schema::account_balance::account_id,
                    schema::account_balance::token_id,
                    schema::account::address,
                    schema::account_balance::balance,
                    schema::account_balance::valid_from,
                    schema::block::number,
                ))
                .get_results::<(i64, i64, Address, Bytes, NaiveDateTime, i64)>(conn)
                .await
                .map_err(PostgresError::from)?
        {
            balances
                .entry(account_id)
                .or_default()
                .push((token_id, token, balance, WriteTimestamp::new(valid_from, number as u64)));
        }

        let mut slots: HashMap<i64, Vec<(Bytes, Option<Bytes>, WriteTimestamp)>> = HashMap::new();
        for (account_id, slot, value, valid_from, number) in schema::contract_storage::table
            .inner_join(schema::transaction::table.inner_join(schema::block::table))
            .filter(schema::contract_storage::account_id.eq_any(contract_ids(chain_id)))
            .filter(schema::contract_storage::valid_to.eq(MAX_TS))
            .select((
                schema::contract_storage::account_id,
                schema::contract_storage::slot,
                schema::contract_storage::value,
                schema::contract_storage::valid_from,
                schema::block::number,
            ))
            .get_results::<(i64, Bytes, Option<Bytes>, NaiveDateTime, i64)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            slots
                .entry(account_id)
                .or_default()
                .push((slot, value, WriteTimestamp::new(valid_from, number as u64)));
        }

        let mut out = Vec::with_capacity(accounts.len());
        for (id, address, title) in accounts {
            let (code, code_hash, code_written_at, code_tx) =
                codes.remove(&id).ok_or_else(|| {
                    StorageError::NotFound("ContractCode".to_string(), address.to_string())
                })?;
            let mut native_balance = None;
            let mut token_balances = HashMap::new();
            let mut token_balance_written_at = HashMap::new();
            for (token_id, token, balance, written_at) in balances.remove(&id).unwrap_or_default() {
                if token_id == native_token_id {
                    native_balance = Some((balance, written_at));
                    continue;
                }
                token_balances.insert(
                    token.clone(),
                    AccountBalance::new(address.clone(), token.clone(), balance, Bytes::default()),
                );
                token_balance_written_at.insert(token, written_at);
            }
            let (native_balance, native_balance_written_at) = native_balance.ok_or_else(|| {
                StorageError::NotFound("native_balance".to_string(), address.to_string())
            })?;
            let mut account_slots = HashMap::new();
            let mut slot_written_at = HashMap::new();
            for (slot, value, written_at) in slots.remove(&id).unwrap_or_default() {
                account_slots.insert(slot.clone(), value.unwrap_or_default());
                slot_written_at.insert(slot, written_at);
            }
            let account = Account::new(
                *chain,
                address,
                title,
                account_slots,
                native_balance,
                token_balances,
                code,
                code_hash,
                Bytes::zero(32),
                code_tx,
                None,
            );
            out.push(AccountSnapshot {
                account,
                slot_written_at,
                native_balance_written_at,
                code_written_at,
                token_balance_written_at,
            });
        }
        Ok(out)
    }

    /// Every component of `chain` that is not deleted, with its live attributes and balances,
    /// stamped with the newest write among its rows or, for a component without rows, the block
    /// of its `creation_tx`. Order is unspecified.
    pub(crate) async fn snapshot_components(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentSnapshot>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;

        let components: Vec<(i64, String, String, NaiveDateTime, i64)> =
            schema::protocol_component::table
                .inner_join(schema::protocol_system::table)
                .inner_join(
                    schema::transaction::table
                        .on(schema::transaction::id.eq(schema::protocol_component::creation_tx)),
                )
                .inner_join(
                    schema::block::table.on(schema::block::id.eq(schema::transaction::block_id)),
                )
                .filter(schema::protocol_component::id.eq_any(component_ids(chain_id)))
                .select((
                    schema::protocol_component::id,
                    schema::protocol_component::external_id,
                    schema::protocol_system::name,
                    schema::block::ts,
                    schema::block::number,
                ))
                .get_results(conn)
                .await
                .map_err(PostgresError::from)?;

        let mut attributes: HashMap<i64, Vec<(String, Bytes, WriteTimestamp)>> = HashMap::new();
        for (component_id, name, value, valid_from, number) in schema::protocol_state::table
            .inner_join(schema::transaction::table.inner_join(schema::block::table))
            .filter(schema::protocol_state::protocol_component_id.eq_any(component_ids(chain_id)))
            .filter(schema::protocol_state::valid_to.eq(MAX_TS))
            .select((
                schema::protocol_state::protocol_component_id,
                schema::protocol_state::attribute_name,
                schema::protocol_state::attribute_value,
                schema::protocol_state::valid_from,
                schema::block::number,
            ))
            .get_results::<(i64, String, Bytes, NaiveDateTime, i64)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            attributes
                .entry(component_id)
                .or_default()
                .push((name, value, WriteTimestamp::new(valid_from, number as u64)));
        }

        let mut balances: HashMap<i64, Vec<(Address, Bytes, WriteTimestamp)>> = HashMap::new();
        for (component_id, token, balance, valid_from, number) in schema::component_balance::table
            .inner_join(schema::token::table.inner_join(schema::account::table))
            .inner_join(schema::transaction::table.inner_join(schema::block::table))
            .filter(
                schema::component_balance::protocol_component_id.eq_any(component_ids(chain_id)),
            )
            .filter(schema::component_balance::valid_to.eq(MAX_TS))
            .select((
                schema::component_balance::protocol_component_id,
                schema::account::address,
                schema::component_balance::new_balance,
                schema::component_balance::valid_from,
                schema::block::number,
            ))
            .get_results::<(i64, Address, Bytes, NaiveDateTime, i64)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            balances
                .entry(component_id)
                .or_default()
                .push((token, balance, WriteTimestamp::new(valid_from, number as u64)));
        }

        let mut out = Vec::with_capacity(components.len());
        for (id, external_id, system, created_ts, created_number) in components {
            let mut updated_at = WriteTimestamp::new(created_ts, created_number as u64);
            let mut attrs = HashMap::new();
            for (name, value, written_at) in attributes
                .remove(&id)
                .unwrap_or_default()
            {
                updated_at = updated_at.max(written_at);
                attrs.insert(name, value);
            }
            let mut bals = HashMap::new();
            for (token, balance, written_at) in balances.remove(&id).unwrap_or_default() {
                updated_at = updated_at.max(written_at);
                bals.insert(token, balance);
            }
            out.push(ComponentSnapshot {
                system,
                state: ProtocolComponentState::new(&external_id, attrs, bals),
                updated_at,
            });
        }
        Ok(out)
    }

    /// All live state of `chain`, read inside the caller's transaction.
    pub(crate) async fn state_snapshot(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<StateSnapshot, StorageError> {
        Ok(StateSnapshot {
            accounts: self
                .snapshot_accounts(chain, conn)
                .await?,
            components: self
                .snapshot_components(chain, conn)
                .await?,
        })
    }
}

/// The isolation level a state snapshot read runs under: `READ ONLY`, `REPEATABLE READ`.
fn snapshot_transaction(conn: &mut AsyncPgConnection) -> TransactionBuilder<'_, AsyncPgConnection> {
    conn.build_transaction()
        .read_only()
        .repeatable_read()
}

/// Reads the live state of `chain` in one `REPEATABLE READ`, read-only transaction on a pooled
/// connection. Every part of the result comes from the same database snapshot.
pub(crate) async fn read_state_snapshot(
    gateway: &PostgresGateway,
    pool: &Pool<AsyncPgConnection>,
    chain: Chain,
) -> Result<StateSnapshot, StorageError> {
    let mut conn = pool.get().await.map_err(|err| {
        StorageError::Unexpected(format!("No connection for the state snapshot: {err}"))
    })?;
    snapshot_transaction(&mut conn)
        .run(|conn| {
            async move {
                gateway
                    .state_snapshot(&chain, conn)
                    .await
                    .map_err(PostgresError::from)
            }
            .scope_boxed()
        })
        .await
        .map_err(StorageError::from)
}

#[cfg(test)]
mod test_serial_db {
    use std::str::FromStr;

    use tycho_common::{keccak256, models::Chain, Bytes};

    use super::*;
    use crate::postgres::{db_fixtures, testing::run_against_db, PostgresGateway};

    const C0: &str = "6B175474E89094C44Da98b954EedeAC495271d0F";
    const C1: &str = "73BcE791c239c8010Cd3C857d96580037CCdd0EE";
    const C2: &str = "94a3F312366b8D0a32A00986194053C0ed0CdDb1";
    const USDC: &str = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    struct Fixture {
        chain_id: i64,
        txn: Vec<i64>,
        native_token: i64,
        usdc: i64,
        c0: i64,
    }

    /// Two live contracts, one deleted contract, one token-only account.
    async fn setup_accounts(conn: &mut AsyncPgConnection) -> Fixture {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        let txn = db_fixtures::insert_txns(
            conn,
            &[
                (
                    blk[0],
                    1i64,
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                (
                    blk[0],
                    2i64,
                    "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54",
                ),
                (
                    blk[1],
                    1i64,
                    "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7",
                ),
                (
                    blk[1],
                    2i64,
                    "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388",
                ),
            ],
        )
        .await;
        let (_, native_token) = db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let (_, usdc) = db_fixtures::insert_token(conn, chain_id, USDC, "USDC", 6, Some(100)).await;
        let ts = db_fixtures::yesterday_midnight();
        let ts_p1 = db_fixtures::yesterday_half_past_midnight();

        let c0 = db_fixtures::insert_account(conn, C0, "c0", chain_id, Some(txn[0])).await;
        db_fixtures::insert_contract_code(conn, c0, txn[0], Bytes::from("C0C0C0")).await;
        db_fixtures::insert_account_balance(conn, 100, native_token, txn[0], Some(&ts_p1), c0)
            .await;
        db_fixtures::insert_account_balance(conn, 101, native_token, txn[3], None, c0).await;
        db_fixtures::insert_account_balance(conn, 1000, usdc, txn[0], None, c0).await;
        // slot 0: superseded version, then live version; slot 2: one live version
        db_fixtures::insert_slots(conn, c0, txn[1], &ts, Some(&ts_p1), &[(0, 1, None)]).await;
        db_fixtures::insert_slots(conn, c0, txn[3], &ts_p1, None, &[(0, 2, Some(1))]).await;
        db_fixtures::insert_slots(conn, c0, txn[1], &ts, None, &[(2, 1, None)]).await;

        let c1 = db_fixtures::insert_account(conn, C1, "c1", chain_id, Some(txn[2])).await;
        db_fixtures::insert_contract_code(conn, c1, txn[2], Bytes::from("C1C1C1")).await;
        db_fixtures::insert_account_balance(conn, 50, native_token, txn[2], None, c1).await;
        db_fixtures::insert_slots(conn, c1, txn[3], &ts_p1, None, &[(0, 128, None)]).await;

        let c2 = db_fixtures::insert_account(conn, C2, "c2", chain_id, Some(txn[1])).await;
        db_fixtures::insert_contract_code(conn, c2, txn[1], Bytes::from("C2C2C2")).await;
        db_fixtures::insert_account_balance(conn, 25, native_token, txn[1], None, c2).await;
        db_fixtures::insert_slots(conn, c2, txn[1], &ts, None, &[(1, 2, None)]).await;
        db_fixtures::delete_account(conn, c2, &ts_p1).await;

        Fixture { chain_id, txn, native_token, usdc, c0 }
    }

    async fn setup_components(conn: &mut AsyncPgConnection, f: &Fixture) {
        let ambient = db_fixtures::insert_protocol_system(conn, "ambient".to_owned()).await;
        let zigzag = db_fixtures::insert_protocol_system(conn, "zigzag".to_owned()).await;
        let pool_type = db_fixtures::insert_protocol_type(conn, "Pool", None, None, None).await;
        let p1 = db_fixtures::insert_protocol_component(
            conn,
            "p1",
            f.chain_id,
            ambient,
            pool_type,
            f.txn[0],
            Some(vec![f.usdc]),
            None,
        )
        .await;
        // reserve: superseded version at block 1, live version at block 2
        db_fixtures::insert_protocol_state(
            conn,
            p1,
            f.txn[0],
            "reserve".to_owned(),
            Bytes::from(1u64),
            None,
            Some(f.txn[3]),
        )
        .await;
        db_fixtures::insert_protocol_state(
            conn,
            p1,
            f.txn[3],
            "reserve".to_owned(),
            Bytes::from(2u64),
            Some(Bytes::from(1u64)),
            None,
        )
        .await;
        db_fixtures::insert_protocol_state(
            conn,
            p1,
            f.txn[0],
            "fee".to_owned(),
            Bytes::from(30u64),
            None,
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Bytes::from(1000u64),
            Bytes::default(),
            1000.0,
            f.usdc,
            f.txn[1],
            p1,
            None,
        )
        .await;
        let _p2 = db_fixtures::insert_protocol_component(
            conn, "p2", f.chain_id, zigzag, pool_type, f.txn[2], None, None,
        )
        .await;
        let p3 = db_fixtures::insert_protocol_component(
            conn, "p3", f.chain_id, ambient, pool_type, f.txn[0], None, None,
        )
        .await;
        diesel::update(
            schema::protocol_component::table.filter(schema::protocol_component::id.eq(p3)),
        )
        .set(schema::protocol_component::deleted_at.eq(db_fixtures::yesterday_half_past_midnight()))
        .execute(conn)
        .await
        .unwrap();
    }

    fn by_address(accounts: Vec<AccountSnapshot>) -> HashMap<Bytes, AccountSnapshot> {
        accounts
            .into_iter()
            .map(|s| (s.account.address.clone(), s))
            .collect()
    }

    #[tokio::test]
    async fn snapshot_accounts_returns_live_contracts_with_their_write_stamps_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            setup_accounts(&mut conn).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;
            let ts = db_fixtures::yesterday_midnight();
            let ts_p1 = db_fixtures::yesterday_half_past_midnight();

            let accounts = by_address(
                gw.snapshot_accounts(&Chain::Ethereum, &mut conn)
                    .await
                    .unwrap(),
            );

            assert_eq!(accounts.len(), 2, "c2 is deleted, token accounts have no code");
            let c0 = &accounts[&Bytes::from_str(C0).unwrap()];
            assert_eq!(c0.account.title, "c0");
            assert_eq!(c0.account.code, Bytes::from("C0C0C0"));
            assert_eq!(c0.account.code_hash, Bytes::from(keccak256(Bytes::from("C0C0C0"))));
            assert_eq!(
                c0.account.code_modify_tx,
                Bytes::from_str(
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945"
                )
                .unwrap()
            );
            assert_eq!(c0.code_written_at, WriteTimestamp::new(ts, 1));
            assert_eq!(c0.account.native_balance, Bytes::from(101u64).lpad(32, 0));
            assert_eq!(c0.native_balance_written_at, WriteTimestamp::new(ts_p1, 2));
            assert_eq!(c0.account.balance_modify_tx, Bytes::zero(32));
            assert_eq!(c0.account.creation_tx, None);
            let slot0 = Bytes::from(0u64).lpad(32, 0);
            let slot2 = Bytes::from(2u64).lpad(32, 0);
            assert_eq!(
                c0.account.slots[&slot0],
                Bytes::from(2u64).lpad(32, 0),
                "live version wins"
            );
            assert_eq!(c0.slot_written_at[&slot0], WriteTimestamp::new(ts_p1, 2));
            assert_eq!(c0.slot_written_at[&slot2], WriteTimestamp::new(ts, 1));
            let usdc = Bytes::from_str(USDC).unwrap();
            assert_eq!(c0.account.token_balances[&usdc].balance, Bytes::from(1000u64).lpad(32, 0));
            assert_eq!(c0.account.token_balances[&usdc].token, usdc);
            assert_eq!(c0.account.token_balances[&usdc].account, c0.account.address);
            assert_eq!(c0.account.token_balances[&usdc].modify_tx, Bytes::default());
            assert_eq!(c0.token_balance_written_at[&usdc], WriteTimestamp::new(ts, 1));
            assert!(
                !c0.account
                    .token_balances
                    .contains_key(&Bytes::zero(20)),
                "the native balance is not a token balance"
            );
            let c1 = &accounts[&Bytes::from_str(C1).unwrap()];
            assert_eq!(c1.account.slots.len(), 1);
            assert!(c1.account.token_balances.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn snapshot_accounts_reads_a_null_slot_as_zero_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            let slot = Bytes::from(7u64).lpad(32, 0);
            diesel::insert_into(schema::contract_storage::table)
                .values((
                    schema::contract_storage::slot.eq(&slot),
                    schema::contract_storage::value.eq(None::<Bytes>),
                    schema::contract_storage::account_id.eq(f.c0),
                    schema::contract_storage::modify_tx.eq(f.txn[3]),
                    schema::contract_storage::valid_from
                        .eq(db_fixtures::yesterday_half_past_midnight()),
                    schema::contract_storage::valid_to.eq(MAX_TS),
                    schema::contract_storage::ordinal.eq(0i64),
                ))
                .execute(&mut conn)
                .await
                .unwrap();
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let accounts = by_address(
                gw.snapshot_accounts(&Chain::Ethereum, &mut conn)
                    .await
                    .unwrap(),
            );

            assert_eq!(
                accounts[&Bytes::from_str(C0).unwrap()]
                    .account
                    .slots[&slot],
                Bytes::default()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn snapshot_accounts_fails_without_a_native_balance_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            diesel::delete(
                schema::account_balance::table
                    .filter(schema::account_balance::account_id.eq(f.c0))
                    .filter(schema::account_balance::token_id.eq(f.native_token)),
            )
            .execute(&mut conn)
            .await
            .unwrap();
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let err = gw
                .snapshot_accounts(&Chain::Ethereum, &mut conn)
                .await
                .unwrap_err();

            assert!(
                matches!(err, StorageError::NotFound(ref what, _) if what == "native_balance"),
                "{err}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn snapshot_components_returns_live_state_stamped_with_the_newest_write_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let components: HashMap<String, ComponentSnapshot> = gw
                .snapshot_components(&Chain::Ethereum, &mut conn)
                .await
                .unwrap()
                .into_iter()
                .map(|c| (c.state.component_id.clone(), c))
                .collect();

            assert_eq!(components.len(), 2, "p3 is deleted");
            let p1 = &components["p1"];
            assert_eq!(p1.system, "ambient");
            assert_eq!(p1.state.attributes["reserve"], Bytes::from(2u64), "live version wins");
            assert_eq!(p1.state.attributes["fee"], Bytes::from(30u64));
            assert_eq!(p1.state.balances[&Bytes::from_str(USDC).unwrap()], Bytes::from(1000u64));
            assert_eq!(
                p1.updated_at,
                WriteTimestamp::new(db_fixtures::yesterday_half_past_midnight(), 2)
            );
            let p2 = &components["p2"];
            assert_eq!(p2.system, "zigzag");
            assert!(p2.state.attributes.is_empty());
            assert!(p2.state.balances.is_empty());
            assert_eq!(
                p2.updated_at,
                WriteTimestamp::new(db_fixtures::yesterday_half_past_midnight(), 2),
                "creation block 2"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn state_snapshot_reads_every_part_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;
            drop(conn);

            let snapshot = read_state_snapshot(&gw, &pool, Chain::Ethereum)
                .await
                .unwrap();

            assert_eq!(snapshot.accounts.len(), 2);
            assert_eq!(snapshot.components.len(), 2);
        })
        .await;
    }

    #[tokio::test]
    async fn state_snapshot_of_an_empty_chain_is_empty_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
            db_fixtures::insert_token(
                &mut conn,
                chain_id,
                "0000000000000000000000000000000000000000",
                "ETH",
                18,
                Some(100),
            )
            .await;
            let gw = PostgresGateway::from_connection(&mut conn).await;
            drop(conn);

            let snapshot = read_state_snapshot(&gw, &pool, Chain::Ethereum)
                .await
                .unwrap();

            assert!(snapshot.accounts.is_empty());
            assert!(snapshot.components.is_empty());
        })
        .await;
    }

    /// The reads run inside one `REPEATABLE READ` transaction, so a row committed by another
    /// connection after the first read is invisible to the later reads.
    #[tokio::test]
    async fn snapshot_reads_ignore_writes_after_the_transaction_started_serial_db() {
        run_against_db(|pool| async move {
            let mut writer = pool.get().await.unwrap();
            let f = setup_accounts(&mut writer).await;
            let gw = PostgresGateway::from_connection(&mut writer).await;
            let mut reader = pool.get().await.unwrap();

            let accounts = snapshot_transaction(&mut reader)
                .run(|conn| {
                    async {
                        let before = gw
                            .snapshot_accounts(&Chain::Ethereum, conn)
                            .await
                            .map_err(PostgresError::from)?;
                        assert_eq!(before.len(), 2);
                        db_fixtures::insert_slots(
                            &mut writer,
                            f.c0,
                            f.txn[3],
                            &db_fixtures::yesterday_one_am(),
                            None,
                            &[(9, 9, None)],
                        )
                        .await;
                        gw.snapshot_accounts(&Chain::Ethereum, conn)
                            .await
                            .map_err(PostgresError::from)
                    }
                    .scope_boxed()
                })
                .await
                .unwrap();

            let c0 = accounts
                .iter()
                .find(|a| a.account.title == "c0")
                .unwrap();
            assert_eq!(c0.account.slots.len(), 2, "slot 9 was committed after the snapshot");

            let mut fresh = pool.get().await.unwrap();
            let after = gw
                .snapshot_accounts(&Chain::Ethereum, &mut fresh)
                .await
                .unwrap();
            let c0_after = after
                .iter()
                .find(|a| a.account.title == "c0")
                .unwrap();
            assert_eq!(
                c0_after.account.slots.len(),
                3,
                "the committed slot is visible outside the snapshot"
            );
        })
        .await;
    }

    /// A revert reopens rows with `MAX_TS` instead of `NULL` (`revert_state`); they are live.
    #[tokio::test]
    async fn snapshot_reads_rows_reopened_by_a_revert_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            diesel::update(
                schema::contract_code::table.filter(schema::contract_code::valid_to.is_null()),
            )
            .set(schema::contract_code::valid_to.eq(MAX_TS))
            .execute(&mut conn)
            .await
            .unwrap();
            diesel::update(
                schema::account_balance::table.filter(schema::account_balance::valid_to.is_null()),
            )
            .set(schema::account_balance::valid_to.eq(MAX_TS))
            .execute(&mut conn)
            .await
            .unwrap();
            diesel::update(schema::account::table.filter(schema::account::deleted_at.is_null()))
                .set(schema::account::deleted_at.eq(MAX_TS))
                .execute(&mut conn)
                .await
                .unwrap();
            diesel::update(
                schema::protocol_component::table
                    .filter(schema::protocol_component::deleted_at.is_null()),
            )
            .set(schema::protocol_component::deleted_at.eq(MAX_TS))
            .execute(&mut conn)
            .await
            .unwrap();
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let accounts = gw
                .snapshot_accounts(&Chain::Ethereum, &mut conn)
                .await
                .unwrap();
            let components = gw
                .snapshot_components(&Chain::Ethereum, &mut conn)
                .await
                .unwrap();

            assert_eq!(accounts.len(), 2, "reopened code, balance and account rows are live");
            assert_eq!(components.len(), 2, "reopened component rows are live");
        })
        .await;
    }
}
