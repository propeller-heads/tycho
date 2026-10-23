//! Election of the single process that writes extraction results to a database.
//!
//! Every process that may write competes for one Postgres session advisory lock. The process that
//! holds it is the leader. The lock lives as long as the session that took it: when the leader
//! exits or its connection breaks, Postgres releases the lock and another process can take it.
//!
//! The lock elects the leader but does not stop a former leader from writing; the writer epoch
//! (see [`CachedGateway::increment_writer_epoch`](super::cache::CachedGateway::increment_writer_epoch))
//! does that.
use diesel::{
    sql_query,
    sql_types::{BigInt, Bool},
    QueryableByName,
};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use tycho_common::storage::StorageError;

use super::SESSION_KEEPALIVES;

/// Postgres advisory lock key of the leader election.
const LEADER_LOCK_KEY: i64 = 0x7479_6368_6f5f_6c64;

#[derive(QueryableByName)]
struct LockResult {
    #[diesel(sql_type = Bool)]
    acquired: bool,
}

/// A dedicated database session competing for the leader lock.
///
/// The session is not taken from a pool: the lock belongs to the session, so it must stay open for
/// as long as this process is leader. The session sets the server-side TCP keepalives, so Postgres
/// releases the lock about 25 seconds after the leader's host disappears, not hours later.
pub struct LeaderLock {
    conn: AsyncPgConnection,
}

impl LeaderLock {
    /// Opens the session the lock is taken on.
    pub async fn connect(database_url: &str) -> Result<Self, StorageError> {
        let mut conn = AsyncPgConnection::establish(database_url)
            .await
            .map_err(|err| {
                StorageError::Unexpected(format!("Failed to open the leader lock session: {err}"))
            })?;
        conn.batch_execute(SESSION_KEEPALIVES)
            .await
            .map_err(|err| {
                StorageError::Unexpected(format!(
                    "Failed to set keepalives on the leader lock session: {err}"
                ))
            })?;
        Ok(Self { conn })
    }

    /// Tries to take the leader lock without waiting.
    ///
    /// Returns `true` if this session holds the lock, also when it already held it before the
    /// call, and `false` if another session holds it.
    pub async fn try_acquire(&mut self) -> Result<bool, StorageError> {
        let result: LockResult = sql_query("SELECT pg_try_advisory_lock($1) AS acquired")
            .bind::<BigInt, _>(LEADER_LOCK_KEY)
            .get_result(&mut self.conn)
            .await
            .map_err(|err| {
                StorageError::Unexpected(format!("Failed to try the leader lock: {err}"))
            })?;
        Ok(result.acquired)
    }

    /// Fails if the lock session is no longer usable.
    ///
    /// A leader whose session is gone has lost the lock, even if the error only shows up here.
    pub async fn check_alive(&mut self) -> Result<(), StorageError> {
        self.conn
            .batch_execute("SELECT 1")
            .await
            .map_err(|err| StorageError::Unexpected(format!("Leader lock session lost: {err}")))
    }
}
