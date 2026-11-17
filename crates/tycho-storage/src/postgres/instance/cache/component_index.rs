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
//! instance until restart, and the new instance has no unit.
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
    sync::{Mutex, RwLock},
    time::Instant,
};

use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncPgConnection,
    RunQueryDsl,
};
use metrics::gauge;
use tracing::info;
use tycho_common::{models::PaginationParams, storage::StorageError};

use crate::postgres::{
    instance::{
        active::active_protocols,
        cache::component_page::{index_tvl, unix_now, ComponentPage, SystemIndex, TvlRow, NO_TVL},
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
    #[cfg_attr(test, expect(dead_code, reason = "the poll reads it"))]
    refresh_state: Mutex<RefreshState>,
}

/// Bookkeeping of the refresh loop.
#[derive(Debug)]
#[cfg_attr(test, expect(dead_code, reason = "the poll reads it"))]
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
    #[cfg_attr(test, expect(dead_code, reason = "the poll reads it"))]
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

        for (instance, unit) in &units {
            info!(
                protocol = %unit.protocol,
                %instance,
                n_components = unit.components.ids.len(),
                "Loaded component index unit"
            );
        }
        let n_components: usize = units
            .values()
            .map(|unit| unit.components.ids.len())
            .sum();
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

    fn read_units(&self) -> std::sync::RwLockReadGuard<'_, HashMap<InstanceId, InstanceIndex>> {
        self.units
            .read()
            .expect("component index lock poisoned")
    }
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
