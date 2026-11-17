use std::time::Duration;

use chrono::NaiveDateTime;

use crate::postgres::count_to_i64;

/// TVL of a component without a `component_tvl` row. It never compares greater than a threshold.
pub const NO_TVL: f64 = f64::NEG_INFINITY;

/// Longest a refresh may run before the task abandons it and its connection. Reads already stop
/// after the snapshot statement timeout; this also covers a connection that stopped answering.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Longest wait after a failed refresh. The wait grows by one refresh period per consecutive
/// failure, so a failing full reload does not rerun every period.
pub const MAX_REFRESH_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// One page of a query: the database ids on the page, in ascending order, and the number of
/// components matching the filters on all pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentPage {
    pub(crate) ids: Vec<i64>,
    pub(crate) total: i64,
}

/// What a [`ComponentIndex::refresh`](crate::postgres::component_index::ComponentIndex::refresh)
/// did with TVL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// No TVL row changed.
    TvlUnchanged,
    /// Applied the TVL rows that changed.
    TvlDelta { n_rows: usize },
    /// Read components and TVL again.
    FullReload { n_components: usize },
}

impl RefreshOutcome {
    /// The `outcome` label of `component_index_refreshes_total`.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            RefreshOutcome::TvlUnchanged => "tvl_unchanged",
            RefreshOutcome::TvlDelta { .. } => "tvl_delta",
            RefreshOutcome::FullReload { .. } => "full_reload",
        }
    }
}

/// Seconds since the Unix epoch, for the timestamp gauges.
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// Components of one protocol system, in ascending id order.
#[derive(Debug, Default)]
pub struct SystemIndex {
    pub(crate) ids: Vec<i64>,
    /// TVL per position in `ids`, [`NO_TVL`] when the component has no TVL row.
    pub(crate) tvl: Vec<f64>,
}

impl SystemIndex {
    /// Adds a component and keeps ids in ascending order. A component already present keeps its
    /// current TVL.
    pub(crate) fn insert(&mut self, id: i64, tvl: f64) {
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

    /// Writes the TVL of the rows whose component is in this system. `rows` must be sorted by
    /// component id; other rows are ignored.
    pub(crate) fn apply_tvl(&mut self, rows: &[TvlRow]) {
        let mut position = 0;
        for row in rows {
            while position < self.ids.len() && self.ids[position] < row.component_id {
                position += 1;
            }
            if position == self.ids.len() {
                break;
            }
            if self.ids[position] == row.component_id {
                self.tvl[position] = row.tvl;
            }
        }
    }

    /// Ids on the page `[offset, offset + limit)` of the components with a TVL above `min_tvl`,
    /// and the number of such components on all pages.
    pub(crate) fn query(&self, min_tvl: Option<f64>, offset: usize, limit: usize) -> ComponentPage {
        let Some(threshold) = min_tvl else {
            let start = offset.min(self.ids.len());
            let end = offset
                .saturating_add(limit)
                .min(self.ids.len());
            return ComponentPage {
                ids: self.ids[start..end].to_vec(),
                total: count_to_i64(self.ids.len()),
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
        ComponentPage { ids, total: count_to_i64(total) }
    }
}

/// One `component_tvl` row, in index representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TvlRow {
    pub(crate) component_id: i64,
    pub(crate) tvl: f64,
    pub(crate) modified_ts: NaiveDateTime,
}

/// Maps a `component_tvl.tvl` value to its index representation. Postgres orders `NaN` above every
/// other float, so `NaN > threshold` holds there; `f64::INFINITY` keeps that result for any
/// threshold a request can carry.
pub fn index_tvl(tvl: f64) -> f64 {
    if tvl.is_nan() {
        f64::INFINITY
    } else {
        tvl
    }
}
