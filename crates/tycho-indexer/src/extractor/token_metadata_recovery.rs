//! Retries enrichment after pending token identities have reached finalized storage.
//! No RPC work or recovery write runs on the extractor's block-processing future.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use futures03::{stream, StreamExt};
use metrics::{counter, gauge};
use tokio::time::{sleep, timeout, Instant};
use tracing::warn;
use tycho_common::{
    models::{blockchain::BlockTag, Chain},
    Bytes,
};
use tycho_ethereum::services::token_pre_processor::EthereumTokenPreProcessor;
use tycho_storage::postgres::cache::CachedGateway;

use super::{
    protocol_cache::{ProtocolDataCache, ProtocolMemoryCache},
    token_analysis_cron::find_token_owners,
    ExtractionError,
};

const PAGE_SIZE: i64 = 32;
const CONCURRENCY: usize = 4;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// With nothing pending the sweep is a single aggregate query; there is no reason to run it at
/// the page cadence.
const IDLE_INTERVAL: Duration = Duration::from_secs(60);
/// Longest wait between attempts for one token. Bounds the RPC cost of a token that keeps
/// failing without giving up on it: a stalled provider or an unfunded pool can recover later.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// Per-token exponential backoff, kept in memory only. A restart resets it, which is
/// acceptable: the durable queue is the source of truth and one extra attempt is cheap.
#[derive(Default)]
struct RetryBackoff {
    attempts: HashMap<Bytes, (u32, Instant)>,
}

impl RetryBackoff {
    fn is_due(&self, address: &Bytes, now: Instant) -> bool {
        self.attempts
            .get(address)
            .is_none_or(|(_, due)| *due <= now)
    }

    fn record_failure(&mut self, address: Bytes, now: Instant) {
        let attempts = self
            .attempts
            .get(&address)
            .map_or(0, |(attempts, _)| *attempts);
        let delay = RETRY_INTERVAL
            .saturating_mul(1u32 << attempts.min(16))
            .min(MAX_BACKOFF);
        self.attempts
            .insert(address, (attempts.saturating_add(1), now + delay));
    }

    fn record_success(&mut self, address: &Bytes) {
        self.attempts.remove(address);
    }

    /// Drops bookkeeping for tokens that left the durable queue through another writer.
    fn retain_pending(&mut self, still_pending: &HashSet<Bytes>) {
        self.attempts
            .retain(|address, _| still_pending.contains(address));
    }
}

