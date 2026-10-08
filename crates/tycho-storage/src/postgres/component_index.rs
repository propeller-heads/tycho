//! An in-memory index over the `protocol_component` table, used to count and page
//! `get_protocol_components` results without SQL.
//!
//! # Why this exists
//!
//! A protocol system can hold millions of components. In SQL, every page of a request runs a
//! `COUNT` over all components of the system and an `OFFSET` scan to the requested page, both
//! joined to `component_tvl`. Each page repeats that work, so paging through a whole system costs
//! time quadratic in its size, and a deep page takes tens of seconds. The index answers the count
//! and picks the ids of the page from memory. Postgres then loads only the components on the page,
//! by primary key, which costs the same for every page.
//!
//! # What is held
//!
//! Only what filtering, counting and paging need: per chain and protocol system, the database ids
//! of the components in ascending order (the order of the SQL path) and the TVL of each. The
//! component contents (tokens, contracts, attributes) stay in Postgres. They never change after
//! insertion, but on large chains there are too many of them to hold in memory.
//!
//! TVL follows the SQL semantics of `LEFT JOIN component_tvl ... WHERE tvl > threshold`: a
//! component without a TVL row never matches a threshold, whatever its value. Soft-deleted
//! components stay in the index, because the SQL path returns them too.
//!
//! Only requests with a protocol system and without component ids use the index. The others go to
//! SQL, where id lookups are cheap.
//!
//! # How the index stays correct
//!
//! The database is the source of truth. The two halves of the index change differently, so each
//! has its own way to stay fresh.
//!
//! **Components** are only ever inserted, apart from rare manual hard deletes:
//!
//! - The write path adds the components it inserted, after its transaction commits. A rolled-back
//!   transaction therefore never leaves an id behind.
//! - Every refresh loads the components with an id above the highest indexed id. This catches
//!   components inserted by other processes, with one primary-key range read.
//!
//! **TVL** is written by other processes, at a cadence and in a way this code does not control:
//!
//! - Every refresh reads the write counters of `component_tvl` from `pg_stat_user_tables`. They
//!   move on every insert, update and delete, including writes that bypass triggers, and reading
//!   them scans no table. Unchanged counters mean no TVL query at all.
//! - Inserts or updates: load the rows with a `modified_ts` no load has read yet (see
//!   [`TvlCursor`]). When the counters grew but no such row exists, the writes did not move
//!   `modified_ts`, and all TVL is reloaded instead. Rolled-back writes and statistics resets also
//!   move the counters, which only costs an extra load.
//! - A write that touched a large share of the rows reads all TVL directly (see
//!   [`BULK_WRITE_SHARE`]).
//! - Deletes: `modified_ts` cannot show a deleted row, so all TVL is reloaded.
//!
//! Known limit: a writer that bypasses `modified_ts` for some rows while it moves it for others is
//! not detected; the periodic full reload corrects those rows.
//!
//! **Full reload**: at startup, when the delete counter of `protocol_component` moves, and at a
//! long fixed interval as a safety net, components and TVL are both read again and swapped in. This
//! also picks up a component another process committed with a lower id than one already indexed,
//! which the id check cannot see.
//!
//! Full reads of components or TVL are sequential scans of one table without `ORDER BY`; the
//! other reads use the primary key or the `modified_ts` index. Rows are sorted here, and TVL rows
//! are matched to components by id, so no read joins the two tables.
//!
//! # Concurrency
//!
//! Each chain's index sits behind one `RwLock`, never held across an `await`. A query holds the
//! read lock for one scan of one protocol system. TVL loads take the write lock only to write the
//! fetched values. A full reload builds a new index without the lock, then swaps it in. Components
//! written through while it ran are recorded in a journal and applied again on the swap, so the
//! swap never drops them.
//!
//! # Cost
//!
//! 16 bytes per component plus a small per-system overhead: ~85 MB for 5.25M components. A load
//! holds the fetched rows, ~24 bytes each, until they are applied; a full reload also holds a
//! second index until the swap.
use std::{
    collections::{BTreeSet, HashMap},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::NaiveDateTime;
use diesel::{prelude::*, sql_types::BigInt};
use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection, RunQueryDsl};
use tracing::{debug, error, info, warn};
use tycho_common::{
    models::{Chain, PaginationParams},
    storage::StorageError,
};

use crate::postgres::{schema, PostgresError};

/// TVL of a component without a `component_tvl` row. It never compares greater than a threshold.
const NO_TVL: f64 = f64::NEG_INFINITY;

/// How far behind the newest `modified_ts` already applied a TVL load reads again. The update
/// trigger stamps a row with the start time of its transaction, so a writer transaction that ran
/// while an earlier load read the table commits rows older than what that load saw.
const TVL_OVERLAP: chrono::Duration = chrono::Duration::minutes(10);

/// Share of the indexed components a TVL write must touch to be read as a full TVL reload instead
/// of a delta. Right after a large write, the planner's statistics do not show it yet, so a delta
/// read would walk the `modified_ts` index over most of the table instead of scanning it.
const BULK_WRITE_SHARE: f64 = 0.1;

/// Fewest written rows that count as a bulk write. A delta read of fewer rows is cheap whatever
/// plan Postgres picks.
const BULK_WRITE_MIN_ROWS: i64 = 10_000;

/// Most `modified_ts` values a TVL load excludes in SQL. Each one splits the read into one more
/// index range: `modified_ts <> ALL(...)` would hide the excluded rows from the planner's estimate
/// and turn the read into a sequential scan. Above this, the load reads the whole overlap window
/// and drops the rows already read after receiving them.
const MAX_EXCLUDED_STAMPS: usize = 50;

/// A component row inserted by the write path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NewComponentRow {
    pub(crate) chain_id: i64,
    pub(crate) protocol_system_id: i64,
    pub(crate) id: i64,
}

/// One page of a query: the database ids on the page, in ascending order, and the number of
/// components matching the filters on all pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComponentPage {
    pub(crate) ids: Vec<i64>,
    pub(crate) total: i64,
}

/// What a [`ComponentIndex::refresh`] did, besides loading new component ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The TVL counters did not move; no TVL was read.
    Unchanged,
    /// Loaded the TVL rows with a recent `modified_ts`.
    TvlDelta { n_rows: usize },
    /// Reloaded all TVL: rows were deleted, or a write did not move `modified_ts`.
    TvlReload { n_rows: usize },
    /// Reloaded components and TVL.
    FullReload { n_components: usize },
}

/// Components of one protocol system, in ascending id order.
#[derive(Debug, Default)]
struct SystemIndex {
    ids: Vec<i64>,
    /// TVL per position in `ids`, [`NO_TVL`] when the component has no TVL row.
    tvl: Vec<f64>,
}

impl SystemIndex {
    /// Adds a component and keeps ids in ascending order. A component already present keeps its
    /// current TVL.
    fn insert(&mut self, id: i64, tvl: f64) {
        if self
            .ids
            .last()
            .is_none_or(|&last| last < id)
        {
            self.ids.push(id);
            self.tvl.push(tvl);
            return;
        }
        if let Err(position) = self.ids.binary_search(&id) {
            self.ids.insert(position, id);
            self.tvl.insert(position, tvl);
        }
    }

