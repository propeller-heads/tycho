use std::time::Instant;

use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use metrics::histogram;
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

    #[test]
    fn pool_role_labels() {
        assert_eq!(PoolRole::Extraction.label(), "extraction");
        assert_eq!(PoolRole::Rpc.label(), "rpc");
        assert_eq!(PoolRole::Direct.label(), "direct");
    }
}
