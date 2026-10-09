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
//! # How the index stays fresh
//!
//! The database is the source of truth. The index converges to it through three mechanisms:
//!
//! 1. **New-component poll** — every refresh reads the components with an id above the highest id
//!    the index has seen. The index sees a new component only once its transaction commits. Until
//!    the next poll, the RPC still serves it from the pending-deltas buffer, which keeps a block
//!    until the extractor reports its commit.
//! 2. **TVL poll** — every refresh reads the `component_tvl` rows whose `modified_ts` is newer than
//!    the newest one read so far. The update trigger and the column default set `modified_ts` on
//!    every write. When more than [`TVL_POLL_MAX_ROWS`] rows changed since the last poll, such as
//!    after a cron rewrote the whole table, the poll stops and all TVL is read in one sequential
//!    scan instead.
//! 3. **Full reload** — at startup and at a fixed interval, components and TVL are read again and
//!    swapped in.
//!
//! Full reads of components or TVL are sequential scans of one table without `ORDER BY`; the polls
//! use the primary key and the `modified_ts` index. Rows are sorted here, and TVL rows are matched
//! to components by id, so no read joins the two tables.
//!
//! # Unexpected cases
//!
//! The polls cannot see the cases below. None happens in normal operation; the next full reload
//! corrects each of them.
//!
//! - **Deleted rows.** A deleted component stays in the index with its last TVL: every request that
//!   matches it counts it in `total` and returns its page one row short. A deleted TVL row stays in
//!   the index with its last value.
//! - **Writes that do not move `modified_ts`.** An insert that sets `modified_ts` explicitly, or a
//!   write with triggers disabled, as restore tooling does.
//! - **Concurrent TVL writers.** `modified_ts` is the start time of the writing transaction, not
//!   its commit time. With two TVL writers, one can commit rows stamped before rows of the other
//!   that a poll already read; the poll then never reads them.
//! - **Component ids committed out of order** by another process: the new-component poll only reads
//!   ids above the highest one it has seen.
//!
//! # Concurrency
//!
//! The index sits behind one `RwLock`, never held across an `await`. A query holds the read lock
//! for one scan of one protocol system. TVL reads take the write lock only to write the fetched
//! values. A full reload builds a new index without the lock, then swaps it in; components
//! committed after its snapshot arrive with the next poll.
//!
//! # Metrics
//!
//! `component_index_last_refresh_timestamp_seconds` and
//! `component_index_last_full_reload_timestamp_seconds` hold the time of the last successful
//! refresh and full reload, so a stopped refresh task shows as a timestamp that stops moving.
//! `component_index_refreshes_total{outcome}` counts refreshes by outcome, failures included, and
//! `component_index_components` holds the number of indexed components. Every failed refresh logs
//! an error, and the task backs off linearly, up to 5 minutes, until a refresh succeeds again.
//!
//! # Cost
//!
//! 16 bytes per component plus a small per-system overhead: ~85 MB for 5.25M components. A full TVL
//! read holds the fetched rows, 32 bytes each, until they are applied; a full reload also holds a
//! second index until the swap.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{
    pooled_connection::deadpool::{Object, Pool},
    scoped_futures::ScopedFutureExt,
    AsyncPgConnection, RunQueryDsl,
};
use metrics::{counter, gauge};
use tracing::{debug, error, info};
use tycho_common::{
    models::{Chain, PaginationParams},
    storage::StorageError,
};

use crate::postgres::{
    schema,
    snapshot::{bound_snapshot_reads, snapshot_transaction},
    PostgresError,
};

/// TVL of a component without a `component_tvl` row. It never compares greater than a threshold.
const NO_TVL: f64 = f64::NEG_INFINITY;

/// Most rows a TVL poll applies. A poll that finds more reads all TVL in one sequential scan
/// instead: right after a large write, the planner's statistics do not show it yet, and a read
/// through the `modified_ts` index would visit most of the table in random order.
const TVL_POLL_MAX_ROWS: usize = 10_000;

