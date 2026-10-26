//! Fetch complete snapshots when another source signals a change.

use futures::{Stream, StreamExt};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

use super::{
    errors::FeedError,
    failures::FailureTracker,
    http::{HttpFeedConfig, HttpSource},
    Publisher,
};

#[derive(Clone, Default)]
struct Trigger {
    version: u64,
    invalidation: u64,
    available: bool,
    fatal: Option<FeedError>,
}

struct TriggerTask(JoinHandle<()>);

impl Drop for TriggerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// A watch slot coalesces bursts while a fetch is pending. Invalidation survives a quick
// disconnect/reconnect, so a snapshot from the old connection cannot escape afterward.
fn triggers(
    events: impl Stream<Item = Result<(), FeedError>> + Send + 'static,
) -> (watch::Receiver<Trigger>, TriggerTask) {
    let (tx, rx) = watch::channel(Trigger::default());
    let task = tokio::spawn(async move {
        tokio::pin!(events);
        let mut state = Trigger::default();
        while let Some(event) = events.next().await {
            state.version = state.version.wrapping_add(1);
            match event {
                Ok(()) => state.available = true,
                Err(error) => {
                    tracing::warn!(%error, "snapshot trigger unavailable");
                    state.available = false;
                    state.invalidation = state.invalidation.wrapping_add(1);
                    if error.is_fatal() {
                        state.fatal = Some(error);
                    }
                }
            }
            let fatal = state.fatal.is_some();
            if tx.send(state.clone()).is_err() || fatal {
                return;
            }
        }
        state.version = state.version.wrapping_add(1);
        state.invalidation = state.invalidation.wrapping_add(1);
        state.available = false;
        let _ = tx.send(state);
    });
    (rx, TriggerTask(task))
}

/// Uses the same source, request deadline, failure budget and freshness policy as HTTP polling.
/// Successful triggers fetch immediately; the interval is used only for failure backoff.
pub(crate) async fn run_triggered_feed<S: HttpSource>(
    config: HttpFeedConfig,
    publisher: Publisher<S::Snapshot>,
    source: S,
    events: impl Stream<Item = Result<(), FeedError>> + Send + 'static,
) -> Result<(), FeedError> {
    if config.poll_interval.is_zero() ||
        config
            .max_snapshot_age
            .is_some_and(|age| age.is_zero())
    {
        return Err(FeedError::InvalidInput("feed durations must be positive".into()));
    }
    let max_age = config.max_snapshot_age;
    publisher
        .publishing_updates(max_age, fetched_snapshots(config, source, events))
        .await
}

