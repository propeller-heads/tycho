//! Election of the process that writes extraction results to the database.
//!
//! Every `index` process extracts and serves; only the leader commits. A process that starts
//! while another holds the leader lock follows: it publishes the leader's committed heights to
//! its extractors and retries the lock until it gets it. A leader that loses its lock session
//! ends with an error, so the process exits instead of writing next to a new leader.
use std::{collections::HashMap, time::Duration};

use tokio::{sync::watch, task::JoinHandle, time::Instant};
use tracing::{error, info, warn};
use tycho_common::models::Chain;
use tycho_storage::postgres::{cache::CachedGateway, leader_lock::LeaderLock};

use crate::extractor::{
    writer_status::{Role, WriterStatus},
    ExtractionError,
};

/// How often a follower polls the committed heights and retries the lock, and how often a
/// leader checks its lock session.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long a leader waits for its lock session to answer before it gives up the lead.
///
/// Shorter than the server-side keepalive timeout of the lock session, so a leader that cannot
/// reach the database stops before Postgres can hand the lock to another process.
const LOCK_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Decides whether this process leads, and keeps that decision current in the background.
///
/// Returns the [`WriterStatus`] for the extractors and the background task. A process that gets
/// the lock at once increments the writer epoch on `gateway` and is leader from the start.
/// Otherwise it is a follower: the task polls the committed heights of `chain` and retries the
/// lock. When it gets the lock, it increments the writer epoch and only then switches the role, so
/// no extractor commits before the former leader is fenced. The lock checks start with the epoch
/// increment. The task only ends when the leader's lock session is lost or fails.
pub async fn elect(
    database_url: &str,
    gateway: CachedGateway,
    chain: Chain,
) -> Result<(WriterStatus, JoinHandle<Result<(), ExtractionError>>), ExtractionError> {
    let mut lock = LeaderLock::connect(database_url).await?;
    let (heights_tx, heights_rx) = watch::channel(HashMap::new());
    let (confirmed_tx, confirmed_rx) = watch::channel(Instant::now());

    if lock.try_acquire().await? {
        let epoch = gateway.increment_writer_epoch().await?;
        info!(epoch, "Elected database writer at startup");
        metrics::gauge!("tycho_leader").set(1.0);
        let (_role_tx, role_rx) = watch::channel(Role::Leader);
        confirmed_tx.send_replace(Instant::now());
        let task = tokio::spawn(hold_lead(lock, confirmed_tx));
        return Ok((WriterStatus::new(role_rx, heights_rx, confirmed_rx), task));
    }

    info!("Another process writes the database; following it");
    metrics::gauge!("tycho_leader").set(0.0);
    publish_committed_heights(&gateway, &chain, &heights_tx).await?;
    let (role_tx, role_rx) = watch::channel(Role::Follower);
    let database_url = database_url.to_string();
    let task = tokio::spawn(async move {
        let lock = follow_until_elected(lock, &database_url, &gateway, &chain, &heights_tx).await;
        let epoch = gateway.increment_writer_epoch().await?;
        info!(epoch, "Elected database writer; taking over commits");
        confirmed_tx.send_replace(Instant::now());
        metrics::gauge!("tycho_leader").set(1.0);
        role_tx
            .send(Role::Leader)
            .map_err(|_| ExtractionError::Unknown("Every writer status was dropped".into()))?;
        hold_lead(lock, confirmed_tx).await
    });
    Ok((WriterStatus::new(role_rx, heights_rx, confirmed_rx), task))
}

/// Publishes the committed heights and retries the lock until this process holds it.
async fn follow_until_elected(
    mut lock: LeaderLock,
    database_url: &str,
    gateway: &CachedGateway,
    chain: &Chain,
    heights_tx: &watch::Sender<HashMap<String, u64>>,
) -> LeaderLock {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if let Err(err) = publish_committed_heights(gateway, chain, heights_tx).await {
            warn!(error = %err, "Failed to poll the leader's committed heights");
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

async fn publish_committed_heights(
    gateway: &CachedGateway,
    chain: &Chain,
    heights_tx: &watch::Sender<HashMap<String, u64>>,
) -> Result<(), ExtractionError> {
    let heights = gateway
        .get_committed_heights(chain)
        .await?;
    heights_tx.send_replace(heights);
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
