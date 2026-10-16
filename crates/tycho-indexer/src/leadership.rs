//! Election of the process that writes extraction results to the database.
//!
//! Every `index` process extracts and serves; only the leader commits. A process that starts
//! while another holds the leader lock follows: it publishes the leader's committed blocks to
//! its extractors and retries the lock until it gets it. A leader that loses its lock session
//! ends with an error, so the process exits instead of writing next to a new leader.
use std::{collections::HashMap, future::Future, time::Duration};

use tokio::{sync::watch, task::JoinHandle, time::Instant};
use tracing::{error, info, warn};
use tycho_common::models::Chain;
use tycho_storage::postgres::{
    cache::{CachedGateway, CommittedBlock},
    leader_lock::LeaderLock,
};

use crate::extractor::{
    writer_status::{Role, WriterStatus},
    ExtractionError,
};

/// How often a follower polls the committed blocks and retries the lock, and how often a
/// leader checks its lock session.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long a leader waits for its lock session to answer before it gives up the lead.
///
/// Shorter than the server-side keepalive timeout of the lock session, so a leader that cannot
/// reach the database stops before Postgres can hand the lock to another process.
const LOCK_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a promoted process waits for `on_promotion` before it gives up the lead.
///
/// After the claim the former leader is fenced, and this process does not commit until the
/// promotion ends, so a promotion that hangs leaves the database with no writer. The election
/// task then ends with an error, the process exits, and its lock goes to another process.
#[cfg(not(test))]
const PROMOTION_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const PROMOTION_TIMEOUT: Duration = Duration::from_secs(3);

/// Decides whether this process leads, and keeps that decision current in the background.
///
/// Returns the [`WriterStatus`] for the extractors and the background task. A process that gets
/// the lock at once claims the writer epoch on `gateway` and is leader from the start. Otherwise
/// it is a follower: the task polls the committed blocks of `chain` and retries the lock. When
/// it gets the lock, it claims the writer epoch, runs `on_promotion`, and only then switches the
/// role, so no extractor commits before the former leader is fenced. The lock checks start with
/// the claim. The task only ends when the leader's lock session is lost or fails, or when
/// `on_promotion` does not finish within [`PROMOTION_TIMEOUT`].
///
/// `gateway` must come from a builder with
/// [`defer_writer_claim`](tycho_storage::postgres::builder::GatewayBuilder::defer_writer_claim).
pub async fn elect<F, Fut>(
    database_url: &str,
    gateway: CachedGateway,
    chain: Chain,
    on_promotion: F,
) -> Result<(WriterStatus, JoinHandle<Result<(), ExtractionError>>), ExtractionError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let mut lock = LeaderLock::connect(database_url).await?;
    let (committed_tx, committed_rx) = watch::channel(HashMap::new());
    let (confirmed_tx, confirmed_rx) = watch::channel(Instant::now());

    if lock.try_acquire().await? {
        let epoch = gateway.claim_writer().await?;
        info!(epoch, "Elected database writer at startup");
        metrics::gauge!("tycho_leader").set(1.0);
        let (_role_tx, role_rx) = watch::channel(Role::Leader);
        confirmed_tx.send_replace(Instant::now());
        let task = tokio::spawn(hold_lead(lock, confirmed_tx));
        let status = WriterStatus::new(role_rx, committed_rx).with_lead_confirmations(confirmed_rx);
        return Ok((status, task));
    }

    info!("Another process writes the database; following it");
    metrics::gauge!("tycho_leader").set(0.0);
    publish_committed_blocks(&gateway, &chain, &committed_tx).await?;
    let (role_tx, role_rx) = watch::channel(Role::Follower);
    let database_url = database_url.to_string();
    let task = tokio::spawn(async move {
        let lock = follow_until_elected(lock, &database_url, &gateway, &chain, &committed_tx).await;
        let epoch = gateway.claim_writer().await?;
        info!(epoch, "Elected database writer; taking over commits");
        confirmed_tx.send_replace(Instant::now());
        let mut lead = std::pin::pin!(hold_lead(lock, confirmed_tx));
        tokio::select! {
            lost = &mut lead => return lost,
            promoted = tokio::time::timeout(PROMOTION_TIMEOUT, on_promotion()) => {
                promoted.map_err(|_| {
                    error!("Promotion did not finish; stopping so another process can lead");
                    ExtractionError::Unknown(format!(
                        "Promotion did not finish within {PROMOTION_TIMEOUT:?}"
                    ))
                })?;
            }
        }
        metrics::gauge!("tycho_leader").set(1.0);
        role_tx
            .send(Role::Leader)
            .map_err(|_| ExtractionError::Unknown("Every writer status was dropped".into()))?;
        lead.await
    });
    let status = WriterStatus::new(role_rx, committed_rx).with_lead_confirmations(confirmed_rx);
    Ok((status, task))
}

