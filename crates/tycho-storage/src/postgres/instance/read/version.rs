use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::storage::{StorageError, Version};

use crate::postgres::{
    instance::{schema::tycho::extraction_state, InstanceId},
    maybe_lookup_version_ts, PostgresError,
};

/// Checks that the current tables of `instance` can answer the version `at`, and returns the
/// timestamp of `at`.
///
/// - `None`: the latest version, which the current tables always answer. Returns `None`.
/// - a timestamp at or after the head of the instance (its extraction state), or any timestamp when
///   the instance has no extraction state yet: returns the timestamp.
/// - a timestamp before the head: `Unsupported`, because past versions need the history tables.
///
/// The current tables hold every row valid at or after the head, because the writer saves the
/// extraction state in the same transaction as the rows. A version kind other than `Last` is
/// `Unsupported`.
pub(super) async fn check_current(
    conn: &mut AsyncPgConnection,
    instance: InstanceId,
    at: Option<&Version>,
) -> Result<Option<NaiveDateTime>, StorageError> {
    let Some(at) = at else {
        return Ok(None);
    };
    let ts = maybe_lookup_version_ts(at, conn).await?;
    let head = extraction_state::table
        .find(instance)
        .select(extraction_state::block_ts)
        .first::<NaiveDateTime>(conn)
        .await
        .optional()
        .map_err(PostgresError::from)?;
    match head {
        Some(head) if ts < head => Err(StorageError::Unsupported(format!(
            "version {ts} is before the head of instance {instance}: past versions need the \
             history tables"
        ))),
        _ => Ok(Some(ts)),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};
    use diesel_async::AsyncPgConnection;
    use tycho_common::{
        models::Chain,
        storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
    };

    use super::check_current;
    use crate::postgres::{
        db_fixtures, instance::InstanceId, schema::block, testing::instance_test_connection,
    };

    fn day(d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, d)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    fn at(ts: NaiveDateTime) -> Version {
        Version(BlockOrTimestamp::Timestamp(ts), VersionKind::Last)
    }

    async fn curve_instance(conn: &mut AsyncPgConnection) -> InstanceId {
        let system_id = db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
        db_fixtures::insert_instance(conn, system_id, "0.0.1").await
    }

    #[tokio::test]
    async fn test_no_version_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = curve_instance(&mut conn).await;

        assert_eq!(
            check_current(&mut conn, instance, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_version_at_or_after_head_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = curve_instance(&mut conn).await;
        db_fixtures::set_instance_head(&mut conn, instance, day(2)).await;

        for ts in [day(2), day(3)] {
            assert_eq!(
                check_current(&mut conn, instance, Some(&at(ts)))
                    .await
                    .unwrap(),
                Some(ts)
            );
        }
    }

    #[tokio::test]
    async fn test_version_before_head_is_unsupported() {
        let mut conn = instance_test_connection().await;
        let instance = curve_instance(&mut conn).await;
        db_fixtures::set_instance_head(&mut conn, instance, day(2)).await;

        let result = check_current(&mut conn, instance, Some(&at(day(1)))).await;

        assert!(matches!(result, Err(StorageError::Unsupported(_))), "{result:?}");
    }

    #[tokio::test]
    async fn test_latest_block_is_current() {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        let mut conn = instance_test_connection().await;
        let instance = curve_instance(&mut conn).await;
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
        db_fixtures::insert_blocks(&mut conn, chain_id).await;
        let head: NaiveDateTime = block::table
            .select(diesel::dsl::max(block::ts))
            .first::<Option<NaiveDateTime>>(&mut conn)
            .await
            .unwrap()
            .expect("a block");
        db_fixtures::set_instance_head(&mut conn, instance, head).await;
        let latest = Version(
            BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)),
            VersionKind::Last,
        );

        assert_eq!(
            check_current(&mut conn, instance, Some(&latest))
                .await
                .unwrap(),
            Some(head)
        );
    }

    #[tokio::test]
    async fn test_instance_without_head_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = curve_instance(&mut conn).await;

        assert_eq!(
            check_current(&mut conn, instance, Some(&at(day(1))))
                .await
                .unwrap(),
            Some(day(1))
        );
    }
}
