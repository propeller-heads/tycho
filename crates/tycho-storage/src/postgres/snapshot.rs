//! One-shot read of all live state for the entity cache.
//!
//! Every query filters live rows directly: `valid_to = MAX_TS` on the partitioned tables,
//! `valid_to IS NULL` on `contract_code` and `account_balance`. Versioned reads are not needed,
//! because one live row per key is the versioning invariant, and they are much slower on tables
//! this size.

// Wired into the startup load in a follow-up commit.
#![allow(dead_code)]

use std::collections::HashMap;

use chrono::NaiveDateTime;
use diesel::{ExpressionMethods, QueryDsl};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tokio::sync::mpsc;
use tycho_common::{
    models::{
        contract::{Account, AccountBalance},
        protocol::ProtocolComponentState,
        Address, Chain,
    },
    storage::{AccountSnapshot, ComponentSnapshot, SnapshotChunk, StorageError, WriteTimestamp},
    Bytes,
};

use super::{schema, PostgresError, PostgresGateway, MAX_TS};

pub(crate) type ChunkSender = mpsc::Sender<Result<SnapshotChunk, StorageError>>;

/// Account ids per `Accounts` chunk.
pub(crate) const ACCOUNT_CHUNK: i64 = 500;

/// Component ids per `Components` chunk.
pub(crate) const COMPONENT_CHUNK: i64 = 5_000;

fn receiver_gone() -> StorageError {
    StorageError::Unexpected("Snapshot receiver dropped before the snapshot ended".to_string())
}

impl PostgresGateway {
    /// Sends every live contract of `chain` as `SnapshotChunk::Accounts`, `chunk_size` accounts
    /// per chunk, in ascending `account.id` order. A contract is an account that is not deleted
    /// and has a live code row.
    ///
    /// # Errors
    ///
    /// `StorageError::NotFound("native_balance", address)` when a contract has no live native
    /// balance row, as the DB read path reports it. `StorageError::Unexpected` when the receiver
    /// is gone.
    pub(crate) async fn snapshot_accounts(
        &self,
        chain: &Chain,
        chunk_size: i64,
        tx: &ChunkSender,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let native_token_id = self.get_native_token_id(chain)?;
        let mut last_id = 0i64;
        loop {
            let accounts: Vec<(i64, Address, String)> = schema::account::table
                .filter(schema::account::chain_id.eq(chain_id))
                .filter(schema::account::deleted_at.is_null())
                .filter(schema::account::id.gt(last_id))
                .filter(
                    schema::account::id.eq_any(
                        schema::contract_code::table
                            .filter(schema::contract_code::valid_to.is_null())
                            .select(schema::contract_code::account_id),
                    ),
                )
                .order_by(schema::account::id)
                .limit(chunk_size)
                .select((schema::account::id, schema::account::address, schema::account::title))
                .get_results(conn)
                .await
                .map_err(PostgresError::from)?;
            let Some((chunk_last, _, _)) = accounts.last() else {
                return Ok(());
            };
            last_id = *chunk_last;
            let chunk = self
                .assemble_accounts(chain, native_token_id, accounts, conn)
                .await?;
            let done = (chunk.len() as i64) < chunk_size;
            tx.send(Ok(SnapshotChunk::Accounts(chunk)))
                .await
                .map_err(|_| receiver_gone())?;
            if done {
                return Ok(());
            }
        }
    }

