use std::time::{Duration, Instant};

use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use metrics::{gauge, histogram};
use tracing::instrument;
use tycho_common::storage::StorageError;

pub(crate) const EXTRACTION_POOL: &str = "extraction";
pub(crate) const RPC_POOL: &str = "rpc";
pub(crate) const DIRECT_POOL: &str = "direct";

pub(crate) type PooledConnection =
    diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>;

/// Takes a connection from `pool`. The span and the histogram show how long the call waits.
#[instrument(name = "db_pool_checkout", skip_all, fields(pool = label))]
pub(crate) async fn checkout(
    pool: &Pool<AsyncPgConnection>,
    label: &'static str,
) -> Result<PooledConnection, StorageError> {
    let start = Instant::now();
    let result = pool.get().await;
    histogram!("db_pool_checkout_duration_seconds", "pool" => label)
        .record(start.elapsed().as_secs_f64());
    result.map_err(|e| StorageError::Unexpected(format!("Failed to retrieve connection: {e}")))
}

pub(crate) const POOL_STATUS_PERIOD: Duration = Duration::from_secs(1);

/// Exports the state of `pool` as gauges every `period`.
pub(crate) fn spawn_status_gauges(
    pool: Pool<AsyncPgConnection>,
    label: &'static str,
    period: Duration,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let status = pool.status();
            let in_use = status
                .size
                .saturating_sub(status.available);
            gauge!("db_pool_connections", "pool" => label, "state" => "in_use").set(in_use as f64);
            gauge!("db_pool_connections", "pool" => label, "state" => "available")
                .set(status.available as f64);
            gauge!("db_pool_waiting", "pool" => label).set(status.waiting as f64);
            gauge!("db_pool_max_size", "pool" => label).set(status.max_size as f64);
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

        let err = checkout(&pool, RPC_POOL)
            .await
            .err()
            .expect("checkout must fail");

        let StorageError::Unexpected(msg) = err else {
            panic!("unexpected error variant: {err:?}")
        };
        assert!(msg.starts_with("Failed to retrieve connection: "), "{msg}");
    }
}
