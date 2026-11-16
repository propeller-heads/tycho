//! Catch-ups of the entity cache from the database.
//!
//! The cache gets every block of a system that this process folds. It misses the blocks of a
//! system whose extractor does not run here: while a follower waits for the leader, and between
//! two builds of an extractor. A catch-up reads what the database committed since the last read
//! and writes it into the cache ([`EntityCache::apply_catch_up`]), and returns the cursor of the
//! system's extractor from the same read, so the extractor resumes right after it.

use std::{sync::Arc, time::Instant};

use metrics::{counter, histogram};
use tracing::info;
use tycho_common::{
    models::Chain,
    storage::{CommitWatermark, ExtractorCursor, StateSnapshotGateway, StorageError},
};

use super::cache::EntityCache;

/// Brings an [`EntityCache`] up to date with the database, one protocol system at a time.
pub struct CacheCatchUp {
    cache: Arc<EntityCache>,
    gateway: Arc<dyn StateSnapshotGateway + Send + Sync>,
    chain: Chain,
    /// The watermark of the newest database read, of the startup snapshot or of a catch-up. Every
    /// read covers the whole chain, so one watermark serves every system: the next read takes the
    /// account rows written since it. Components need none, because a read takes all of a
    /// system's components.
    ///
    /// A catch-up holds the lock from its read until it has written the cache, so one catch-up
    /// runs at a time, and each starts from the watermark the one before it left.
    watermark: tokio::sync::Mutex<CommitWatermark>,
}

impl CacheCatchUp {
    /// Catches `cache` up from `gateway`. `watermark` is the one of the snapshot `cache` was built
    /// from.
    pub fn new(
        cache: Arc<EntityCache>,
        gateway: Arc<dyn StateSnapshotGateway + Send + Sync>,
        chain: Chain,
        watermark: CommitWatermark,
    ) -> Self {
        Self { cache, gateway, chain, watermark: tokio::sync::Mutex::new(watermark) }
    }

    /// Brings the components of `system`, and the accounts, up to date with the database, and
    /// returns the cursor of `system`'s extractor from the same read. The cache then holds every
    /// block of `system` up to that cursor, so the extractor resumes right after it. The cursor is
    /// `None` when the extractor has committed nothing.
    ///
    /// A call waits for the catch-up that runs. Run it only while `system`'s extractor is
    /// stopped: the catch-up replaces the system's components.
    ///
    /// # Errors
    ///
    /// `StorageError` when the read fails. The cache and the watermark do not change then, so the
    /// next call reads the same rows again.
    pub async fn catch_up(&self, system: &str) -> Result<Option<ExtractorCursor>, StorageError> {
        let mut watermark = self.watermark.lock().await;
        let started = Instant::now();
        let since = *watermark;
        let read = self
            .gateway
            .catch_up_read(&self.chain, since, system)
            .await?;
        let read_duration = started.elapsed();
        let cursor = read.cursors.get(system).cloned();
        *watermark = read.watermark;
        let counts = self.cache.apply_catch_up(read, system);
        let elapsed = started.elapsed();
        counter!("entity_cache_catch_ups_total").increment(1);
        histogram!("entity_cache_catch_up_duration_seconds").record(elapsed.as_secs_f64());
        info!(
            system,
            ?since,
            read = ?read_duration,
            ?elapsed,
            ?counts,
            "Entity cache caught up"
        );
        Ok(cursor)
    }
}

#[cfg(test)]
mod test {
    use std::collections::{HashMap, VecDeque};

    use tycho_common::{
        models::protocol::ProtocolComponentState,
        storage::{CatchUpRead, ComponentSnapshot, StateSnapshot, WriteTimestamp},
        Bytes,
    };

    use super::*;
    use crate::testing;

    const SYSTEM: &str = "ex";

    /// Answers `catch_up_read` with queued reads in order and records each call.
    struct QueuedGateway {
        reads: std::sync::Mutex<VecDeque<CatchUpRead>>,
        calls: std::sync::Mutex<Vec<(CommitWatermark, String)>>,
    }