/// Longest a refresh may run before the task abandons it and its connection. Reads already stop
/// after the snapshot statement timeout; this also covers a connection that stopped answering.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// One page of a query: the database ids on the page, in ascending order, and the number of
/// components matching the filters on all pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComponentPage {
    pub(crate) ids: Vec<i64>,
    pub(crate) total: i64,
}

/// What a [`ComponentIndex::refresh`] did with TVL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    /// No TVL row changed.
    TvlUnchanged,
    /// Applied the TVL rows that changed.
    TvlDelta { n_rows: usize },
    /// Too many TVL rows changed; read all TVL.
    TvlReload { n_rows: usize },
    /// Read components and TVL again.
    FullReload { n_components: usize },
}

impl RefreshOutcome {
    /// The `outcome` label of `component_index_refreshes_total`.
    fn label(&self) -> &'static str {
        match self {
            RefreshOutcome::TvlUnchanged => "tvl_unchanged",
            RefreshOutcome::TvlDelta { .. } => "tvl_delta",
            RefreshOutcome::TvlReload { .. } => "tvl_reload",
            RefreshOutcome::FullReload { .. } => "full_reload",
        }
    }
}

/// Seconds since the Unix epoch, for the timestamp gauges.
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
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
}

impl ChainIndex {
    /// Builds an index without TVL from `(id, protocol system id)` rows sorted by id.
    fn from_sorted_components(rows: &[(i64, i64)]) -> Self {
        let mut chain_index = ChainIndex::default();
        for (id, protocol_system_id) in rows {
            chain_index.insert(*protocol_system_id, *id, NO_TVL);
        }
        chain_index.max_id = rows.last().map_or(0, |(id, _)| *id);
        chain_index
    }

