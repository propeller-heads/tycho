//! Fetch complete snapshots when another source signals a change.

use futures::{Stream, StreamExt};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug_span, instrument::WithSubscriber, Instrument};

use super::{
    errors::FeedError,
    failures::FailureTracker,
    http::{HttpFeedConfig, HttpSource},
    Publisher,
};

#[derive(Clone, Copy)]
pub enum TriggerEvent {
    Fetch,
    Invalidate,
}

#[derive(Clone, Default)]
struct Trigger {
    version: u64,
    invalidation: u64,
    generation: u64,
    available: bool,
    fatal: Option<FeedError>,
}

struct TriggerTask(JoinHandle<()>);

impl TriggerTask {
    async fn finish(&mut self) -> Result<(), FeedError> {
        match (&mut self.0).await {
            Ok(()) => Ok(()),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(FeedError::Fatal(format!("snapshot trigger task ended: {error}"))),
        }
    }
}

impl Drop for TriggerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// A watch slot coalesces bursts while a fetch is pending. Invalidation survives a quick
// disconnect/reconnect, so a snapshot from the old connection cannot escape afterward.
fn triggers(
    events: impl Stream<Item = Result<TriggerEvent, FeedError>> + Send + 'static,
) -> (watch::Receiver<Trigger>, TriggerTask) {
    let (tx, rx) = watch::channel(Trigger::default());
    let task = tokio::spawn(
        async move {
            tokio::pin!(events);
            let mut state = Trigger::default();
            while let Some(event) = events.next().await {
                state.version = state.version.wrapping_add(1);
                match event {
                    Ok(TriggerEvent::Fetch) => state.available = true,
                    Ok(TriggerEvent::Invalidate) => {
                        state.available = true;
                        state.invalidation = state.invalidation.wrapping_add(1);
                        state.generation = state.generation.wrapping_add(1);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "snapshot trigger unavailable");
                        state.available = false;
                        state.generation = state.generation.wrapping_add(1);
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
            state.available = false;
            let _closed_receiver = tx.send(state);
        }
        .instrument(tracing::Span::current())
        .with_current_subscriber(),
    );
    (rx, TriggerTask(task))
}

/// Fetches immediately on triggers, retaining snapshots through transient failures until their TTL.
/// Explicit invalidation withdraws a snapshot immediately. The interval sets failure backoff.
pub async fn run_triggered_feed<S: HttpSource>(
    config: HttpFeedConfig,
    publisher: Publisher<S::Snapshot>,
    source: S,
    events: impl Stream<Item = Result<TriggerEvent, FeedError>> + Send + 'static,
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
        .publishing_updates(max_age, fetched_snapshots(&config, source, events))
        .await
}

fn fetched_snapshots<S: HttpSource>(
    config: &HttpFeedConfig,
    source: S,
    events: impl Stream<Item = Result<TriggerEvent, FeedError>> + Send + 'static,
) -> impl Stream<Item = Result<Option<S::Snapshot>, FeedError>> {
    let HttpFeedConfig {
        poll_interval,
        request_timeout,
        max_backoff_exp,
        max_consecutive_failures,
        max_snapshot_age: _,
    } = *config;
    async_stream::try_stream! {
        let (mut trigger, mut task) = triggers(events);
        let mut failures = FailureTracker::new(
            max_consecutive_failures, poll_interval, max_backoff_exp,
        );
        let mut handled = 0;
        let mut invalidation = 0;
        let mut retry = false;
        loop {
            let needs_trigger = !retry && trigger.borrow().version == handled;
            if needs_trigger && trigger.changed().await.is_err() { task.finish().await?; return; }
            retry = false;
            let current = trigger.borrow_and_update().clone();
            handled = current.version;
            let generation = current.generation;
            if current.invalidation != invalidation {
                invalidation = current.invalidation;
                yield None;
            }
            if let Some(error) = current.fatal { Err(error)?; }
            if !current.available { continue; }
            let result = {
                let fetch = timeout(request_timeout, source.fetch()).instrument(debug_span!("fetch"));
                tokio::pin!(fetch);
                loop {
                    let changed = tokio::select! {
                        result = &mut fetch => break Some(result),
                        changed = trigger.changed() => changed,
                    };
                    if changed.is_err() { task.finish().await?; return; }
                    let newest = trigger.borrow().clone();
                    if !newest.available || newest.generation != generation {
                        break None;
                    }
                    // Coalesced heads do not restart an in-flight read.
                }
            };
            let Some(result) = result else { continue; };
            let result = result.unwrap_or_else(|_timeout| {
                Err(FeedError::Connection(format!("fetch timed out after {request_timeout:?}")))
            });
            match result {
                Ok(snapshot) => {
                    let latest = trigger.borrow().clone();
                    if trigger.has_changed().is_err() { task.finish().await?; return; }
                    if !latest.available || latest.generation != generation { continue; }
                    failures.record_success();
                    yield Some(snapshot);
                }
                Err(error) => {
                    if error.is_fatal() { Err(error.clone())?; }
                    let failure = failures.record_failure().map_err(|_consecutive| error.clone())?;
                    tracing::warn!(%error, consecutive = failure.consecutive, backoff = ?failure.backoff, "triggered fetch failed, backing off");
                    let backoff = sleep(failure.backoff);
                    tokio::pin!(backoff);
                    loop {
                        let changed = tokio::select! {
                            _ = &mut backoff => break,
                            changed = trigger.changed() => changed,
                        };
                        if changed.is_err() { task.finish().await?; return; }
                        let latest = trigger.borrow().clone();
                        if latest.invalidation != invalidation {
                            invalidation = latest.invalidation;
                            yield None;
                        }
                        if let Some(error) = latest.fatal { Err(error)?; }
                    }
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
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
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
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        for _ in 0..100 {
            events
                .send(Ok(TriggerEvent::Fetch))
                .unwrap();
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
    async fn disconnection_retains_snapshot_and_cancels_an_inflight_fetch() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            Source { calls: calls.clone(), delay: Duration::from_secs(1) },
            UnboundedReceiverStream::new(rx),
        ));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        events
            .send(Err(FeedError::Connection("gone".into())))
            .unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(*snapshots.borrow(), Some(1));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
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
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
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
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
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
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        events
            .send(Err(FeedError::Connection("replaced".into())))
            .unwrap();
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        assert!(snapshots.borrow_and_update().is_none());
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(2));
        drop(snapshots);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_fetch_retains_the_snapshot_until_expiry() {
        let (events, rx) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let source_calls = Arc::clone(&calls);
        let (publisher, mut snapshots) = Publisher::channel();
        let task = tokio::spawn(run_triggered_feed(
            HttpFeedConfig {
                max_snapshot_age: Some(Duration::from_secs(3)),
                ..default_http_feed_config()
            },
            publisher,
            move || {
                let n = source_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        Ok(1usize)
                    } else {
                        Err(FeedError::Connection("temporary fetch failure".into()))
                    }
                }
            },
            UnboundedReceiverStream::new(rx),
        ));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        advance(Duration::from_secs(3)).await;
        settle().await;
        assert!(snapshots.borrow().is_none());
        drop(snapshots);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn invalidation_is_immediate_even_during_fetch_backoff() {
        let (events, rx) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let source_calls = Arc::clone(&calls);
        let (publisher, mut snapshots) = Publisher::channel();
        let task = tokio::spawn(run_triggered_feed(
            HttpFeedConfig { poll_interval: Duration::from_secs(10), ..default_http_feed_config() },
            publisher,
            move || {
                let n = source_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 1 {
                        Err(FeedError::Connection("temporary fetch failure".into()))
                    } else {
                        Ok(n)
                    }
                }
            },
            UnboundedReceiverStream::new(rx),
        ));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(0));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(0));
        events
            .send(Ok(TriggerEvent::Invalidate))
            .unwrap();
        settle().await;
        assert!(snapshots.borrow_and_update().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(2));
        drop(snapshots);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dropping_the_consumer_cancels_the_trigger_source() {
        struct Dropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let events = futures::stream::once(async { Ok(TriggerEvent::Fetch) }).chain(
            futures::stream::once(async move {
                let _guard = guard;
                std::future::pending::<Result<TriggerEvent, FeedError>>().await
            }),
        );
        let (publisher, mut snapshots) = Publisher::channel();
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            || async { Ok(1usize) },
            events,
        ));
        snapshots.changed().await.unwrap();
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        drop(snapshots);
        task.await.unwrap().unwrap();
        settle().await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_invalidation_withdraws_before_fetching_a_replacement() {
        let (events, rx) = mpsc::unbounded_channel();
        let (publisher, mut snapshots) = Publisher::channel();
        let task = tokio::spawn(run_triggered_feed(
            default_http_feed_config(),
            publisher,
            Source { calls: Arc::new(AtomicUsize::new(0)), delay: Duration::from_secs(1) },
            UnboundedReceiverStream::new(rx),
        ));
        events
            .send(Ok(TriggerEvent::Fetch))
            .unwrap();
        settle().await;
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(1));
        events
            .send(Ok(TriggerEvent::Invalidate))
            .unwrap();
        settle().await;
        assert!(snapshots.borrow_and_update().is_none());
        advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(*snapshots.borrow_and_update(), Some(2));
        drop(snapshots);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_trigger_panic_is_reported_as_a_panicked_feed() {
        use crate::snapshot_feed::{
            SnapshotFeed, SnapshotFeedEvent, SnapshotFeedOutcome, SnapshotFeedStream,
        };
        struct Feed;
        impl SnapshotFeed for Feed {
            type Snapshot = usize;
            type Error = FeedError;
            async fn run(self, publisher: Publisher<usize>) -> Result<(), FeedError> {
                run_triggered_feed(
                    default_http_feed_config(),
                    publisher,
                    || async { Ok(1usize) },
                    futures::stream::once(async { panic!("trigger failed") }),
                )
                .await
            }
        }
        let mut stream = SnapshotFeedStream::spawn("test", Feed);
        let outcome = stream.next().await.unwrap();
        assert!(matches!(outcome, SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Panicked(_))));
    }

    #[tokio::test]
    async fn trigger_tasks_inherit_the_provider_span() {
        let subscriber = tracing::Dispatch::new(tracing_subscriber::Registry::default());
        let span = tracing::dispatcher::with_default(&subscriber, || debug_span!("provider"));
        let (seen, received) = tokio::sync::oneshot::channel();
        let events = futures::stream::once(async move {
            seen.send(
                tracing::Span::current()
                    .metadata()
                    .map(|metadata| metadata.name()),
            )
            .unwrap();
            Ok(TriggerEvent::Fetch)
        })
        .chain(futures::stream::pending());
        let (publisher, snapshots) = Publisher::channel();
        let task = tokio::spawn(
            run_triggered_feed(
                default_http_feed_config(),
                publisher,
                || async { Ok(1usize) },
                events,
            )
            .instrument(span)
            .with_subscriber(subscriber),
        );
        assert_eq!(received.await.unwrap(), Some("provider"));
        drop(snapshots);
        task.await.unwrap().unwrap();
    }
}