    impl QueuedGateway {
        fn new(reads: Vec<CatchUpRead>) -> Arc<Self> {
            Arc::new(Self {
                reads: std::sync::Mutex::new(reads.into()),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(CommitWatermark, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl StateSnapshotGateway for QueuedGateway {
        async fn state_snapshot(&self, _chain: &Chain) -> Result<StateSnapshot, StorageError> {
            Err(StorageError::Unexpected("not used".to_string()))
        }

        async fn catch_up_read(
            &self,
            _chain: &Chain,
            since: CommitWatermark,
            system: &str,
        ) -> Result<CatchUpRead, StorageError> {
            self.calls
                .lock()
                .unwrap()
                .push((since, system.to_string()));
            self.reads
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| StorageError::Unexpected("no read queued".to_string()))
        }
    }

    fn component(id: &str, block: u64) -> ComponentSnapshot {
        ComponentSnapshot {
            system: SYSTEM.to_string(),
            state: ProtocolComponentState::new(
                id,
                HashMap::from([("x".to_string(), Bytes::from(block))]),
                HashMap::new(),
            ),
            updated_at: WriteTimestamp::from(&testing::block(block)),
        }
    }

    fn read(watermark: u64) -> CatchUpRead {
        CatchUpRead {
            accounts: vec![],
            changes: HashMap::new(),
            components: vec![],
            cursors: HashMap::new(),
            watermark: CommitWatermark::new(watermark),
        }
    }

    fn cursor(block: u64) -> ExtractorCursor {
        ExtractorCursor {
            cursor: format!("cursor@{block}").into_bytes(),
            block: testing::block(block),
        }
    }

    /// A catch-up of a cache built from a snapshot at watermark 100 that holds component `c1`.
    fn catch_up_from(gateway: Arc<QueuedGateway>) -> (Arc<EntityCache>, CacheCatchUp) {
        let cache = Arc::new(EntityCache::from_snapshot(StateSnapshot {
            accounts: vec![],
            components: vec![component("c1", 5)],
            cursors: HashMap::new(),
            watermark: CommitWatermark::new(100),
        }));
        let catch_up =
            CacheCatchUp::new(cache.clone(), gateway, Chain::Ethereum, CommitWatermark::new(100));
        (cache, catch_up)
    }

    fn holds(cache: &EntityCache, id: &str) -> bool {
        cache
            .read()
            .component(SYSTEM, id)
            .is_some()
    }

    #[tokio::test]
    async fn catch_up_writes_the_read_and_returns_the_cursor_of_the_system() {
        let mut caught_up = read(200);
        caught_up.components = vec![component("c2", 8)];
        caught_up.cursors =
            HashMap::from([(SYSTEM.to_string(), cursor(9)), ("other".to_string(), cursor(3))]);
        let gateway = QueuedGateway::new(vec![caught_up]);
        let (cache, catch_up) = catch_up_from(gateway.clone());

        let resume = catch_up.catch_up(SYSTEM).await.unwrap();

        assert_eq!(resume, Some(cursor(9)));
        assert_eq!(gateway.calls(), vec![(CommitWatermark::new(100), SYSTEM.to_string())]);
        assert!(!holds(&cache, "c1"));
        assert!(holds(&cache, "c2"));
    }

    #[tokio::test]
    async fn the_next_catch_up_reads_from_the_watermark_of_the_last_one() {
        let gateway = QueuedGateway::new(vec![read(200), read(300)]);
        let (_, catch_up) = catch_up_from(gateway.clone());

        catch_up.catch_up("a").await.unwrap();
        catch_up.catch_up("b").await.unwrap();

        assert_eq!(
            gateway.calls(),
            vec![
                (CommitWatermark::new(100), "a".to_string()),
                (CommitWatermark::new(200), "b".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_read_leaves_the_cache_and_the_watermark_unchanged() {
        let gateway = QueuedGateway::new(vec![]);
        let (cache, catch_up) = catch_up_from(gateway.clone());

        let err = catch_up
            .catch_up(SYSTEM)
            .await
            .unwrap_err();
        assert!(catch_up.catch_up(SYSTEM).await.is_err());

        assert_eq!(err, StorageError::Unexpected("no read queued".to_string()));
        assert!(holds(&cache, "c1"));
        assert_eq!(
            gateway.calls(),
            vec![
                (CommitWatermark::new(100), SYSTEM.to_string()),
                (CommitWatermark::new(100), SYSTEM.to_string()),
            ]
        );
    }
}