    fn insert(&mut self, protocol_system_id: i64, id: i64, tvl: f64) {
        self.systems
            .entry(protocol_system_id)
            .or_default()
            .insert(id, tvl);
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

/// Bookkeeping of the refresh loop.
#[derive(Debug)]
struct RefreshState {
    /// Newest `modified_ts` among the TVL rows read so far.
    newest_tvl_ts: NaiveDateTime,
    last_full_reload: Instant,
}

/// In-memory filter and paging index over `protocol_component`.
///
/// Holds only the database id and the TVL of each component of one chain, grouped by protocol
/// system in ascending id order. Answers which components match a request and how many there are;
/// the component contents stay in Postgres. See the module docs for the design.
pub(crate) struct ComponentIndex {
    chain: Chain,
    chain_db_id: i64,
    index: RwLock<ChainIndex>,
    refresh_state: Mutex<RefreshState>,
    /// [`TVL_POLL_MAX_ROWS`], lowered by tests.
    tvl_poll_max_rows: usize,
}

impl ComponentIndex {
    /// Like [`Self::from_connection`], with a connection from `pool`.
    pub(crate) async fn from_pool(
        pool: Pool<AsyncPgConnection>,
        chain: Chain,
        chain_db_id: i64,
    ) -> Result<Self, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;
        Self::from_connection(&mut conn, chain, chain_db_id).await
    }

    /// Loads the components of `chain`, whose id in the `chain` table is `chain_db_id`.
    pub(crate) async fn from_connection(
        conn: &mut AsyncPgConnection,
        chain: Chain,
        chain_db_id: i64,
    ) -> Result<Self, StorageError> {
        let index = Self {
            chain,
            chain_db_id,
            index: RwLock::new(ChainIndex::default()),
            refresh_state: Mutex::new(RefreshState {
                newest_tvl_ts: NaiveDateTime::default(),
                last_full_reload: Instant::now(),
            }),
            tvl_poll_max_rows: TVL_POLL_MAX_ROWS,
        };
        index.full_reload(conn).await?;
        Ok(index)
    }

    /// The page of components of `protocol_system_id` on `chain` with a TVL above `min_tvl`, in
    /// ascending id order, with the same rows and total as the SQL path. Without pagination, all
    /// matching components form the page. Returns `None` for a chain this index does not hold.
    pub(crate) fn query(
        &self,
        chain: &Chain,
        protocol_system_id: i64,
        min_tvl: Option<f64>,
        pagination: Option<&PaginationParams>,
    ) -> Option<ComponentPage> {
        if *chain != self.chain {
            return None;
        }
        let (offset, limit) = pagination
            .map(|params| (params.offset().max(0) as usize, params.page_size.max(0) as usize))
            .unwrap_or((0, usize::MAX));
        let index = self.read_index();
        let page = match index.systems.get(&protocol_system_id) {
            Some(system) => system.query(min_tvl, offset, limit),
            None => ComponentPage { ids: Vec::new(), total: 0 },
        };
        Some(page)
    }

    /// Brings the index up to date with the database: a full reload when the last one is older
    /// than `full_reload_interval`, otherwise the new-component and TVL polls. Each refresh reads
    /// in one snapshot, so every TVL row it reads belongs to a component it can see.
    pub(crate) async fn refresh(
        &self,
        conn: &mut AsyncPgConnection,
        full_reload_interval: Duration,
    ) -> Result<RefreshOutcome, StorageError> {
        if self
            .refresh_state()
            .last_full_reload
            .elapsed() >=
            full_reload_interval
        {
            let n_components = self.full_reload(conn).await?;
            return Ok(RefreshOutcome::FullReload { n_components });
        }

        let chain_db_id = self.chain_db_id;
        let max_id = self.read_index().max_id;
        let newest = self.refresh_state().newest_tvl_ts;
        let poll_max_rows = self.tvl_poll_max_rows;
        let (new_components, changed_tvl, all_tvl) = snapshot_transaction(conn)
            .run(|conn| {
                async move {
                    bound_snapshot_reads(conn).await?;
                    let new_components = load_components(conn, chain_db_id, max_id).await?;
                    let changed_tvl =
                        load_tvl(conn, Some((newest, poll_max_rows as i64 + 1))).await?;
                    let all_tvl = if changed_tvl.len() > poll_max_rows {
                        Some(load_tvl(conn, None).await?)
                    } else {
                        None
                    };
                    Result::<_, PostgresError>::Ok((new_components, changed_tvl, all_tvl))
                }
                .scope_boxed()
            })
            .await?;

        self.add_components(&new_components);
        let outcome = if let Some(rows) = all_tvl {
            self.write_tvl(&rows, true);
            RefreshOutcome::TvlReload { n_rows: rows.len() }
        } else if changed_tvl.is_empty() {
            RefreshOutcome::TvlUnchanged
        } else {
            self.write_tvl(&changed_tvl, false);
            RefreshOutcome::TvlDelta { n_rows: changed_tvl.len() }
        };
        gauge!("component_index_last_refresh_timestamp_seconds").set(unix_now());
        Ok(outcome)
    }

    /// Adds `(id, protocol system id)` rows of components the index does not hold yet.
    fn add_components(&self, rows: &[(i64, i64)]) {
        let Some(max_id) = rows.iter().map(|(id, _)| *id).max() else {
            return;
        };
        debug!(n_components = rows.len(), "Component index polled new components");
        let mut index = self.write_index();
        for (id, protocol_system_id) in rows {
            index.insert(*protocol_system_id, *id, NO_TVL);
        }
        index.max_id = index.max_id.max(max_id);
        gauge!("component_index_components").set(index.n_components() as f64);
    }

    /// Writes `rows`, sorted by component id, into the index. With `replace_all`, all other
    /// components lose their TVL.
    fn write_tvl(&self, rows: &[TvlRow], replace_all: bool) {
        {
            let mut index = self.write_index();
            if replace_all {
                index.clear_tvl();
            }
            index.apply_tvl(rows);
        }
        self.advance_newest_tvl_ts(rows);
    }

    fn advance_newest_tvl_ts(&self, rows: &[TvlRow]) {
        if let Some(newest) = rows
            .iter()
            .map(|row| row.modified_ts)
            .max()
        {
            let mut state = self.refresh_state();
            state.newest_tvl_ts = state.newest_tvl_ts.max(newest);
        }
    }

    /// Reads components and TVL again, in one snapshot, and swaps the result in. Returns the number
    /// of components.
    async fn full_reload(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        let started = Instant::now();
        let chain_db_id = self.chain_db_id;
        let (mut rows, tvl_rows) = snapshot_transaction(conn)
            .run(|conn| {
                async move {
                    bound_snapshot_reads(conn).await?;
                    let rows = load_components(conn, chain_db_id, 0).await?;
                    let tvl_rows = load_tvl(conn, None).await?;
                    Result::<_, PostgresError>::Ok((rows, tvl_rows))
                }
                .scope_boxed()
            })
            .await?;
        rows.sort_unstable();

        let mut rebuilt = ChainIndex::from_sorted_components(&rows);
        rebuilt.apply_tvl(&tvl_rows);
        let n_components = rebuilt.n_components();
        info!(
            chain = %self.chain,
            n_components,
            n_protocol_systems = rebuilt.systems.len(),
            n_tvl_rows = tvl_rows.len(),
            elapsed = ?started.elapsed(),
            "Component index reloaded"
        );
        *self.write_index() = rebuilt;
        self.advance_newest_tvl_ts(&tvl_rows);
        self.refresh_state().last_full_reload = Instant::now();
        let now = unix_now();
        gauge!("component_index_last_full_reload_timestamp_seconds").set(now);
        gauge!("component_index_last_refresh_timestamp_seconds").set(now);
        gauge!("component_index_components").set(n_components as f64);
        Ok(n_components)
    }

    fn read_index(&self) -> std::sync::RwLockReadGuard<'_, ChainIndex> {
        self.index
            .read()
            .expect("component index lock poisoned")
    }

