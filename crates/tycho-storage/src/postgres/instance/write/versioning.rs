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
pub trait VersionedRow: Clone + Debug + Send + Sync + Sized {
    /// Name of the entity, for error messages.
    const ENTITY: &'static str;

    /// Key of the row inside its instance.
    type Key: Clone + Ord + Debug + Send + Sync;

    /// Row of the entity's history table.
    type History: Send + Sync;

    fn key(&self) -> Self::Key;

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

/// A change to one key of a versioned entity, at its place on the chain.
#[derive(Debug)]
pub struct VersionedChange<R: VersionedRow> {
    pub origin: ChangeOrigin,
    pub kind: ChangeKind<R>,
}

#[derive(Debug)]
pub enum ChangeKind<R: VersionedRow> {
    /// A new version of the row's key. The row starts at the change's origin.
    Update(R),
    /// The end of the latest version of the key.
    Delete(R::Key),
}

impl<R: VersionedRow> VersionedChange<R> {
    fn key(&self) -> R::Key {
        match &self.kind {
            ChangeKind::Update(row) => row.key(),
            ChangeKind::Delete(key) => key.clone(),
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

/// Chains `changes` after the `stored` current rows of their keys, in the order of their origins.
///
/// Fails with [`StorageError::Unexpected`] when a change does not start after the version it
/// replaces, and with [`StorageError::NotFound`] when a deletion finds no version of its key.
fn plan_writes<R: VersionedRow>(
    stored: Vec<R>,
    mut changes: Vec<VersionedChange<R>>,
    retention_horizon: NaiveDateTime,
) -> Result<VersionWrites<R>, StorageError> {
    changes.sort_by_key(|change| change.origin);

    let mut last_version = BTreeMap::new();
    let mut latest = BTreeMap::new();
    for row in stored {
        last_version.insert(row.key(), row.version());
        latest.insert(row.key(), row);
    }

    let mut changed = BTreeSet::new();
    let mut history = Vec::new();
    for change in changes {
        let key = change.key();
        let version = (change.origin.valid_from, change.origin.tx_index);
        if let Some(last) = last_version.insert(key.clone(), version) {
            if version <= last {
                return Err(StorageError::Unexpected(format!(
                    "{} {key:?}: a change at {version:?} does not start after the version it \
                     replaces, at {last:?}",
                    R::ENTITY
                )));
            }
        }

        let replaced = latest.remove(&key);
        match change.kind {
            ChangeKind::Update(mut row) => {
                if let Some(replaced) = &replaced {
                    replaced.link_next(&mut row);
                }
                latest.insert(key.clone(), row);
            }
            ChangeKind::Delete(_) if replaced.is_none() => {
                return Err(StorageError::NotFound(R::ENTITY.to_owned(), format!("{key:?}")));
            }
            ChangeKind::Delete(_) => {}
        }
        if let Some(replaced) = replaced {
            if change.origin.valid_from > retention_horizon {
                history.push(replaced.into_history(change.origin.valid_from));
            }
        }
        changed.insert(key);
    }

    let mut current = Vec::new();
    let mut deleted = Vec::new();
    for key in changed {
        match latest.remove(&key) {
            Some(row) => current.push(row),
            None => deleted.push(key),
        }
    }
    Ok(VersionWrites { current, deleted, history })
}

impl PostgresGatewayNew {
    /// Applies `changes` of `instance` to the current and history tables of the entity `R`.
    ///
    /// The changes can come in any order: they apply in the order of their origins. Fails when a
    /// change does not start after the version it replaces, when a deletion finds no version of
    /// its key, and when the history day of a replaced version does not exist. On a failure, some
    /// rows can already be written; the caller rolls back its transaction.
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
        let writes = plan_writes(stored, changes, self.retention_horizon)?;

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
