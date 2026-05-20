use std::sync::Arc;

use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot, Mutex,
};
use tracing::{error, info, warn};
use tycho_common::models::ExtractorIdentity;

pub use crate::extractor::factory::{DCIType, ExtractorConfig, ProtocolTypeConfig};
use crate::extractor::{
    factory::ExtractorFactory,
    runner::{ControlMessage, ExtractorHandle, SubscriptionsMap},
    DeltaCommand, ExtractionError,
};

/// Long-lived per-extractor task that owns the factory and manages restart lifecycle.
///
/// The supervisor:
/// - Builds an extractor and runner via its factory.
/// - Runs the runner and waits for it to exit.
/// - On failure: clears WS subscriptions, sends `DeltaCommand::ExtractorRestarted` to
///   `PendingDeltas` applies exponential backoff, then rebuilds from scratch.
/// - Forwards `ControlMessage::Subscribe` from the `ExtractorHandle` to the WS subscription map.
/// - Forwards `ControlMessage::Stop` by signalling the runner's stop channel.
pub struct ExtractorSupervisor {
    factory: ExtractorFactory,
    ctrl_tx: Sender<ControlMessage>,
    control_rx: Receiver<ControlMessage>,
    ws_subscriptions: Arc<Mutex<SubscriptionsMap>>,
    pending_deltas_tx: Sender<DeltaCommand>,
    id: ExtractorIdentity,
    max_restarts: Option<u32>,
    next_subscriber_id: u64,
}

impl ExtractorSupervisor {
    pub fn new(
        factory: ExtractorFactory,
        ws_subscriptions: Arc<Mutex<SubscriptionsMap>>,
        pending_deltas_tx: Sender<DeltaCommand>,
    ) -> Self {
        let id = factory.extractor_id();
        let max_restarts: Option<u32> = factory.config.max_restarts;
        let (ctrl_tx, control_rx) = mpsc::channel(128);
        Self {
            factory,
            ctrl_tx,
            control_rx,
            ws_subscriptions,
            pending_deltas_tx,
            id,
            max_restarts,
            next_subscriber_id: 0,
        }
    }

    /// Returns an [`ExtractorHandle`] that can be used to subscribe or stop this extractor.
    pub fn handle(&self) -> ExtractorHandle {
        ExtractorHandle::new(self.id.clone(), self.ctrl_tx.clone())
    }

    /// Runs the supervision loop. Returns when the extractor has been stopped via
    /// `ControlMessage::Stop` or has exhausted all restart attempts.
    pub async fn run(mut self) -> Result<(), ExtractionError> {
        let mut restart_count: u32 = 0;

        loop {
            let (stop_tx, stop_rx) = oneshot::channel();
            let runner = match self
                .factory
                .build_runner(
                    self.ws_subscriptions.clone(),
                    Some(self.pending_deltas_tx.clone()),
                    stop_rx,
                )
                .await
            {
                Ok(r) => r,
                Err(err) => {
                    error!(extractor = %self.id, error = %err, "Failed to build extractor");
                    metrics::counter!(
                        "extractor_restart_failed",
                        "extractor" => self.id.name.clone()
                    )
                    .increment(1);
                    return Err(err);
                }
            };

            let mut run_handle = runner.run();

            // Drive the runner, handling control messages in parallel.
            let runner_result = loop {
                tokio::select! {
                    result = &mut run_handle => {
                        break result;
                    }
                    Some(ctrl) = self.control_rx.recv() => {
                        match ctrl {
                            ControlMessage::Stop => {
                                info!(extractor = %self.id, "Stop signal received by supervisor");
                                let _ = stop_tx.send(());
                                let result = run_handle.await;
                                return match result {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(e)) => Err(e),
                                    Err(join_err) => Err(ExtractionError::Unknown(
                                        format!("Runner panicked: {join_err}")
                                    )),
                                };
                            }
                            ControlMessage::Subscribe(sender) => {
                                let subscriber_id = self.next_subscriber_id;
                                self.next_subscriber_id += 1;
                                info!(
                                    extractor = %self.id,
                                    subscriber_id,
                                    "New WS subscription via supervisor"
                                );
                                self.ws_subscriptions
                                    .lock()
                                    .await
                                    .insert(subscriber_id, sender);
                            }
                        }
                    }
                }
            };

            // Runner exited — classify the result.
            match runner_result {
                Ok(Ok(())) => {
                    info!(extractor = %self.id, "Extractor exited gracefully");
                    metrics::counter!(
                        "extractor_stopped",
                        "extractor" => self.id.name.clone(),
                        "reason" => "graceful"
                    )
                    .increment(1);
                    return Ok(());
                }
                Ok(Err(ref err)) => {
                    error!(
                        extractor = %self.id,
                        error = %err,
                        restart_count,
                        "Extractor failed"
                    );
                    metrics::counter!(
                        "extractor_stopped",
                        "extractor" => self.id.name.clone(),
                        "reason" => err.variant_name()
                    )
                    .increment(1);
                }
                Err(ref join_err) => {
                    error!(
                        extractor = %self.id,
                        error = %join_err,
                        "Extractor task panicked"
                    );
                    metrics::counter!(
                        "extractor_stopped",
                        "extractor" => self.id.name.clone(),
                        "reason" => "panic"
                    )
                    .increment(1);
                }
            }

            if self
                .max_restarts
                .is_some_and(|max| restart_count >= max)
            {
                error!(
                    extractor = %self.id,
                    max_restarts = ?self.max_restarts,
                    "Extractor permanently stopped — restart limit reached"
                );
                metrics::counter!(
                    "extractor_permanently_stopped",
                    "extractor" => self.id.name.clone()
                )
                .increment(1);
                return runner_result
                    .map_err(|e| ExtractionError::Unknown(format!("Runner panicked: {e}")))?;
            }

            // Clear WS subscriptions — clients must reconnect after a restart.
            // TODO: can we keep the ws connections alive and handle this on the client side?
            {
                let mut subs = self.ws_subscriptions.lock().await;
                let count = subs.len();
                subs.clear();
                if count > 0 {
                    info!(
                        extractor = %self.id,
                        dropped_subscribers = count,
                        "Cleared WS subscriptions before restart"
                    );
                }
            }

            // Signal PendingDeltas to reset its buffer for this extractor.
            // Sent on the same per-extractor channel as block messages, so it is guaranteed to
            // arrive after all blocks the runner emitted before failing.
            if let Err(err) = self
                .pending_deltas_tx
                .send(DeltaCommand::ExtractorRestarted(self.id.name.clone()))
                .await
            {
                warn!(
                    extractor = %self.id,
                    error = %err,
                    "Failed to send ExtractorRestarted to PendingDeltas"
                );
            }

            // Exponential backoff: 120s, 240s, 480s, 960s, 1920s, 3840s, 7680s, 14400s
            // (capped at 4 hours).
            let exp = restart_count.min(7); // 120 * 2^7 = 14400s = 4 hours, cap here to avoid overflow.
            let backoff = std::time::Duration::from_secs(120 * 2u64.pow(exp));
            warn!(
                extractor = %self.id,
                ?backoff,
                restart_count,
                "Waiting for backoff before restarting extractor"
            );
            tokio::time::sleep(backoff).await;
            warn!(
                extractor = %self.id,
                ?backoff,
                restart_count,
                "Restarting extractor after backoff"
            );
            restart_count += 1;
        }
    }
}