    /// Ids on the page `[offset, offset + limit)` of the components with a TVL above `min_tvl`,
    /// and the number of such components on all pages.
    fn query(&self, min_tvl: Option<f64>, offset: usize, limit: usize) -> ComponentPage {
        let Some(threshold) = min_tvl else {
            let start = offset.min(self.ids.len());
            let end = offset
                .saturating_add(limit)
                .min(self.ids.len());
            return ComponentPage {
                ids: self.ids[start..end].to_vec(),
                total: self.ids.len() as i64,
            };
        };

        let mut total = 0usize;
        let mut ids = Vec::new();
        for (id, tvl) in self.ids.iter().zip(&self.tvl) {
            if *tvl > threshold {
                if total >= offset && ids.len() < limit {
                    ids.push(*id);
                }
                total += 1;
            }
        }
        ComponentPage { ids, total: total as i64 }
    }
}

/// All components of one chain, grouped by protocol system id.
#[derive(Debug, Default)]
struct ChainIndex {
    systems: HashMap<i64, SystemIndex>,
    /// Highest component id in the index, 0 when empty.
    max_id: i64,
    /// Components inserted while a full reload runs, as `(protocol system id, id)`. `None` when no
    /// full reload runs.
    journal: Option<Vec<(i64, i64)>>,
}

impl ChainIndex {
    /// Builds an index without TVL from `(id, protocol system id)` rows sorted by id.
    fn from_sorted_components(rows: &[(i64, i64)]) -> Self {
        let mut chain_index = ChainIndex::default();
        for (id, protocol_system_id) in rows {
            chain_index.insert(*protocol_system_id, *id, NO_TVL);
        }
        chain_index
    }

    fn insert(&mut self, protocol_system_id: i64, id: i64, tvl: f64) {
        self.systems
            .entry(protocol_system_id)
            .or_default()
            .insert(id, tvl);
        self.max_id = self.max_id.max(id);
        if let Some(journal) = &mut self.journal {
            journal.push((protocol_system_id, id));
        }
    }

    /// Writes the TVL of `rows`, which must be sorted by component id. Rows of components that are
    /// not in the index are ignored: they belong to another chain, or to a component the index
    /// does not hold yet.
    ///
    /// Every system's ids are sorted too, so one cursor per system walks forward with the rows:
    /// one pass over the index in total, and no id-to-system map in memory.
    fn apply_tvl(&mut self, rows: &[TvlRow]) {
        let mut cursors: Vec<(&mut SystemIndex, usize)> = self
            .systems
            .values_mut()
            .map(|system| (system, 0))
            .collect();
        for row in rows {
            for (system, position) in cursors.iter_mut() {
                while *position < system.ids.len() && system.ids[*position] < row.component_id {
                    *position += 1;
                }
                if *position < system.ids.len() && system.ids[*position] == row.component_id {
                    system.tvl[*position] = row.tvl;
                    break;
                }
            }
        }
    }

    fn clear_tvl(&mut self) {
        for system in self.systems.values_mut() {
            system.tvl.fill(NO_TVL);
        }
    }

    fn n_components(&self) -> usize {
        self.systems
            .values()
            .map(|system| system.ids.len())
            .sum()
    }
}

/// One `component_tvl` row, in index representation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TvlRow {
    component_id: i64,
    tvl: f64,
    modified_ts: NaiveDateTime,
}

/// Write counters of the tables the index derives from, as reported by `pg_stat_user_tables`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WriteCounters {
    pub(crate) tvl_inserts: i64,
    pub(crate) tvl_updates: i64,
    pub(crate) tvl_deletes: i64,
    pub(crate) component_deletes: i64,
}

#[derive(QueryableByName)]
struct TableCounters {
    #[diesel(sql_type = diesel::sql_types::Text)]
    relname: String,
    #[diesel(sql_type = BigInt)]
    n_tup_ins: i64,
    #[diesel(sql_type = BigInt)]
    n_tup_upd: i64,
    #[diesel(sql_type = BigInt)]
    n_tup_del: i64,
}

impl WriteCounters {
    async fn read(conn: &mut AsyncPgConnection) -> Result<Self, StorageError> {
        let rows: Vec<TableCounters> = diesel::sql_query(
            "SELECT relname::text AS relname, n_tup_ins, n_tup_upd, n_tup_del \
             FROM pg_stat_user_tables \
             WHERE relid IN ('component_tvl'::regclass, 'protocol_component'::regclass)",
        )
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

        let mut counters = WriteCounters::default();
        for row in rows {
            match row.relname.as_str() {
                "component_tvl" => {
                    counters.tvl_inserts = row.n_tup_ins;
                    counters.tvl_updates = row.n_tup_upd;
                    counters.tvl_deletes = row.n_tup_del;
                }
                "protocol_component" => counters.component_deletes = row.n_tup_del,
                other => warn!(table = other, "Unexpected table in component index counters"),
            }
        }
        Ok(counters)
    }

    fn tvl_writes(&self) -> i64 {
        self.tvl_inserts + self.tvl_updates
    }
}

/// Which `component_tvl` rows a TVL load still has to read.
///
/// A writer transaction stamps all its rows with one `modified_ts`, its start time, and its rows
/// become visible together when it commits. Once a load has read rows with a given stamp, no row
/// with that stamp can arrive later. A row committed after an earlier load can still carry a stamp
/// older than the newest one read, so loads read again from [`TVL_OVERLAP`] before the newest
/// stamp, minus the stamps already read in that window.
#[derive(Debug, Default)]
struct TvlCursor {
    /// Newest `modified_ts` read so far.
    newest: NaiveDateTime,
    /// Stamps read within [`TVL_OVERLAP`] of `newest`.
    seen: BTreeSet<NaiveDateTime>,
}

impl TvlCursor {
    /// Rows with a `modified_ts` above this may still be unread.
    fn since(&self) -> NaiveDateTime {
        self.newest - TVL_OVERLAP
    }

    /// Stamps a load can exclude in SQL, empty when there are too many to list.
    fn excluded_stamps(&self) -> Vec<NaiveDateTime> {
        if self.seen.len() > MAX_EXCLUDED_STAMPS {
            return Vec::new();
        }
        self.seen.iter().copied().collect()
    }

    fn is_unread(&self, modified_ts: NaiveDateTime) -> bool {
        modified_ts > self.since() && !self.seen.contains(&modified_ts)
    }

    fn record(&mut self, rows: &[TvlRow]) {
        for row in rows {
            self.newest = self.newest.max(row.modified_ts);
        }
        let since = self.since();
        self.seen.extend(
            rows.iter()
                .map(|row| row.modified_ts)
                .filter(|ts| *ts > since),
        );
        self.seen = self.seen.split_off(&since);
        self.seen.remove(&since);
    }
}

/// Bookkeeping of the refresh loop.
#[derive(Debug)]
struct RefreshState {
    /// Counters read before the last successful refresh.
    counters: WriteCounters,
    tvl_cursor: TvlCursor,
    last_full_reload: Instant,
}

/// In-memory filter and paging index over `protocol_component`, one per chain.
///
/// Holds only the database id and the TVL of each component, grouped by protocol system in
/// ascending id order. Answers which components match a request and how many there are; the
/// component contents stay in Postgres. See the module docs for the design.
pub struct ComponentIndex {
    chains: HashMap<Chain, RwLock<ChainIndex>>,
    /// Database id of each indexed chain.
    chain_db_ids: HashMap<Chain, i64>,
    refresh_state: Mutex<RefreshState>,
}

