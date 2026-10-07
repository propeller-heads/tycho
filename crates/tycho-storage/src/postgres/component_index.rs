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
//! The database is the source of truth. Other processes write the TVL, at a cadence and in a way
//! this code does not control, so the index depends on neither. It converges through four
//! mechanisms:
//!
//! 1. **Full rebuild** — at startup, one read of `(id, protocol system, tvl)` for every component
//!    of the chain, inside a read-only repeatable-read transaction.
//! 2. **Write-through** — the write path adds the components it inserted, after its transaction
//!    commits. A rolled-back transaction therefore never leaves an id behind.
//! 3. **Change check** (see [`ComponentIndex::refresh`]) — two cheap reads:
//!    - components with an id above the highest id in the index. This catches components inserted
//!      by other processes.
//!    - the write counters of `component_tvl`, and the delete counter of `protocol_component`, from
//!      `pg_stat_user_tables`. Any change triggers a full rebuild. The counters move on every
//!      insert, update and delete, including cascades and writes that bypass triggers, and reading
//!      them scans no table. Rolled-back writes and statistics resets move them too, which only
//!      costs an extra rebuild.
//! 4. **Periodic rebuild** — a full rebuild at a fixed interval, whatever the counters say. This
//!    bounds staleness if a change goes unnoticed.
//!
//! Known limit: the new-component check assumes that ids are committed in increasing order. This
//! holds when one write executor serializes the inserts of a chain. A component that another
//! process commits out of order appears at the next periodic rebuild.
//!
//! # Concurrency
//!
//! Each chain's index sits behind one `RwLock`, never held across an `await`. A query holds the
//! read lock for one scan of one protocol system. A rebuild loads into a new structure without the
//! lock, then swaps it in. Components written through while the rebuild ran are recorded in a
//! journal and applied again on the swap, so the swap never drops them.
//!
//! # Cost
//!
//! 16 bytes per component plus a small per-system overhead: ~85 MB for 5.25M components. A rebuild
//! holds a second copy until the swap.
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use diesel::{prelude::*, sql_types::BigInt};
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncPgConnection,
    RunQueryDsl,
};
use tracing::{debug, error, info, warn};
use tycho_common::{
    models::{Chain, PaginationParams},
    storage::StorageError,
};

use crate::postgres::{schema, snapshot::snapshot_transaction, PostgresError};

/// Number of rows fetched per query when loading components.
const LOAD_BATCH_SIZE: i64 = 500_000;

/// TVL of a component without a `component_tvl` row. It never compares greater than a threshold.
const NO_TVL: f64 = f64::NEG_INFINITY;

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
    /// Components inserted while a rebuild runs, as `(protocol system id, id, tvl)`. `None` when
    /// no rebuild runs.
    journal: Option<Vec<(i64, i64, f64)>>,
}

impl ChainIndex {
    fn insert(&mut self, protocol_system_id: i64, id: i64, tvl: f64) {
        self.systems
            .entry(protocol_system_id)
            .or_default()
            .insert(id, tvl);
        self.max_id = self.max_id.max(id);
        if let Some(journal) = &mut self.journal {
            journal.push((protocol_system_id, id, tvl));
        }
    }

    fn n_components(&self) -> usize {
        self.systems
            .values()
            .map(|system| system.ids.len())
            .sum()
    }
}

/// Write counters of the tables the index derives from, as reported by `pg_stat_user_tables`.
/// The index rebuilds when they change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct WriteCounters {
    tvl_inserts: i64,
    tvl_updates: i64,
    tvl_deletes: i64,
    component_deletes: i64,
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
}

