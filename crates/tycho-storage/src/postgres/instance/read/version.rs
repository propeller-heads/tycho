use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    hash::Hash,
};

use chrono::NaiveDateTime;
use diesel_async::AsyncPgConnection;
use metrics::counter;
use tycho_common::storage::{
    BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind,
};

use crate::postgres::{
    instance::{active::ActiveInstance, InstanceId},
    maybe_lookup_version_ts,
};

/// Which rows answer a read at a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadAt {
    /// The current tables answer: every current row is valid at this version, so the read takes
    /// them all, unfiltered. No current row is newer than the head, because the writer saves the
    /// extraction state in the same transaction as the rows. `None` for the latest version, else
    /// the timestamp of the version.
    Current(Option<NaiveDateTime>),
    /// A version before the head: current rows with `valid_from <= ts`, and history rows with
    /// `valid_from <= ts < valid_to` and `valid_to <= head`.
    ///
    /// When the head and the rows come from one snapshot, `valid_to <= head` removes no row: it
    /// lets Postgres skip the history days that pg_partman makes in advance. Read the head in the
    /// same snapshot as the rows. Outside one snapshot, the filter drops a key that the writer
    /// moves between the head lookup and the row queries.
    Past { ts: NaiveDateTime, head: NaiveDateTime },
}

impl ReadAt {
    /// The `valid_from` bound of the current rows: `None` for `Current`, which takes every current
    /// row, and `ts` for `Past`.
    pub(super) fn current_until(self) -> Option<NaiveDateTime> {
        match self {
            ReadAt::Current(_) => None,
            ReadAt::Past { ts, .. } => Some(ts),
        }
    }
}

