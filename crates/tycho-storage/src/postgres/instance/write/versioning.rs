//! Versions of the versioned entities of an instance: protocol state, component balances, contract
//! code, contract storage and native balances.
//!
//! The current table of an entity holds one row per key: its latest version. A change that replaces
//! that row moves the replaced version to the entity's history table, with `valid_to` set to the
//! start of the change. A deletion moves the row to history and removes it from the current table.
//! A replaced version that ends at or before the retention horizon is not archived.
//!
//! Each block of an instance starts strictly later than the one before, and a transaction changes
//! a key at most once. So every change of a key starts after the version it replaces. A change that
//! doesn't fails the batch: it is never skipped.
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
#[async_trait]
pub trait VersionedRow: Debug + Send + Sync + Sized {
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
pub enum VersionedChange<R: VersionedRow> {
    /// A new version of the row's key. It starts at the row's `valid_from` and `tx_index`.
    Update(R),
    /// The end of the latest version of `key`, at `origin`.
    Delete { key: R::Key, origin: ChangeOrigin },
}

impl<R: VersionedRow> VersionedChange<R> {
    fn key(&self) -> R::Key {
        match self {
            Self::Update(row) => row.key(),
            Self::Delete { key, .. } => key.clone(),
        }
    }

    /// Start of the change: its `valid_from` and `tx_index`.
    fn version(&self) -> (NaiveDateTime, i32) {
        match self {
            Self::Update(row) => row.version(),
            Self::Delete { origin, .. } => (origin.valid_from, origin.tx_index),
        }
    }
}

/// The writes that apply the changes of one batch to one versioned entity.
#[derive(Debug)]
struct VersionWrites<R: VersionedRow> {
    /// The latest version of each changed key that still exists.
    current: Vec<R>,
    /// The changed keys that no longer exist.
    deleted: Vec<R::Key>,
    /// The versions that the changes replaced or deleted, inside the retention period.
    history: Vec<R::History>,
}

/// Chains `changes` after the `stored` current rows of their keys, in the order they start.
///
/// Fails with [`StorageError::Unexpected`] when an updated row is not of `instance` or a change
/// does not start after the version it replaces, and with [`StorageError::NotFound`] when a
/// deletion finds no version of its key.
fn plan_writes<R: VersionedRow>(
    instance: InstanceId,
    stored: Vec<R>,
    mut changes: Vec<VersionedChange<R>>,
    retention_horizon: NaiveDateTime,
) -> Result<VersionWrites<R>, StorageError> {
    changes.sort_by_key(VersionedChange::version);

    // The start of the last version of each key, and its latest row: `None` once deleted.
    let mut latest: BTreeMap<_, _> = stored
        .into_iter()
        .map(|row| (row.key(), (row.version(), Some(row))))
        .collect();

    let mut history = Vec::new();
    for change in changes {
        let key = change.key();
        let version = change.version();
        let replaced = match latest.remove(&key) {
            Some((last, _)) if version <= last => {
                return Err(StorageError::Unexpected(format!(
                    "{} {key:?}: a change at {version:?} does not start after the version it \
                     replaces, at {last:?}",
                    R::ENTITY
                )));
            }
            Some((_, replaced)) => replaced,
            None => None,
        };
        let next = match change {
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
            VersionedChange::Delete { .. } if replaced.is_none() => {
                return Err(StorageError::NotFound(R::ENTITY.to_owned(), format!("{key:?}")));
            }
            VersionedChange::Delete { .. } => None,
        };
        if let Some(replaced) = replaced {
            let (valid_from, _) = version;
            if valid_from > retention_horizon {
                history.push(replaced.into_history(valid_from));
            }
        }
        latest.insert(key, (version, next));
    }

    let mut current = Vec::new();
    let mut deleted = Vec::new();
    for (key, (_, row)) in latest {
        match row {
            Some(row) => current.push(row),
            None => deleted.push(key),
        }
    }
    Ok(VersionWrites { current, deleted, history })
}

impl PostgresGatewayNew {
    /// Applies `changes` of `instance` to the current and history tables of the entity `R`.
    ///
    /// The changes can come in any order: they apply in the order they start. Fails when an
    /// updated row is not of `instance`, when a change does not start after the version it
    /// replaces, when a deletion finds no version of its key, and when the history day of a
    /// replaced version does not exist. On a failure, some rows can already be written; the
    /// caller rolls back its transaction.
    pub(crate) async fn write_versions<R: VersionedRow>(
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
    }

    #[derive(Debug, PartialEq)]
    struct HistoryRow {
        row: Row,
        valid_to: NaiveDateTime,
    }

    #[async_trait]
    impl VersionedRow for Row {
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
        Row { key, value, previous: None, valid_from: ts(block), tx_index }
    }

    fn update(key: u8, value: u8, block: u32, tx_index: i32) -> VersionedChange<Row> {
        VersionedChange::Update(row(key, value, block, tx_index))
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
    fn test_change_at_the_stored_version_fails() {
        let result = plan(vec![row(1, 10, 2, 0)], vec![update(1, 20, 2, 0)]);

        assert!(matches!(result, Err(StorageError::Unexpected(_))));
    }

    #[test]
    fn test_change_before_the_stored_version_fails() {
        let result = plan(vec![row(1, 10, 2, 4)], vec![update(1, 20, 2, 3)]);

        assert!(matches!(result, Err(StorageError::Unexpected(_))));
    }

    #[test]
    fn test_two_changes_of_one_key_in_one_transaction_fail() {
        let result = plan(vec![], vec![update(1, 20, 2, 0), update(1, 21, 2, 0)]);

        assert!(matches!(result, Err(StorageError::Unexpected(_))));
    }

    #[test]
    fn test_deletion_of_a_key_without_version_fails() {
        let result = plan(vec![], vec![delete(1, 2, 0)]);

        assert!(matches!(result, Err(StorageError::NotFound(..))));
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
