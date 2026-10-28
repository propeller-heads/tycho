//! Tracks how far each extractor is behind the chain, so that `/protocol_systems` lists only the
//! protocol systems a client can stream.

use std::{
    collections::HashMap,
    sync::{Arc, PoisonError, RwLock},
};

use chrono::{Duration, NaiveDateTime};
use tracing::info;
use tycho_common::{
    models::Chain,
    storage::{BlockIdentifier, ChainGateway, ExtractionStateGateway, StorageError},
};

/// The time of the newest block of each protocol system's extractor, and the lag above which
/// the system counts as not fresh.
///
/// A backfilling extractor emits old blocks much faster than a client can process them, so a
/// client that streams it falls behind and drops its subscription.
#[derive(Clone)]
pub struct ProtocolFreshness {
    newest_block_ts: Arc<RwLock<HashMap<String, NaiveDateTime>>>,
    max_lag: Duration,
}

impl ProtocolFreshness {
    /// Starts with the time of each system's last committed block. A system whose extractor has
    /// committed no block yet gets no entry.
    ///
    /// # Errors
    ///
    /// Returns the storage error if an extraction state or its block cannot be read for a reason
    /// other than the state not existing.
    pub async fn load<G>(
        gateway: &G,
        chain: &Chain,
        protocol_systems: &[String],
        max_lag: Duration,
    ) -> Result<Self, StorageError>
    where
        G: ExtractionStateGateway + ChainGateway,
    {
        let mut newest_block_ts = HashMap::new();
        for system in protocol_systems {
            let state = match gateway.get_state(system, chain).await {
                Ok(state) => state,
                Err(StorageError::NotFound(..)) => {
                    info!(system, "No committed block yet, protocol system is not listed");
                    continue;
                }
                Err(err) => return Err(err),
            };
            let block = gateway
                .get_block(&BlockIdentifier::Hash(state.block_hash))
                .await?;
            newest_block_ts.insert(system.clone(), block.ts);
        }
        Ok(Self { newest_block_ts: Arc::new(RwLock::new(newest_block_ts)), max_lag })
    }

    /// Records `ts` as the time of the newest block of `system`.
    pub fn record(&self, system: &str, ts: NaiveDateTime) {
        // The map holds plain timestamps, so a writer that panicked cannot leave it inconsistent.
        let mut newest = self
            .newest_block_ts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        newest.insert(system.to_string(), ts);
    }

    /// Returns true when the newest block of `system` is at most `max_lag` older than `now`.
    /// A system with no known block is not fresh.
    pub fn is_fresh(&self, system: &str, now: NaiveDateTime) -> bool {
        let newest = self
            .newest_block_ts
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        newest
            .get(system)
            .is_some_and(|ts| now - *ts <= self.max_lag)
    }
}

#[cfg(test)]
mod test {
    use chrono::DateTime;
    use tycho_common::{
        models::{blockchain::Block, ExtractionState},
        Bytes,
    };

    use super::*;
    use crate::testing::MockGateway;

    const MAX_LAG_MINUTES: i64 = 60;

    fn ts(minute: i64) -> NaiveDateTime {
        DateTime::from_timestamp(minute * 60, 0)
            .unwrap()
            .naive_utc()
    }

    fn block_hash(system: &str) -> Bytes {
        Bytes::from(system.as_bytes().to_vec())
    }

    /// A gateway where `committed` systems have a last block at the given minute and every other
    /// system has no extraction state.
    fn gateway(committed: &[(&str, i64)]) -> MockGateway {
        let committed: HashMap<String, i64> = committed
            .iter()
            .map(|(system, minute)| (system.to_string(), *minute))
            .collect();
        let blocks: HashMap<Bytes, i64> = committed
            .iter()
            .map(|(system, minute)| (block_hash(system), *minute))
            .collect();
        let mut gw = MockGateway::new();
        gw.expect_get_state()
            .returning(move |name, chain| match committed.get(name) {
                Some(_) => {
                    Ok(ExtractionState::new(name.to_string(), *chain, None, &[], block_hash(name)))
                }
                None => {
                    Err(StorageError::NotFound("ExtractionState".to_string(), name.to_string()))
                }
            });
        gw.expect_get_block()
            .returning(move |id| {
                let BlockIdentifier::Hash(hash) = id else {
                    panic!("expected a block hash, got {id:?}");
                };
                Ok(Block::new(1, Chain::Ethereum, hash.clone(), Bytes::default(), ts(blocks[hash])))
            });
        gw
    }

    async fn load(gw: &MockGateway, systems: &[&str]) -> Result<ProtocolFreshness, StorageError> {
        let systems: Vec<String> = systems
            .iter()
            .map(|s| s.to_string())
            .collect();
        ProtocolFreshness::load(gw, &Chain::Ethereum, &systems, Duration::minutes(MAX_LAG_MINUTES))
            .await
    }

    #[tokio::test]
    async fn load_takes_the_time_of_the_last_committed_block() {
        let freshness = load(&gateway(&[("uniswap_v2", 100)]), &["uniswap_v2"])
            .await
            .unwrap();

        assert!(freshness.is_fresh("uniswap_v2", ts(100 + MAX_LAG_MINUTES)));
        assert!(!freshness.is_fresh("uniswap_v2", ts(100 + MAX_LAG_MINUTES + 1)));
    }

    #[tokio::test]
    async fn a_system_without_a_committed_block_is_not_fresh() {
        let freshness = load(&gateway(&[]), &["ekubo_v3"])
            .await
            .unwrap();

        assert!(!freshness.is_fresh("ekubo_v3", ts(0)));
    }

    #[tokio::test]
    async fn load_fails_when_the_state_cannot_be_read() {
        let mut gw = MockGateway::new();
        gw.expect_get_state()
            .returning(|_, _| Err(StorageError::Unexpected("connection refused".to_string())));

        let err = load(&gw, &["uniswap_v2"])
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string()
                .contains("connection refused"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_recorded_block_replaces_the_loaded_time() {
        let freshness = load(&gateway(&[("uniswap_v2", 0)]), &["uniswap_v2", "ekubo_v3"])
            .await
            .unwrap();

        freshness.record("uniswap_v2", ts(500));
        freshness.record("ekubo_v3", ts(500));

        assert!(freshness.is_fresh("uniswap_v2", ts(500)));
        assert!(freshness.is_fresh("ekubo_v3", ts(500)));
    }

    #[tokio::test]
    async fn a_backfilling_system_becomes_fresh_when_it_reaches_the_chain_head() {
        let freshness = load(&gateway(&[("ekubo_v3", 0)]), &["ekubo_v3"])
            .await
            .unwrap();
        let now = ts(10_000);

        freshness.record("ekubo_v3", ts(5_000));
        assert!(!freshness.is_fresh("ekubo_v3", now));

        freshness.record("ekubo_v3", ts(9_999));
        assert!(freshness.is_fresh("ekubo_v3", now));
    }
}
