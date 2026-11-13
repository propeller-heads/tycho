//! One snapshot for every statement of one read.
//!
//! Outside a transaction, each statement takes a new snapshot, so a block that commits between two
//! statements of one read can change its answer: a version check passes against the old head and
//! the next query reads the rows of the new block. A read calls [`begin_snapshot`] after its
//! argument checks and [`end_snapshot`] at the end, so all its statements see one moment.
//!
//! Between the two calls there is no `?` and no `return`: the body yields a `Result` that goes to
//! [`end_snapshot`]. Otherwise the transaction stays open, and the pool drops the connection.
//!
//! A read that runs inside a transaction, such as one read called by another, starts nothing and
//! uses the outer snapshot.

use diesel_async::{AnsiTransactionManager, AsyncPgConnection, TransactionManager};
use tracing::warn;
use tycho_common::storage::StorageError;

use crate::postgres::PostgresError;

/// Starts a read-only `REPEATABLE READ` transaction on `conn`, and returns `true`. Returns `false`
/// and starts nothing when `conn` is already in a transaction.
///
/// Give the return value to [`end_snapshot`]. If the read is cancelled before it ends the snapshot,
/// the transaction stays open: diesel-async marks the connection broken and the pool drops it.
pub(super) async fn begin_snapshot(conn: &mut AsyncPgConnection) -> Result<bool, StorageError> {
    let depth = AnsiTransactionManager::transaction_manager_status_mut(conn)
        .transaction_depth()
        .map_err(PostgresError::from)?;
    if depth.is_some() {
        return Ok(false);
    }
    AnsiTransactionManager::begin_transaction_sql(
        conn,
        "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY",
    )
    .await
    .map_err(PostgresError::from)?;
    Ok(true)
}

/// Ends the snapshot that [`begin_snapshot`] started, and returns `result`.
///
/// - `started`: the return value of [`begin_snapshot`]. If `false`, ends nothing.
/// - `result`: the answer of the read. `Ok` commits; a commit error replaces the answer. `Err`
///   rolls back, and the read's error is returned even if the rollback fails.
pub(super) async fn end_snapshot<T>(
    conn: &mut AsyncPgConnection,
    started: bool,
    result: Result<T, StorageError>,
) -> Result<T, StorageError> {
    if !started {
        return result;
    }
    match result {
        Ok(value) => {
            AnsiTransactionManager::commit_transaction(conn)
                .await
                .map_err(PostgresError::from)?;
            Ok(value)
        }
        Err(err) => {
            // A failed rollback marks the connection broken, so the pool drops it.
            if let Err(rollback) = AnsiTransactionManager::rollback_transaction(conn).await {
                warn!(%rollback, "rolling back a read snapshot failed");
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use diesel::{dsl::sql, prelude::*, sql_query, sql_types::Text};
    use diesel_async::{
        AnsiTransactionManager, AsyncPgConnection, RunQueryDsl, TransactionManager,
    };
    use tycho_common::storage::StorageError;

    use super::{begin_snapshot, end_snapshot};
    use crate::postgres::{
        db_fixtures,
        schema::protocol_system,
        testing::{instance_test_connection, run_against_db},
        PostgresError,
    };

    fn depth(conn: &mut AsyncPgConnection) -> Option<u32> {
        AnsiTransactionManager::transaction_manager_status_mut(conn)
            .transaction_depth()
            .expect("a valid transaction manager")
            .map(u32::from)
    }

    async fn setting(conn: &mut AsyncPgConnection, name: &str) -> String {
        diesel::select(sql::<Text>(&format!("current_setting('{name}')")))
            .get_result(conn)
            .await
            .unwrap()
    }

    async fn system_count(conn: &mut AsyncPgConnection) -> i64 {
        protocol_system::table
            .count()
            .get_result(conn)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_snapshot_inside_a_transaction_starts_nothing() {
        let mut conn = instance_test_connection().await;

        let started = begin_snapshot(&mut conn).await.unwrap();
        let result = end_snapshot(&mut conn, started, Ok(7)).await;

        assert!(!started);
        assert_eq!(result, Ok(7));
        assert_eq!(depth(&mut conn), Some(1));
        system_count(&mut conn).await;
    }

    #[tokio::test]
    async fn test_serial_db_snapshot_is_repeatable_read_only() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();

            let started = begin_snapshot(&mut conn).await.unwrap();
            let isolation = setting(&mut conn, "transaction_isolation").await;
            let read_only = setting(&mut conn, "transaction_read_only").await;
            let result = end_snapshot(&mut conn, started, Ok(())).await;

            assert!(started);
            assert_eq!(isolation, "repeatable read");
            assert_eq!(read_only, "on");
            assert_eq!(result, Ok(()));
            assert_eq!(depth(&mut conn), None);
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_snapshot_hides_a_commit_between_statements() {
        run_against_db(|pool| async move {
            let mut reader = pool.get().await.unwrap();
            let mut writer = pool.get().await.unwrap();

            let started = begin_snapshot(&mut reader)
                .await
                .unwrap();
            let before = system_count(&mut reader).await;
            db_fixtures::insert_protocol_system(&mut writer, "vm:curve".to_string()).await;
            let during = system_count(&mut reader).await;
            end_snapshot(&mut reader, started, Ok(()))
                .await
                .unwrap();
            let after = system_count(&mut reader).await;

            assert_eq!(during, before);
            assert_eq!(after, before + 1);
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_snapshot_rolls_back_on_error() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();

            let started = begin_snapshot(&mut conn).await.unwrap();
            let error: StorageError = PostgresError::from(
                sql_query("SELECT 1/0")
                    .execute(&mut conn)
                    .await
                    .expect_err("a division by zero"),
            )
            .into();
            let result: Result<(), StorageError> =
                end_snapshot(&mut conn, started, Err(error.clone())).await;

            assert_eq!(result, Err(error));
            assert_eq!(depth(&mut conn), None);
            system_count(&mut conn).await;
        })
        .await;
    }
}