fn fetched_snapshots<S: HttpSource>(
    config: HttpFeedConfig,
    source: S,
    events: impl Stream<Item = Result<(), FeedError>> + Send + 'static,
) -> impl Stream<Item = Result<Option<S::Snapshot>, FeedError>> {
    async_stream::try_stream! {
        let (mut trigger, _task) = triggers(events);
        let mut failures = FailureTracker::new(
            config.max_consecutive_failures, config.poll_interval, config.max_backoff_exp,
        );
        let mut handled = 0;
        let mut invalidation = 0;
        let mut retry = false;
        loop {
            let needs_trigger = !retry && trigger.borrow().version == handled;
            if needs_trigger && trigger.changed().await.is_err() { return; }
            retry = false;
            let current = trigger.borrow_and_update().clone();
            handled = current.version;
            if current.invalidation != invalidation {
                invalidation = current.invalidation;
                yield None;
            }
            if let Some(error) = current.fatal { Err(error)?; }
            if !current.available { continue; }
            let result = {
                let fetch = timeout(config.request_timeout, source.fetch());
                tokio::pin!(fetch);
                loop {
                    tokio::select! {
                        result = &mut fetch => break Some(result),
                        changed = trigger.changed() => {
                            if changed.is_err() { break None; }
                            let newest = trigger.borrow().clone();
                            if !newest.available || newest.invalidation != invalidation {
                                break None;
                            }
                            // A higher-frequency trigger never restarts a slow read. The next
                            // iteration fetches once more from the latest coalesced trigger.
                        }
                    }
                }
            };
            let Some(result) = result else { continue; };
            match result {
                Ok(Ok(snapshot)) => {
                    let latest = trigger.borrow().clone();
                    if !latest.available || latest.invalidation != invalidation { continue; }
                    failures.record_success();
                    yield Some(snapshot);
                }
                result => {
                    let error = match result {
                        Ok(Err(error)) => error,
                        Err(_) => FeedError::Connection(format!("fetch timed out after {:?}", config.request_timeout)),
                        Ok(Ok(_)) => unreachable!(),
                    };
                    yield None;
                    if error.is_fatal() { Err(error.clone())?; }
                    let failure = failures.record_failure().map_err(|_| error.clone())?;
                    tracing::warn!(%error, consecutive = failure.consecutive, backoff = ?failure.backoff, "triggered fetch failed, backing off");
                    sleep(failure.backoff).await;
                    retry = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use tokio::{sync::mpsc, time::advance};
    use tokio_stream::wrappers::UnboundedReceiverStream;

    use super::*;
    use crate::snapshot_feed::http::default_http_feed_config;

    struct Source {
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }
    impl HttpSource for Source {
        type Snapshot = usize;
        async fn fetch(&self) -> Result<usize, FeedError> {
            let n = self
                .calls
                .fetch_add(1, Ordering::SeqCst) +
                1;
            sleep(self.delay).await;
            Ok(n)
        }
    }
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn triggers_fetch_immediately_and_never_poll_while_idle() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            Source { calls: calls.clone(), delay: Duration::ZERO },
            UnboundedReceiverStream::new(rx),
        ));
        events.send(Ok(())).unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        events.send(Ok(())).unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(2));
        drop(snapshots);
        settle().await;
        assert!(task.is_finished());
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_fetch_coalesces_triggers_and_preserves_the_last_update() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            Source { calls: calls.clone(), delay: Duration::from_secs(1) },
            UnboundedReceiverStream::new(rx),
        ));
        events.send(Ok(())).unwrap();
        settle().await;
        for _ in 0..100 {
            events.send(Ok(())).unwrap();
        }
        settle().await;
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(2));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(snapshots);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn disconnection_withdraws_and_cancels_an_inflight_fetch() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            Source { calls: calls.clone(), delay: Duration::from_secs(1) },
            UnboundedReceiverStream::new(rx),
        ));
        events.send(Ok(())).unwrap();
        settle().await;
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        events.send(Ok(())).unwrap();
        settle().await;
        events
            .send(Err(FeedError::Connection("gone".into())))
            .unwrap();
        settle().await;
        assert!(snapshots.borrow_and_update().is_none());
        advance(Duration::from_secs(2)).await;
        settle().await;
        assert!(snapshots.borrow().is_none());
        events.send(Ok(())).unwrap();
        settle().await;
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(3));
        drop(snapshots);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn snapshots_expire_even_while_no_triggers_arrive() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let task = tokio::spawn(run_triggered_feed(
            HttpFeedConfig {
                max_snapshot_age: Some(Duration::from_secs(2)),
                ..default_http_feed_config()
            },
            publisher,
            Source { calls: Arc::new(AtomicUsize::new(0)), delay: Duration::ZERO },
            UnboundedReceiverStream::new(rx),
        ));
        events.send(Ok(())).unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        advance(Duration::from_secs(2)).await;
        settle().await;
        assert!(snapshots.borrow().is_none());
        drop(snapshots);
        task.await.unwrap().unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn timeouts_retry_with_backoff_and_stop_at_the_shared_failure_budget() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, snapshots) = Publisher::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_triggered_feed(
            HttpFeedConfig {
                poll_interval: Duration::from_secs(2),
                request_timeout: Duration::from_secs(1),
                max_consecutive_failures: Some(2),
                ..default_http_feed_config()
            },
            publisher,
            Source { calls: calls.clone(), delay: Duration::from_secs(10) },
            UnboundedReceiverStream::new(rx),
        ));
        events.send(Ok(())).unwrap();
        settle().await;
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert!(matches!(task.await.unwrap(), Err(FeedError::Connection(_))));
        assert!(snapshots.borrow().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_during_fetch_discards_the_previous_connection_result() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            Source { calls: calls.clone(), delay: Duration::from_secs(1) },
            UnboundedReceiverStream::new(rx),
        ));
        events.send(Ok(())).unwrap();
        settle().await;
        events
            .send(Err(FeedError::Connection("replaced".into())))
            .unwrap();
        events.send(Ok(())).unwrap();
        settle().await;
        assert!(snapshots.borrow_and_update().is_none());
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(2));
        drop(snapshots);
        task.await.unwrap().unwrap();
    }
}
