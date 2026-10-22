//! Metrics on how long callers wait for a pooled connection and how long they hold it.
//!
//! - `db_pool_wait_ms{pool, op}`: time from asking the pool for a connection until it is handed
//!   out, also when the pool returns an error.
//! - `db_conn_hold_ms{pool, op}`: time from handing out a connection until the caller drops it. It
//!   includes every query, and all work the caller does while it holds the connection.
//! - `db_pool_connections{pool, state}` with `state` = `max`, `size` or `available`, and
//!   `db_pool_waiting{pool}`: the pool status, sampled every [`POOL_GAUGES_PERIOD`].
use std::{
    ops::{Deref, DerefMut},
    time::{Duration, Instant},
};

use diesel_async::{
    pooled_connection::deadpool::{Object, Pool, PoolError},
    AsyncPgConnection,
};
use tokio::task::JoinHandle;

/// Name of the pool that extractors, the database writer and the cache refresh tasks use.
pub(crate) const EXTRACTION_POOL: &str = "extraction";
/// Name of the pool that request handling uses.
pub(crate) const RPC_POOL: &str = "rpc";

const POOL_GAUGES_PERIOD: Duration = Duration::from_secs(5);

/// A pooled connection that records how long it was held when it is dropped.
pub(crate) struct TimedConn {
    conn: Object<AsyncPgConnection>,
    pool: &'static str,
    op: &'static str,
    acquired: Instant,
}

impl Deref for TimedConn {
    type Target = AsyncPgConnection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl DerefMut for TimedConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl Drop for TimedConn {
    fn drop(&mut self) {
        metrics::histogram!("db_conn_hold_ms", "pool" => self.pool, "op" => self.op)
            .record(self.acquired.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Gets a connection from `pool` like [`Pool::get`], and records the wait under `pool_name` and
/// `op`. The returned connection records how long it was held when it is dropped.
///
/// # Errors
///
/// Returns the error of [`Pool::get`] unchanged.
pub(crate) async fn get_timed(
    pool: &Pool<AsyncPgConnection>,
    pool_name: &'static str,
    op: &'static str,
) -> Result<TimedConn, PoolError> {
    let started = Instant::now();
    let result = pool.get().await;
    metrics::histogram!("db_pool_wait_ms", "pool" => pool_name, "op" => op)
        .record(started.elapsed().as_secs_f64() * 1000.0);
    let conn = result?;
    Ok(TimedConn { conn, pool: pool_name, op, acquired: Instant::now() })
}

/// Sets the pool status gauges of `pool` under `pool_name` every [`POOL_GAUGES_PERIOD`] until the
/// returned task is aborted.
pub(crate) fn spawn_pool_gauges(
    pool: Pool<AsyncPgConnection>,
    pool_name: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POOL_GAUGES_PERIOD);
        loop {
            interval.tick().await;
            let status = pool.status();
            for (state, value) in
                [("max", status.max_size), ("size", status.size), ("available", status.available)]
            {
                metrics::gauge!("db_pool_connections", "pool" => pool_name, "state" => state)
                    .set(value as f64);
            }
            metrics::gauge!("db_pool_waiting", "pool" => pool_name).set(status.waiting as f64);
        }
    })
}

#[cfg(test)]
mod tests {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;

    use super::*;

    #[tokio::test]
    async fn test_get_timed_returns_the_pool_error() {
        // A closed pool fails before it tries to connect, so no database is needed.
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(
            "postgres://user:password@localhost:1/none",
        );
        let pool = Pool::builder(manager)
            .build()
            .expect("pool config should be valid");
        pool.close();

        let err = get_timed(&pool, RPC_POOL, "test")
            .await
            .err()
            .expect("a closed pool should not hand out connections");

        assert!(matches!(err, PoolError::Closed), "unexpected error: {err}");
    }
}

#[cfg(test)]
mod test_serial_db {
    use diesel::sql_query;
    use diesel_async::RunQueryDsl;

    use super::*;
    use crate::postgres::testing::run_against_db;

    #[tokio::test]
    async fn test_get_timed_hands_out_a_working_connection() {
        run_against_db(|pool| async move {
            let mut conn = get_timed(&pool, RPC_POOL, "test")
                .await
                .expect("pool should hand out a connection");

            sql_query("SELECT 1")
                .execute(&mut conn)
                .await
                .expect("connection should run a query");
        })
        .await;
    }
}