impl ComponentIndex {
    pub async fn from_pool(
        pool: Pool<AsyncPgConnection>,
        chains: &[Chain],
    ) -> Result<Self, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;
        Self::from_connection(&mut conn, chains).await
    }

    /// Loads the components of the given chains. Chains present in the `chain` table but not
    /// requested are not loaded, and requests for them go to SQL. Chain rows whose name this build
    /// does not recognize are skipped with a warning, so a shared database cannot prevent startup.
    pub async fn from_connection(
        conn: &mut AsyncPgConnection,
        chains: &[Chain],
    ) -> Result<Self, StorageError> {
        if chains.is_empty() {
            return Err(StorageError::Unexpected(
                "Component index requires at least one configured chain".to_string(),
            ));
        }

        let chain_rows: Vec<(i64, String)> = schema::chain::table
            .select((schema::chain::id, schema::chain::name))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut chain_db_ids = HashMap::new();
        for (chain_db_id, chain_name) in chain_rows {
            let Ok(chain) = Chain::from_str(&chain_name) else {
                warn!(chain = %chain_name, "Skipping unknown chain in chain table");
                continue;
            };
            if chains.contains(&chain) {
                chain_db_ids.insert(chain, chain_db_id);
            }
        }

        let index = Self {
            chains: chain_db_ids
                .keys()
                .map(|chain| (*chain, RwLock::new(ChainIndex::default())))
                .collect(),
            chain_db_ids,
            refresh_state: Mutex::new(RefreshState {
                counters: WriteCounters::default(),
                tvl_cursor: TvlCursor::default(),
                last_full_reload: Instant::now(),
            }),
        };
        let counters = WriteCounters::read(conn).await?;
        index.full_reload(conn).await?;
        index.state().counters = counters;
        Ok(index)
    }

    /// The page of components of `protocol_system_id` on `chain` with a TVL above `min_tvl`, in
    /// ascending id order, with the same rows and total as the SQL path. Without pagination, all
    /// matching components form the page. Returns `None` when the chain is not indexed.
    pub(crate) fn query(
        &self,
        chain: &Chain,
        protocol_system_id: i64,
        min_tvl: Option<f64>,
        pagination: Option<&PaginationParams>,
    ) -> Option<ComponentPage> {
        let (offset, limit) = pagination
            .map(|params| (params.offset().max(0) as usize, params.page_size.max(0) as usize))
            .unwrap_or((0, usize::MAX));
        let chain_index = self
            .chains
            .get(chain)?
            .read()
            .expect("component index lock poisoned");
        let page = match chain_index
            .systems
            .get(&protocol_system_id)
        {
            Some(system) => system.query(min_tvl, offset, limit),
            None => ComponentPage { ids: Vec::new(), total: 0 },
        };
        Some(page)
    }

    /// Adds components inserted by the write path. Rows of chains that are not indexed are
    /// ignored. A new component has no TVL until a TVL load finds its row, like in the database.
    pub(crate) fn insert(&self, rows: &[NewComponentRow]) {
        if rows.is_empty() {
            return;
        }
        for (chain, chain_lock) in &self.chains {
            let chain_db_id = self.chain_db_ids[chain];
            let mut chain_index = chain_lock
                .write()
                .expect("component index lock poisoned");
            for row in rows
                .iter()
                .filter(|row| row.chain_id == chain_db_id)
            {
                chain_index.insert(row.protocol_system_id, row.id, NO_TVL);
            }
        }
    }

    /// Brings the index up to date with the database: loads new component ids, then reads TVL or
    /// reloads everything as the write counters require (see the module docs). Does a full
    /// reload when the last one is older than `full_reload_interval`. On error the counters are
    /// not advanced, so the next call retries.
    pub async fn refresh(
        &self,
        conn: &mut AsyncPgConnection,
        full_reload_interval: Duration,
    ) -> Result<RefreshOutcome, StorageError> {
        let counters = WriteCounters::read(conn).await?;
        self.refresh_with_counters(conn, counters, full_reload_interval)
            .await
    }

    /// [`Self::refresh`] with counters read by the caller. The counters must be read before any
    /// table, so a write that lands during this refresh moves them again for the next one.
    async fn refresh_with_counters(
        &self,
        conn: &mut AsyncPgConnection,
        counters: WriteCounters,
        full_reload_interval: Duration,
    ) -> Result<RefreshOutcome, StorageError> {
        let (last, full_reload_due) = {
            let state = self.state();
            (state.counters, state.last_full_reload.elapsed() >= full_reload_interval)
        };

        let outcome = if full_reload_due || counters.component_deletes != last.component_deletes {
            RefreshOutcome::FullReload { n_components: self.full_reload(conn).await? }
        } else {
            self.load_new_components(conn).await?;
            if counters.tvl_deletes != last.tvl_deletes {
                RefreshOutcome::TvlReload { n_rows: self.reload_tvl(conn).await? }
            } else if counters.tvl_writes() != last.tvl_writes() {
                let n_writes = counters.tvl_writes() - last.tvl_writes();
                if self.is_bulk_write(n_writes) {
                    RefreshOutcome::TvlReload { n_rows: self.reload_tvl(conn).await? }
                } else {
                    self.load_changed_tvl(conn, n_writes)
                        .await?
                }
            } else {
                RefreshOutcome::Unchanged
            }
        };

        self.state().counters = counters;
        Ok(outcome)
    }

    /// Adds the components with an id above the highest indexed id of their chain.
    async fn load_new_components(&self, conn: &mut AsyncPgConnection) -> Result<(), StorageError> {
        for (chain, chain_db_id) in &self.chain_db_ids {
            let chain_lock = &self.chains[chain];
            let max_id = chain_lock
                .read()
                .expect("component index lock poisoned")
                .max_id;
            let rows: Vec<(i64, i64)> = schema::protocol_component::table
                .filter(schema::protocol_component::chain_id.eq(*chain_db_id))
                .filter(schema::protocol_component::id.gt(max_id))
                .select((
                    schema::protocol_component::id,
                    schema::protocol_component::protocol_system_id,
                ))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;
            if rows.is_empty() {
                continue;
            }
            debug!(chain = %chain, n_components = rows.len(), "Component index loaded new components");
            let mut chain_index = chain_lock
                .write()
                .expect("component index lock poisoned");
            for (id, protocol_system_id) in rows {
                chain_index.insert(protocol_system_id, id, NO_TVL);
            }
        }
        Ok(())
    }

    /// Loads the TVL rows the [`TvlCursor`] has not read yet. Falls back to a full TVL reload when
    /// there are none: the counters reported `n_writes` writes, so those writes did not move
    /// `modified_ts`.
    async fn load_changed_tvl(
        &self,
        conn: &mut AsyncPgConnection,
        n_writes: i64,
    ) -> Result<RefreshOutcome, StorageError> {
        let (since, excluded) = {
            let state = self.state();
            (state.tvl_cursor.since(), state.tvl_cursor.excluded_stamps())
        };
        let mut rows = load_tvl(conn, Some(since), &excluded).await?;
        {
            let state = self.state();
            rows.retain(|row| {
                state
                    .tvl_cursor
                    .is_unread(row.modified_ts)
            });
        }
        if rows.is_empty() {
            warn!(n_writes, "TVL writes without a new modified_ts; reloading all component TVL");
            return Ok(RefreshOutcome::TvlReload { n_rows: self.reload_tvl(conn).await? });
        }
        let n_rows = rows.len();
        self.write_tvl(&rows, false);
        Ok(RefreshOutcome::TvlDelta { n_rows })
    }

    /// Reads every TVL row and replaces the TVL of the index with it. Returns the number of rows.
    async fn reload_tvl(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        let rows = load_tvl(conn, None, &[]).await?;
        self.write_tvl(&rows, true);
        Ok(rows.len())
    }

    /// Writes `rows`, sorted by component id, into every chain's index. With `replace_all`, all
    /// other components lose their TVL. Records the rows as read.
    fn write_tvl(&self, rows: &[TvlRow], replace_all: bool) {
        for chain_lock in self.chains.values() {
            let mut chain_index = chain_lock
                .write()
                .expect("component index lock poisoned");
            if replace_all {
                chain_index.clear_tvl();
            }
            chain_index.apply_tvl(rows);
        }
        self.state().tvl_cursor.record(rows);
    }

    /// Reads components and TVL again and swaps the result in. Returns the number of components.
    async fn full_reload(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        let started = Instant::now();
        for chain_lock in self.chains.values() {
            chain_lock
                .write()
                .expect("component index lock poisoned")
                .journal = Some(Vec::new());
        }

        let loaded = self.load_full(conn).await;

        let mut n_components = 0;
        let rebuilt = match loaded {
            Ok(rebuilt) => rebuilt,
            Err(err) => {
                for chain_lock in self.chains.values() {
                    chain_lock
                        .write()
                        .expect("component index lock poisoned")
                        .journal = None;
                }
                return Err(err);
            }
        };
        let (mut rebuilt, tvl_rows) = rebuilt;
        for (chain, chain_lock) in &self.chains {
            let mut new_index = rebuilt
                .remove(chain)
                .unwrap_or_default();
            let mut chain_index = chain_lock
                .write()
                .expect("component index lock poisoned");
            for (protocol_system_id, id) in chain_index
                .journal
                .take()
                .unwrap_or_default()
            {
                new_index.insert(protocol_system_id, id, NO_TVL);
            }
            n_components += new_index.n_components();
            info!(
                chain = %chain,
                n_components = new_index.n_components(),
                n_protocol_systems = new_index.systems.len(),
                n_tvl_rows = tvl_rows.len(),
                elapsed = ?started.elapsed(),
                "Reloaded component index"
            );
            *chain_index = new_index;
        }
        let mut state = self.state();
        state.tvl_cursor.record(&tvl_rows);
        state.last_full_reload = Instant::now();
        Ok(n_components)
    }

    /// Reads the components of every indexed chain, then all TVL, and builds new chain indexes.
    async fn load_full(
        &self,
        conn: &mut AsyncPgConnection,
    ) -> Result<(HashMap<Chain, ChainIndex>, Vec<TvlRow>), StorageError> {
        let mut components = HashMap::new();
        for (chain, chain_db_id) in &self.chain_db_ids {
            let mut rows: Vec<(i64, i64)> = schema::protocol_component::table
                .filter(schema::protocol_component::chain_id.eq(*chain_db_id))
                .select((
                    schema::protocol_component::id,
                    schema::protocol_component::protocol_system_id,
                ))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;
            rows.sort_unstable();
            components.insert(*chain, rows);
        }
        let tvl_rows = load_tvl(conn, None, &[]).await?;

        let mut rebuilt = HashMap::new();
        for (chain, rows) in components {
            let mut chain_index = ChainIndex::from_sorted_components(&rows);
            chain_index.apply_tvl(&tvl_rows);
            rebuilt.insert(chain, chain_index);
        }
        Ok((rebuilt, tvl_rows))
    }

    fn is_bulk_write(&self, n_writes: i64) -> bool {
        n_writes >= BULK_WRITE_MIN_ROWS &&
            n_writes as f64 >= self.n_components() as f64 * BULK_WRITE_SHARE
    }

    fn n_components(&self) -> usize {
        self.chains
            .values()
            .map(|chain_lock| {
                chain_lock
                    .read()
                    .expect("component index lock poisoned")
                    .n_components()
            })
            .sum()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RefreshState> {
        self.refresh_state
            .lock()
            .expect("component index lock poisoned")
    }

    /// Spawns a detached task that calls [`Self::refresh`] every `period`, with a full reload at
    /// least every `full_reload_interval`.
    pub fn spawn_refresh_task(
        self: &Arc<Self>,
        pool: Pool<AsyncPgConnection>,
        period: Duration,
        full_reload_interval: Duration,
    ) {
        let index = Arc::clone(self);
        tokio::spawn(async move {
            info!(
                period_secs = period.as_secs(),
                full_reload_interval_secs = full_reload_interval.as_secs(),
                "Component index refresh task started"
            );
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it, the index was just loaded.
            interval.tick().await;
            loop {
                interval.tick().await;
                // A bounded wait so pool starvation is visible instead of a silently stalled task.
                match tokio::time::timeout(Duration::from_secs(30), pool.get()).await {
                    Ok(Ok(mut conn)) => {
                        let started = Instant::now();
                        match index
                            .refresh(&mut conn, full_reload_interval)
                            .await
                        {
                            Ok(RefreshOutcome::Unchanged) => {}
                            Ok(outcome) => info!(
                                ?outcome,
                                elapsed = ?started.elapsed(),
                                "Component index refreshed"
                            ),
                            Err(err) => error!(%err, "Component index refresh failed"),
                        }
                    }
                    Ok(Err(err)) => {
                        error!(%err, "Component index refresh could not get a connection")
                    }
                    Err(_) => {
                        error!("Component index refresh timed out waiting for a DB connection")
                    }
                }
            }
        });
    }
}

