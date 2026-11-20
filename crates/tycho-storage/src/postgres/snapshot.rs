//! One-shot read of all live state for the entity cache.
//!
//! Every query filters live rows directly. On the partitioned tables a live row has
//! `valid_to = MAX_TS`. On `contract_code` and `account_balance`, and for `deleted_at` on
//! `account` and `protocol_component`, a live row is `NULL` as written or `MAX_TS` as reopened by
//! `revert_state`, so both count. The write path closes only `NULL` rows, so a key written after
//! a revert has two live rows on `contract_code` and `account_balance`; the newest one wins.
//! Versioned reads are not needed, because otherwise one live row per key is the versioning
//! invariant, and they are much slower on tables this size. Rows are selected by a subquery, never
//! by an id list: Postgres caps bind parameters at 65,535 and a chain can hold more components
//! than that.

use std::collections::HashMap;

use chrono::NaiveDateTime;
use diesel::{
    dsl::sql,
    pg::Pg,
    sql_types::{BigInt, Bool, Text},
    BoolExpressionMethods, ExpressionMethods, QueryDsl,
};
use diesel_async::{pg::TransactionBuilder, AsyncPgConnection, RunQueryDsl};
use tracing::debug;
use tycho_common::{
    models::{
        contract::{Account, AccountBalance},
        protocol::ProtocolComponentState,
        Address, Chain,
    },
    storage::{
        AccountChanges, AccountSnapshot, AccountWriteTimestamps, CommitWatermark,
        ComponentSnapshot, StorageError, WriteTimestamp,
    },
    Bytes,
};

use super::{schema, PostgresError, PostgresGateway, MAX_TS};

/// A read-only `REPEATABLE READ` transaction, so every read in it sees the same transaction
/// snapshot.
pub fn snapshot_transaction(
    conn: &mut AsyncPgConnection,
) -> TransactionBuilder<'_, AsyncPgConnection> {
    conn.build_transaction()
        .read_only()
        .repeatable_read()
}

/// Longest one snapshot read may run before Postgres cancels it.
const SNAPSHOT_STATEMENT_TIMEOUT: &str = "10min";

/// How often Postgres checks that the client is still connected while a snapshot read runs. A read
/// that builds a hash or sorts writes nothing to the socket, so without this check it keeps running
/// after the client is gone.
const SNAPSHOT_CONNECTION_CHECK_INTERVAL: &str = "10s";

/// Bounds every later read of the current transaction: a statement is cancelled after
/// [`SNAPSHOT_STATEMENT_TIMEOUT`], and stops within [`SNAPSHOT_CONNECTION_CHECK_INTERVAL`] of the
/// client disconnecting. The settings end with the transaction, so the pooled connection keeps its
/// own.
pub async fn bound_snapshot_reads(conn: &mut AsyncPgConnection) -> Result<(), PostgresError> {
    for (name, value) in [
        ("statement_timeout", SNAPSHOT_STATEMENT_TIMEOUT),
        ("client_connection_check_interval", SNAPSHOT_CONNECTION_CHECK_INTERVAL),
    ] {
        diesel::sql_query("SELECT set_config($1, $2, true)")
            .bind::<Text, _>(name)
            .bind::<Text, _>(value)
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;
    }
    Ok(())
}

/// Filter that keeps the rows of `table` written at or after `since`: rows whose `xmin`, the
/// transaction that wrote the row version, is not older than the watermark. `age` counts
/// transactions back from the current one and handles the 32-bit wraparound of `xmin`. A frozen
/// row or one older than about two billion transactions counts as old.
///
/// Limits of the Postgres internals this relies on:
/// - No index covers `xmin`, so the filter scans every row of the table.
/// - A watermark more than about two billion transactions old makes every row count as old. At the
///   rate the indexer writes, that is many years without a catch-up.
/// - Transaction ids are local to one cluster. A watermark from before a restore or a move by
///   logical replication (such as an RDS blue/green switch) means nothing in the new cluster:
///   restart the indexer, so it loads a new snapshot.
/// - A long-running transaction holds the watermark back, so each catch-up reads more rows again.
///   That costs time, but loses nothing.
fn written_since(table: &str, since: &CommitWatermark) -> diesel::expression::SqlLiteral<Bool> {
    // The low 32 bits of a 64-bit transaction id are its `xid`. Ids below 3 are reserved; the
    // first normal id is the oldest a row can carry.
    let xid = (since.transaction_id() & u64::from(u32::MAX)).max(3);
    sql::<Bool>(&format!("age(\"{table}\".xmin) <= age('{xid}'::xid)"))
}

/// Subquery for the ids of the live accounts of a chain: not deleted, with a live code row. With
/// `since`, only the accounts whose own row was written at or after it.
fn account_ids(
    chain_id: i64,
    since: Option<&CommitWatermark>,
) -> schema::account::BoxedQuery<'static, Pg, BigInt> {
    let mut query = schema::account::table
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
        .into_boxed();
    if let Some(since) = since {
        query = query.filter(written_since("account", since));
    }
    query
}

/// Subquery for the ids of the components of a chain that are not deleted. With `system`, only
/// the components of that protocol system.
fn component_ids(
    chain_id: i64,
    system: Option<&str>,
) -> schema::protocol_component::BoxedQuery<'static, Pg, BigInt> {
    let mut query = schema::protocol_component::table
        .filter(schema::protocol_component::chain_id.eq(chain_id))
        .filter(
            schema::protocol_component::deleted_at
                .is_null()
                .or(schema::protocol_component::deleted_at.eq(MAX_TS)),
        )
        .select(schema::protocol_component::id)
        .into_boxed();
    if let Some(system) = system {
        query = query.filter(
            schema::protocol_component::protocol_system_id.eq_any(
                schema::protocol_system::table
                    .filter(schema::protocol_system::name.eq(system.to_string()))
                    .select(schema::protocol_system::id),
            ),
        );
    }
    query
}