    fn write_index(&self) -> std::sync::RwLockWriteGuard<'_, ChainIndex> {
        self.index
            .write()
            .expect("component index lock poisoned")
    }

    fn refresh_state(&self) -> std::sync::MutexGuard<'_, RefreshState> {
        self.refresh_state
            .lock()
            .expect("component index lock poisoned")
    }

    /// Spawns a detached task that calls [`Self::refresh`] every `period`, with a full reload at
    /// least every `full_reload_interval`.
    pub(crate) fn spawn_refresh_task(
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
            let mut failures: u32 = 0;
            loop {
                interval.tick().await;
                let started = Instant::now();
                // A bounded wait so pool starvation is visible instead of a silently stalled task.
                let result = match tokio::time::timeout(Duration::from_secs(30), pool.get()).await {
                    Ok(Ok(mut conn)) => {
                        let refresh = index.refresh(&mut conn, full_reload_interval);
                        match tokio::time::timeout(REFRESH_TIMEOUT, refresh).await {
                            Ok(result) => result.map_err(|err| err.to_string()),
                            Err(_) => {
                                drop(Object::take(conn));
                                Err(format!("timed out after {REFRESH_TIMEOUT:?}"))
                            }
                        }
                    }
                    Ok(Err(err)) => Err(format!("no DB connection: {err}")),
                    Err(_) => Err("timed out waiting for a DB connection".to_string()),
                };
                match result {
                    Ok(outcome) => {
                        if failures > 0 {
                            info!(failures, "Component index refresh recovered");
                        }
                        failures = 0;
                        counter!("component_index_refreshes_total", "outcome" => outcome.label())
                            .increment(1);
                        if outcome != RefreshOutcome::TvlUnchanged {
                            info!(?outcome, elapsed = ?started.elapsed(), "Component index refreshed");
                        }
                    }
                    Err(err) => {
                        failures += 1;
                        counter!("component_index_refreshes_total", "outcome" => "error")
                            .increment(1);
                        error!(%err, failures, "Component index refresh failed; the index is getting stale");
                        // Back off linearly, up to 5 minutes, so a failing full reload does not
                        // rerun every tick.
                        tokio::time::sleep(period * failures.min(60)).await;
                    }
                }
            }
        });
    }
}

