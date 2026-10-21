use std::time::{Duration, Instant};

use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use metrics::{gauge, histogram};
use tracing::instrument;
use tycho_common::storage::StorageError;

#[derive(Clone, Copy, Debug)]
pub(crate) enum PoolRole {
    Extraction,
    Rpc,
    Direct,
}

impl PoolRole {
    pub(crate) fn label(self) -> &'static str {
        match self {
            PoolRole::Extraction => "extraction",
            PoolRole::Rpc => "rpc",
            PoolRole::Direct => "direct",
        }
    }
}

pub(crate) type PooledConnection =
    diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>;

/// Takes a connection from `pool`. The span and the histogram show how long the call waits.
#[instrument(name = "db_pool_checkout", skip_all, fields(pool = role.label()))]
pub(crate) async fn checkout(
    pool: &Pool<AsyncPgConnection>,
    role: PoolRole,
) -> Result<PooledConnection, StorageError> {
    let start = Instant::now();
    let result = pool.get().await;
    histogram!("db_pool_checkout_duration_seconds", "pool" => role.label())
        .record(start.elapsed().as_secs_f64());
    result.map_err(|e| StorageError::Unexpected(format!("Failed to retrieve connection: {e}")))
}

pub(crate) const POOL_STATUS_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug)]
pub(crate) struct PoolUsage {
    pub(crate) max_size: usize,
    pub(crate) in_use: usize,
    pub(crate) available: usize,
    pub(crate) waiting: usize,
}

impl PoolUsage {
    pub(crate) fn of(pool: &Pool<AsyncPgConnection>) -> Self {
        let status = pool.status();
        Self {
            max_size: status.max_size,
            in_use: status
                .size
                .saturating_sub(status.available),
            available: status.available,
            waiting: status.waiting,
        }
    }
}

/// Exports the state of `pool` as gauges every `period`.
pub(crate) fn spawn_status_gauges(pool: Pool<AsyncPgConnection>, role: PoolRole, period: Duration) {
    tokio::spawn(async move {
        let label = role.label();
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let usage = PoolUsage::of(&pool);
            gauge!("db_pool_connections", "pool" => label, "state" => "in_use")
                .set(usage.in_use as f64);
            gauge!("db_pool_connections", "pool" => label, "state" => "available")
                .set(usage.available as f64);
            gauge!("db_pool_waiting", "pool" => label).set(usage.waiting as f64);
            gauge!("db_pool_max_size", "pool" => label).set(usage.max_size as f64);
        }
    });
}

#[cfg(test)]
mod test {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;

    use super::*;

    #[tokio::test]
    async fn checkout_error_keeps_message() {
        // Port 1 refuses connections, so the pool cannot create one.
        let manager =
            AsyncDieselConnectionManager::<AsyncPgConnection>::new("postgres://u:p@127.0.0.1:1/db");
        let pool = Pool::builder(manager).build().unwrap();

        let err = checkout(&pool, PoolRole::Rpc)
            .await
            .err()
            .expect("checkout must fail");

        let StorageError::Unexpected(msg) = err else {
            panic!("unexpected error variant: {err:?}")
        };
        assert!(msg.starts_with("Failed to retrieve connection: "), "{msg}");
    }

    #[tokio::test]
    async fn usage_of_a_fresh_pool() {
        let manager =
            AsyncDieselConnectionManager::<AsyncPgConnection>::new("postgres://u:p@127.0.0.1:1/db");
        let pool = Pool::builder(manager)
            .max_size(3)
            .build()
            .unwrap();

        let usage = PoolUsage::of(&pool);

        assert_eq!((usage.max_size, usage.in_use, usage.available, usage.waiting), (3, 0, 0, 0));
    }

    #[test]
    fn pool_role_labels() {
        assert_eq!(PoolRole::Extraction.label(), "extraction");
        assert_eq!(PoolRole::Rpc.label(), "rpc");
        assert_eq!(PoolRole::Direct.label(), "direct");
    }
}