/// Publishes the committed blocks and retries the lock until this process holds it.
async fn follow_until_elected(
    mut lock: LeaderLock,
    database_url: &str,
    gateway: &CachedGateway,
    chain: &Chain,
    committed_tx: &watch::Sender<HashMap<String, CommittedBlock>>,
) -> LeaderLock {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if let Err(err) = publish_committed_blocks(gateway, chain, committed_tx).await {
            warn!(error = %err, "Failed to poll the leader's committed blocks");
        }
        match lock.try_acquire().await {
            Ok(true) => return lock,
            Ok(false) => {}
            Err(err) => {
                warn!(error = %err, "Leader lock session failed; reconnecting");
                match LeaderLock::connect(database_url).await {
                    Ok(new_lock) => lock = new_lock,
                    Err(err) => warn!(error = %err, "Failed to reconnect the leader lock session"),
                }
            }
        }
    }
}

async fn publish_committed_blocks(
    gateway: &CachedGateway,
    chain: &Chain,
    committed_tx: &watch::Sender<HashMap<String, CommittedBlock>>,
) -> Result<(), ExtractionError> {
    let committed = gateway
        .get_committed_blocks(chain)
        .await?;
    committed_tx.send_replace(committed);
    Ok(())
}

/// Checks the lock session until it fails, and returns that failure.
///
/// Publishes the time of each successful check on `confirmed_tx`. The health endpoint uses it to
/// report a leader whose checks stopped, for example because its runtime is stuck while it still
/// holds the lock.
async fn hold_lead(
    mut lock: LeaderLock,
    confirmed_tx: watch::Sender<Instant>,
) -> Result<(), ExtractionError> {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let failure = match tokio::time::timeout(LOCK_CHECK_TIMEOUT, lock.check_alive()).await {
            Ok(Ok(())) => {
                confirmed_tx.send_replace(Instant::now());
                continue;
            }
            Ok(Err(err)) => ExtractionError::Storage(err),
            Err(_) => ExtractionError::Unknown(format!(
                "Leader lock session did not answer within {LOCK_CHECK_TIMEOUT:?}"
            )),
        };
        error!(error = %failure, "Lost the leader lock; stopping so another process can lead");
        metrics::gauge!("tycho_leader").set(0.0);
        return Err(failure);
    }
}

#[cfg(test)]
mod test_serial_db {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use tycho_common::{
        models::ExtractionState,
        storage::{ChainGateway, ExtractionStateGateway, StorageError},
    };
    use tycho_storage::postgres::{builder::GatewayBuilder, testing::run_against_db};

    use super::*;
    use crate::testing::block;

    fn database_url() -> String {
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
    }

    async fn writer_gateway() -> (CachedGateway, JoinHandle<()>) {
        GatewayBuilder::new(&database_url())
            .set_chains(&[Chain::Ethereum])
            .defer_writer_claim()
            .build()
            .await
            .expect("gateway should build")
    }

