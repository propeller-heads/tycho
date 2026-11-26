//! Versions of the rows of an instance's versioned entities.
//!
//! The current table of an entity holds one row per key: its latest version. When a change
//! replaces that row, `write_versions` moves the replaced version to the entity's history table,
//! with `valid_to` set to the start of the change. When a change deletes a key, `write_versions`
//! moves its row to history and removes it from the current table. `write_versions` does not
//! archive a version that ends at or before the retention horizon.
//!
//! Each block of an instance starts strictly later than the one before, and a transaction changes
//! a key at most once. So every change of a key must start after the version it replaces. When a
//! change starts at or before that version, `write_versions` fails the whole batch.
//!
//! An entity can say that a change only repeats the version it replaces
//! ([`VersionedRowNew::repeats`]). A repeat adds no version: the replaced version stays the latest
//! one and does not move to history, and a key with only repeats is not written again. A repeat
//! still counts for the order: the next change of its key must start after the repeat.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel_async::AsyncPgConnection;
use tycho_common::storage::StorageError;

use crate::postgres::instance::{gateway::PostgresGatewayNew, origins::ChangeOrigin, InstanceId};

/// A row of the current table of a versioned entity.
///
/// A working name while both versioning traits exist: the cut-over renames it to `VersionedRow`
/// and deletes the trait on the old tables.
#[async_trait]
pub trait VersionedRowNew: Debug + Send + Sync + Sized {
    /// Name of the entity, for error messages.
    const ENTITY: &'static str;

    /// Key of the row inside its instance.
    type Key: Clone + Ord + Debug + Send + Sync;

    /// Row of the entity's history table.
    type History: Send + Sync;

    fn key(&self) -> Self::Key;

    /// Instance of the row.
    fn instance(&self) -> InstanceId;

    /// Start of the version: its `valid_from` and `tx_index`.
    fn version(&self) -> (NaiveDateTime, i32);

    /// Records on `next` what it keeps of the version it replaces, such as the previous value.
    fn link_next(&self, next: &mut Self);

    /// Whether this row only repeats `replaced`, the version it would replace. A repeat adds no
    /// version: `replaced` stays the latest version of the key.
    ///
    /// No entity repeats by default: every change is a new version.
    fn repeats(&self, _replaced: &Self) -> bool {
        false
    }

    /// The history row of this version, ended at `valid_to`.
    fn into_history(self, valid_to: NaiveDateTime) -> Self::History;

