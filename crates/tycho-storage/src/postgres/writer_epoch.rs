//! Fencing for the single database writer.
//!
//! Only one process may write extraction results to a database. The process that becomes the
//! writer claims a new epoch, and every write transaction checks that the stored epoch is still
//! the claimed one. A process that lost the writer role then fails its writes instead of
//! interleaving them with the new writer's.
use std::{
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tracing::warn;
use tycho_common::storage::StorageError;

use super::{schema::writer_epoch, PostgresError};

/// The writer epoch this process holds, shared by the write executor and the gateways feeding it.
///
/// The table starts at epoch 0 and every claim increments it, so a claimed epoch is positive and
/// 0 stands for "not claimed yet".
#[derive(Clone, Debug, Default)]
pub(crate) struct ClaimedEpoch(Arc<AtomicI64>);

impl ClaimedEpoch {
    /// Returns the claimed epoch, or `None` while this process has not claimed one.
    pub(crate) fn get(&self) -> Option<i64> {
        match AtomicI64::load(&self.0, Ordering::Acquire) {
            0 => None,
            epoch => Some(epoch),
        }
    }

    /// Claims a new epoch in the database and makes it the one writes are fenced on.
    pub(crate) async fn claim(&self, conn: &mut AsyncPgConnection) -> Result<i64, StorageError> {
        let epoch = claim(conn).await?;
        self.0.store(epoch, Ordering::Release);
        Ok(epoch)
    }
}

/// How often a claim that waits for the epoch row logs a warning.
const CLAIM_WAIT_WARNING_INTERVAL: Duration = Duration::from_secs(10);

/// Increments the stored writer epoch and returns the new value.
///
/// Any transaction that still checks against an older epoch fails with
/// [`StorageError::Fenced`] from then on.
///
/// The claim waits until every open write transaction of a former writer has ended, because
/// each of them holds a share lock on the epoch row (see [`ensure_current`]). It logs a warning
/// while it waits.
pub(crate) async fn claim(conn: &mut AsyncPgConnection) -> Result<i64, StorageError> {
    let update = diesel::update(writer_epoch::table)
        .set(writer_epoch::epoch.eq(writer_epoch::epoch + 1))
        .returning(writer_epoch::epoch)
        .get_result(conn);
    tokio::pin!(update);
    let mut waited = Duration::ZERO;
    loop {
        if let Ok(result) = tokio::time::timeout(CLAIM_WAIT_WARNING_INTERVAL, &mut update).await {
            return result.map_err(|err| {
                StorageError::Unexpected(format!("Failed to claim the writer epoch: {err}"))
            });
        }
        waited += CLAIM_WAIT_WARNING_INTERVAL;
        warn!(
            waited_secs = waited.as_secs(),
            "Waiting to claim the writer epoch: an open write transaction of a former writer \
             holds the epoch row until it ends"
        );
    }
}

/// Fails with [`StorageError::Fenced`] unless the stored writer epoch equals `claimed`.
///
/// Holds a share lock on the epoch row until the surrounding transaction ends, so a concurrent
/// [`claim`] waits for that transaction. A claim that commits after the transaction's snapshot
/// makes this read fail with a serialization error, which callers retry with a fresh snapshot.
pub(crate) async fn ensure_current(
    conn: &mut AsyncPgConnection,
    claimed: i64,
) -> Result<(), PostgresError> {
    let current: i64 = writer_epoch::table
        .select(writer_epoch::epoch)
        .for_share()
        .get_result(conn)
        .await?;
    if current != claimed {
        return Err(PostgresError(StorageError::Fenced { claimed, current }));
    }
    Ok(())
}

#[cfg(test)]
mod test_serial_db {
    use diesel_async::{AsyncConnection, SimpleAsyncConnection};

    use super::*;

    async fn open_session() -> AsyncPgConnection {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        AsyncPgConnection::establish(&url)
            .await
            .expect("session should connect")
    }

    #[tokio::test]
    async fn test_claim_waits_until_open_write_transactions_end() {
        let mut writer = open_session().await;
        writer
            .batch_execute("BEGIN; SELECT epoch FROM writer_epoch FOR SHARE;")
            .await
            .unwrap();
        let mut claimer = open_session().await;
        let claim = tokio::spawn(async move { claim(&mut claimer).await });

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!claim.is_finished(), "the claim must wait for the open write transaction");

        writer
            .batch_execute("COMMIT")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), claim)
            .await
            .expect("the claim should finish once the transaction ends")
            .unwrap()
            .expect("the claim should succeed");
    }
}
