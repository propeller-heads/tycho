use chrono::NaiveDateTime;
use diesel_async::AsyncPgConnection;
use tycho_common::storage::{
    BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind,
};

use crate::postgres::{instance::active::ActiveInstance, maybe_lookup_version_ts};

/// Checks that the current tables of `instance` can answer the version `at`, and returns the
/// timestamp of `at`. The head is the one the read looked up with the instance.
///
/// - `None` or the `Latest` block: the latest version, which the current tables always answer.
///   Returns `None` and reads nothing.
/// - a timestamp at or after the head of the instance (its extraction state), or any timestamp when
///   the instance has no extraction state yet: returns the timestamp.
/// - a timestamp before the head: `Unsupported`, because past versions need the history tables.
///
/// The current tables hold every row valid at or after the head, because the writer saves the
/// extraction state in the same transaction as the rows. A version kind other than `Last` is
/// `Unsupported`.
pub(super) async fn check_current(
    instance: &ActiveInstance,
    at: Option<&Version>,
    conn: &mut AsyncPgConnection,
) -> Result<Option<NaiveDateTime>, StorageError> {
    let Some(at) = at else {
        return Ok(None);
    };
    if let Version(BlockOrTimestamp::Block(BlockIdentifier::Latest(_)), VersionKind::Last) = at {
        return Ok(None);
    }
    let ts = maybe_lookup_version_ts(at, conn).await?;
    match instance.head {
        Some(head) if ts < head => Err(StorageError::Unsupported(format!(
            "version {ts} is before the head of instance {}: past versions need the history tables",
            instance.id
        ))),
        _ => Ok(Some(ts)),
    }
}