/// Reads the `component_tvl` rows modified after `since` with a `modified_ts` not in `excluded`
/// (sorted), or all rows, sorted by component id. Rows of every chain are returned; `component_tvl`
/// has no chain column, and matching by id discards the others.
async fn load_tvl(
    conn: &mut AsyncPgConnection,
    since: Option<NaiveDateTime>,
    excluded: &[NaiveDateTime],
) -> Result<Vec<TvlRow>, StorageError> {
    let mut query = schema::component_tvl::table
        .select((
            schema::component_tvl::protocol_component_id,
            schema::component_tvl::tvl,
            schema::component_tvl::modified_ts,
        ))
        .into_boxed();
    if let Some(since) = since {
        let mut bounds = vec![since];
        bounds.extend(
            excluded
                .iter()
                .copied()
                .filter(|stamp| *stamp > since),
        );
        query = query.filter(between_stamps(&bounds));
    }
    let rows: Vec<(i64, f64, NaiveDateTime)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;
    let mut rows: Vec<TvlRow> = rows
        .into_iter()
        .map(|(component_id, tvl, modified_ts)| TvlRow {
            component_id,
            tvl: index_tvl(tvl),
            modified_ts,
        })
        .collect();
    rows.sort_unstable_by_key(|row| row.component_id);
    Ok(rows)
}