/// Bookkeeping of the refresh loop.
#[derive(Debug)]
struct RefreshState {
    /// Counters read just before the last successful rebuild.
    counters: WriteCounters,
    last_rebuild: Instant,
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
                last_rebuild: Instant::now(),
            }),
        };
        index.rebuild(conn).await?;
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
    /// ignored. A new component has no TVL until the next rebuild, like in the database.
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

    /// Brings the index up to date with the database.
    ///
    /// Rebuilds every chain when the write counters changed since the last rebuild, or when the
    /// last rebuild is older than `rebuild_interval`. Otherwise only loads components with an id
    /// above the highest indexed id. A failed rebuild leaves the previous index in place and is
    /// retried on the next call. Returns whether a rebuild ran.
    pub async fn refresh(
        &self,
        conn: &mut AsyncPgConnection,
        rebuild_interval: Duration,
    ) -> Result<bool, StorageError> {
        let counters = WriteCounters::read(conn).await?;
        let rebuild_due = {
            let state = self
                .refresh_state
                .lock()
                .expect("component index lock poisoned");
            state.counters != counters || state.last_rebuild.elapsed() >= rebuild_interval
        };
        if rebuild_due {
            self.rebuild(conn).await?;
            return Ok(true);
        }
        self.load_new_components(conn).await?;
        Ok(false)
    }

    /// Adds the components with an id above the highest indexed id of their chain.
    async fn load_new_components(&self, conn: &mut AsyncPgConnection) -> Result<(), StorageError> {
        for (chain, chain_db_id) in &self.chain_db_ids {
            let chain_lock = &self.chains[chain];
            let max_id = chain_lock
                .read()
                .expect("component index lock poisoned")
                .max_id;
            let rows = load_components(conn, *chain_db_id, max_id).await?;
            if rows.is_empty() {
                continue;
            }
            debug!(chain = %chain, n_components = rows.len(), "Component index loaded new components");
            let mut chain_index = chain_lock
                .write()
                .expect("component index lock poisoned");
            for (id, protocol_system_id, tvl) in rows {
                chain_index.insert(protocol_system_id, id, tvl);
            }
        }
        Ok(())
    }

    /// Replaces the index of every chain with a fresh read of the database. The counters are
    /// read before the components, so a write that lands during the read triggers another rebuild.
    async fn rebuild(&self, conn: &mut AsyncPgConnection) -> Result<(), StorageError> {
        let counters = WriteCounters::read(conn).await?;
        let started = Instant::now();
        for (chain, chain_db_id) in &self.chain_db_ids {
            let chain_lock = &self.chains[chain];
            chain_lock
                .write()
                .expect("component index lock poisoned")
                .journal = Some(Vec::new());

            let loaded = load_chain(conn, *chain_db_id).await;

            let mut chain_index = chain_lock
                .write()
                .expect("component index lock poisoned");
            let journal = chain_index
                .journal
                .take()
                .unwrap_or_default();
            let mut rebuilt = loaded?;
            for (protocol_system_id, id, tvl) in journal {
                rebuilt.insert(protocol_system_id, id, tvl);
            }
            info!(
                chain = %chain,
                n_components = rebuilt.n_components(),
                n_protocol_systems = rebuilt.systems.len(),
                elapsed = ?started.elapsed(),
                "Rebuilt component index"
            );
            *chain_index = rebuilt;
        }

        *self
            .refresh_state
            .lock()
            .expect("component index lock poisoned") =
            RefreshState { counters, last_rebuild: Instant::now() };
        Ok(())
    }

    /// Spawns a detached task that calls [`Self::refresh`] every `period`, rebuilding at least
    /// every `rebuild_interval`.
    pub fn spawn_refresh_task(
        self: &Arc<Self>,
        pool: Pool<AsyncPgConnection>,
        period: Duration,
        rebuild_interval: Duration,
    ) {
        let index = Arc::clone(self);
        tokio::spawn(async move {
            info!(
                period_secs = period.as_secs(),
                rebuild_interval_secs = rebuild_interval.as_secs(),
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
                        if let Err(err) = index
                            .refresh(&mut conn, rebuild_interval)
                            .await
                        {
                            error!(%err, "Component index refresh failed");
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

#[cfg(test)]
impl ComponentIndex {
    /// Forgets the counters of the last rebuild, so the next refresh rebuilds.
    fn invalidate_counters(&self) {
        self.refresh_state
            .lock()
            .expect("component index lock poisoned")
            .counters = WriteCounters { tvl_inserts: -1, ..WriteCounters::default() };
    }
}

/// Reads all components of a chain in one snapshot.
async fn load_chain(
    conn: &mut AsyncPgConnection,
    chain_db_id: i64,
) -> Result<ChainIndex, StorageError> {
    let chain_index = snapshot_transaction(conn)
        .run(|conn| {
            async move {
                let mut chain_index = ChainIndex::default();
                let mut after_id = 0;
                loop {
                    let batch = load_batch(conn, chain_db_id, after_id).await?;
                    let batch_len = batch.len() as i64;
                    for (id, protocol_system_id, tvl) in batch {
                        chain_index.insert(protocol_system_id, id, tvl);
                        after_id = id;
                    }
                    if batch_len < LOAD_BATCH_SIZE {
                        break;
                    }
                }
                Result::<_, PostgresError>::Ok(chain_index)
            }
            .scope_boxed()
        })
        .await?;
    Ok(chain_index)
}

/// Reads the components of a chain with an id above `after_id`, in ascending id order.
async fn load_components(
    conn: &mut AsyncPgConnection,
    chain_db_id: i64,
    mut after_id: i64,
) -> Result<Vec<(i64, i64, f64)>, StorageError> {
    let mut rows = Vec::new();
    loop {
        let batch = load_batch(conn, chain_db_id, after_id).await?;
        let batch_len = batch.len() as i64;
        if let Some((last_id, _, _)) = batch.last() {
            after_id = *last_id;
        }
        rows.extend(batch);
        if batch_len < LOAD_BATCH_SIZE {
            return Ok(rows);
        }
    }
}

/// One batch of `(id, protocol system id, tvl)` rows with an id above `after_id`.
async fn load_batch(
    conn: &mut AsyncPgConnection,
    chain_db_id: i64,
    after_id: i64,
) -> Result<Vec<(i64, i64, f64)>, PostgresError> {
    let rows: Vec<(i64, i64, Option<f64>)> = schema::protocol_component::table
        .left_join(schema::component_tvl::table)
        .filter(schema::protocol_component::chain_id.eq(chain_db_id))
        .filter(schema::protocol_component::id.gt(after_id))
        .order(schema::protocol_component::id.asc())
        .limit(LOAD_BATCH_SIZE)
        .select((
            schema::protocol_component::id,
            schema::protocol_component::protocol_system_id,
            schema::component_tvl::tvl.nullable(),
        ))
        .load(conn)
        .await
        .map_err(PostgresError::from)?;
    Ok(rows
        .into_iter()
        .map(|(id, protocol_system_id, tvl)| (id, protocol_system_id, index_tvl(tvl)))
        .collect())
}

/// Maps a `component_tvl.tvl` value to its index representation. Postgres orders `NaN` above every
/// other float, so `NaN > threshold` holds there; `f64::INFINITY` keeps that result for any
/// threshold a request can carry.
fn index_tvl(tvl: Option<f64>) -> f64 {
    match tvl {
        None => NO_TVL,
        Some(value) if value.is_nan() => f64::INFINITY,
        Some(value) => value,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn system(entries: &[(i64, Option<f64>)]) -> SystemIndex {
        let mut system = SystemIndex::default();
        for (id, tvl) in entries {
            system.insert(*id, index_tvl(*tvl));
        }
        system
    }

    fn page(ids: &[i64], total: i64) -> ComponentPage {
        ComponentPage { ids: ids.to_vec(), total }
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
                last_rebuild: Instant::now(),
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
    fn test_insert_during_rebuild_is_journaled() {
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
        assert_eq!(journal, vec![(7, 100, NO_TVL)]);
    }
}

/// Equivalence tests against a real database: every query must return the same rows, order and
/// total through the index as through the SQL path.
#[cfg(test)]
mod serial_db_test {
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::postgres::{db_fixtures, testing::run_against_db, PostgresGateway};

    const TX_HASH_0: &str = "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945";

    struct Fixture {
        system_ids: Vec<i64>,
        component_db_ids: Vec<i64>,
    }

    /// Inserts two protocol systems on ethereum with these components:
    /// - `sys_a`: a0 (tvl 5), a1 (no tvl row), a2 (tvl 0), a3 (tvl 9), a4 (tvl -0.5, soft-deleted)
    /// - `sys_b`: b0 (tvl 1)
    ///
    /// plus one `sys_a` component on starknet, which ethereum queries must never return.
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

        Fixture { system_ids: vec![sys_a, sys_b], component_db_ids }
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

    fn with_index(gateway: &PostgresGateway, index: ComponentIndex) -> PostgresGateway {
        let mut indexed = gateway.clone();
        indexed.component_index = Some(Arc::new(index));
        indexed
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
            setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            assert!(sql_gateway.component_index.is_none());
            let index = ComponentIndex::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();
            let indexed_gateway = with_index(&sql_gateway, index);

            assert_equivalent(&sql_gateway, &indexed_gateway, &mut conn).await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_unknown_protocol_system_fails_like_sql() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let index = ComponentIndex::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();
            let indexed_gateway = with_index(&sql_gateway, index);

            for gateway in [&sql_gateway, &indexed_gateway] {
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
    async fn test_serial_db_rebuild_picks_up_tvl_changes() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let index = ComponentIndex::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();
            let indexed_gateway = with_index(&sql_gateway, index);

            // a1 gains a TVL row, a3 drops below the thresholds, b0's row is deleted.
            set_tvl(&mut conn, fixture.component_db_ids[2], 7.0).await;
            set_tvl(&mut conn, fixture.component_db_ids[4], -3.0).await;
            diesel::delete(schema::component_tvl::table)
                .filter(
                    schema::component_tvl::protocol_component_id.eq(fixture.component_db_ids[1]),
                )
                .execute(&mut conn)
                .await
                .unwrap();

            let index = indexed_gateway
                .component_index
                .as_ref()
                .unwrap();
            index.invalidate_counters();
            let rebuilt = index
                .refresh(&mut conn, Duration::from_secs(3600))
                .await
                .unwrap();

            assert!(rebuilt);
            assert_equivalent(&sql_gateway, &indexed_gateway, &mut conn).await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_refresh_loads_new_components_and_drops_hard_deleted() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let fixture = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let index = ComponentIndex::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();
            let indexed_gateway = with_index(&sql_gateway, index);
            let index = indexed_gateway
                .component_index
                .as_ref()
                .unwrap();

            let chain_id = schema::chain::table
                .filter(schema::chain::name.eq("ethereum"))
                .select(schema::chain::id)
                .first::<i64>(&mut conn)
                .await
                .unwrap();
            let (type_id, tx_id, token_id): (i64, i64, i64) = schema::protocol_component::table
                .inner_join(schema::protocol_component_holds_token::table)
                .filter(schema::protocol_component::id.eq(fixture.component_db_ids[0]))
                .select((
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
            set_tvl(&mut conn, new_component, 3.0).await;

            index
                .load_new_components(&mut conn)
                .await
                .unwrap();
            assert_equivalent(&sql_gateway, &indexed_gateway, &mut conn).await;

            // A hard delete cascades to the TVL row; the next rebuild drops the component.
            diesel::delete(schema::protocol_component::table)
                .filter(schema::protocol_component::id.eq(fixture.component_db_ids[0]))
                .execute(&mut conn)
                .await
                .unwrap();
            index.invalidate_counters();
            index
                .refresh(&mut conn, Duration::from_secs(3600))
                .await
                .unwrap();
            assert_equivalent(&sql_gateway, &indexed_gateway, &mut conn).await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_periodic_rebuild() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            setup(&mut conn).await;
            let index = ComponentIndex::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            let rebuilt = index
                .refresh(&mut conn, Duration::ZERO)
                .await
                .unwrap();

            assert!(rebuilt);
        })
        .await;
    }
}