    /// Reads code, balances and slots of `accounts` and builds one snapshot per account.
    async fn assemble_accounts(
        &self,
        chain: &Chain,
        native_token_id: i64,
        accounts: Vec<(i64, Address, String)>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<AccountSnapshot>, StorageError> {
        let ids: Vec<i64> = accounts
            .iter()
            .map(|(id, _, _)| *id)
            .collect();

        let mut codes: HashMap<i64, (Bytes, Bytes, WriteTimestamp, Bytes)> =
            schema::contract_code::table
                .inner_join(schema::transaction::table.inner_join(schema::block::table))
                .filter(schema::contract_code::account_id.eq_any(&ids))
                .filter(schema::contract_code::valid_to.is_null())
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

        let balance_rows: Vec<(i64, i64, Bytes, NaiveDateTime, i64)> =
            schema::account_balance::table
                .inner_join(schema::transaction::table.inner_join(schema::block::table))
                .filter(schema::account_balance::account_id.eq_any(&ids))
                .filter(schema::account_balance::valid_to.is_null())
                .select((
                    schema::account_balance::account_id,
                    schema::account_balance::token_id,
                    schema::account_balance::balance,
                    schema::account_balance::valid_from,
                    schema::block::number,
                ))
                .get_results(conn)
                .await
                .map_err(PostgresError::from)?;
        let token_ids: Vec<i64> = balance_rows
            .iter()
            .map(|(_, token_id, _, _, _)| *token_id)
            .filter(|token_id| *token_id != native_token_id)
            .collect();
        let token_addresses: HashMap<i64, Address> = schema::token::table
            .inner_join(schema::account::table)
            .filter(schema::token::id.eq_any(&token_ids))
            .select((schema::token::id, schema::account::address))
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();
        let mut balances: HashMap<i64, Vec<(i64, Bytes, WriteTimestamp)>> = HashMap::new();
        for (account_id, token_id, balance, valid_from, number) in balance_rows {
            balances
                .entry(account_id)
                .or_default()
                .push((token_id, balance, WriteTimestamp::new(valid_from, number as u64)));
        }

        let mut slots: HashMap<i64, Vec<(Bytes, Option<Bytes>, WriteTimestamp)>> = HashMap::new();
        for (account_id, slot, value, valid_from, number) in schema::contract_storage::table
            .inner_join(schema::transaction::table.inner_join(schema::block::table))
            .filter(schema::contract_storage::account_id.eq_any(&ids))
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
            for (token_id, balance, written_at) in balances.remove(&id).unwrap_or_default() {
                if token_id == native_token_id {
                    native_balance = Some((balance, written_at));
                    continue;
                }
                let token = token_addresses
                    .get(&token_id)
                    .ok_or_else(|| {
                        StorageError::NotFound("Token".to_string(), token_id.to_string())
                    })?
                    .clone();
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

    /// Sends every component of `chain` that is not deleted as `SnapshotChunk::Components`,
    /// `chunk_size` components per chunk, in ascending `protocol_component.id` order. A
    /// component with no live attribute and no balance row is sent with empty maps and the block
    /// of its `creation_tx` as `updated_at`.
    pub(crate) async fn snapshot_components(
        &self,
        chain: &Chain,
        chunk_size: i64,
        tx: &ChunkSender,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let mut last_id = 0i64;
        loop {
            let components: Vec<(i64, String, String, i64)> = schema::protocol_component::table
                .inner_join(schema::protocol_system::table)
                .filter(schema::protocol_component::chain_id.eq(chain_id))
                .filter(schema::protocol_component::deleted_at.is_null())
                .filter(schema::protocol_component::id.gt(last_id))
                .order_by(schema::protocol_component::id)
                .limit(chunk_size)
                .select((
                    schema::protocol_component::id,
                    schema::protocol_component::external_id,
                    schema::protocol_system::name,
                    schema::protocol_component::creation_tx,
                ))
                .get_results(conn)
                .await
                .map_err(PostgresError::from)?;
            let Some((chunk_last, _, _, _)) = components.last() else {
                return Ok(());
            };
            last_id = *chunk_last;
            let chunk = Self::assemble_components(components, conn).await?;
            let done = (chunk.len() as i64) < chunk_size;
            tx.send(Ok(SnapshotChunk::Components(chunk)))
                .await
                .map_err(|_| receiver_gone())?;
            if done {
                return Ok(());
            }
        }
    }

    /// Reads attributes and balances of `components` and builds one snapshot per component.
    async fn assemble_components(
        components: Vec<(i64, String, String, i64)>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentSnapshot>, StorageError> {
        let ids: Vec<i64> = components
            .iter()
            .map(|(id, _, _, _)| *id)
            .collect();

        let creation_txs: Vec<i64> = components
            .iter()
            .map(|(_, _, _, tx)| *tx)
            .collect();
        let created_at: HashMap<i64, WriteTimestamp> = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq_any(&creation_txs))
            .select((schema::transaction::id, schema::block::ts, schema::block::number))
            .get_results::<(i64, NaiveDateTime, i64)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .map(|(tx, ts, number)| (tx, WriteTimestamp::new(ts, number as u64)))
            .collect();

        let mut attributes: HashMap<i64, Vec<(String, Bytes, WriteTimestamp)>> = HashMap::new();
        for (component_id, name, value, valid_from, number) in schema::protocol_state::table
            .inner_join(schema::transaction::table.inner_join(schema::block::table))
            .filter(schema::protocol_state::protocol_component_id.eq_any(&ids))
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

        let balance_rows: Vec<(i64, i64, Bytes, NaiveDateTime, i64)> =
            schema::component_balance::table
                .inner_join(schema::transaction::table.inner_join(schema::block::table))
                .filter(schema::component_balance::protocol_component_id.eq_any(&ids))
                .filter(schema::component_balance::valid_to.eq(MAX_TS))
                .select((
                    schema::component_balance::protocol_component_id,
                    schema::component_balance::token_id,
                    schema::component_balance::new_balance,
                    schema::component_balance::valid_from,
                    schema::block::number,
                ))
                .get_results(conn)
                .await
                .map_err(PostgresError::from)?;
        let token_ids: Vec<i64> = balance_rows
            .iter()
            .map(|(_, token_id, _, _, _)| *token_id)
            .collect();
        let token_addresses: HashMap<i64, Address> = schema::token::table
            .inner_join(schema::account::table)
            .filter(schema::token::id.eq_any(&token_ids))
            .select((schema::token::id, schema::account::address))
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();
        let mut balances: HashMap<i64, Vec<(i64, Bytes, WriteTimestamp)>> = HashMap::new();
        for (component_id, token_id, balance, valid_from, number) in balance_rows {
            balances
                .entry(component_id)
                .or_default()
                .push((token_id, balance, WriteTimestamp::new(valid_from, number as u64)));
        }

        let mut out = Vec::with_capacity(components.len());
        for (id, external_id, system, creation_tx) in components {
            let mut updated_at = *created_at
                .get(&creation_tx)
                .ok_or_else(|| {
                    StorageError::NotFound("Transaction".to_string(), creation_tx.to_string())
                })?;
            let mut attrs = HashMap::new();
            for (name, value, written_at) in attributes
                .remove(&id)
                .unwrap_or_default()
            {
                updated_at = updated_at.max(written_at);
                attrs.insert(name, value);
            }
            let mut bals = HashMap::new();
            for (token_id, balance, written_at) in balances.remove(&id).unwrap_or_default() {
                let token = token_addresses
                    .get(&token_id)
                    .ok_or_else(|| {
                        StorageError::NotFound("Token".to_string(), token_id.to_string())
                    })?
                    .clone();
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

    async fn collect_components(
        gw: &PostgresGateway,
        chunk_size: i64,
        conn: &mut AsyncPgConnection,
    ) -> Vec<Vec<ComponentSnapshot>> {
        let (tx, mut rx) = mpsc::channel(16);
        gw.snapshot_components(&Chain::Ethereum, chunk_size, &tx, conn)
            .await
            .unwrap();
        drop(tx);
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            match chunk.unwrap() {
                SnapshotChunk::Components(components) => chunks.push(components),
                other => panic!("unexpected chunk {other:?}"),
            }
        }
        chunks
    }

    async fn collect_accounts(
        gw: &PostgresGateway,
        chunk_size: i64,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Vec<AccountSnapshot>>, StorageError> {
        let (tx, mut rx) = mpsc::channel(16);
        gw.snapshot_accounts(&Chain::Ethereum, chunk_size, &tx, conn)
            .await?;
        drop(tx);
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            match chunk? {
                SnapshotChunk::Accounts(accounts) => chunks.push(accounts),
                other => panic!("unexpected chunk {other:?}"),
            }
        }
        Ok(chunks)
    }

    fn by_address(chunks: Vec<Vec<AccountSnapshot>>) -> HashMap<Bytes, AccountSnapshot> {
        chunks
            .into_iter()
            .flatten()
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
                collect_accounts(&gw, 500, &mut conn)
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
    async fn snapshot_accounts_splits_chunks_by_account_id_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            setup_accounts(&mut conn).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let chunks = collect_accounts(&gw, 1, &mut conn)
                .await
                .unwrap();

            assert_eq!(
                chunks
                    .iter()
                    .map(Vec::len)
                    .collect::<Vec<_>>(),
                vec![1, 1]
            );
            assert_eq!(chunks[0][0].account.title, "c0");
            assert_eq!(chunks[1][0].account.title, "c1");
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
                collect_accounts(&gw, 500, &mut conn)
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

            let err = collect_accounts(&gw, 500, &mut conn)
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

            let components: HashMap<String, ComponentSnapshot> =
                collect_components(&gw, 5_000, &mut conn)
                    .await
                    .into_iter()
                    .flatten()
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
    async fn snapshot_components_splits_chunks_by_component_id_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let chunks = collect_components(&gw, 1, &mut conn).await;

            assert_eq!(
                chunks
                    .iter()
                    .map(Vec::len)
                    .collect::<Vec<_>>(),
                vec![1, 1]
            );
            assert_eq!(chunks[0][0].state.component_id, "p1");
            assert_eq!(chunks[1][0].state.component_id, "p2");
        })
        .await;
    }
}
