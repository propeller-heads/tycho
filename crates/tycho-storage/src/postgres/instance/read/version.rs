use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    hash::Hash,
};

use chrono::NaiveDateTime;
use diesel_async::AsyncPgConnection;
use tycho_common::storage::{
    BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind,
};

use crate::postgres::{instance::active::ActiveInstance, maybe_lookup_version_ts};

/// Which rows answer a read at a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadAt {
    /// The current tables answer: every current row is valid at this version. `None` for the
    /// latest version, else the timestamp of the version.
    Current(Option<NaiveDateTime>),
    /// A version before the head: current rows with `valid_from <= ts`, and history rows with
    /// `valid_from <= ts < valid_to` and `valid_to <= head`.
    Past { ts: NaiveDateTime, head: NaiveDateTime },
}

/// Resolves the version `at` of `instance` to the rows that answer it. The head, the base and the
/// retention horizon are the ones the read looked up with the instance.
///
/// In this order:
/// - `None` or the `Latest` block with kind `Last`: `Current(None)`. Reads nothing.
/// - a timestamp at or after the head, or any timestamp when the instance has no head:
///   `Current(Some(ts))`.
/// - a timestamp before the retention horizon: `Unsupported`, because older history is dropped.
/// - a timestamp before the base block of the instance: `Unsupported`, because the instance has no
///   history before it.
/// - any other timestamp before the head: `Past { ts, head }`.
///
/// A version kind other than `Last` is `Unsupported`.
pub(super) async fn resolve_version(
    instance: &ActiveInstance,
    at: Option<&Version>,
    conn: &mut AsyncPgConnection,
) -> Result<ReadAt, StorageError> {
    let Some(at) = at else {
        return Ok(ReadAt::Current(None));
    };
    if let Version(BlockOrTimestamp::Block(BlockIdentifier::Latest(_)), VersionKind::Last) = at {
        return Ok(ReadAt::Current(None));
    }
    let ts = maybe_lookup_version_ts(at, conn).await?;
    let Some(head) = instance.head.filter(|head| ts < *head) else {
        return Ok(ReadAt::Current(Some(ts)));
    };
    if ts < instance.retention_horizon {
        return Err(StorageError::Unsupported(format!(
            "version {ts} is before the history retention horizon {}: older history is dropped",
            instance.retention_horizon
        )));
    }
    if let Some(base) = instance.base.filter(|base| ts < *base) {
        return Err(StorageError::Unsupported(format!(
            "version {ts} is before the base block of instance {} at {base}: the instance has no \
             history before it",
            instance.id
        )));
    }
    Ok(ReadAt::Past { ts, head })
}

/// Collects `rows` into a map by key. `Unexpected` if two rows have the same key: two rows of
/// `entity` are then valid at one version, which the writer must prevent.
pub(super) fn unique_by_key<K: Eq + Hash + Debug, V>(
    entity: &str,
    rows: impl IntoIterator<Item = (K, V)>,
) -> Result<HashMap<K, V>, StorageError> {
    rows.into_iter()
        .try_fold(HashMap::new(), |mut map, (key, value)| match map.entry(key) {
            Entry::Occupied(e) => Err(StorageError::Unexpected(format!(
                "{entity}: two rows of {:?} are valid at one version",
                e.key()
            ))),
            Entry::Vacant(e) => {
                e.insert(value);
                Ok(map)
            }
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use tycho_common::{
        models::Chain,
        storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
    };

    use super::{resolve_version, unique_by_key, ReadAt};
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

    fn instance(
        head: Option<NaiveDateTime>,
        base: Option<NaiveDateTime>,
        retention_horizon: NaiveDateTime,
    ) -> ActiveInstance {
        ActiveInstance { id: InstanceId(1), head, base, retention_horizon }
    }

    fn resolver(head: Option<NaiveDateTime>, base: Option<NaiveDateTime>) -> ActiveInstance {
        instance(head, base, day(2))
    }

    #[tokio::test]
    async fn test_resolve_latest_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = resolver(Some(day(5)), None);
        let latest = Version(
            BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)),
            VersionKind::Last,
        );

        for at in [None, Some(&latest)] {
            assert_eq!(
                resolve_version(&instance, at, &mut conn)
                    .await
                    .unwrap(),
                ReadAt::Current(None)
            );
        }
    }

    #[tokio::test]
    async fn test_resolve_at_or_after_head_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = resolver(Some(day(5)), None);

        for ts in [day(5), day(6)] {
            assert_eq!(
                resolve_version(&instance, Some(&at(ts)), &mut conn)
                    .await
                    .unwrap(),
                ReadAt::Current(Some(ts))
            );
        }
    }

    #[tokio::test]
    async fn test_resolve_without_head_is_current() {
        let mut conn = instance_test_connection().await;
        let instance = resolver(None, None);

        assert_eq!(
            resolve_version(&instance, Some(&at(day(1))), &mut conn)
                .await
                .unwrap(),
            ReadAt::Current(Some(day(1)))
        );
    }

    #[tokio::test]
    async fn test_resolve_before_head_is_past() {
        let mut conn = instance_test_connection().await;
        let instance = resolver(Some(day(5)), None);

        assert_eq!(
            resolve_version(&instance, Some(&at(day(3))), &mut conn)
                .await
                .unwrap(),
            ReadAt::Past { ts: day(3), head: day(5) }
        );
    }

    #[tokio::test]
    async fn test_resolve_at_the_limits_is_past() {
        let mut conn = instance_test_connection().await;

        let at_horizon = resolver(Some(day(5)), None);
        assert_eq!(
            resolve_version(&at_horizon, Some(&at(day(2))), &mut conn)
                .await
                .unwrap(),
            ReadAt::Past { ts: day(2), head: day(5) }
        );

        let at_base = resolver(Some(day(5)), Some(day(3)));
        assert_eq!(
            resolve_version(&at_base, Some(&at(day(3))), &mut conn)
                .await
                .unwrap(),
            ReadAt::Past { ts: day(3), head: day(5) }
        );
    }

    #[tokio::test]
    async fn test_resolve_before_retention_is_unsupported() {
        let mut conn = instance_test_connection().await;
        let instance = resolver(Some(day(5)), None);

        let result = resolve_version(&instance, Some(&at(day(1))), &mut conn).await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("retention horizon"), "{msg}");
    }

    #[tokio::test]
    async fn test_resolve_before_base_is_unsupported() {
        let mut conn = instance_test_connection().await;
        let instance = resolver(Some(day(5)), Some(day(4)));

        let result = resolve_version(&instance, Some(&at(day(3))), &mut conn).await;

        let Err(StorageError::Unsupported(msg)) = result else {
            panic!("expected Unsupported, got {result:?}");
        };
        assert!(msg.contains("base block"), "{msg}");
    }

    #[test]
    fn test_unique_by_key_collects_rows() {
        let map = unique_by_key("protocol_state", [(1, "a"), (2, "b"), (3, "c")]).unwrap();

        assert_eq!(map, HashMap::from([(1, "a"), (2, "b"), (3, "c")]));
    }

    #[test]
    fn test_unique_by_key_fails_on_a_repeated_key() {
        let result = unique_by_key("protocol_state", [(1, "a"), (1, "b")]);

        let Err(StorageError::Unexpected(msg)) = result else {
            panic!("expected Unexpected, got {result:?}");
        };
        assert!(msg.contains("protocol_state"), "{msg}");
        assert!(msg.contains('1'), "{msg}");
    }
}
