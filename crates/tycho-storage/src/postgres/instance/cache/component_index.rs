//! The id and TVL of the components of each active instance in memory, to count and page
//! `get_protocol_components` without SQL.
//!
//! [`SystemIndex`] describes what one id list holds and how it filters and pages.
//!
//! # Units
//!
//! A unit holds the components of one active instance: their database ids in ascending order, the
//! TVL of each, and the highest id. Component ids repeat across instances, so each instance has
//! its own unit, and a query names the instance.
//!
//! Only the instances that are active at load time get a unit, and each of them gets one, also
//! when it has no components. When a pointer moves later, the index keeps the unit of the old
//! instance until the next full reload, and the new instance has no unit until then.
//!
//! # Unexpected cases
//!
//! The polls cannot see the cases below, in any unit. None happens in normal operation; the next
//! full reload corrects each of them.
//!
//! - **Deleted rows.** A deleted component stays in its unit with its last TVL: every request that
//!   matches it counts it in `total` and returns its page one row short. A deleted TVL row stays in
//!   the unit with its last value.
//! - **Writes that do not move `modified_ts`.** An insert that sets `modified_ts` explicitly, or a
//!   write with triggers disabled, as restore tooling does.
//! - **Concurrent TVL writers.** `modified_ts` is the start time of the writing transaction, not
//!   its commit time. With two TVL writers, one can commit rows stamped before rows of the other
//!   that a poll already read; the poll then never reads them.
//! - **Component ids committed out of order** by another process: the new-component poll only reads
//!   ids above the highest id of the unit.
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
use tycho_common::{models::PaginationParams, storage::StorageError};

use crate::postgres::{
    instance::{
        active::active_protocols,
        cache::component_page::{
            index_tvl, unix_now, ComponentPage, RefreshOutcome, SystemIndex, TvlRow,
            MAX_REFRESH_BACKOFF, NO_TVL, REFRESH_TIMEOUT,
        },
        schema::tycho::{component_tvl, protocol_component},
        InstanceId,
    },
    page_window,
    snapshot::{bound_snapshot_reads, force_custom_plans, snapshot_transaction},
    PostgresError,
};

/// The components of each active instance (see the module docs).
pub struct ComponentIndex {
    units: RwLock<HashMap<InstanceId, InstanceIndex>>,
    refresh_state: Mutex<RefreshState>,
}

/// Bookkeeping of the refresh loop.
#[derive(Debug)]
struct RefreshState {
    /// Newest `modified_ts` among the TVL rows read so far.
    newest_tvl_ts: NaiveDateTime,
    last_full_reload: Instant,
}

/// The components of one active instance.
struct InstanceIndex {
    protocol: String,
    components: SystemIndex,
    /// Highest component id of the instance, 0 when empty.
    max_id: i64,
}