/// Reads the `(id, protocol system id)` rows of the chain's components with an id above `after_id`,
/// in no particular order.
async fn load_components(
    conn: &mut AsyncPgConnection,
    chain_db_id: i64,
    after_id: i64,
) -> Result<Vec<(i64, i64)>, PostgresError> {
    schema::protocol_component::table
        .filter(schema::protocol_component::chain_id.eq(chain_db_id))
        .filter(schema::protocol_component::id.gt(after_id))
        .select((schema::protocol_component::id, schema::protocol_component::protocol_system_id))
        .load(conn)
        .await
        .map_err(PostgresError::from)
}

/// Reads the `component_tvl` rows, sorted by component id: with `newer_than = (ts, limit)` at most
/// `limit` rows with a `modified_ts` after `ts`, otherwise all rows. Rows of every chain are
/// returned; `component_tvl` has no chain column, and matching by id discards the others.
async fn load_tvl(
    conn: &mut AsyncPgConnection,
    newer_than: Option<(NaiveDateTime, i64)>,
) -> Result<Vec<TvlRow>, PostgresError> {
    let mut query = schema::component_tvl::table
        .select((
            schema::component_tvl::protocol_component_id,
            schema::component_tvl::tvl,
            schema::component_tvl::modified_ts,
        ))
        .into_boxed();
    if let Some((since, limit)) = newer_than {
        query = query
            .filter(schema::component_tvl::modified_ts.gt(since))
            .limit(limit);
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

    /// Ids of the components with a TVL above `min_tvl`.
    fn above(system: &SystemIndex, min_tvl: f64) -> Vec<i64> {
        system
            .query(Some(min_tvl), 0, usize::MAX)
            .ids
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

        assert_eq!(system.query(None, 0, usize::MAX).ids, vec![5, 10, 20, 30]);
        // The duplicate insert of 30 kept its TVL of 3.
        assert_eq!(above(&system, 2.5), vec![30]);
        assert_eq!(above(&system, 1.5), vec![20, 30]);
        assert_eq!(above(&system, 0.75), vec![10, 20, 30]);
        assert_eq!(above(&system, 0.25), vec![5, 10, 20, 30]);
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

        let (system_7, system_8) = (&chain_index.systems[&7], &chain_index.systems[&8]);
        assert_eq!(above(system_7, f64::MIN), vec![4, 9]);
        assert_eq!(above(system_7, 5.0), vec![9]);
        assert_eq!(above(system_8, f64::MIN), vec![2]);
        // No row of an unknown id landed on another component.
        assert!(above(system_7, 50.0).is_empty());
        assert!(above(system_8, 50.0).is_empty());
    }

    #[test]
    fn test_clear_tvl_then_apply_replaces_all_values() {
        let mut chain_index = ChainIndex::from_sorted_components(&[(1, 7), (2, 7)]);
        chain_index.apply_tvl(&[tvl_row(1, 1.0), tvl_row(2, 2.0)]);

        chain_index.clear_tvl();
        chain_index.apply_tvl(&[tvl_row(2, 3.0)]);

        let system = &chain_index.systems[&7];
        assert_eq!(above(system, f64::MIN), vec![2]);
        assert_eq!(above(system, 2.5), vec![2]);
    }

    fn index_with(rows: &[(i64, i64)]) -> ComponentIndex {
        let mut sorted: Vec<(i64, i64)> = rows
            .iter()
            .map(|(protocol_system_id, id)| (*id, *protocol_system_id))
            .collect();
        sorted.sort_unstable();
        ComponentIndex {
            chain: Chain::Ethereum,
            chain_db_id: 1,
            index: RwLock::new(ChainIndex::from_sorted_components(&sorted)),
            refresh_state: Mutex::new(RefreshState {
                newest_tvl_ts: NaiveDateTime::default(),
                last_full_reload: Instant::now(),
            }),
            tvl_poll_max_rows: TVL_POLL_MAX_ROWS,
        }
    }

    #[test]
    fn test_query_unknown_system_is_empty_and_unknown_chain_is_none() {
        let index = index_with(&[(7, 100)]);

        assert_eq!(index.query(&Chain::Ethereum, 8, None, None), Some(page(&[], 0)));
        assert_eq!(index.query(&Chain::Base, 7, None, None), None);
    }
}

/// Benchmark of the load, refresh and query paths against a real database.
///
/// Read-only unless `BENCH_WRITES=1`, which also updates `component_tvl` to time the TVL polls;
/// only set it against a disposable database. Run with:
///   DATABASE_URL=... BENCH_CHAIN=bsc cargo test -p tycho-storage --release --lib \
///     component_index_benchmark -- --ignored --nocapture
#[cfg(test)]
mod benchmark {
    use std::str::FromStr;

    use diesel_async::AsyncConnection;

    use super::*;

    /// Long enough that the benchmark never triggers the periodic full reload.
    const NO_PERIODIC_RELOAD: Duration = Duration::from_secs(24 * 3600);

    /// Resident memory of this process, from `/proc` (Linux only).
    fn rss() -> String {
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
            .map_or("n/a".to_string(), |kib| format!("{:.0} MiB", kib / 1024.0))
    }

    fn time_queries(index: &ComponentIndex, chain: Chain, system_id: i64, min_tvl: Option<f64>) {
        let total = index
            .query(&chain, system_id, min_tvl, Some(&PaginationParams::new(0, 1)))
            .unwrap()
            .total;
        let page_size = 2550;
        let deep_page = total / page_size / 2;
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
    #[ignore = "benchmark; needs DATABASE_URL"]
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

        let rss_before = rss();
        let started = Instant::now();
        let chain_db_id: i64 = schema::chain::table
            .filter(schema::chain::name.eq(chain.to_string()))
            .select(schema::chain::id)
            .first(&mut conn)
            .await
            .expect("BENCH_CHAIN missing from the chain table");
        let index = ComponentIndex::from_connection(&mut conn, chain, chain_db_id)
            .await
            .unwrap();
        let load_elapsed = started.elapsed();
        let (n_components, systems) = {
            let chain_index = index.read_index();
            let mut systems: Vec<(i64, usize)> = chain_index
                .systems
                .iter()
                .map(|(id, system)| (*id, system.ids.len()))
                .collect();
            systems.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
            (chain_index.n_components(), systems)
        };
        println!(
            "startup load: {load_elapsed:?}, {n_components} components, RSS {rss_before} -> {}",
            rss()
        );
        println!("systems (id, components): {systems:?}");

        let largest = systems[0].0;
        for min_tvl in [None, Some(-1.0), Some(0.1)] {
            time_queries(&index, chain, largest, min_tvl);
        }

        let started = Instant::now();
        let outcome = index
            .refresh(&mut conn, NO_PERIODIC_RELOAD)
            .await
            .unwrap();
        println!("refresh, nothing written: {:?} -> {outcome:?}", started.elapsed());

        let started = Instant::now();
        let rows = load_tvl(&mut conn, None).await.unwrap();
        let fetch_elapsed = started.elapsed();
        let started = Instant::now();
        index.write_tvl(&rows, true);
        println!(
            "full TVL read: fetch+sort {fetch_elapsed:?} ({} rows), write under lock {:?}, RSS {}",
            rows.len(),
            started.elapsed(),
            rss()
        );
        drop(rows);

        let started = Instant::now();
        let outcome = index
            .refresh(&mut conn, Duration::ZERO)
            .await
            .unwrap();
        println!("refresh, full reload: {:?} -> {outcome:?}", started.elapsed());

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
        ] {
            diesel::sql_query(sql)
                .execute(&mut conn)
                .await
                .unwrap();
            if label == "autoanalyze" {
                continue;
            }
            let started = Instant::now();
            let outcome = index
                .refresh(&mut conn, NO_PERIODIC_RELOAD)
                .await
                .unwrap();
            println!("refresh, {label}: {:?} -> {outcome:?}", started.elapsed());
        }
    }
}