    /// Loads the current rows of `instance` with one of `keys`. A key with no row is left out.
    async fn load_current(
        instance: InstanceId,
        keys: &[Self::Key],
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError>;

    /// Inserts `rows` into the current table, replacing the rows with the same key.
    async fn upsert_current(
        rows: &[Self],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError>;

    /// Removes the current rows of `instance` with one of `keys`.
    async fn delete_current(
        instance: InstanceId,
        keys: &[Self::Key],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError>;

    /// Inserts `rows` into the history table.
    async fn insert_history(
        rows: &[Self::History],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError>;
}

/// A change to one key of a versioned entity.
#[derive(Debug)]
pub enum VersionedChange<R: VersionedRowNew> {
    /// A new version of the row's key. It starts at the row's `valid_from` and `tx_index`.
    Update(R),
    /// The end of the latest version of `key`, at `origin`.
    #[cfg_attr(not(test), expect(dead_code, reason = "no instance write op deletes rows yet"))]
    Delete { key: R::Key, origin: ChangeOrigin },
}

impl<R: VersionedRowNew> VersionedChange<R> {
    fn key(&self) -> R::Key {
        match self {
            Self::Update(row) => row.key(),
            Self::Delete { key, origin: _ } => key.clone(),
        }
    }

    /// Start of the change: its `valid_from` and `tx_index`.
    fn version(&self) -> (NaiveDateTime, i32) {
        match self {
            Self::Update(row) => row.version(),
            Self::Delete { key: _, origin } => (origin.valid_from, origin.tx_index),
        }
    }
}

/// The writes that apply the changes of one batch to one versioned entity.
#[derive(Debug)]
struct VersionWrites<R: VersionedRowNew> {
    /// The latest version of each changed key that still exists.
    current: Vec<R>,
    /// The changed keys that no longer exist.
    deleted: Vec<R::Key>,
    /// The versions that the changes replaced or deleted, inside the retention period.
    history: Vec<R::History>,
}

/// The last change of a key in [`plan_writes`].
struct Latest<R: VersionedRowNew> {
    /// Start of the last change: a later change must start after it.
    version: (NaiveDateTime, i32),
    /// The latest row of the key: `None` once deleted.
    row: Option<R>,
    /// Whether the batch changed the stored row. A key with only repeats is not written.
    changed: bool,
}

/// Chains `changes` after the `stored` current rows of their keys, in the order they start. An
/// update that repeats the version it replaces is dropped, and a key with only repeats is not
/// written.
///
/// Fails on each failure that `write_versions` lists, except a missing history day: only the
/// history insert finds that one.
fn plan_writes<R: VersionedRowNew>(
    instance: InstanceId,
    stored: Vec<R>,
    mut changes: Vec<VersionedChange<R>>,
    retention_horizon: NaiveDateTime,
) -> Result<VersionWrites<R>, StorageError> {
    changes.sort_by_key(VersionedChange::version);

    let mut latest: BTreeMap<_, _> = stored
        .into_iter()
        .map(|row| (row.key(), Latest { version: row.version(), row: Some(row), changed: false }))
        .collect();

    let mut history = Vec::new();
    for change in changes {
        let key = change.key();
        let version = change.version();
        let (replaced, changed) = match latest.remove(&key) {
            Some(last) if version <= last.version => {
                return Err(StorageError::Unexpected(format!(
                    "{} {key:?}: a change at {version:?} does not start after the version it \
                     replaces, at {:?}",
                    R::ENTITY,
                    last.version
                )));
            }
            Some(last) => (last.row, last.changed),
            None => (None, false),
        };
        let next = match change {
            VersionedChange::Update(row)
                if replaced
                    .as_ref()
                    .is_some_and(|replaced| row.repeats(replaced)) =>
            {
                latest.insert(key, Latest { version, row: replaced, changed });
                continue;
            }
            VersionedChange::Update(mut row) => {
                if row.instance() != instance {
                    return Err(StorageError::Unexpected(format!(
                        "{} {key:?}: the row is of instance {}, but the batch writes instance \
                         {instance}",
                        R::ENTITY,
                        row.instance()
                    )));
                }
                if let Some(replaced) = &replaced {
                    replaced.link_next(&mut row);
                }
                Some(row)
            }
            VersionedChange::Delete { key: _, origin: _ } if replaced.is_none() => {
                return Err(StorageError::Unexpected(format!(
                    "{} {key:?}: a deletion at {version:?} finds no version to end in instance \
                     {instance}",
                    R::ENTITY
                )));
            }
            VersionedChange::Delete { key: _, origin: _ } => None,
        };
        if let Some(replaced) = replaced {
            let (valid_from, _) = version;
            if valid_from > retention_horizon {
                history.push(replaced.into_history(valid_from));
            }
        }
        latest.insert(key, Latest { version, row: next, changed: true });
    }

    let mut current = Vec::new();
    let mut deleted = Vec::new();
    for (key, last) in latest {
        match last {
            Latest { changed: false, .. } => {}
            Latest { row: Some(row), .. } => current.push(row),
            Latest { row: None, .. } => deleted.push(key),
        }
    }
    Ok(VersionWrites { current, deleted, history })
}

impl PostgresGatewayNew {
    /// Applies `changes` of `instance` to the current and history tables of the entity `R`.
    ///
    /// The changes can come in any order: they apply in the order they start. Fails with
    /// [`StorageError::Unexpected`] when an updated row is not of `instance`, when a change does
    /// not start after the version it replaces, when a deletion finds no version of its key, or
    /// when the history day of a replaced version does not exist. On a failure, this method can
    /// have written some rows already; the caller rolls back its transaction.
    pub(crate) async fn write_versions<R: VersionedRowNew>(
        &self,
        instance: InstanceId,
        changes: Vec<VersionedChange<R>>,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        if changes.is_empty() {
            return Ok(());
        }
        let keys: Vec<R::Key> = changes
            .iter()
            .map(VersionedChange::key)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let stored = R::load_current(instance, &keys, conn).await?;
        let writes = plan_writes(instance, stored, changes, self.retention_horizon)?;

        if !writes.history.is_empty() {
            R::insert_history(&writes.history, conn).await?;
        }
        if !writes.deleted.is_empty() {
            R::delete_current(instance, &writes.deleted, conn).await?;
        }
        if !writes.current.is_empty() {
            R::upsert_current(&writes.current, conn).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use rstest::rstest;

    use super::*;

    /// Instance of every test row.
    const INSTANCE: InstanceId = InstanceId(1);

    #[derive(Debug, Clone, PartialEq)]
    struct Row {
        key: u8,
        value: u8,
        previous: Option<u8>,
        valid_from: NaiveDateTime,
        tx_index: i32,
        /// Whether a change with the value of the version it replaces only repeats it.
        drops_repeats: bool,
    }

    #[derive(Debug, PartialEq)]
    struct HistoryRow {
        row: Row,
        valid_to: NaiveDateTime,
    }

    #[async_trait]
    impl VersionedRowNew for Row {
        const ENTITY: &'static str = "Row";
        type Key = u8;
        type History = HistoryRow;

        fn key(&self) -> u8 {
            self.key
        }

        fn instance(&self) -> InstanceId {
            INSTANCE
        }

        fn version(&self) -> (NaiveDateTime, i32) {
            (self.valid_from, self.tx_index)
        }

        fn link_next(&self, next: &mut Self) {
            next.previous = Some(self.value);
        }

        fn repeats(&self, replaced: &Self) -> bool {
            self.drops_repeats && self.value == replaced.value
        }

        fn into_history(self, valid_to: NaiveDateTime) -> HistoryRow {
            HistoryRow { row: self, valid_to }
        }

        async fn load_current(
            _instance: InstanceId,
            _keys: &[u8],
            _conn: &mut AsyncPgConnection,
        ) -> Result<Vec<Self>, StorageError> {
            Err(StorageError::Unsupported("the planning tests use no database".to_owned()))
        }

        async fn upsert_current(
            _rows: &[Self],
            _conn: &mut AsyncPgConnection,
        ) -> Result<(), StorageError> {
            Err(StorageError::Unsupported("the planning tests use no database".to_owned()))
        }

        async fn delete_current(
            _instance: InstanceId,
            _keys: &[u8],
            _conn: &mut AsyncPgConnection,
        ) -> Result<(), StorageError> {
            Err(StorageError::Unsupported("the planning tests use no database".to_owned()))
        }

        async fn insert_history(
            _rows: &[HistoryRow],
            _conn: &mut AsyncPgConnection,
        ) -> Result<(), StorageError> {
            Err(StorageError::Unsupported("the planning tests use no database".to_owned()))
        }
    }

    fn ts(second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 18)
            .and_then(|date| date.and_hms_opt(12, 0, second))
            .expect("a valid timestamp")
    }

    /// A change in block `block`, which starts at second `block`.
    fn origin(block: u32, tx_index: i32) -> ChangeOrigin {
        ChangeOrigin { block_number: block.into(), tx_index, valid_from: ts(block) }
    }

    fn row(key: u8, value: u8, block: u32, tx_index: i32) -> Row {
        Row { key, value, previous: None, valid_from: ts(block), tx_index, drops_repeats: false }
    }

    fn update(key: u8, value: u8, block: u32, tx_index: i32) -> VersionedChange<Row> {
        VersionedChange::Update(row(key, value, block, tx_index))
    }

    /// An update of an entity that drops a change with the value of the version it replaces.
    fn update_dropping_repeats(
        key: u8,
        value: u8,
        block: u32,
        tx_index: i32,
    ) -> VersionedChange<Row> {
        VersionedChange::Update(Row { drops_repeats: true, ..row(key, value, block, tx_index) })
    }

    fn delete(key: u8, block: u32, tx_index: i32) -> VersionedChange<Row> {
        VersionedChange::Delete { key, origin: origin(block, tx_index) }
    }

    fn archived(row: Row, valid_to_block: u32) -> HistoryRow {
        HistoryRow { row, valid_to: ts(valid_to_block) }
    }

    fn plan(
        stored: Vec<Row>,
        changes: Vec<VersionedChange<Row>>,
    ) -> Result<VersionWrites<Row>, StorageError> {
        plan_writes(INSTANCE, stored, changes, NaiveDateTime::default())
    }

    #[test]
    fn test_new_key_becomes_current_without_history() {
        let writes = plan(vec![], vec![update(1, 10, 2, 0)]).unwrap();

        assert_eq!(writes.current, vec![row(1, 10, 2, 0)]);
        assert!(writes.deleted.is_empty());
        assert!(writes.history.is_empty());
    }

    #[test]
    fn test_update_archives_the_stored_version_until_the_change() {
        let stored = row(1, 10, 1, 3);

        let writes = plan(vec![stored.clone()], vec![update(1, 20, 2, 0)]).unwrap();

        assert_eq!(writes.current, vec![Row { previous: Some(10), ..row(1, 20, 2, 0) }]);
        assert_eq!(writes.history, vec![archived(stored, 2)]);
    }

    #[test]
    fn test_changes_chain_in_origin_order() {
        let changes = vec![update(1, 30, 3, 0), update(1, 21, 2, 5), update(1, 20, 2, 1)];

        let writes = plan(vec![], changes).unwrap();

        assert_eq!(writes.current, vec![Row { previous: Some(21), ..row(1, 30, 3, 0) }]);
        assert_eq!(
            writes.history,
            vec![
                archived(row(1, 20, 2, 1), 2),
                archived(Row { previous: Some(20), ..row(1, 21, 2, 5) }, 3),
            ]
        );
    }

    #[test]
    fn test_deletion_archives_the_stored_version_and_removes_the_key() {
        let stored = row(1, 10, 1, 0);

        let writes = plan(vec![stored.clone()], vec![delete(1, 2, 0)]).unwrap();

        assert!(writes.current.is_empty());
        assert_eq!(writes.deleted, vec![1]);
        assert_eq!(writes.history, vec![archived(stored, 2)]);
    }

    #[test]
    fn test_key_added_again_after_its_deletion_is_current_without_previous_value() {
        let stored = row(1, 10, 1, 0);

        let writes =
            plan(vec![stored.clone()], vec![delete(1, 2, 0), update(1, 30, 3, 0)]).unwrap();

        assert_eq!(writes.current, vec![row(1, 30, 3, 0)]);
        assert!(writes.deleted.is_empty());
        assert_eq!(writes.history, vec![archived(stored, 2)]);
    }

    #[test]
    fn test_version_ending_at_or_before_the_horizon_is_not_archived() {
        let stored = vec![row(1, 10, 1, 0), row(2, 10, 1, 0)];
        let changes = vec![update(1, 20, 2, 0), update(2, 20, 3, 0)];

        let writes = plan_writes(INSTANCE, stored, changes, ts(2)).unwrap();

        assert_eq!(writes.current.len(), 2);
        assert_eq!(writes.history, vec![archived(row(2, 10, 1, 0), 3)]);
    }

    #[test]
    fn test_repeat_of_the_stored_version_writes_nothing() {
        let stored = row(1, 10, 1, 0);

        let writes = plan(vec![stored], vec![update_dropping_repeats(1, 10, 2, 0)]).unwrap();

        assert!(writes.current.is_empty());
        assert!(writes.deleted.is_empty());
        assert!(writes.history.is_empty());
    }

    #[test]
    fn test_repeat_after_a_change_keeps_the_change() {
        let stored = row(1, 10, 1, 0);
        let changes =
            vec![update_dropping_repeats(1, 20, 2, 0), update_dropping_repeats(1, 20, 3, 0)];

        let writes = plan(vec![stored.clone()], changes).unwrap();

        assert_eq!(
            writes.current,
            vec![Row { previous: Some(10), drops_repeats: true, ..row(1, 20, 2, 0) }]
        );
        assert_eq!(writes.history, vec![archived(stored, 2)]);
    }

    #[test]
    fn test_repeat_in_a_batch_is_dropped_between_changes() {
        let changes = vec![
            update_dropping_repeats(1, 20, 2, 0),
            update_dropping_repeats(1, 20, 3, 0),
            update_dropping_repeats(1, 30, 4, 0),
        ];

        let writes = plan(vec![], changes).unwrap();

        let first = Row { drops_repeats: true, ..row(1, 20, 2, 0) };
        assert_eq!(
            writes.current,
            vec![Row { previous: Some(20), drops_repeats: true, ..row(1, 30, 4, 0) }]
        );
        assert_eq!(writes.history, vec![archived(first, 4)]);
    }

    #[test]
    fn test_change_at_a_dropped_repeat_fails() {
        let changes = vec![update_dropping_repeats(1, 10, 2, 0), update(1, 20, 2, 0)];

        let result = plan(vec![row(1, 10, 1, 0)], changes);

        assert!(matches!(result, Err(StorageError::Unexpected(_))));
    }

    #[test]
    fn test_same_value_is_a_new_version_by_default() {
        let stored = row(1, 10, 1, 0);

        let writes = plan(vec![stored.clone()], vec![update(1, 10, 2, 0)]).unwrap();

        assert_eq!(writes.current, vec![Row { previous: Some(10), ..row(1, 10, 2, 0) }]);
        assert_eq!(writes.history, vec![archived(stored, 2)]);
    }

    #[rstest]
    #[case::at_the_stored_version(vec![row(1, 10, 2, 0)], vec![update(1, 20, 2, 0)])]
    #[case::before_the_stored_version(vec![row(1, 10, 2, 4)], vec![update(1, 20, 2, 3)])]
    #[case::two_changes_in_one_transaction(vec![], vec![update(1, 20, 2, 0), update(1, 21, 2, 0)])]
    fn test_change_not_after_the_replaced_version_fails(
        #[case] stored: Vec<Row>,
        #[case] changes: Vec<VersionedChange<Row>>,
    ) {
        let result = plan(stored, changes);

        assert!(matches!(result, Err(StorageError::Unexpected(_))));
    }

    #[test]
    fn test_deletion_of_a_key_without_version_fails() {
        let result = plan(vec![], vec![delete(1, 2, 0)]);

        assert!(matches!(
            result,
            Err(StorageError::Unexpected(msg)) if msg.contains("finds no version to end in instance 1")
        ));
    }

    #[test]
    fn test_row_of_another_instance_fails() {
        let other_instance = InstanceId(2);

        let result = plan_writes(
            other_instance,
            vec![],
            vec![update(1, 20, 2, 0)],
            NaiveDateTime::default(),
        );

        assert!(matches!(
            result,
            Err(StorageError::Unexpected(msg)) if msg.contains("of instance 1")
        ));
    }
}