/// Runs until cancelled by its owner. Errors leave rows pending for a later sweep.
///
/// Recovery applies the same rules as the hot path: metadata that reverts gets the legacy
/// fallbacks, a token without a funded owner gets a Bad verdict, and only an RPC that does not
/// answer keeps the token pending. Otherwise a token that was merely deferred by a slow RPC
/// would end up in a different state than the same token fetched on a fast one.
pub async fn run(
    chain: Chain,
    gateway: CachedGateway,
    cache: ProtocolMemoryCache,
    processor: EthereumTokenPreProcessor,
) -> Result<(), ExtractionError> {
    let mut after_id = 0;
    let mut backoff = RetryBackoff::default();
    let mut seen_this_sweep = HashSet::new();
    loop {
        if after_id == 0 {
            backoff.retain_pending(&seen_this_sweep);
            seen_this_sweep.clear();
            match timeout(ATTEMPT_TIMEOUT, gateway.pending_token_metadata_stats(chain)).await {
                Ok(Ok((count, oldest))) => {
                    gauge!("token_metadata_pending_count", "chain" => chain.to_string())
                        .set(count as f64);
                    let age = oldest
                        .map(|ts| {
                            (chrono::Utc::now().naive_utc() - ts)
                                .num_seconds()
                                .max(0)
                        })
                        .unwrap_or(0);
                    gauge!("token_metadata_pending_age_seconds", "chain" => chain.to_string())
                        .set(age as f64);
                    if count == 0 {
                        sleep(IDLE_INTERVAL).await;
                        continue;
                    }
                }
                result => warn!(?result, "Could not measure pending token metadata"),
            }
            // A successful repair in another process (or an ambiguous commit response) can
            // remove a row from the durable queue before this cache has observed it. Addresses
            // without a database row (reverted blocks) simply never come back ready.
            for addresses in cache
                .pending_token_addresses()
                .await
                .chunks(PAGE_SIZE as usize)
            {
                match timeout(ATTEMPT_TIMEOUT, gateway.ready_token_metadata(chain, addresses)).await
                {
                    Ok(Ok(ready)) => cache.add_tokens(ready).await?,
                    result => warn!(?result, "Could not reconcile pending token cache"),
                }
            }
        }
        // The cursor advances even on failed attempts. A poison token must not keep later
        // pages from being visited, and a shrinking pending set must not shift an OFFSET.
        let page = match timeout(
            ATTEMPT_TIMEOUT,
            gateway.pending_token_metadata(chain, after_id, PAGE_SIZE),
        )
        .await
        {
            Ok(Ok(page)) => page,
            result => {
                warn!(?result, "Could not read pending token metadata");
                sleep(RETRY_INTERVAL).await;
                continue;
            }
        };
        if page.is_empty() {
            after_id = 0;
            sleep(RETRY_INTERVAL).await;
            continue;
        }
        after_id = page.last().expect("nonempty page").0;
        let now = Instant::now();
        let addresses: Vec<_> = page
            .iter()
            .map(|(_, token, _)| token.address.clone())
            .inspect(|address| {
                seen_this_sweep.insert(address.clone());
            })
            .filter(|address| backoff.is_due(address, now))
            .collect();
        if addresses.is_empty() {
            continue;
        }
        let owners =
            match timeout(ATTEMPT_TIMEOUT, find_token_owners(chain, &addresses, &gateway)).await {
                Ok(Ok(owners)) => Arc::new(owners),
                result => {
                    warn!(?result, "Could not resolve pending token owners");
                    sleep(RETRY_INTERVAL).await;
                    continue;
                }
            };
        let mut work = stream::iter(addresses)
            .map(|address| {
                let owners = owners.clone();
                let processor = &processor;
                async move {
                    let attempt =
                        processor.recover_token(address.clone(), owners, BlockTag::Latest);
                    (address, timeout(ATTEMPT_TIMEOUT, attempt).await)
                }
            })
            .buffer_unordered(CONCURRENCY);
        let mut ready = Vec::new();
        while let Some((address, result)) = work.next().await {
            match result {
                Ok(Ok(token)) => ready.push(token),
                result => {
                    backoff.record_failure(address, now);
                    counter!("token_metadata_recovery_attempts", "chain" => chain.to_string(), "outcome" => "deferred").increment(1);
                    warn!(?result, "Token metadata recovery deferred");
                }
            }
        }
        // Do not time out the commit future: cancelling after PostgreSQL commits would make
        // its acknowledgement ambiguous. A DB failure leaves the cache untouched.
        if !ready.is_empty() {
            match gateway
                .complete_token_metadata(&ready)
                .await
            {
                Ok(completed) => {
                    let already_complete = ready.len() - completed.len();
                    counter!("token_metadata_recovery_attempts", "chain" => chain.to_string(), "outcome" => "completed").increment(completed.len() as u64);
                    counter!("token_metadata_recovery_attempts", "chain" => chain.to_string(), "outcome" => "already_complete").increment(already_complete as u64);
                    for token in &ready {
                        backoff.record_success(&token.address);
                    }
                    cache.add_tokens(completed).await?;
                }
                Err(error) => {
                    counter!("token_metadata_recovery_attempts", "chain" => chain.to_string(), "outcome" => "commit_failed").increment(ready.len() as u64);
                    warn!(%error, "Could not persist recovered token metadata");
                }
            }
        }
        sleep(RETRY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(id: u8) -> Bytes {
        Bytes::from(vec![id])
    }

    #[test]
    fn unknown_tokens_are_due_immediately() {
        let backoff = RetryBackoff::default();
        assert!(backoff.is_due(&address(1), Instant::now()));
    }

    #[test]
    fn failures_double_the_wait_up_to_the_cap() {
        let mut backoff = RetryBackoff::default();
        let now = Instant::now();
        let token = address(1);
        let mut expected = RETRY_INTERVAL;
        for _ in 0..20 {
            backoff.record_failure(token.clone(), now);
            let (_, due) = backoff.attempts[&token];
            assert_eq!(due - now, expected);
            assert!(!backoff.is_due(&token, now + expected - Duration::from_millis(1)));
            assert!(backoff.is_due(&token, now + expected));
            expected = (expected * 2).min(MAX_BACKOFF);
        }
        assert_eq!(backoff.attempts[&token].1 - now, MAX_BACKOFF);
    }

    #[test]
    fn success_resets_and_stale_entries_are_pruned() {
        let mut backoff = RetryBackoff::default();
        let now = Instant::now();
        backoff.record_failure(address(1), now);
        backoff.record_failure(address(2), now);
        backoff.record_success(&address(1));
        assert!(backoff.is_due(&address(1), now));
        assert!(!backoff.is_due(&address(2), now));

        backoff.retain_pending(&HashSet::new());
        assert!(backoff.is_due(&address(2), now));
    }
}