    /// Commits block `number` and an extraction state named `extractor` pointing at it.
    async fn commit_block(
        gateway: &CachedGateway,
        extractor: &str,
        number: u64,
    ) -> Result<(), StorageError> {
        let block = block(number);
        gateway
            .start_transaction(&block, Some(extractor))
            .await;
        gateway
            .upsert_block(std::slice::from_ref(&block))
            .await?;
        gateway
            .save_state(&ExtractionState::new(
                extractor.to_string(),
                Chain::Ethereum,
                None,
                b"cursor",
                block.hash.clone(),
            ))
            .await?;
        gateway.commit_transaction(0).await
    }

    async fn wait_until(condition: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    async fn test_follower_takes_over_when_the_leader_session_ends() {
        run_against_db(|_| async move {
            let (leader_gw, _leader_executor) = writer_gateway().await;
            let (leader_status, leader_task) =
                elect(&database_url(), leader_gw.clone(), Chain::Ethereum, || async {})
                    .await
                    .expect("first process should be elected");
            assert_eq!(leader_status.role(), Role::Leader);
            tokio::time::sleep(POLL_INTERVAL * 3).await;
            let since_confirmed = leader_status
                .time_since_lead_confirmed()
                .expect("a leader tracks its lock confirmations");
            assert!(
                since_confirmed < POLL_INTERVAL * 2,
                "the leader must keep confirming its lock, last confirmed {since_confirmed:?} ago"
            );
            commit_block(&leader_gw, "uniswap_v2", 1)
                .await
                .expect("the leader should write");

            let (follower_gw, _follower_executor) = writer_gateway().await;
            let promoted = Arc::new(AtomicBool::new(false));
            let promoted_flag = promoted.clone();
            let (follower_status, _follower_task) =
                elect(&database_url(), follower_gw.clone(), Chain::Ethereum, move || async move {
                    promoted_flag.store(true, Ordering::SeqCst);
                })
                .await
                .expect("second process should start");
            assert_eq!(follower_status.role(), Role::Follower);
            assert_eq!(follower_status.time_since_lead_confirmed(), None);
            assert_eq!(follower_status.committed_height("uniswap_v2"), Some(1));

            commit_block(&leader_gw, "uniswap_v2", 2)
                .await
                .expect("the leader keeps writing");
            assert!(
                wait_until(|| follower_status.committed_height("uniswap_v2") == Some(2)).await,
                "the follower must see the leader's new commit"
            );
            assert!(!promoted.load(Ordering::SeqCst));

            leader_task.abort();
            assert!(
                wait_until(|| follower_status.role() == Role::Leader).await,
                "the follower must take over once the leader's session ends"
            );

            assert!(promoted.load(Ordering::SeqCst), "promotion must run before the role flips");
            commit_block(&follower_gw, "uniswap_v2", 3)
                .await
                .expect("the new leader should write");
            let stale = commit_block(&leader_gw, "uniswap_v2", 4).await;
            assert!(
                matches!(stale, Err(StorageError::Fenced { .. })),
                "the former leader must be fenced, got {stale:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_promotion_that_does_not_finish_gives_up_the_lead() {
        run_against_db(|_| async move {
            let (leader_gw, _leader_executor) = writer_gateway().await;
            let (_, leader_task) = elect(&database_url(), leader_gw, Chain::Ethereum, || async {})
                .await
                .expect("first process should be elected");

            let (follower_gw, _follower_executor) = writer_gateway().await;
            let (follower_status, follower_task) =
                elect(&database_url(), follower_gw, Chain::Ethereum, std::future::pending::<()>)
                    .await
                    .expect("second process should start");
            leader_task.abort();

            let ended = tokio::time::timeout(POLL_INTERVAL * 3 + PROMOTION_TIMEOUT, follower_task)
                .await
                .expect("the election task must end when the promotion does not finish")
                .expect("the election task should not panic");
            assert!(
                matches!(&ended, Err(ExtractionError::Unknown(msg)) if msg.contains("Promotion")),
                "the election task must fail on the promotion timeout, got {ended:?}"
            );
            assert_eq!(follower_status.role(), Role::Follower, "a stuck promotion never leads");
        })
        .await;
    }
}
