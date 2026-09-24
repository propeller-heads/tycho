//! Builds the [`EntityCache`] from a state snapshot stream.

// Wired into startup in a follow-up commit.
#![allow(dead_code)]

use std::collections::HashMap;

use thiserror::Error;
use tokio::sync::mpsc;
use tycho_common::{
    models::{Address, ComponentId, ProtocolSystem},
    storage::{
        AccountSnapshot, ComponentSnapshot, CursorSnapshot, SnapshotChunk, SnapshotTotals,
        StorageError,
    },
};

use super::cache::{AccountWriteTimestamps, CachedAccount, CachedComponentState, EntityCache};

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("Snapshot read failed: {0}")]
    Storage(#[from] StorageError),
    #[error("Snapshot ended before totals and cursors arrived")]
    Incomplete,
}

impl EntityCache {
    /// Builds the cache from one snapshot stream.
    ///
    /// Consumes every chunk, turns each row into a cache entry stamped with the block that wrote
    /// it, and returns the cache once the `Cursors` chunk arrived. `extractors` names the
    /// configured extractors whose cursors are reported.
    ///
    /// # Errors
    ///
    /// `LoadError::Storage` on the first `Err` chunk; `LoadError::Incomplete` when the stream ends
    /// without a `Totals` or a `Cursors` chunk. No cache exists after an error.
    pub(crate) async fn from_chunks(
        mut snapshot: mpsc::Receiver<Result<SnapshotChunk, StorageError>>,
        _extractors: &[String],
    ) -> Result<Self, LoadError> {
        let mut totals: Option<SnapshotTotals> = None;
        let mut cursors: Option<Vec<CursorSnapshot>> = None;
        let mut accounts: HashMap<Address, CachedAccount> = HashMap::new();
        let mut components: HashMap<ProtocolSystem, HashMap<ComponentId, CachedComponentState>> =
            HashMap::new();
        while let Some(chunk) = snapshot.recv().await {
            match chunk? {
                SnapshotChunk::Totals(t) => totals = Some(t),
                SnapshotChunk::Accounts(chunk) => {
                    for row in chunk {
                        let address = row.account.address.clone();
                        accounts.insert(address, account_entry(row));
                    }
                }
                SnapshotChunk::Components(chunk) => {
                    for row in chunk {
                        let id = row.state.component_id.clone();
                        components
                            .entry(row.system.clone())
                            .or_default()
                            .insert(id, component_entry(row));
                    }
                }
                SnapshotChunk::Cursors(c) => cursors = Some(c),
            }
        }
        let (Some(_totals), Some(_cursors)) = (totals, cursors) else {
            return Err(LoadError::Incomplete);
        };
        Ok(Self::from_snapshot(accounts, components))
    }
}

fn account_entry(row: AccountSnapshot) -> CachedAccount {
    let timestamps = AccountWriteTimestamps {
        slots: row.slot_written_at,
        native_balance: row.native_balance_written_at,
        code: row.code_written_at,
        token_balances: row.token_balance_written_at,
    };
    CachedAccount::from_snapshot(row.account, timestamps)
}

fn component_entry(row: ComponentSnapshot) -> CachedComponentState {
    CachedComponentState::from_snapshot(row.state, row.updated_at)
}

#[cfg(test)]
mod test {
    use tycho_common::{
        models::{contract::Account, protocol::ProtocolComponentState, Chain},
        storage::WriteTimestamp,
        Bytes,
    };

    use super::*;
    use crate::{
        services::state::window::FoldSink,
        testing::{self, aggregated_changes, with_state_delta},
    };

    const EXTRACTOR: &str = "ex";

    fn snapshot(
        chunks: Vec<Result<SnapshotChunk, StorageError>>,
    ) -> mpsc::Receiver<Result<SnapshotChunk, StorageError>> {
        let (tx, rx) = mpsc::channel(chunks.len().max(1));
        for chunk in chunks {
            tx.try_send(chunk).unwrap();
        }
        rx
    }

    fn totals(
        accounts: u64,
        slots: u64,
        components: u64,
        attributes: u64,
    ) -> Result<SnapshotChunk, StorageError> {
        Ok(SnapshotChunk::Totals(SnapshotTotals { accounts, slots, components, attributes }))
    }