impl PostgresGateway {
    /// Every live account of `chain` (one with a live code row) with its code, balances and
    /// slots, each value timestamped with its row's `valid_from`. Order is
    /// unspecified.
    ///
    /// # Errors
    ///
    /// `StorageError::NotFound("native_balance", address)` when a contract has no live native
    /// balance row, the same error `get_contracts` returns.
    pub(crate) async fn account_snapshots(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<AccountSnapshot>, StorageError> {
        self.read_account_snapshots(chain, None, conn)
            .await
    }

    /// Like [`Self::account_snapshots`], but only the accounts whose own row was written at or
    /// after `since`, such as a new contract. Each one comes with its full state.
    pub(crate) async fn account_snapshots_written_since(
        &self,
        chain: &Chain,
        since: &CommitWatermark,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<AccountSnapshot>, StorageError> {
        self.read_account_snapshots(chain, Some(since), conn)
            .await
    }

    async fn read_account_snapshots(
        &self,
        chain: &Chain,
        since: Option<&CommitWatermark>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<AccountSnapshot>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let native_token_id = self.get_native_token_id(chain)?;

        // Postgres cannot estimate the `xmin` filter of `account_ids`. As a subquery, it expects
        // thousands of accounts and scans every live storage slot to keep a few. As a list, it
        // reads the few accounts through the `account_id` indexes.
        let written_ids = match since {
            Some(since) => {
                let ids = account_ids(chain_id, Some(since))
                    .get_results::<i64>(conn)
                    .await
                    .map_err(PostgresError::from)?;
                if ids.is_empty() {
                    return Ok(Vec::new());
                }
                Some(ids)
            }
            None => None,
        };

        let mut accounts_query = schema::contract_code::table
            .inner_join(schema::account::table)
            .inner_join(schema::transaction::table)
            .filter(schema::account::chain_id.eq(chain_id))
            .filter(
                schema::account::deleted_at
                    .is_null()
                    .or(schema::account::deleted_at.eq(MAX_TS)),
            )
            .filter(
                schema::contract_code::valid_to
                    .is_null()
                    .or(schema::contract_code::valid_to.eq(MAX_TS)),
            )
            .distinct_on(schema::contract_code::account_id)
            .order_by((schema::contract_code::account_id, schema::contract_code::valid_from.desc()))
            .select((
                schema::account::id,
                schema::account::address,
                schema::account::title,
                schema::contract_code::code,
                schema::contract_code::hash,
                schema::contract_code::valid_from,
                schema::transaction::hash,
            ))
            .into_boxed();
        if let Some(ids) = &written_ids {
            accounts_query = accounts_query.filter(schema::account::id.eq_any(ids.clone()));
        }
        let accounts = accounts_query
            .get_results::<(i64, Address, String, Bytes, Bytes, NaiveDateTime, Bytes)>(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut balances_by_account: HashMap<i64, Vec<(i64, Address, Bytes, WriteTimestamp)>> =
            HashMap::new();
        let mut balances_query = schema::account_balance::table
            .inner_join(schema::token::table.inner_join(schema::account::table))
            .filter(
                schema::account_balance::valid_to
                    .is_null()
                    .or(schema::account_balance::valid_to.eq(MAX_TS)),
            )
            .distinct_on((schema::account_balance::account_id, schema::account_balance::token_id))
            .order_by((
                schema::account_balance::account_id,
                schema::account_balance::token_id,
                schema::account_balance::valid_from.desc(),
            ))
            .select((
                schema::account_balance::account_id,
                schema::account_balance::token_id,
                schema::account::address,
                schema::account_balance::balance,
                schema::account_balance::valid_from,
            ))
            .into_boxed();
        balances_query = match &written_ids {
            Some(ids) => {
                balances_query.filter(schema::account_balance::account_id.eq_any(ids.clone()))
            }
            None => balances_query
                .filter(schema::account_balance::account_id.eq_any(account_ids(chain_id, None))),
        };
        for (account_id, token_id, token, balance, valid_from) in balances_query
            .get_results::<(i64, i64, Address, Bytes, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            balances_by_account
                .entry(account_id)
                .or_default()
                .push((token_id, token, balance, WriteTimestamp::new(valid_from)));
        }

        let mut slots_by_account: HashMap<i64, Vec<(Bytes, Option<Bytes>, WriteTimestamp)>> =
            HashMap::new();
        let mut slots_query = schema::contract_storage::table
            .filter(schema::contract_storage::valid_to.eq(MAX_TS))
            .select((
                schema::contract_storage::account_id,
                schema::contract_storage::slot,
                schema::contract_storage::value,
                schema::contract_storage::valid_from,
            ))
            .into_boxed();
        slots_query = match &written_ids {
            Some(ids) => {
                slots_query.filter(schema::contract_storage::account_id.eq_any(ids.clone()))
            }
            None => slots_query
                .filter(schema::contract_storage::account_id.eq_any(account_ids(chain_id, None))),
        };
        for (account_id, slot, value, valid_from) in slots_query
            .get_results::<(i64, Bytes, Option<Bytes>, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            slots_by_account
                .entry(account_id)
                .or_default()
                .push((slot, value, WriteTimestamp::new(valid_from)));
        }

        let mut snapshots = Vec::with_capacity(accounts.len());
        for (id, address, title, code, code_hash, code_valid_from, code_tx) in accounts {
            let code_written_at = WriteTimestamp::new(code_valid_from);
            let mut native_balance = None;
            let mut token_balances = HashMap::new();
            let mut token_balance_written_at = HashMap::new();
            for (token_id, token, balance, written_at) in balances_by_account
                .remove(&id)
                .unwrap_or_default()
            {
                if token_id == native_token_id {
                    native_balance = Some((balance, written_at));
                    continue;
                }
                token_balances.insert(
                    token.clone(),
                    AccountBalance::new(address.clone(), token.clone(), balance, Bytes::zero(32)),
                );
                token_balance_written_at.insert(token, written_at);
            }
            let (native_balance, native_balance_written_at) = native_balance.ok_or_else(|| {
                StorageError::NotFound("native_balance".to_string(), address.to_string())
            })?;
            let mut slots = HashMap::new();
            let mut slot_written_at = HashMap::new();
            for (slot, value, written_at) in slots_by_account
                .remove(&id)
                .unwrap_or_default()
            {
                slots.insert(slot.clone(), value.unwrap_or_default());
                slot_written_at.insert(slot, written_at);
            }
            let account = Account::new(
                *chain,
                address,
                title,
                slots,
                native_balance,
                token_balances,
                code,
                code_hash,
                Bytes::zero(32),
                code_tx,
                None,
            );
            snapshots.push(AccountSnapshot {
                account,
                written_at: AccountWriteTimestamps {
                    slots: slot_written_at,
                    native_balance: native_balance_written_at,
                    code: code_written_at,
                    token_balances: token_balance_written_at,
                },
            });
        }
        Ok(snapshots)
    }

    /// The live slots, balances and code of the accounts of `chain` whose rows were written at or
    /// after `since`, by account address. Each value is timestamped with its row's `valid_from`;
    /// of two live rows for one key, the newer one wins.
    ///
    /// The `xmin` filter alone scans every row of the value tables. When
    /// [`Self::transaction_floor`] finds a bound, the reads also keep only the rows whose
    /// `modify_tx` is at or above it, so Postgres finds them through the `modify_tx` index. The
    /// result is the same.
    pub(crate) async fn account_changes_since(
        &self,
        chain: &Chain,
        since: &CommitWatermark,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, AccountChanges>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;
        let native_token_id = self.get_native_token_id(chain)?;
        let lowest_modify_tx = self
            .transaction_floor(chain_id, since, conn)
            .await?
            .unwrap_or(i64::MIN);
        let mut changes: HashMap<Address, AccountChanges> = HashMap::new();

        for (address, slot, value, valid_from) in schema::contract_storage::table
            .inner_join(schema::account::table)
            .filter(schema::account::chain_id.eq(chain_id))
            .filter(schema::contract_storage::valid_to.eq(MAX_TS))
            .filter(written_since("contract_storage", since))
            .filter(schema::contract_storage::modify_tx.ge(lowest_modify_tx))
            .select((
                schema::account::address,
                schema::contract_storage::slot,
                schema::contract_storage::value,
                schema::contract_storage::valid_from,
            ))
            .get_results::<(Address, Bytes, Option<Bytes>, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            changes
                .entry(address)
                .or_default()
                .slots
                .insert(slot, (value.unwrap_or_default(), WriteTimestamp::new(valid_from)));
        }

        for (address, code, hash, valid_from) in schema::contract_code::table
            .inner_join(schema::account::table)
            .filter(schema::account::chain_id.eq(chain_id))
            .filter(
                schema::contract_code::valid_to
                    .is_null()
                    .or(schema::contract_code::valid_to.eq(MAX_TS)),
            )
            .filter(written_since("contract_code", since))
            .filter(schema::contract_code::modify_tx.ge(lowest_modify_tx))
            .select((
                schema::account::address,
                schema::contract_code::code,
                schema::contract_code::hash,
                schema::contract_code::valid_from,
            ))
            .get_results::<(Address, Bytes, Bytes, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            let written_at = WriteTimestamp::new(valid_from);
            let code_slot = &mut changes.entry(address).or_default().code;
            if code_slot
                .as_ref()
                .is_none_or(|(_, _, at)| *at < written_at)
            {
                *code_slot = Some((code, hash, written_at));
            }
        }

        let balance_rows = schema::account_balance::table
            .inner_join(schema::token::table.inner_join(schema::account::table))
            .filter(
                schema::account_balance::account_id.eq_any(
                    schema::account::table
                        .filter(schema::account::chain_id.eq(chain_id))
                        .select(schema::account::id)
                        .into_boxed(),
                ),
            )
            .filter(
                schema::account_balance::valid_to
                    .is_null()
                    .or(schema::account_balance::valid_to.eq(MAX_TS)),
            )
            .filter(written_since("account_balance", since))
            .filter(schema::account_balance::modify_tx.ge(lowest_modify_tx))
            .select((
                schema::account_balance::account_id,
                schema::account_balance::token_id,
                schema::account::address,
                schema::account_balance::balance,
                schema::account_balance::valid_from,
            ))
            .get_results::<(i64, i64, Address, Bytes, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?;
        let owners: HashMap<i64, Address> = schema::account::table
            .filter(
                schema::account::id.eq_any(
                    schema::account_balance::table
                        .filter(
                            schema::account_balance::valid_to
                                .is_null()
                                .or(schema::account_balance::valid_to.eq(MAX_TS)),
                        )
                        .filter(written_since("account_balance", since))
                        .filter(schema::account_balance::modify_tx.ge(lowest_modify_tx))
                        .select(schema::account_balance::account_id),
                ),
            )
            .select((schema::account::id, schema::account::address))
            .get_results::<(i64, Address)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .collect();
        for (account_id, token_id, token, balance, valid_from) in balance_rows {
            let Some(owner) = owners.get(&account_id) else {
                continue;
            };
            let written_at = WriteTimestamp::new(valid_from);
            let account = changes
                .entry(owner.clone())
                .or_default();
            if token_id == native_token_id {
                if account
                    .native_balance
                    .as_ref()
                    .is_none_or(|(_, at)| *at < written_at)
                {
                    account.native_balance = Some((balance, written_at));
                }
                continue;
            }
            let newer = account
                .token_balances
                .get(&token)
                .is_none_or(|(_, at)| *at < written_at);
            if newer {
                let value =
                    AccountBalance::new(owner.clone(), token.clone(), balance, Bytes::zero(32));
                account
                    .token_balances
                    .insert(token, (value, written_at));
            }
        }
        Ok(changes)
    }

    /// The lowest `transaction.id` that a value row written at or after `since` can reference, or
    /// `None` when this read cannot bound it.
    ///
    /// An extractor writes the rows of a block with the ids of that block's transactions. After
    /// `since`, it writes only blocks after the one it had committed then. So every row written
    /// after `since` references a transaction of a block after the lowest of those blocks, among
    /// the extractors that committed since. The bound is the lowest id of those transactions.
    ///
    /// The id order is not the commit order: several extractors write one block in separate
    /// commits, and the later commits reuse the transaction rows of the first. That is why the
    /// bound comes from the blocks, not from the newest id at `since`.
    ///
    /// `None` when an extractor that committed since `since` had committed nothing at `since`,
    /// when no extractor committed since, or when no transaction follows the lowest block.
    pub(crate) async fn transaction_floor(
        &self,
        chain_id: i64,
        since: &CommitWatermark,
        conn: &mut AsyncPgConnection,
    ) -> Result<Option<i64>, StorageError> {
        let committed_since: Vec<String> = schema::extraction_state::table
            .filter(schema::extraction_state::chain_id.eq(chain_id))
            .filter(written_since("extraction_state", since))
            .select(schema::extraction_state::name)
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?;
        let mut lowest_block: Option<u64> = None;
        for name in &committed_since {
            let Some(&block) = since.committed_blocks().get(name) else {
                debug!(extractor = name, "No catch-up bound: the extractor had no block");
                return Ok(None);
            };
            lowest_block = Some(lowest_block.map_or(block, |lowest| lowest.min(block)));
        }
        let Some(lowest_block) = lowest_block else {
            return Ok(None);
        };
        let lowest_block = i64::try_from(lowest_block).map_err(|_too_large| {
            StorageError::Unexpected(format!("Block number {lowest_block} does not fit an i64"))
        })?;
        let floor: Option<i64> = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::block::chain_id.eq(chain_id))
            .filter(schema::block::number.gt(lowest_block))
            .select(diesel::dsl::min(schema::transaction::id))
            .get_result(conn)
            .await
            .map_err(PostgresError::from)?;
        debug!(
            extractors = committed_since.len(),
            lowest_block,
            ?floor,
            "Catch-up bound on transaction ids"
        );
        Ok(floor)
    }

    /// The watermark of the current transaction snapshot: every row written after the
    /// snapshot was taken is at or after it.
    pub(crate) async fn snapshot_watermark(
        &self,
        conn: &mut AsyncPgConnection,
    ) -> Result<CommitWatermark, StorageError> {
        #[derive(diesel::QueryableByName)]
        struct Row {
            #[diesel(sql_type = Text)]
            xmin: String,
        }
        let row: Row =
            diesel::sql_query("SELECT pg_snapshot_xmin(pg_current_snapshot())::text AS xmin")
                .get_result(conn)
                .await
                .map_err(PostgresError::from)?;
        let transaction_id = row.xmin.parse::<u64>().map_err(|err| {
            StorageError::Unexpected(format!("Snapshot xmin {} is not a number: {err}", row.xmin))
        })?;
        Ok(CommitWatermark::new(transaction_id))
    }

    /// Every component of `chain` that is not deleted, with its live attributes and balances,
    /// timestamped with the newest `valid_from` among its rows or, for a component without rows,
    /// its `created_at`. With `system`, only the components of that protocol system. Order is
    /// unspecified.
    pub(crate) async fn component_snapshots(
        &self,
        chain: &Chain,
        system: Option<&str>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentSnapshot>, StorageError> {
        let chain_id = self.get_chain_id(chain)?;

        let mut components_query = schema::protocol_component::table
            .inner_join(schema::protocol_system::table)
            .filter(schema::protocol_component::chain_id.eq(chain_id))
            .filter(
                schema::protocol_component::deleted_at
                    .is_null()
                    .or(schema::protocol_component::deleted_at.eq(MAX_TS)),
            )
            .select((
                schema::protocol_component::id,
                schema::protocol_component::external_id,
                schema::protocol_system::name,
                schema::protocol_component::created_at,
            ))
            .into_boxed();
        if let Some(system) = system {
            components_query =
                components_query.filter(schema::protocol_system::name.eq(system.to_string()));
        }
        let components: Vec<(i64, String, String, NaiveDateTime)> = components_query
            .get_results(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut attributes_by_component: HashMap<i64, Vec<(String, Bytes, WriteTimestamp)>> =
            HashMap::new();
        for (component_id, name, value, valid_from) in schema::protocol_state::table
            .filter(
                schema::protocol_state::protocol_component_id
                    .eq_any(component_ids(chain_id, system)),
            )
            .filter(schema::protocol_state::valid_to.eq(MAX_TS))
            .select((
                schema::protocol_state::protocol_component_id,
                schema::protocol_state::attribute_name,
                schema::protocol_state::attribute_value,
                schema::protocol_state::valid_from,
            ))
            .get_results::<(i64, String, Bytes, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            attributes_by_component
                .entry(component_id)
                .or_default()
                .push((name, value, WriteTimestamp::new(valid_from)));
        }

        let mut balances_by_component: HashMap<i64, Vec<(Address, Bytes, WriteTimestamp)>> =
            HashMap::new();
        for (component_id, token, balance, valid_from) in schema::component_balance::table
            .inner_join(schema::token::table.inner_join(schema::account::table))
            .filter(
                schema::component_balance::protocol_component_id
                    .eq_any(component_ids(chain_id, system)),
            )
            .filter(schema::component_balance::valid_to.eq(MAX_TS))
            .select((
                schema::component_balance::protocol_component_id,
                schema::account::address,
                schema::component_balance::new_balance,
                schema::component_balance::valid_from,
            ))
            .get_results::<(i64, Address, Bytes, NaiveDateTime)>(conn)
            .await
            .map_err(PostgresError::from)?
        {
            balances_by_component
                .entry(component_id)
                .or_default()
                .push((token, balance, WriteTimestamp::new(valid_from)));
        }

        let mut snapshots = Vec::with_capacity(components.len());
        for (id, external_id, system, created_at) in components {
            let mut updated_at = WriteTimestamp::new(created_at);
            let mut attributes = HashMap::new();
            for (name, value, written_at) in attributes_by_component
                .remove(&id)
                .unwrap_or_default()
            {
                updated_at = updated_at.max(written_at);
                attributes.insert(name, value);
            }
            let mut balances = HashMap::new();
            for (token, balance, written_at) in balances_by_component
                .remove(&id)
                .unwrap_or_default()
            {
                updated_at = updated_at.max(written_at);
                balances.insert(token, balance);
            }
            snapshots.push(ComponentSnapshot {
                system,
                state: ProtocolComponentState { component_id: external_id, attributes, balances },
                updated_at,
            });
        }
        Ok(snapshots)
    }
}

#[cfg(test)]
mod test_serial_db {
    use std::{collections::HashSet, str::FromStr};

    use diesel_async::{pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt};
    use tokio::sync::mpsc;
    use tycho_common::{keccak256, models::Chain, storage::StateSnapshotGateway, Bytes};

    use super::*;
    use crate::postgres::{
        cache::CachedGateway, db_fixtures, orm, testing::run_against_db, PostgresGateway,
    };

    const C0: &str = "6B175474E89094C44Da98b954EedeAC495271d0F";
    const C1: &str = "73BcE791c239c8010Cd3C857d96580037CCdd0EE";
    const C2: &str = "94a3F312366b8D0a32A00986194053C0ed0CdDb1";
    const C3: &str = "Ab5801a7D398351b8bE11C439e05C5B3259aeC9B";
    const USDC: &str = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    struct Fixture {
        chain_id: i64,
        txn: Vec<i64>,
        native_token: i64,
        usdc: i64,
        c0: i64,
    }

    /// Two live contracts, one deleted contract, two token accounts without code.
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
        // p4: created at block 1, one balance at block 2, no attributes
        let p4 = db_fixtures::insert_protocol_component(
            conn,
            "p4",
            f.chain_id,
            zigzag,
            pool_type,
            f.txn[0],
            Some(vec![f.usdc]),
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Bytes::from(7u64),
            Bytes::default(),
            7.0,
            f.usdc,
            f.txn[3],
            p4,
            None,
        )
        .await;
    }

    fn by_address(accounts: Vec<AccountSnapshot>) -> HashMap<Bytes, AccountSnapshot> {
        accounts
            .into_iter()
            .map(|s| (s.account.address.clone(), s))
            .collect()
    }

    fn cached_gateway(pool: &Pool<AsyncPgConnection>, gw: PostgresGateway) -> CachedGateway {
        let (tx, _rx) = mpsc::channel(1);
        CachedGateway::new(tx, pool.clone(), gw)
    }

    fn expected_c0() -> AccountSnapshot {
        let ts = db_fixtures::yesterday_midnight();
        let ts_p1 = db_fixtures::yesterday_half_past_midnight();
        let address = Bytes::from_str(C0).unwrap();
        let usdc = Bytes::from_str(USDC).unwrap();
        let slot0 = Bytes::from(0u64).lpad(32, 0);
        let slot2 = Bytes::from(2u64).lpad(32, 0);
        let code = Bytes::from("C0C0C0");
        let account = Account::new(
            Chain::Ethereum,
            address.clone(),
            "c0".to_string(),
            HashMap::from([
                (slot0.clone(), Bytes::from(2u64).lpad(32, 0)),
                (slot2.clone(), Bytes::from(1u64).lpad(32, 0)),
            ]),
            Bytes::from(101u64).lpad(32, 0),
            HashMap::from([(
                usdc.clone(),
                AccountBalance::new(
                    address,
                    usdc.clone(),
                    Bytes::from(1000u64).lpad(32, 0),
                    Bytes::zero(32),
                ),
            )]),
            code.clone(),
            keccak256(&code).into(),
            Bytes::zero(32),
            Bytes::from_str("0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945")
                .unwrap(),
            None,
        );
        AccountSnapshot {
            account,
            written_at: AccountWriteTimestamps {
                slots: HashMap::from([
                    (slot0, WriteTimestamp::new(ts_p1)),
                    (slot2, WriteTimestamp::new(ts)),
                ]),
                native_balance: WriteTimestamp::new(ts_p1),
                code: WriteTimestamp::new(ts),
                token_balances: HashMap::from([(usdc, WriteTimestamp::new(ts))]),
            },
        }
    }

    fn expected_c1() -> AccountSnapshot {
        let ts_p1 = db_fixtures::yesterday_half_past_midnight();
        let address = Bytes::from_str(C1).unwrap();
        let slot0 = Bytes::from(0u64).lpad(32, 0);
        let code = Bytes::from("C1C1C1");
        let account = Account::new(
            Chain::Ethereum,
            address,
            "c1".to_string(),
            HashMap::from([(slot0.clone(), Bytes::from(128u64).lpad(32, 0))]),
            Bytes::from(50u64).lpad(32, 0),
            HashMap::new(),
            code.clone(),
            keccak256(&code).into(),
            Bytes::zero(32),
            Bytes::from_str("0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7")
                .unwrap(),
            None,
        );
        AccountSnapshot {
            account,
            written_at: AccountWriteTimestamps {
                slots: HashMap::from([(slot0, WriteTimestamp::new(ts_p1))]),
                native_balance: WriteTimestamp::new(ts_p1),
                code: WriteTimestamp::new(ts_p1),
                token_balances: HashMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn account_snapshots_return_live_accounts_with_their_write_timestamps_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            setup_accounts(&mut conn).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let accounts = by_address(
                gw.account_snapshots(&Chain::Ethereum, &mut conn)
                    .await
                    .unwrap(),
            );

            assert_eq!(
                accounts,
                by_address(vec![expected_c0(), expected_c1()]),
                "c2 is deleted, token accounts have no code, the native balance is not a token \
                 balance, the live slot version wins"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn account_snapshots_read_a_null_slot_as_zero_serial_db() {
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
                gw.account_snapshots(&Chain::Ethereum, &mut conn)
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
    async fn account_snapshots_fail_without_a_native_balance_serial_db() {
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
                .account_snapshots(&Chain::Ethereum, &mut conn)
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
    async fn component_snapshots_return_live_state_timestamped_with_the_newest_write_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let components: HashMap<String, ComponentSnapshot> = gw
                .component_snapshots(&Chain::Ethereum, None, &mut conn)
                .await
                .unwrap()
                .into_iter()
                .map(|c| (c.state.component_id.clone(), c))
                .collect();

            assert_eq!(components.len(), 3, "p3 is deleted");
            let p1 = &components["p1"];
            assert_eq!(p1.system, "ambient");
            assert_eq!(p1.state.attributes["reserve"], Bytes::from(2u64), "live version wins");
            assert_eq!(p1.state.attributes["fee"], Bytes::from(30u64));
            assert_eq!(p1.state.balances[&Bytes::from_str(USDC).unwrap()], Bytes::from(1000u64));
            assert_eq!(
                p1.updated_at,
                WriteTimestamp::new(db_fixtures::yesterday_half_past_midnight())
            );
            let p2 = &components["p2"];
            assert_eq!(p2.system, "zigzag");
            assert!(p2.state.attributes.is_empty());
            assert!(p2.state.balances.is_empty());
            assert_eq!(
                p2.updated_at,
                WriteTimestamp::new(db_fixtures::yesterday_half_past_midnight()),
                "creation block 2"
            );
            let p4 = &components["p4"];
            assert!(p4.state.attributes.is_empty());
            assert_eq!(p4.state.balances[&Bytes::from_str(USDC).unwrap()], Bytes::from(7u64));
            assert_eq!(
                p4.updated_at,
                WriteTimestamp::new(db_fixtures::yesterday_half_past_midnight()),
                "the balance at block 2 is newer than the creation at block 1"
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
            let (block_id, block_number): (i64, i64) = schema::block::table
                .order_by(schema::block::number.desc())
                .select((schema::block::id, schema::block::number))
                .first(&mut conn)
                .await
                .unwrap();
            let chain_id: i64 = schema::chain::table
                .select(schema::chain::id)
                .first(&mut conn)
                .await
                .unwrap();
            diesel::insert_into(schema::extraction_state::table)
                .values(&orm::NewExtractionState {
                    name: "ambient",
                    chain_id,
                    attributes: None,
                    cursor: Some("cursor".as_bytes()),
                    version: "0.1.0",
                    modified_ts: chrono::Utc::now().naive_utc(),
                    block_id,
                })
                .execute(&mut conn)
                .await
                .unwrap();
            let gw = PostgresGateway::from_connection(&mut conn).await;
            drop(conn);

            let snapshot = cached_gateway(&pool, gw)
                .state_snapshot(&Chain::Ethereum)
                .await
                .unwrap();

            assert_eq!(snapshot.accounts.len(), 2);
            assert_eq!(snapshot.components.len(), 3);
            let ambient = &snapshot.cursors["ambient"];
            assert_eq!(ambient.cursor, "cursor".as_bytes());
            assert_eq!(ambient.block.number, u64::try_from(block_number).unwrap());
        })
        .await;
    }

    /// Rows written after the watermark come back, rows written before do not, and only the
    /// requested system's components are read.
    #[tokio::test]
    async fn state_delta_reads_what_was_written_after_the_watermark_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            let gateway = cached_gateway(&pool, PostgresGateway::from_connection(&mut conn).await);
            let watermark = gateway
                .state_snapshot(&Chain::Ethereum)
                .await
                .unwrap()
                .watermark;
            let written = db_fixtures::yesterday_one_am();
            db_fixtures::insert_slots(&mut conn, f.c0, f.txn[3], &written, None, &[(9, 9, None)])
                .await;
            let c3 =
                db_fixtures::insert_account(&mut conn, C3, "c3", f.chain_id, Some(f.txn[3])).await;
            db_fixtures::insert_contract_code(&mut conn, c3, f.txn[3], Bytes::from("C3C3C3")).await;
            db_fixtures::insert_account_balance(&mut conn, 7, f.native_token, f.txn[3], None, c3)
                .await;
            db_fixtures::insert_slots(&mut conn, c3, f.txn[3], &written, None, &[(5, 5, None)])
                .await;
            drop(conn);

            let delta = gateway
                .catch_up_read(&Chain::Ethereum, watermark.clone(), "ambient")
                .await
                .unwrap();

            let c0 = Bytes::from_str(C0).unwrap();
            let c3 = Bytes::from_str(C3).unwrap();
            assert_eq!(
                delta.changes[&c0].slots,
                HashMap::from([(
                    Bytes::from(9u64).lpad(32, 0),
                    (Bytes::from(9u64).lpad(32, 0), WriteTimestamp::new(written))
                )]),
                "only the slot written after the watermark"
            );
            assert!(delta.changes[&c0]
                .native_balance
                .is_none());
            assert_eq!(
                delta
                    .changes
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>(),
                HashSet::from([c0, c3.clone()])
            );
            assert_eq!(
                delta
                    .accounts
                    .iter()
                    .map(|snapshot| snapshot.account.address.clone())
                    .collect::<Vec<_>>(),
                vec![c3],
                "the new contract comes with its full state"
            );
            assert_eq!(
                delta.accounts[0].account.slots,
                HashMap::from([(Bytes::from(5u64).lpad(32, 0), Bytes::from(5u64).lpad(32, 0))])
            );
            assert!(!delta.components.is_empty());
            assert!(delta
                .components
                .iter()
                .all(|component| component.system == "ambient"));
            assert!(delta.watermark.transaction_id() > watermark.transaction_id());
        })
        .await;
    }

    async fn insert_extraction_state(
        conn: &mut AsyncPgConnection,
        chain_id: i64,
        name: &str,
        block_id: i64,
    ) {
        diesel::insert_into(schema::extraction_state::table)
            .values(&orm::NewExtractionState {
                name,
                chain_id,
                attributes: None,
                cursor: Some("cursor".as_bytes()),
                version: "0.1.0",
                modified_ts: chrono::Utc::now().naive_utc(),
                block_id,
            })
            .execute(conn)
            .await
            .unwrap();
    }

    /// Moves the extractor `name` to `block_id`, as its next commit does.
    async fn commit_extraction_state(conn: &mut AsyncPgConnection, name: &str, block_id: i64) {
        diesel::update(
            schema::extraction_state::table.filter(schema::extraction_state::name.eq(name)),
        )
        .set(schema::extraction_state::block_id.eq(block_id))
        .execute(conn)
        .await
        .unwrap();
    }

    /// The ids of the blocks of `setup_accounts`, by number: blocks 1 and 2.
    async fn block_ids(conn: &mut AsyncPgConnection) -> Vec<i64> {
        schema::block::table
            .order_by(schema::block::number)
            .select(schema::block::id)
            .load(conn)
            .await
            .unwrap()
    }

    /// Block 3 after the blocks of `setup_accounts`, with one transaction. Returns the id of the
    /// transaction.
    async fn insert_block_3_with_transaction(conn: &mut AsyncPgConnection, chain_id: i64) -> i64 {
        let block_id: i64 = diesel::insert_into(schema::block::table)
            .values((
                schema::block::hash.eq(Bytes::from([3u8; 32])),
                schema::block::parent_hash.eq(Bytes::from([2u8; 32])),
                schema::block::number.eq(3),
                schema::block::ts.eq(db_fixtures::yesterday_one_am()),
                schema::block::chain_id.eq(chain_id),
            ))
            .returning(schema::block::id)
            .get_result(conn)
            .await
            .unwrap();
        db_fixtures::insert_txns(
            conn,
            &[(block_id, 1, "0x3333333333333333333333333333333333333333333333333333333333333333")],
        )
        .await[0]
    }

    /// Several extractors write one block in separate commits, and the later commits reuse the
    /// transaction rows of the first. A row written after the watermark can then reference a
    /// transaction inserted before it, and the catch-up must still read it.
    #[tokio::test]
    async fn catch_up_reads_rows_that_reference_a_transaction_older_than_the_watermark_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            let blocks = block_ids(&mut conn).await;
            insert_extraction_state(&mut conn, f.chain_id, "ambient", blocks[0]).await;
            let gateway = cached_gateway(&pool, PostgresGateway::from_connection(&mut conn).await);
            let watermark = gateway
                .state_snapshot(&Chain::Ethereum)
                .await
                .unwrap()
                .watermark;
            assert_eq!(watermark.committed_blocks(), &HashMap::from([("ambient".to_string(), 1)]));
            let written = db_fixtures::yesterday_one_am();
            // `txn[2]` is a transaction of block 2, inserted before the watermark.
            db_fixtures::insert_slots(&mut conn, f.c0, f.txn[2], &written, None, &[(7, 7, None)])
                .await;
            // `txn[0]` is a transaction of block 1, which the extractor had committed. No commit
            // after the watermark writes such a row, so the bound leaves it out.
            db_fixtures::insert_slots(&mut conn, f.c0, f.txn[0], &written, None, &[(8, 8, None)])
                .await;
            commit_extraction_state(&mut conn, "ambient", blocks[1]).await;
            drop(conn);

            let read = gateway
                .catch_up_read(&Chain::Ethereum, watermark, "ambient")
                .await
                .unwrap();

            let slots = &read.changes[&Bytes::from_str(C0).unwrap()].slots;
            assert!(slots.contains_key(&Bytes::from(7u64).lpad(32, 0)));
            assert!(!slots.contains_key(&Bytes::from(8u64).lpad(32, 0)));
            assert_eq!(read.watermark.committed_blocks()["ambient"], 2);
        })
        .await;
    }

    /// Only the extractors that committed since the watermark count, and one that had no block
    /// at the watermark leaves the read without a bound.
    #[tokio::test]
    async fn transaction_floor_follows_the_extractors_that_committed_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            let blocks = block_ids(&mut conn).await;
            let block_3_tx = insert_block_3_with_transaction(&mut conn, f.chain_id).await;
            insert_extraction_state(&mut conn, f.chain_id, "stopped", blocks[0]).await;
            insert_extraction_state(&mut conn, f.chain_id, "running", blocks[1]).await;
            let gw = PostgresGateway::from_connection(&mut conn).await;
            let watermark = cached_gateway(&pool, gw.clone())
                .state_snapshot(&Chain::Ethereum)
                .await
                .unwrap()
                .watermark;

            assert_eq!(
                gw.transaction_floor(f.chain_id, &watermark, &mut conn)
                    .await
                    .unwrap(),
                None,
                "no extractor committed since the watermark"
            );

            let block_3: i64 = schema::block::table
                .filter(schema::block::number.eq(3))
                .select(schema::block::id)
                .first(&mut conn)
                .await
                .unwrap();
            commit_extraction_state(&mut conn, "running", block_3).await;
            assert_eq!(
                gw.transaction_floor(f.chain_id, &watermark, &mut conn)
                    .await
                    .unwrap(),
                Some(block_3_tx),
                "the first transaction after block 2; the stopped extractor at block 1 is ignored"
            );

            insert_extraction_state(&mut conn, f.chain_id, "new", block_3).await;
            assert_eq!(
                gw.transaction_floor(f.chain_id, &watermark, &mut conn)
                    .await
                    .unwrap(),
                None,
                "the new extractor had no block at the watermark"
            );
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

            let snapshot = cached_gateway(&pool, gw)
                .state_snapshot(&Chain::Ethereum)
                .await
                .unwrap();

            assert!(snapshot.accounts.is_empty());
            assert!(snapshot.components.is_empty());
            assert!(snapshot.cursors.is_empty());
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
                            .account_snapshots(&Chain::Ethereum, conn)
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
                        gw.account_snapshots(&Chain::Ethereum, conn)
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
                .account_snapshots(&Chain::Ethereum, &mut fresh)
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

    /// The write path closes only `NULL` rows, so a key written after a revert keeps its reopened
    /// `MAX_TS` row next to the new `NULL` row. The newer row wins.
    #[tokio::test]
    async fn account_snapshots_take_the_newest_of_two_live_rows_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            let c1: i64 = schema::account::table
                .filter(schema::account::title.eq("c1"))
                .select(schema::account::id)
                .first(&mut conn)
                .await
                .unwrap();
            let stale_code =
                db_fixtures::insert_contract_code(&mut conn, c1, f.txn[0], Bytes::from("0BAD"))
                    .await;
            diesel::update(
                schema::contract_code::table.filter(schema::contract_code::id.eq(stale_code)),
            )
            .set(schema::contract_code::valid_to.eq(MAX_TS))
            .execute(&mut conn)
            .await
            .unwrap();
            diesel::update(
                schema::account_balance::table
                    .filter(schema::account_balance::account_id.eq(f.c0))
                    .filter(schema::account_balance::token_id.eq(f.native_token))
                    .filter(schema::account_balance::valid_to.is_not_null()),
            )
            .set(schema::account_balance::valid_to.eq(MAX_TS))
            .execute(&mut conn)
            .await
            .unwrap();
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let accounts = by_address(
                gw.account_snapshots(&Chain::Ethereum, &mut conn)
                    .await
                    .unwrap(),
            );

            assert_eq!(accounts, by_address(vec![expected_c0(), expected_c1()]));
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
                .account_snapshots(&Chain::Ethereum, &mut conn)
                .await
                .unwrap();
            let components = gw
                .component_snapshots(&Chain::Ethereum, None, &mut conn)
                .await
                .unwrap();

            assert_eq!(accounts.len(), 2, "reopened code, balance and account rows are live");
            assert_eq!(components.len(), 3, "reopened component rows are live");
        })
        .await;
    }

    /// The database versions rows by `valid_from` alone, so a value is stamped with its row's
    /// `valid_from` even where that differs from the timestamp of the `modify_tx` block.
    #[tokio::test]
    async fn account_snapshots_stamp_values_with_their_valid_from_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            let written = db_fixtures::yesterday_one_am();
            db_fixtures::insert_slots(&mut conn, f.c0, f.txn[3], &written, None, &[(9, 9, None)])
                .await;
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let accounts = by_address(
                gw.account_snapshots(&Chain::Ethereum, &mut conn)
                    .await
                    .unwrap(),
            );

            let c0 = &accounts[&Bytes::from_str(C0).unwrap()];
            assert_eq!(
                c0.written_at.slots[&Bytes::from(9u64).lpad(32, 0)],
                WriteTimestamp::new(written),
                "the block of txn 3 is at half past midnight"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn component_snapshots_stamp_a_component_without_rows_with_its_created_at_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let f = setup_accounts(&mut conn).await;
            setup_components(&mut conn, &f).await;
            let created = db_fixtures::yesterday_one_am();
            diesel::update(
                schema::protocol_component::table
                    .filter(schema::protocol_component::external_id.eq("p2")),
            )
            .set(schema::protocol_component::created_at.eq(created))
            .execute(&mut conn)
            .await
            .unwrap();
            let gw = PostgresGateway::from_connection(&mut conn).await;

            let p2 = gw
                .component_snapshots(&Chain::Ethereum, None, &mut conn)
                .await
                .unwrap()
                .into_iter()
                .find(|c| c.state.component_id == "p2")
                .unwrap();

            assert_eq!(
                p2.updated_at,
                WriteTimestamp::new(created),
                "the creation block of p2 is at half past midnight"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn bound_snapshot_reads_applies_only_inside_the_transaction_serial_db() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let setting = |name: &'static str| {
                diesel::select(diesel::dsl::sql::<Text>(&format!("current_setting('{name}')")))
            };
            let before = setting("statement_timeout")
                .get_result::<String>(&mut conn)
                .await
                .unwrap();

            let inside = snapshot_transaction(&mut conn)
                .run(|conn| {
                    async move {
                        bound_snapshot_reads(conn).await?;
                        let timeout = setting("statement_timeout")
                            .get_result::<String>(conn)
                            .await
                            .map_err(PostgresError::from)?;
                        let check = setting("client_connection_check_interval")
                            .get_result::<String>(conn)
                            .await
                            .map_err(PostgresError::from)?;
                        Result::<_, PostgresError>::Ok((timeout, check))
                    }
                    .scope_boxed()
                })
                .await
                .unwrap();
            let after = setting("statement_timeout")
                .get_result::<String>(&mut conn)
                .await
                .unwrap();

            assert_eq!(inside, ("10min".to_string(), "10s".to_string()));
            assert_eq!(after, before, "the pooled connection keeps its own timeout");
        })
        .await;
    }
}