/// `modified_ts` strictly between consecutive `bounds`, or after the last one, as a disjunction of
/// index ranges. `bounds` must be sorted and not empty.
fn between_stamps(
    bounds: &[NaiveDateTime],
) -> Box<
    dyn BoxableExpression<
        schema::component_tvl::table,
        diesel::pg::Pg,
        SqlType = diesel::sql_types::Bool,
    >,
> {
    use schema::component_tvl::modified_ts;

    let last = *bounds
        .last()
        .expect("between_stamps requires at least one bound");
    let mut condition: Box<
        dyn BoxableExpression<
            schema::component_tvl::table,
            diesel::pg::Pg,
            SqlType = diesel::sql_types::Bool,
        >,
    > = Box::new(modified_ts.gt(last));
    for pair in bounds.windows(2) {
        condition = Box::new(
            condition.or(modified_ts
                .gt(pair[0])
                .and(modified_ts.lt(pair[1]))),
        );
    }
    condition
}

/// Maps a `component_tvl.tvl` value to its index representation. Postgres orders `NaN` above every
/// other float, so `NaN > threshold` holds there; `f64::INFINITY` keeps that result for any
/// threshold a request can carry.
fn index_tvl(tvl: f64) -> f64 {
    if tvl.is_nan() {
        f64::INFINITY
    } else {
        tvl
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn system(entries: &[(i64, Option<f64>)]) -> SystemIndex {
        let mut system = SystemIndex::default();
        for (id, tvl) in entries {
            system.insert(*id, tvl.map_or(NO_TVL, index_tvl));
        }
        system
    }

    fn page(ids: &[i64], total: i64) -> ComponentPage {
        ComponentPage { ids: ids.to_vec(), total }
    }

    fn tvl_row(component_id: i64, tvl: f64) -> TvlRow {
        TvlRow { component_id, tvl, modified_ts: NaiveDateTime::default() }
    }

    #[test]
    fn test_query_without_tvl_filter_slices_all_components() {
        let system = system(&[(1, Some(5.0)), (2, None), (3, Some(0.0)), (4, Some(9.0))]);

        assert_eq!(system.query(None, 0, 2), page(&[1, 2], 4));
        assert_eq!(system.query(None, 2, 2), page(&[3, 4], 4));
        assert_eq!(system.query(None, 4, 2), page(&[], 4));
        assert_eq!(system.query(None, 10, 2), page(&[], 4));
        assert_eq!(system.query(None, 0, usize::MAX), page(&[1, 2, 3, 4], 4));
    }

    #[test]
    fn test_tvl_filter_is_strict_and_excludes_missing_tvl() {
        let system = system(&[(1, Some(5.0)), (2, None), (3, Some(0.0)), (4, Some(9.0))]);

        assert_eq!(system.query(Some(5.0), 0, 10), page(&[4], 1));
        assert_eq!(system.query(Some(0.0), 0, 10), page(&[1, 4], 2));
        // A negative threshold still excludes components without a TVL row.
        assert_eq!(system.query(Some(-1.0), 0, 10), page(&[1, 3, 4], 3));
    }

    #[test]
    fn test_tvl_filter_paginates_over_matches() {
        let system = system(&[(1, Some(1.0)), (2, None), (3, Some(2.0)), (4, Some(3.0))]);

        assert_eq!(system.query(Some(0.5), 0, 2), page(&[1, 3], 3));
        assert_eq!(system.query(Some(0.5), 2, 2), page(&[4], 3));
        assert_eq!(system.query(Some(0.5), 4, 2), page(&[], 3));
    }

    #[test]
    fn test_nan_tvl_matches_every_threshold() {
        let system = system(&[(1, Some(f64::NAN))]);

        assert_eq!(system.query(Some(1e300), 0, 10), page(&[1], 1));
    }

    #[test]
    fn test_insert_keeps_ids_sorted_and_ignores_duplicates() {
        let mut system = system(&[(10, Some(1.0)), (30, Some(3.0))]);
        system.insert(20, 2.0);
        system.insert(5, 0.5);
        system.insert(30, NO_TVL);

        assert_eq!(system.ids, vec![5, 10, 20, 30]);
        assert_eq!(system.tvl, vec![0.5, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_apply_tvl_matches_rows_across_systems_and_skips_unknown_ids() {
        let mut chain_index =
            ChainIndex::from_sorted_components(&[(1, 7), (2, 8), (4, 7), (6, 8), (9, 7)]);

        chain_index.apply_tvl(&[
            tvl_row(0, 99.0),
            tvl_row(2, 2.0),
            tvl_row(3, 99.0),
            tvl_row(4, 4.0),
            tvl_row(9, 9.0),
            tvl_row(10, 99.0),
        ]);

        assert_eq!(chain_index.systems[&7].tvl, vec![NO_TVL, 4.0, 9.0]);
        assert_eq!(chain_index.systems[&8].tvl, vec![2.0, NO_TVL]);
    }

    #[test]
    fn test_clear_tvl_then_apply_replaces_all_values() {
        let mut chain_index = ChainIndex::from_sorted_components(&[(1, 7), (2, 7)]);
        chain_index.apply_tvl(&[tvl_row(1, 1.0), tvl_row(2, 2.0)]);

        chain_index.clear_tvl();
        chain_index.apply_tvl(&[tvl_row(2, 3.0)]);

        assert_eq!(chain_index.systems[&7].tvl, vec![NO_TVL, 3.0]);
    }

    fn index_with(chain_db_id: i64, rows: &[(i64, i64)]) -> ComponentIndex {
        let mut chain_index = ChainIndex::default();
        for (protocol_system_id, id) in rows {
            chain_index.insert(*protocol_system_id, *id, NO_TVL);
        }
        ComponentIndex {
            chains: HashMap::from([(Chain::Ethereum, RwLock::new(chain_index))]),
            chain_db_ids: HashMap::from([(Chain::Ethereum, chain_db_id)]),
            refresh_state: Mutex::new(RefreshState {
                counters: WriteCounters::default(),
                tvl_cursor: TvlCursor::default(),
                last_full_reload: Instant::now(),
            }),
        }
    }

    #[test]
    fn test_query_unknown_system_is_empty_and_unknown_chain_is_none() {
        let index = index_with(1, &[(7, 100)]);

        assert_eq!(index.query(&Chain::Ethereum, 8, None, None), Some(page(&[], 0)));
        assert_eq!(index.query(&Chain::Base, 7, None, None), None);
    }

    #[test]
    fn test_insert_ignores_other_chains_and_sets_no_tvl() {
        let index = index_with(1, &[]);
        index.insert(&[
            NewComponentRow { chain_id: 1, protocol_system_id: 7, id: 100 },
            NewComponentRow { chain_id: 2, protocol_system_id: 7, id: 101 },
        ]);

        assert_eq!(index.query(&Chain::Ethereum, 7, None, None), Some(page(&[100], 1)));
        assert_eq!(index.query(&Chain::Ethereum, 7, Some(-1.0), None), Some(page(&[], 0)));
    }

    #[test]
    fn test_insert_during_full_reload_is_journaled() {
        let index = index_with(1, &[]);
        index.chains[&Chain::Ethereum]
            .write()
            .unwrap()
            .journal = Some(Vec::new());

        index.insert(&[NewComponentRow { chain_id: 1, protocol_system_id: 7, id: 100 }]);

        let journal = index.chains[&Chain::Ethereum]
            .write()
            .unwrap()
            .journal
            .take()
            .unwrap();
        assert_eq!(journal, vec![(7, 100)]);
    }

    fn at(secs: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .naive_utc()
    }

    fn stamped(modified_ts: NaiveDateTime) -> TvlRow {
        TvlRow { modified_ts, ..tvl_row(1, 1.0) }
    }

    #[test]
    fn test_tvl_cursor_newest_never_moves_back() {
        let mut cursor = TvlCursor::default();

        cursor.record(&[stamped(at(2_000))]);
        cursor.record(&[stamped(at(1_000))]);
        cursor.record(&[]);

        assert_eq!(cursor.newest, at(2_000));
    }

    #[test]
    fn test_tvl_cursor_reads_unseen_stamps_within_the_overlap_only() {
        let mut cursor = TvlCursor::default();
        let newest = at(100_000);
        cursor.record(&[stamped(newest), stamped(newest - chrono::Duration::minutes(3))]);

        assert!(!cursor.is_unread(newest));
        assert!(!cursor.is_unread(newest - chrono::Duration::minutes(3)));
        assert!(cursor.is_unread(newest - chrono::Duration::minutes(5)));
        assert!(cursor.is_unread(newest + chrono::Duration::seconds(1)));
        assert!(!cursor.is_unread(newest - TVL_OVERLAP));
        assert_eq!(cursor.excluded_stamps().len(), 2);
    }

    #[test]
    fn test_tvl_cursor_forgets_stamps_that_leave_the_overlap() {
        let mut cursor = TvlCursor::default();
        cursor.record(&[stamped(at(100_000))]);

        cursor.record(&[stamped(at(100_000) + TVL_OVERLAP)]);

        assert_eq!(cursor.seen.len(), 1);
    }

    #[test]
    fn test_tvl_cursor_stops_listing_too_many_stamps() {
        let mut cursor = TvlCursor::default();
        let rows: Vec<TvlRow> = (0..=MAX_EXCLUDED_STAMPS as i64)
            .map(|offset| stamped(at(100_000) + chrono::Duration::milliseconds(offset)))
            .collect();

        cursor.record(&rows);

        assert!(cursor.excluded_stamps().is_empty());
        assert!(!cursor.is_unread(at(100_000)));
    }
}

/// Benchmark of the load, refresh and query paths against a real database.
///
/// Read-only unless `BENCH_WRITES=1`, which also updates `component_tvl` to time the TVL delta
/// paths; only set it against a disposable database. Run with:
///   DATABASE_URL=... BENCH_CHAIN=bsc cargo test -p tycho-storage --release --lib \
///     component_index_benchmark -- --ignored --nocapture
#[cfg(test)]
mod benchmark {
    use diesel_async::AsyncConnection;

    use super::*;

    fn rss_mib() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .and_then(|value| {
                            value
                                .trim()
                                .trim_end_matches(" kB")
                                .parse::<f64>()
                                .ok()
                        })
                })
            })
            .map_or(0.0, |kib| kib / 1024.0)
    }

    fn time_queries(index: &ComponentIndex, chain: Chain, system_id: i64, min_tvl: Option<f64>) {
        let total = index
            .query(&chain, system_id, min_tvl, Some(&PaginationParams::new(0, 1)))
            .unwrap()
            .total;
        let page_size = 2550;
        let deep_page = (total / page_size / 2).max(0);
        for page in [0, deep_page] {
            let iterations = 20;
            let started = Instant::now();
            for _ in 0..iterations {
                index.query(
                    &chain,
                    system_id,
                    min_tvl,
                    Some(&PaginationParams::new(page, page_size)),
                );
            }
            println!(
                "query tvl>{min_tvl:?} page {page} of {}: {:?} (total {total})",
                total / page_size,
                started.elapsed() / iterations
            );
        }
    }

    #[tokio::test]
    #[ignore]
    async fn component_index_benchmark() {
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let chain = std::env::var("BENCH_CHAIN")
            .map(|name| Chain::from_str(&name).expect("invalid BENCH_CHAIN"))
            .unwrap_or(Chain::Ethereum);
        let writes = std::env::var("BENCH_WRITES").as_deref() == Ok("1");
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .expect("failed to connect");
        if !writes {
            diesel::sql_query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                .execute(&mut conn)
                .await
                .unwrap();
        }

        let started = Instant::now();
        WriteCounters::read(&mut conn)
            .await
            .unwrap();
        println!("counters read: {:?}", started.elapsed());

        let rss_before = rss_mib();
        let started = Instant::now();
        let index = ComponentIndex::from_connection(&mut conn, &[chain])
            .await
            .unwrap();
        let load_elapsed = started.elapsed();
        let (n_components, systems) = {
            let chain_index = index.chains[&chain].read().unwrap();
            let mut systems: Vec<(i64, usize)> = chain_index
                .systems
                .iter()
                .map(|(id, system)| (*id, system.ids.len()))
                .collect();
            systems.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
            (chain_index.n_components(), systems)
        };
        println!(
            "startup load: {load_elapsed:?}, {n_components} components, RSS {rss_before:.0} -> {:.0} MiB",
            rss_mib()
        );
        println!("systems (id, components): {systems:?}");

        let largest = systems[0].0;
        for min_tvl in [None, Some(-1.0), Some(0.1)] {
            time_queries(&index, chain, largest, min_tvl);
        }

        let started = Instant::now();
        index
            .load_new_components(&mut conn)
            .await
            .unwrap();
        println!("new-ids poll (nothing new): {:?}", started.elapsed());

        let started = Instant::now();
        let rows = load_tvl(&mut conn, None, &[])
            .await
            .unwrap();
        let fetch_elapsed = started.elapsed();
        let started = Instant::now();
        index.write_tvl(&rows, true);
        println!(
            "full TVL reload: fetch+sort {fetch_elapsed:?} ({} rows), write under lock {:?}, RSS {:.0} MiB",
            rows.len(),
            started.elapsed(),
            rss_mib()
        );
        drop(rows);

        let started = Instant::now();
        let n_components = index
            .full_reload(&mut conn)
            .await
            .unwrap();
        println!("full reload: {:?} ({n_components} components)", started.elapsed());

        if !writes {
            return;
        }
        let small_write = "UPDATE component_tvl SET tvl = tvl * 1.01 \
                           WHERE protocol_component_id IN \
                           (SELECT protocol_component_id FROM component_tvl ORDER BY random() LIMIT 1000)";
        for (label, sql) in [
            ("every row rewritten", "UPDATE component_tvl SET tvl = tvl * 1.01"),
            ("1000 rows changed, before autoanalyze", small_write),
            ("autoanalyze", "ANALYZE component_tvl"),
            ("1000 rows changed, after autoanalyze", small_write),
            ("counters moved without a visible write (e.g. rolled back)", "SELECT 1"),
        ] {
            let n_written = diesel::sql_query(sql)
                .execute(&mut conn)
                .await
                .unwrap();
            if label == "autoanalyze" {
                continue;
            }
            let mut counters = index.state().counters;
            counters.tvl_updates += n_written as i64;
            let started = Instant::now();
            let outcome = index
                .refresh_with_counters(&mut conn, counters, Duration::from_secs(6 * 3600))
                .await
                .unwrap();
            println!("TVL refresh, {label}: {:?} -> {outcome:?}", started.elapsed());
        }
    }
}

