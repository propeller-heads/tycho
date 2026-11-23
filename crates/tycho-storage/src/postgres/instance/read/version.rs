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

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};
    use tycho_common::{
        models::Chain,
        storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
    };

    use super::check_current;
    use crate::postgres::{
        instance::{active::ActiveInstance, InstanceId},
        testing::instance_test_connection,
    };

    fn day(d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, d)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    fn at(ts: NaiveDateTime) -> Version {
        Version(BlockOrTimestamp::Timestamp(ts), VersionKind::Last)
    }

    fn instance(head: Option<NaiveDateTime>) -> ActiveInstance {
        ActiveInstance { id: InstanceId(1), head }
    }

    #[tokio::test]
    async fn test_no_version_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = instance(None);

        assert_eq!(
            check_current(&instance, None, &mut conn)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_version_at_or_after_head_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = instance(Some(day(2)));

        for ts in [day(2), day(3)] {
            assert_eq!(
                check_current(&instance, Some(&at(ts)), &mut conn)
                    .await
                    .unwrap(),
                Some(ts)
            );
        }
    }

    #[tokio::test]
    async fn test_version_before_head_is_unsupported() {
        let mut conn = instance_test_connection().await;
        let instance = instance(Some(day(2)));

        let result = check_current(&instance, Some(&at(day(1))), &mut conn).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_check_current_answers_latest_without_a_block() {
        let mut conn = instance_test_connection().await;
        let instance = instance(Some(day(2)));
        let latest = Version(
            BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)),
            VersionKind::Last,
        );

        assert_eq!(
            check_current(&instance, Some(&latest), &mut conn)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_instance_without_head_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = instance(None);

        assert_eq!(
            check_current(&instance, Some(&at(day(1))), &mut conn)
                .await
                .unwrap(),
            Some(day(1))
        );
    }
}