/// Tests against a real database: every query must return the same rows, order and total
/// through the index as through the SQL path, after each kind of refresh.
#[cfg(test)]
mod serial_db_test {
    use std::str::FromStr;

    use tycho_common::{
        models::{protocol::ProtocolComponent, ChangeType},
        Bytes,
    };

    use super::*;
    use crate::postgres::{db_fixtures, testing::run_against_db, PostgresGateway};

    const TX_HASH_0: &str = "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945";
    const TOKEN: &str = "0000000000000000000000000000000000000001";

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

        async fn refresh(&self, conn: &mut AsyncPgConnection) -> RefreshOutcome {
            self.index()
                .refresh(conn, NO_PERIODIC_RELOAD)
                .await
                .unwrap()
        }

        /// Inserts a `sys_b` component the way another process would: straight into the table.
        async fn insert_component_elsewhere(
            &self,
            conn: &mut AsyncPgConnection,
            external_id: &str,
        ) -> i64 {
            let (chain_id, type_id, tx_id, token_id): (i64, i64, i64, i64) =
                schema::protocol_component::table
                    .inner_join(schema::protocol_component_holds_token::table)
                    .filter(schema::protocol_component::id.eq(self.component_db_ids[0]))
                    .select((
                        schema::protocol_component::chain_id,
                        schema::protocol_component::protocol_type_id,
                        schema::protocol_component::creation_tx,
                        schema::protocol_component_holds_token::token_id,
                    ))
                    .first(conn)
                    .await
                    .unwrap();
            db_fixtures::insert_protocol_component(
                conn,
                external_id,
                chain_id,
                self.system_ids[1],
                type_id,
                tx_id,
                Some(vec![token_id]),
                None,
            )
            .await
        }
    }

    /// Inserts two protocol systems on ethereum with these components:
    /// - `sys_a`: a0 (tvl 5), a1 (no tvl row), a2 (tvl 0), a3 (tvl 9), a4 (tvl -0.5, soft-deleted),
    ///   a5 (tvl NaN, which Postgres orders above every number)
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
        let (_, token_id) =
            db_fixtures::insert_token(conn, chain_id, TOKEN, "T1", 18, Some(100)).await;
        let (_, starknet_token_id) =
            db_fixtures::insert_token(conn, starknet_id, TOKEN, "T1", 18, Some(100)).await;
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
            ("a5", sys_a, Some(f64::NAN)),
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
        let index = ComponentIndex::from_connection(conn, Chain::Ethereum, chain_id)
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
    async fn test_serial_db_refresh_without_writes_is_unchanged() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;

            assert_eq!(fixture.refresh(&mut conn).await, RefreshOutcome::TvlUnchanged);
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_tvl_poll_applies_only_the_changed_rows() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            // a1 gains a TVL row, a3 drops below the thresholds.
            set_tvl(&mut conn, fixture.component_db_ids[2], 7.0).await;
            set_tvl(&mut conn, fixture.component_db_ids[4], -3.0).await;

            let outcome = fixture.refresh(&mut conn).await;

            assert_eq!(outcome, RefreshOutcome::TvlDelta { n_rows: 2 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_bulk_tvl_write_reads_all_tvl() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let mut fixture = setup(&mut conn).await;
            let index = Arc::get_mut(
                fixture
                    .indexed_gateway
                    .component_index
                    .as_mut()
                    .unwrap(),
            )
            .unwrap();
            index.tvl_poll_max_rows = 1;
            // Three changed rows: more than the poll fetches (its row limit + 1), so only the full
            // read sees all of them.
            set_tvl(&mut conn, fixture.component_db_ids[0], 0.5).await;
            set_tvl(&mut conn, fixture.component_db_ids[1], 50.0).await;
            set_tvl(&mut conn, fixture.component_db_ids[4], 0.2).await;

            let outcome = fixture.refresh(&mut conn).await;

            assert_eq!(outcome, RefreshOutcome::TvlReload { n_rows: 7 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_poll_reads_components_added_through_the_gateway() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let component = ProtocolComponent {
                id: "b1".to_string(),
                protocol_system: "sys_b".to_string(),
                protocol_type_name: "pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![Bytes::from_str(TOKEN).unwrap()],
                contract_addresses: vec![],
                static_attributes: Default::default(),
                change: ChangeType::Creation,
                creation_tx: Bytes::from_str(TX_HASH_0).unwrap(),
                created_at: Default::default(),
            };

            fixture
                .indexed_gateway
                .add_protocol_components(&[component], &mut conn)
                .await
                .unwrap();

            assert_eq!(fixture.refresh(&mut conn).await, RefreshOutcome::TvlUnchanged);
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_poll_reads_components_and_tvl_of_other_writers() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let new_component = fixture
                .insert_component_elsewhere(&mut conn, "b1")
                .await;

            assert_eq!(fixture.refresh(&mut conn).await, RefreshOutcome::TvlUnchanged);
            fixture
                .assert_equivalent(&mut conn)
                .await;

            set_tvl(&mut conn, new_component, 3.0).await;
            assert_eq!(fixture.refresh(&mut conn).await, RefreshOutcome::TvlDelta { n_rows: 1 });
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    /// A component and its TVL row commit while a full reload is between its component read and
    /// its TVL read. Reading both in one snapshot hides both, so the next poll reads both; reading
    /// them separately would see the TVL row without its component, drop it, and move the TVL
    /// watermark past it.
    #[tokio::test]
    async fn test_serial_db_full_reload_reads_components_and_tvl_in_one_snapshot() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let mut writer = pool.get().await.unwrap();
            let mut observer = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let reload_pid: i32 =
                diesel::select(diesel::dsl::sql::<diesel::sql_types::Integer>("pg_backend_pid()"))
                    .get_result(&mut conn)
                    .await
                    .unwrap();

            // The writer holds the TVL table until the reload waits on it, then commits a component
            // and its TVL row.
            diesel::sql_query("BEGIN")
                .execute(&mut writer)
                .await
                .unwrap();
            diesel::sql_query("LOCK TABLE component_tvl IN ACCESS EXCLUSIVE MODE")
                .execute(&mut writer)
                .await
                .unwrap();
            let new_component = fixture
                .insert_component_elsewhere(&mut writer, "b9")
                .await;
            set_tvl(&mut writer, new_component, 4.0).await;

            let reload = fixture
                .index()
                .refresh(&mut conn, Duration::ZERO);
            let commit_when_reload_waits = async {
                let wait_event = format!(
                    "(SELECT wait_event_type FROM pg_stat_activity WHERE pid = {reload_pid})"
                );
                loop {
                    let waiting: Option<String> = diesel::select(diesel::dsl::sql::<
                        diesel::sql_types::Nullable<diesel::sql_types::Text>,
                    >(&wait_event))
                    .get_result(&mut observer)
                    .await
                    .unwrap();
                    if waiting.as_deref() == Some("Lock") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                diesel::sql_query("COMMIT")
                    .execute(&mut writer)
                    .await
                    .unwrap();
            };
            let (outcome, ()) = tokio::join!(reload, commit_when_reload_waits);
            assert!(matches!(outcome.unwrap(), RefreshOutcome::FullReload { .. }));

            fixture.refresh(&mut conn).await;
            fixture
                .assert_equivalent(&mut conn)
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_full_reload_corrects_what_the_polls_cannot_see() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            // A deleted TVL row, and a hard-deleted component (its TVL row goes with it).
            diesel::delete(schema::component_tvl::table)
                .filter(
                    schema::component_tvl::protocol_component_id.eq(fixture.component_db_ids[1]),
                )
                .execute(&mut conn)
                .await
                .unwrap();
            diesel::delete(schema::protocol_component::table)
                .filter(schema::protocol_component::id.eq(fixture.component_db_ids[0]))
                .execute(&mut conn)
                .await
                .unwrap();

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
}