/// Tests against a real database: every query must return the same rows, order and total
/// through the index as through the SQL path, after each kind of refresh.
#[cfg(test)]
mod serial_db_test {
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::postgres::{db_fixtures, testing::run_against_db, PostgresGateway};

    const TX_HASH_0: &str = "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945";

    /// Long enough that no test triggers the periodic full reload by accident.
    const NO_PERIODIC_RELOAD: Duration = Duration::from_secs(3600);

    struct Fixture {
        system_ids: Vec<i64>,
        component_db_ids: Vec<i64>,
        sql_gateway: PostgresGateway,
        indexed_gateway: PostgresGateway,
    }

    impl Fixture {
        fn index(&self) -> &ComponentIndex {
            self.indexed_gateway
                .component_index
                .as_ref()
                .unwrap()
        }

        async fn assert_equivalent(&self, conn: &mut AsyncPgConnection) {
            assert_equivalent(&self.sql_gateway, &self.indexed_gateway, conn).await;
        }

        /// Refreshes with the stored counters grown as given, as if Postgres had reported those
        /// writes.
        async fn refresh(
            &self,
            conn: &mut AsyncPgConnection,
            grow: impl FnOnce(&mut WriteCounters),
        ) -> RefreshOutcome {
            let mut counters = self.index().state().counters;
            grow(&mut counters);
            self.index()
                .refresh_with_counters(conn, counters, NO_PERIODIC_RELOAD)
                .await
                .unwrap()
        }
    }