    fn cursors() -> Result<SnapshotChunk, StorageError> {
        Ok(SnapshotChunk::Cursors(vec![CursorSnapshot {
            extractor: EXTRACTOR.to_string(),
            cursor: b"c".to_vec(),
            block_hash: Bytes::zero(32),
            block_number: 5,
        }]))
    }

    fn addr(n: u64) -> Bytes {
        Bytes::from(n).lpad(20, 0)
    }

    fn account_snapshot(block: u64) -> AccountSnapshot {
        let at = WriteTimestamp::from(&testing::block(block));
        let code = Bytes::from("0x6000");
        let slot = Bytes::from(1u64).lpad(32, 0);
        let account = Account::new(
            Chain::Ethereum,
            addr(1),
            "a".to_string(),
            HashMap::from([(slot.clone(), Bytes::from(1u64).lpad(32, 0))]),
            Bytes::from(10u64),
            HashMap::new(),
            code.clone(),
            tycho_common::keccak256(&code).into(),
            Bytes::zero(32),
            Bytes::from("0x02"),
            None,
        );
        AccountSnapshot {
            account,
            slot_written_at: HashMap::from([(slot, at)]),
            native_balance_written_at: at,
            code_written_at: at,
            token_balance_written_at: HashMap::new(),
        }
    }

    fn component_snapshot(block: u64) -> ComponentSnapshot {
        ComponentSnapshot {
            system: EXTRACTOR.to_string(),
            state: ProtocolComponentState::new(
                "c1",
                HashMap::from([("x".to_string(), Bytes::from(1u64))]),
                HashMap::new(),
            ),
            updated_at: WriteTimestamp::from(&testing::block(block)),
        }
    }

    #[tokio::test]
    async fn load_builds_entries_that_read_back() {
        let rx = snapshot(vec![
            totals(1, 1, 1, 1),
            Ok(SnapshotChunk::Accounts(vec![account_snapshot(5)])),
            Ok(SnapshotChunk::Components(vec![component_snapshot(5)])),
            cursors(),
        ]);

        let cache = EntityCache::from_chunks(rx, &[EXTRACTOR.to_string()])
            .await
            .unwrap();

        let state = cache.read();
        assert_eq!(
            state
                .account(&addr(1))
                .map(Account::from),
            Some(account_snapshot(5).account)
        );
        assert_eq!(
            state
                .component(EXTRACTOR, "c1")
                .map(ProtocolComponentState::from),
            Some(component_snapshot(5).state)
        );
    }

    #[tokio::test]
    async fn load_stamps_entries_with_the_row_block() {
        let rx = snapshot(vec![
            totals(0, 0, 1, 1),
            Ok(SnapshotChunk::Components(vec![component_snapshot(5)])),
            cursors(),
        ]);
        let cache = EntityCache::from_chunks(rx, &[EXTRACTOR.to_string()])
            .await
            .unwrap();
        let x = |cache: &EntityCache| {
            cache
                .read()
                .component(EXTRACTOR, "c1")
                .map(|c| ProtocolComponentState::from(c).attributes["x"].clone())
        };

        cache
            .fold(&with_state_delta(aggregated_changes(EXTRACTOR, 5, 5, Some(5)), "c1", 7))
            .unwrap();
        assert_eq!(x(&cache), Some(Bytes::from(1u64)), "block 5 is the row's own block");

        let mut same_second =
            with_state_delta(aggregated_changes(EXTRACTOR, 6, 6, Some(6)), "c1", 8);
        same_second.block.ts = testing::block(5).ts;
        cache.fold(&same_second).unwrap();
        assert_eq!(x(&cache), Some(Bytes::from(8u64)), "block 6 at the same second is newer");
    }

    #[tokio::test]
    async fn load_fails_on_an_error_chunk() {
        let rx =
            snapshot(vec![totals(0, 0, 0, 0), Err(StorageError::Unexpected("boom".to_string()))]);

        let err = EntityCache::from_chunks(rx, &[])
            .await
            .err()
            .expect("the load must fail");

        assert!(matches!(err, LoadError::Storage(_)), "{err}");
    }

    #[tokio::test]
    async fn load_fails_when_the_stream_ends_before_the_cursors() {
        let rx = snapshot(vec![totals(0, 0, 0, 0)]);

        let err = EntityCache::from_chunks(rx, &[])
            .await
            .err()
            .expect("the load must fail");

        assert!(matches!(err, LoadError::Incomplete), "{err}");
    }
}