impl ComponentIndex {
    /// Loads the index in one snapshot of the database. See [`Self::load`].
    #[cfg_attr(
        test,
        expect(dead_code, reason = "a test transaction cannot start the snapshot transaction")
    )]
    pub(crate) async fn from_pool(pool: Pool<AsyncPgConnection>) -> Result<Self, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;
        snapshot_transaction(&mut conn)
            .run(|conn| {
                async move {
                    bound_snapshot_reads(conn).await?;
                    force_custom_plans(conn).await?;
                    Result::<_, PostgresError>::Ok(Self::load(conn).await?)
                }
                .scope_boxed()
            })
            .await
            .map_err(StorageError::from)
    }

    /// Loads a unit for each active instance, with the components of the instance and their TVL.
    ///
    /// Run it in one `snapshot_transaction`, so that the pointers, the components and the TVL come
    /// from one snapshot.
    pub(crate) async fn load(conn: &mut AsyncPgConnection) -> Result<Self, StorageError> {
        let start = Instant::now();
        let (units, newest_tvl_ts) = load_units(conn).await?;
        report_load(&units, start);
        Ok(Self {
            units: RwLock::new(units),
            refresh_state: Mutex::new(RefreshState {
                newest_tvl_ts: newest_tvl_ts.unwrap_or_default(),
                last_full_reload: Instant::now(),
            }),
        })
    }

    /// The page of `instance`'s components with a TVL above `min_tvl`, in ascending id order,
    /// with the same rows and total as the SQL path. Without pagination, all matching components
    /// form the page. `None` when `instance` has no unit.
    pub(crate) fn query(
        &self,
        instance: InstanceId,
        min_tvl: Option<f64>,
        pagination: Option<&PaginationParams>,
    ) -> Result<Option<ComponentPage>, StorageError> {
        let (offset, limit) = pagination
            .map(page_window)
            .transpose()?
            .unwrap_or((0, usize::MAX));
        let units = self.read_units();
        Ok(units.get(&instance).map(|unit| {
            unit.components
                .query(min_tvl, offset, limit)
        }))
    }

    /// Brings the units up to date with the database. When the last full reload is older than
    /// `full_reload_interval`, it reads the units again from the active instances. Otherwise it
    /// adds the new components of each unit and applies the TVL rows written since the newest one
    /// read so far.
    ///
    /// Starts no transaction. Run it in one `snapshot_transaction` with
    /// `bound_snapshot_reads` and `force_custom_plans`, so that every TVL row it reads belongs to a
    /// component it can see, and each read touches only the leaves of the units.
    pub(crate) async fn refresh(
        &self,
        full_reload_interval: Duration,
        conn: &mut AsyncPgConnection,
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

        let max_ids: Vec<(InstanceId, i64)> = self
            .read_units()
            .iter()
            .map(|(&instance, unit)| (instance, unit.max_id))
            .collect();
        let newest = self.refresh_state().newest_tvl_ts;
        // One statement per unit, so each statement plans and locks only the leaf of its instance.
        let mut new_ids = HashMap::new();
        for &(instance, max_id) in &max_ids {
            let ids = load_new_ids(instance, max_id, conn).await?;
            if !ids.is_empty() {
                new_ids.insert(instance, ids);
            }
        }
        let instances: Vec<InstanceId> = max_ids
            .iter()
            .map(|&(instance, _)| instance)
            .collect();
        let changed_tvl = load_instance_tvl(&instances, Some(newest), conn).await?;

        let n_components = {
            let mut units = self.write_units();
            for (instance, unit) in units.iter_mut() {
                if let Some(ids) = new_ids.get(instance) {
                    debug!(
                        %instance,
                        n_components = ids.len(),
                        "Component index polled new components"
                    );
                    for &id in ids {
                        unit.components.insert(id, NO_TVL);
                    }
                    unit.max_id = unit
                        .max_id
                        .max(ids.last().copied().unwrap_or(0));
                }
                if let Some(rows) = changed_tvl.get(instance) {
                    unit.components.apply_tvl(rows);
                }
            }
            n_components(&units)
        };
        let n_rows: usize = changed_tvl.values().map(Vec::len).sum();
        self.advance_newest_tvl_ts(
            changed_tvl
                .values()
                .flatten()
                .map(|row| row.modified_ts)
                .max(),
        );
        gauge!("component_index_components").set(n_components as f64);
        gauge!("component_index_last_refresh_timestamp_seconds").set(unix_now());
        Ok(if n_rows == 0 {
            RefreshOutcome::TvlUnchanged
        } else {
            RefreshOutcome::TvlDelta { n_rows }
        })
    }

    /// Reads the units again from the active instances and swaps them in. Returns the number of
    /// components.
    async fn full_reload(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        let start = Instant::now();
        let (units, newest_tvl_ts) = load_units(conn).await?;
        let n_components = report_load(&units, start);
        *self.write_units() = units;
        self.advance_newest_tvl_ts(newest_tvl_ts);
        self.refresh_state().last_full_reload = Instant::now();
        Ok(n_components)
    }

    fn advance_newest_tvl_ts(&self, newest: Option<NaiveDateTime>) {
        if let Some(newest) = newest {
            let mut state = self.refresh_state();
            state.newest_tvl_ts = state.newest_tvl_ts.max(newest);
        }
    }

    /// Spawns a detached task that calls [`Self::refresh`] every `period` in one snapshot, with a
    /// full reload at least every `full_reload_interval`.
    #[cfg_attr(test, expect(dead_code, reason = "a test cannot give the task a connection pool"))]
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
                        let index = &index;
                        let refresh = async {
                            snapshot_transaction(&mut conn)
                                .run(|conn| {
                                    async move {
                                        bound_snapshot_reads(conn).await?;
                                        force_custom_plans(conn).await?;
                                        Result::<_, PostgresError>::Ok(
                                            index
                                                .refresh(full_reload_interval, conn)
                                                .await?,
                                        )
                                    }
                                    .scope_boxed()
                                })
                                .await
                                .map_err(StorageError::from)
                        };
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
                        tokio::time::sleep((period * failures).min(MAX_REFRESH_BACKOFF)).await;
                    }
                }
            }
        });
    }

    fn read_units(&self) -> std::sync::RwLockReadGuard<'_, HashMap<InstanceId, InstanceIndex>> {
        self.units
            .read()
            .expect("component index lock poisoned")
    }

    fn write_units(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<InstanceId, InstanceIndex>> {
        self.units
            .write()
            .expect("component index lock poisoned")
    }

    fn refresh_state(&self) -> std::sync::MutexGuard<'_, RefreshState> {
        self.refresh_state
            .lock()
            .expect("component index lock poisoned")
    }
}