    /// Inserts two protocol systems on ethereum with these components:
    /// - `sys_a`: a0 (tvl 5), a1 (no tvl row), a2 (tvl 0), a3 (tvl 9), a4 (tvl -0.5, soft-deleted)
    /// - `sys_b`: b0 (tvl 1)
    ///
    /// plus one `sys_a` component on starknet, which ethereum queries must never return. Then
    /// builds a SQL gateway and one with a freshly loaded index.
    async fn setup(conn: &mut AsyncPgConnection) -> Fixture {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let starknet_id = db_fixtures::insert_chain(conn, "starknet").await;
        for chain in [chain_id, starknet_id] {
            // The gateway constructor requires the chain's native token to exist.
            db_fixtures::insert_token(
                conn,
                chain,
                "0000000000000000000000000000000000000000",
                "ETH",
                18,
                Some(100),
            )
            .await;
        }
        let (_, token_id) = db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000001",
            "T1",
            18,
            Some(100),
        )
        .await;
        let (_, starknet_token_id) = db_fixtures::insert_token(
            conn,
            starknet_id,
            "0000000000000000000000000000000000000001",
            "T1",
            18,
            Some(100),
        )
        .await;
        let blocks = db_fixtures::insert_blocks(conn, chain_id).await;
        let txns = db_fixtures::insert_txns(conn, &[(blocks[0], 1, TX_HASH_0)]).await;
        let sys_a = db_fixtures::insert_protocol_system(conn, "sys_a".to_string()).await;
        let sys_b = db_fixtures::insert_protocol_system(conn, "sys_b".to_string()).await;
        let type_id = db_fixtures::insert_protocol_type(conn, "pool", None, None, None).await;

        let components = [
            ("a0", sys_a, Some(5.0)),
            ("b0", sys_b, Some(1.0)),
            ("a1", sys_a, None),
            ("a2", sys_a, Some(0.0)),
            ("a3", sys_a, Some(9.0)),
            ("a4", sys_a, Some(-0.5)),
        ];
        let mut component_db_ids = Vec::new();
        for (external_id, system_id, tvl) in components {
            let db_id = db_fixtures::insert_protocol_component(
                conn,
                external_id,
                chain_id,
                system_id,
                type_id,
                txns[0],
                Some(vec![token_id]),
                None,
            )
            .await;
            if let Some(tvl) = tvl {
                set_tvl(conn, db_id, tvl).await;
            }
            component_db_ids.push(db_id);
        }
        diesel::update(schema::protocol_component::table)
            .filter(schema::protocol_component::external_id.eq("a4"))
            .set(schema::protocol_component::deleted_at.eq(db_fixtures::yesterday_midnight()))
            .execute(conn)
            .await
            .unwrap();
        let starknet_component = db_fixtures::insert_protocol_component(
            conn,
            "s0",
            starknet_id,
            sys_a,
            type_id,
            txns[0],
            Some(vec![starknet_token_id]),
            None,
        )
        .await;
        set_tvl(conn, starknet_component, 100.0).await;

        let sql_gateway = PostgresGateway::from_connection(conn).await;
        assert!(sql_gateway.component_index.is_none());
        let index = ComponentIndex::from_connection(conn, &[Chain::Ethereum])
            .await
            .unwrap();
        let mut indexed_gateway = sql_gateway.clone();
        indexed_gateway.component_index = Some(Arc::new(index));

