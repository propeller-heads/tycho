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