/// Logs the units of a load and sets the gauges of a full reload. Returns the number of
/// components.
fn report_load(units: &HashMap<InstanceId, InstanceIndex>, start: Instant) -> usize {
    for (instance, unit) in units {
        info!(
            protocol = %unit.protocol,
            %instance,
            n_components = unit.components.ids.len(),
            "Loaded component index unit"
        );
    }
    let n_components = n_components(units);
    info!(
        n_units = units.len(),
        n_components,
        elapsed = ?start.elapsed(),
        "Loaded component index"
    );
    let now = unix_now();
    gauge!("component_index_last_full_reload_timestamp_seconds").set(now);
    gauge!("component_index_last_refresh_timestamp_seconds").set(now);
    gauge!("component_index_components").set(n_components as f64);
    n_components
}

fn n_components(units: &HashMap<InstanceId, InstanceIndex>) -> usize {
    units
        .values()
        .map(|unit| unit.components.ids.len())
        .sum()
}

/// Reads the ids of `instance`'s components above `after_id`, in ascending order.
async fn load_new_ids(
    instance: InstanceId,
    after_id: i64,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<i64>, PostgresError> {
    let mut ids: Vec<i64> = protocol_component::table
        .filter(protocol_component::instance_id.eq(instance))
        .filter(protocol_component::id.gt(after_id))
        .select(protocol_component::id)
        .load(conn)
        .await
        .map_err(PostgresError::from)?;
    ids.sort_unstable();
    Ok(ids)
}

/// Reads a unit for each active instance, and the newest `modified_ts` of the TVL rows it read.
/// The newest time is `None` when no active instance has a TVL row.
async fn load_units(
    conn: &mut AsyncPgConnection,
) -> Result<(HashMap<InstanceId, InstanceIndex>, Option<NaiveDateTime>), StorageError> {
    // Several protocol names can point to one instance; the instance still gets one unit.
    let protocols: HashMap<InstanceId, String> = active_protocols(conn)
        .await?
        .into_iter()
        .map(|(protocol, instance)| (instance, protocol))
        .collect();
    let instances: Vec<InstanceId> = protocols.keys().copied().collect();

    let mut rows: Vec<(InstanceId, i64)> = protocol_component::table
        .filter(protocol_component::instance_id.eq_any(&instances))
        .select((protocol_component::instance_id, protocol_component::id))
        .load(conn)
        .await
        .map_err(PostgresError::from)?;
    let mut tvl = load_instance_tvl(&instances, None, conn).await?;
    let newest_tvl_ts = tvl
        .values()
        .flatten()
        .map(|row| row.modified_ts)
        .max();

    rows.sort_unstable();
    let mut ids: HashMap<InstanceId, Vec<i64>> = rows
        .chunk_by(|a, b| a.0 == b.0)
        .map(|chunk| {
            (
                chunk[0].0,
                chunk
                    .iter()
                    .map(|(_, id)| *id)
                    .collect(),
            )
        })
        .collect();
    let units = protocols
        .into_iter()
        .map(|(instance, protocol)| {
            let ids = ids
                .remove(&instance)
                .unwrap_or_default();
            let max_id = ids.last().copied().unwrap_or(0);
            let mut components = SystemIndex { tvl: vec![NO_TVL; ids.len()], ids };
            components.apply_tvl(
                &tvl.remove(&instance)
                    .unwrap_or_default(),
            );
            (instance, InstanceIndex { protocol, components, max_id })
        })
        .collect();
    Ok((units, newest_tvl_ts))
}

/// Reads the `component_tvl` rows of `instances`, grouped by instance and sorted by component id:
/// with `newer_than`, the rows with a `modified_ts` after it, otherwise all rows. An instance
/// without rows is absent.
async fn load_instance_tvl(
    instances: &[InstanceId],
    newer_than: Option<NaiveDateTime>,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<InstanceId, Vec<TvlRow>>, PostgresError> {
    let mut query = component_tvl::table
        .filter(component_tvl::instance_id.eq_any(instances))
        .select((
            component_tvl::instance_id,
            component_tvl::protocol_component_id,
            component_tvl::tvl,
            component_tvl::modified_ts,
        ))
        .into_boxed();
    if let Some(since) = newer_than {
        query = query.filter(component_tvl::modified_ts.gt(since));
    }
    let mut rows: Vec<(InstanceId, i64, f64, NaiveDateTime)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;
    rows.sort_unstable_by_key(|(instance, component_id, ..)| (*instance, *component_id));
    Ok(rows
        .chunk_by(|a, b| a.0 == b.0)
        .map(|chunk| {
            let unit_rows = chunk
                .iter()
                .map(|&(_, component_id, tvl, modified_ts)| TvlRow {
                    component_id,
                    tvl: index_tvl(tvl),
                    modified_ts,
                })
                .collect();
            (chunk[0].0, unit_rows)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use diesel::{
        sql_query,
        sql_types::{BigInt, Int4, Text},
    };
    use diesel_async::{AsyncPgConnection, RunQueryDsl};

    use super::ComponentIndex;
    use crate::postgres::{
        db_fixtures,
        instance::{cache::component_page::ComponentPage, InstanceId},
        testing::instance_test_connection,
    };

    struct Setup {
        curve_system_id: i64,
        curve: InstanceId,
        inactive: InstanceId,
        uniswap_v4: InstanceId,
    }

    /// "vm:curve" and "uniswap_v4" on ethereum, each pointing to an instance. "vm:curve" has a
    /// second instance, `inactive`, that it does not point to.
    async fn setup(conn: &mut AsyncPgConnection) -> Setup {
        db_fixtures::insert_chain(conn, "ethereum").await;
        let curve_system_id =
            db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
        let uniswap_v4_system_id =
            db_fixtures::insert_protocol_system(conn, "uniswap_v4".to_string()).await;
        let curve = db_fixtures::insert_instance(conn, curve_system_id, "0.0.1").await;
        let inactive = db_fixtures::insert_instance(conn, curve_system_id, "0.0.2").await;
        let uniswap_v4 = db_fixtures::insert_instance(conn, uniswap_v4_system_id, "0.0.1").await;
        db_fixtures::set_active_instance(conn, curve_system_id, Some(curve)).await;
        db_fixtures::set_active_instance(conn, uniswap_v4_system_id, Some(uniswap_v4)).await;
        Setup { curve_system_id, curve, inactive, uniswap_v4 }
    }

    /// Inserts a component with an explicit id, as an import does.
    async fn insert_component_with_id(
        conn: &mut AsyncPgConnection,
        instance_id: InstanceId,
        id: i64,
        external_id: &str,
    ) {
        sql_query(
            "INSERT INTO tycho.protocol_component
                 (instance_id, id, external_id, protocol_type, created_at, creation_block,
                  creation_tx_hash)
             VALUES ($1, $2, $3, 'pool', '2020-01-01', 1, '\\x01')",
        )
        .bind::<Int4, _>(instance_id)
        .bind::<BigInt, _>(id)
        .bind::<Text, _>(external_id)
        .execute(conn)
        .await
        .expect("inserting the component failed");
    }

    fn page(ids: &[i64], total: i64) -> ComponentPage {
        ComponentPage { ids: ids.to_vec(), total }
    }

    #[tokio::test]
    async fn test_index_holds_active_instances_only() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[]).await;
        let b = db_fixtures::insert_instance_component(&mut conn, s.curve, "b", &[]).await;
        db_fixtures::insert_instance_component(&mut conn, s.inactive, "x", &[]).await;
        db_fixtures::insert_instance_component(&mut conn, s.uniswap_v4, "u", &[]).await;

        let index = ComponentIndex::load(&mut conn)
            .await
            .expect("load failed");

        assert_eq!(index.query(s.curve, None, None), Ok(Some(page(&[a, b], 2))));
        assert_eq!(index.query(s.inactive, None, None), Ok(None));
        let uniswap_v4 = index
            .query(s.uniswap_v4, None, None)
            .expect("query failed")
            .expect("uniswap_v4 has a unit");
        assert_eq!(uniswap_v4.total, 1);
    }

    #[tokio::test]
    async fn test_same_component_id_in_two_instances_stays_apart() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        insert_component_with_id(&mut conn, s.curve, 1, "a").await;
        insert_component_with_id(&mut conn, s.uniswap_v4, 1, "u").await;
        db_fixtures::insert_instance_tvl(&mut conn, s.curve, 1, 10.0).await;
        db_fixtures::insert_instance_tvl(&mut conn, s.uniswap_v4, 1, 0.0).await;

        let index = ComponentIndex::load(&mut conn)
            .await
            .expect("load failed");

        assert_eq!(index.query(s.curve, Some(5.0), None), Ok(Some(page(&[1], 1))));
        assert_eq!(index.query(s.uniswap_v4, Some(5.0), None), Ok(Some(page(&[], 0))));
    }

    #[tokio::test]
    async fn test_active_instance_without_components_has_an_empty_unit() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        let index = ComponentIndex::load(&mut conn)
            .await
            .expect("load failed");

        assert_eq!(index.query(s.uniswap_v4, None, None), Ok(Some(page(&[], 0))));
    }

    #[tokio::test]
    async fn test_pointer_move_after_load_keeps_the_old_unit() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = db_fixtures::insert_instance_component(&mut conn, s.curve, "a", &[]).await;
        db_fixtures::insert_instance_component(&mut conn, s.inactive, "x", &[]).await;
        let index = ComponentIndex::load(&mut conn)
            .await
            .expect("load failed");

        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, Some(s.inactive)).await;

        assert_eq!(index.query(s.curve, None, None), Ok(Some(page(&[a], 1))));
        assert_eq!(index.query(s.inactive, None, None), Ok(None));
    }
}