        Fixture { system_ids: vec![sys_a, sys_b], component_db_ids, sql_gateway, indexed_gateway }
    }

    async fn set_tvl(conn: &mut AsyncPgConnection, component_db_id: i64, tvl: f64) {
        diesel::insert_into(schema::component_tvl::table)
            .values((
                schema::component_tvl::protocol_component_id.eq(component_db_id),
                schema::component_tvl::tvl.eq(tvl),
            ))
            .on_conflict(schema::component_tvl::protocol_component_id)
            .do_update()
            .set(schema::component_tvl::tvl.eq(tvl))
            .execute(conn)
            .await
            .unwrap();
    }

    /// Sets a TVL with triggers disabled, so `modified_ts` keeps the given value.
    async fn set_tvl_without_trigger(
        conn: &mut AsyncPgConnection,
        component_db_id: i64,
        tvl: f64,
        modified_ts: NaiveDateTime,
    ) {
        diesel::sql_query("SET session_replication_role = replica")
            .execute(conn)
            .await
            .unwrap();
        diesel::update(schema::component_tvl::table)
            .filter(schema::component_tvl::protocol_component_id.eq(component_db_id))
            .set((
                schema::component_tvl::tvl.eq(tvl),
                schema::component_tvl::modified_ts.eq(modified_ts),
            ))
            .execute(conn)
            .await
            .unwrap();
        diesel::sql_query("SET session_replication_role = origin")
            .execute(conn)
            .await
            .unwrap();
    }

    async fn query(
        gateway: &PostgresGateway,
        conn: &mut AsyncPgConnection,
        system: &str,
        min_tvl: Option<f64>,
        pagination: Option<&PaginationParams>,
    ) -> (Vec<ProtocolComponent>, Option<i64>) {
        let result = gateway
            .get_protocol_components(
                &Chain::Ethereum,
                Some(system.to_string()),
                None,
                min_tvl,
                pagination,
                conn,
            )
            .await
            .unwrap();
        (result.entity, result.total)
    }

    fn ids(components: &[ProtocolComponent]) -> Vec<&str> {
        components
            .iter()
            .map(|component| component.id.as_str())
            .collect()
    }

    async fn assert_equivalent(
        sql_gateway: &PostgresGateway,
        indexed_gateway: &PostgresGateway,
        conn: &mut AsyncPgConnection,
    ) {
        let thresholds =
            [None, Some(-1.0), Some(-0.5), Some(0.0), Some(1.0), Some(5.0), Some(100.0)];
        let paginations = [
            None,
            Some(PaginationParams::new(0, 2)),
            Some(PaginationParams::new(1, 2)),
            Some(PaginationParams::new(2, 2)),
            Some(PaginationParams::new(5, 2)),
            Some(PaginationParams::new(0, 500)),
        ];
        for system in ["sys_a", "sys_b"] {
            for min_tvl in thresholds {
                for pagination in &paginations {
                    let (sql_page, sql_total) =
                        query(sql_gateway, conn, system, min_tvl, pagination.as_ref()).await;
                    let (index_page, index_total) =
                        query(indexed_gateway, conn, system, min_tvl, pagination.as_ref()).await;
                    let context = format!("{system} tvl>{min_tvl:?} {pagination:?}");
                    assert_eq!(sql_total, index_total, "totals differ for {context}");
                    if pagination.is_some() {
                        assert_eq!(sql_page, index_page, "pages differ for {context}");
                    } else {
                        // Without pagination the SQL path has no ORDER BY.
                        let mut sql_ids = ids(&sql_page);
                        sql_ids.sort();
                        let mut index_ids = ids(&index_page);
                        index_ids.sort();
                        assert_eq!(sql_ids, index_ids, "rows differ for {context}");
                    }
                }
            }
        }
    }

    async fn delete_tvl(conn: &mut AsyncPgConnection, component_db_id: i64) {
        diesel::delete(schema::component_tvl::table)
            .filter(schema::component_tvl::protocol_component_id.eq(component_db_id))
            .execute(conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_serial_db_index_matches_sql() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;

            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_unknown_protocol_system_fails_like_sql() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;

            for gateway in [&fixture.sql_gateway, &fixture.indexed_gateway] {
                let result = gateway
                    .get_protocol_components(
                        &Chain::Ethereum,
                        Some("unknown".to_string()),
                        None,
                        None,
                        None,
                        &mut conn,
                    )
                    .await;
                assert!(result.is_err());
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_unchanged_counters_read_no_tvl() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            set_tvl(&mut conn, fixture.component_db_ids[0], 50.0).await;

            let outcome = fixture.refresh(&mut conn, |_| {}).await;

            assert_eq!(outcome, RefreshOutcome::Unchanged);
            let page = fixture
                .index()
                .query(&Chain::Ethereum, fixture.system_ids[0], Some(10.0), None)
                .unwrap();
            assert_eq!(page.total, 0, "a TVL write the counters did not report must not be read");
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_tvl_writes_load_the_changed_rows() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            // a1 gains a TVL row, a3 drops below the thresholds.
            set_tvl(&mut conn, fixture.component_db_ids[2], 7.0).await;
            set_tvl(&mut conn, fixture.component_db_ids[4], -3.0).await;

            let outcome = fixture
                .refresh(&mut conn, |counters| {
                    counters.tvl_inserts += 1;
                    counters.tvl_updates += 1;
                })
                .await;

            // Rows written by transactions an earlier load already read are not read again.
            assert_eq!(outcome, RefreshOutcome::TvlDelta { n_rows: 2 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_delta_reads_rows_stamped_before_the_marker_within_the_overlap() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let newest = fixture
                .index()
                .state()
                .tvl_cursor
                .newest;
            // A writer transaction that started before the last load stamps its rows with its
            // start time, older than the newest stamp read so far.
            set_tvl_without_trigger(
                &mut conn,
                fixture.component_db_ids[0],
                0.5,
                newest - chrono::Duration::minutes(2),
            )
            .await;

            let outcome = fixture
                .refresh(&mut conn, |counters| counters.tvl_updates += 1)
                .await;

            assert_eq!(outcome, RefreshOutcome::TvlDelta { n_rows: 1 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_write_without_modified_ts_reloads_all_tvl() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            // An update that bypasses the trigger keeps the stamp a load already read.
            let modified_ts = schema::component_tvl::table
                .filter(
                    schema::component_tvl::protocol_component_id.eq(fixture.component_db_ids[0]),
                )
                .select(schema::component_tvl::modified_ts)
                .first::<NaiveDateTime>(&mut conn)
                .await
                .unwrap();
            set_tvl_without_trigger(&mut conn, fixture.component_db_ids[0], 0.5, modified_ts).await;

            let outcome = fixture
                .refresh(&mut conn, |counters| counters.tvl_updates += 1)
                .await;

            assert!(matches!(outcome, RefreshOutcome::TvlReload { .. }), "{outcome:?}");
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_bulk_tvl_write_reloads_all_tvl() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            set_tvl(&mut conn, fixture.component_db_ids[0], 0.5).await;

            let outcome = fixture
                .refresh(&mut conn, |counters| counters.tvl_updates += BULK_WRITE_MIN_ROWS)
                .await;

            assert!(matches!(outcome, RefreshOutcome::TvlReload { .. }), "{outcome:?}");
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_tvl_deletes_reload_all_tvl() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            delete_tvl(&mut conn, fixture.component_db_ids[1]).await;

            let outcome = fixture
                .refresh(&mut conn, |counters| counters.tvl_deletes += 1)
                .await;

            assert!(matches!(outcome, RefreshOutcome::TvlReload { .. }), "{outcome:?}");
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_new_components_are_loaded_without_tvl_until_tvl_moves() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let (chain_id, type_id, tx_id, token_id): (i64, i64, i64, i64) =
                schema::protocol_component::table
                    .inner_join(schema::protocol_component_holds_token::table)
                    .filter(schema::protocol_component::id.eq(fixture.component_db_ids[0]))
                    .select((
                        schema::protocol_component::chain_id,
                        schema::protocol_component::protocol_type_id,
                        schema::protocol_component::creation_tx,
                        schema::protocol_component_holds_token::token_id,
                    ))
                    .first(&mut conn)
                    .await
                    .unwrap();
            let new_component = db_fixtures::insert_protocol_component(
                &mut conn,
                "b1",
                chain_id,
                fixture.system_ids[1],
                type_id,
                tx_id,
                Some(vec![token_id]),
                None,
            )
            .await;

            let outcome = fixture.refresh(&mut conn, |_| {}).await;
            assert_eq!(outcome, RefreshOutcome::Unchanged);
            fixture
                .assert_equivalent(&mut conn)
                .await;

            set_tvl(&mut conn, new_component, 3.0).await;
            fixture
                .refresh(&mut conn, |counters| counters.tvl_inserts += 1)
                .await;
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_component_deletes_reload_everything() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            // A hard delete cascades to the TVL row.
            diesel::delete(schema::protocol_component::table)
                .filter(schema::protocol_component::id.eq(fixture.component_db_ids[0]))
                .execute(&mut conn)
                .await
                .unwrap();

            let outcome = fixture
                .refresh(&mut conn, |counters| {
                    counters.component_deletes += 1;
                    counters.tvl_deletes += 1;
                })
                .await;

            assert_eq!(outcome, RefreshOutcome::FullReload { n_components: 5 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_periodic_full_reload() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;

            let outcome = fixture
                .index()
                .refresh(&mut conn, Duration::ZERO)
                .await
                .unwrap();

            assert_eq!(outcome, RefreshOutcome::FullReload { n_components: 6 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_write_counters_are_readable() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();

            let counters = WriteCounters::read(&mut conn).await;

            assert!(counters.is_ok(), "{counters:?}");
        })
        .await;
    }
}