/// Resolves the version `at` of `instance` to the rows that answer it. The head, the base and the
/// history horizon are the ones the read looked up with the instance.
///
/// In this order:
/// - `None` or the `Latest` block with kind `Last`: `Current(None)`. Reads nothing.
/// - a timestamp at or after the head, or any timestamp when the instance has no head:
///   `Current(Some(ts))`.
/// - a timestamp before the history horizon: `Unsupported`, because older history is dropped.
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
    if ts < instance.history_horizon {
        return Err(StorageError::Unsupported(format!(
            "version {ts} is before the history horizon {} (now() - partition_retention()): older \
             history is dropped",
            instance.history_horizon
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

/// Collects `rows` into a map by outer key, then by inner key. `Unexpected` if two rows have the
/// same outer and inner key: two rows of `entity` are then valid at one version, which the writer
/// must prevent. The error names the `instance` and the version of `read`, and
/// `storage_overlapping_versions{entity}` counts it.
pub(super) fn unique_by_key_nested<O, I, V>(
    entity: &'static str,
    instance: InstanceId,
    read: ReadAt,
    rows: impl IntoIterator<Item = ((O, I), V)>,
) -> Result<HashMap<O, HashMap<I, V>>, StorageError>
where
    O: Eq + Hash + Debug,
    I: Eq + Hash + Debug,
{
    rows.into_iter().try_fold(
        HashMap::new(),
        |mut map: HashMap<O, HashMap<I, V>>, ((outer, inner), value)| {
            match map.entry(outer) {
                Entry::Vacant(e) => {
                    e.insert(HashMap::from([(inner, value)]));
                }
                Entry::Occupied(mut e) if !e.get().contains_key(&inner) => {
                    e.get_mut().insert(inner, value);
                }
                Entry::Occupied(e) => {
                    return Err(overlap(entity, instance, read, (e.key(), &inner)));
                }
            }
            Ok(map)
        },
    )
}

/// Collects `rows` into a map by key. `Unexpected` if two rows have the same key, as for
/// [`unique_by_key_nested`].
pub(super) fn unique_by_id<K: Eq + Hash + Debug, V>(
    entity: &'static str,
    instance: InstanceId,
    read: ReadAt,
    rows: impl IntoIterator<Item = (K, V)>,
) -> Result<HashMap<K, V>, StorageError> {
    rows.into_iter()
        .try_fold(HashMap::new(), |mut map, (key, value)| match map.entry(key) {
            Entry::Vacant(e) => {
                e.insert(value);
                Ok(map)
            }
            Entry::Occupied(e) => Err(overlap(entity, instance, read, e.key())),
        })
}

/// The error for two rows of `key` of `entity` valid at the version of `read`. Counts it in
/// `storage_overlapping_versions{entity}`.
fn overlap(
    entity: &'static str,
    instance: InstanceId,
    read: ReadAt,
    key: impl Debug,
) -> StorageError {
    counter!("storage_overlapping_versions", "entity" => entity).increment(1);
    let version = match read {
        ReadAt::Current(None) => "the latest version".to_string(),
        ReadAt::Current(Some(ts)) | ReadAt::Past { ts, .. } => format!("version {ts}"),
    };
    StorageError::Unexpected(format!(
        "{entity} of instance {instance} at {version}: two rows of {key:?} are valid"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use rstest::rstest;
    use tycho_common::{
        models::Chain,
        storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
    };

    use super::{resolve_version, unique_by_id, unique_by_key_nested, ReadAt};
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

    /// An instance with its history horizon at `day(2)`.
    fn instance(head: Option<NaiveDateTime>, base: Option<NaiveDateTime>) -> ActiveInstance {
        ActiveInstance { id: InstanceId(1), head, base, history_horizon: day(2) }
    }

    fn latest() -> Version {
        Version(
            BlockOrTimestamp::Block(BlockIdentifier::Latest(Chain::Ethereum)),
            VersionKind::Last,
        )
    }

    #[rstest]
    #[case::no_version(Some(day(5)), None, None, Ok(ReadAt::Current(None)))]
    #[case::latest(Some(day(5)), None, Some(latest()), Ok(ReadAt::Current(None)))]
    #[case::at_head(Some(day(5)), None, Some(at(day(5))), Ok(ReadAt::Current(Some(day(5)))))]
    #[case::after_head(Some(day(5)), None, Some(at(day(6))), Ok(ReadAt::Current(Some(day(6)))))]
    #[case::without_head(None, None, Some(at(day(1))), Ok(ReadAt::Current(Some(day(1)))))]
    #[case::before_head(Some(day(5)), None, Some(at(day(3))), Ok(ReadAt::Past { ts: day(3), head: day(5) }))]
    #[case::at_horizon(Some(day(5)), None, Some(at(day(2))), Ok(ReadAt::Past { ts: day(2), head: day(5) }))]
    #[case::at_base(Some(day(5)), Some(day(3)), Some(at(day(3))), Ok(ReadAt::Past { ts: day(3), head: day(5) }))]
    #[case::before_horizon(Some(day(5)), None, Some(at(day(1))), Err("history horizon"))]
    #[case::before_base(Some(day(5)), Some(day(4)), Some(at(day(3))), Err("base block"))]
    #[tokio::test]
    async fn test_resolve_version(
        #[case] head: Option<NaiveDateTime>,
        #[case] base: Option<NaiveDateTime>,
        #[case] at: Option<Version>,
        #[case] expected: Result<ReadAt, &str>,
    ) {
        let mut conn = instance_test_connection().await;

        let result = resolve_version(&instance(head, base), at.as_ref(), &mut conn).await;

        match (result, expected) {
            (Ok(read), Ok(expected)) => assert_eq!(read, expected),
            (Err(StorageError::Unsupported(msg)), Err(part)) => {
                assert!(msg.contains(part), "{msg}")
            }
            (result, expected) => panic!("expected {expected:?}, got {result:?}"),
        }
    }

    #[test]
    fn test_unique_by_key_nested_collects_rows() {
        let map = unique_by_key_nested(
            "protocol_state",
            InstanceId(7),
            ReadAt::Current(None),
            [((1, "x"), "a"), ((1, "y"), "b"), ((2, "x"), "c")],
        )
        .unwrap();

        assert_eq!(
            map,
            HashMap::from([
                (1, HashMap::from([("x", "a"), ("y", "b")])),
                (2, HashMap::from([("x", "c")])),
            ])
        );
    }

    #[rstest]
    #[case::latest(ReadAt::Current(None), "the latest version")]
    #[case::current_at(ReadAt::Current(Some(day(6))), "version 2020-01-06 00:00:00")]
    #[case::past(ReadAt::Past { ts: day(3), head: day(5) }, "version 2020-01-03 00:00:00")]
    fn test_unique_by_key_nested_fails_on_a_repeated_key(
        #[case] read: ReadAt,
        #[case] version: &str,
    ) {
        let result = unique_by_key_nested(
            "protocol_state",
            InstanceId(7),
            read,
            [((42, "x"), "a"), ((42, "x"), "b")],
        );

        let Err(StorageError::Unexpected(msg)) = result else {
            panic!("expected Unexpected, got {result:?}");
        };
        assert_eq!(
            msg,
            format!("protocol_state of instance 7 at {version}: two rows of (42, \"x\") are valid")
        );
    }

    #[test]
    fn test_unique_by_id_collects_rows() {
        let map = unique_by_id(
            "contract_code",
            InstanceId(7),
            ReadAt::Current(None),
            [(1, "a"), (2, "b")],
        )
        .unwrap();

        assert_eq!(map, HashMap::from([(1, "a"), (2, "b")]));
    }

    #[test]
    fn test_unique_by_id_fails_on_a_repeated_key() {
        let read = ReadAt::Past { ts: day(3), head: day(5) };
        let result = unique_by_id("contract_code", InstanceId(7), read, [(42, "a"), (42, "b")]);

        let Err(StorageError::Unexpected(msg)) = result else {
            panic!("expected Unexpected, got {result:?}");
        };
        assert_eq!(
            msg,
            "contract_code of instance 7 at version 2020-01-03 00:00:00: two rows of 42 are valid"
        );
    }
}
