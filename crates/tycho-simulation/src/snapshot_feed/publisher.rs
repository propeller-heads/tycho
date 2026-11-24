//! What a feed publishes through, and the only way it reaches a consumer.

use std::{future::Future, time::Duration};

use futures::{Stream, StreamExt};
use tokio::{
    select,
    sync::watch,
    time::{sleep_until, Instant},
};
use tracing::{debug, info, warn};

/// A feed's end of the channel a consumer reads it through, and the age of what it holds.
///
/// A feed receives one of these from whichever consumer type runs it
/// ([`SnapshotFeedStream`](crate::snapshot_feed::SnapshotFeedStream),
/// [`SnapshotFeedStreams`](crate::snapshot_feed::SnapshotFeedStreams) or
/// [`SnapshotFeedWatch`](crate::snapshot_feed::SnapshotFeedWatch)) and cannot make one: the
/// channel, its `None` seed and the task the feed runs in all belong to whoever reads the feed,
/// so no consumer can hold a feed's future and give it their own pace.
///
/// [`publishing`](Self::publishing) is the whole of what a feed does with it — hand it the
/// snapshots and the age at which one goes stale, and everything else is taken care of.
pub struct Publisher<T> {
    tx: watch::Sender<Option<T>>,
    /// When the published snapshot was published; `None` while nothing servable is published.
    published_at: Option<Instant>,
    deadline: Option<Instant>,
}

impl<T> Publisher<T> {
    /// A publisher and the receiver that reads it, seeded with `None` — nothing servable yet.
    /// Crate-private on purpose: it is what keeps a feed's future out of consumer hands.
    pub(crate) fn channel() -> (Self, watch::Receiver<Option<T>>) {
        let (tx, rx) = watch::channel(None);
        (Publisher { tx, published_at: None, deadline: None }, rx)
    }

    /// Publishes every snapshot `snapshots` produces, withdrawing one that goes `max_age` without
    /// being refreshed, and resolves with the error the feed ended on. A `max_age` of `None` keeps
    /// whatever was published until the feed ends.
    ///
    /// Resolves `Ok(())` when the stream runs out, which a feed holding a connection never does,
    /// and as soon as the last receiver goes away, since everything the feed produces from then
    /// on would reach nobody.
    ///
    /// Everything a feed does between two snapshots — ticking, fetching, connecting, reading,
    /// backing off — belongs inside `snapshots`: this awaits nothing else, so that is the wait
    /// staleness and the last receiver leaving can cut into. Work a feed does elsewhere runs to
    /// its end before either is noticed.
    pub async fn publishing<E>(
        self,
        max_age: Option<Duration>,
        snapshots: impl Stream<Item = Result<T, E>>,
    ) -> Result<(), E> {
        self.publishing_with_deadline(max_age, snapshots, |_snapshot| None)
            .await
    }

    /// Uses the earlier of the source's deadline and the configured maximum age.
    pub(crate) async fn publishing_with_deadline<E>(
        mut self,
        max_age: Option<Duration>,
        snapshots: impl Stream<Item = Result<T, E>>,
        mut deadline: impl FnMut(&T) -> Option<Instant>,
    ) -> Result<(), E> {
        tokio::pin!(snapshots);
        loop {
            match self
                .next_snapshot(max_age, snapshots.next())
                .await
            {
                Some(Ok(snapshot)) => {
                    let expires_at = deadline(&snapshot);
                    if expires_at.is_some_and(|expires| expires <= Instant::now()) {
                        withdraw(&self.tx, &mut self.published_at);
                        continue;
                    }
                    self.publish(snapshot);
                    self.deadline = expires_at;
                }
                Some(Err(error)) => return Err(error),
                None => return Ok(()),
            }
        }
    }

    /// The next snapshot the feed produces, or `None` when there will not be a useful one: the
    /// stream ran out, or the last receiver went away and nothing the feed produces can reach
    /// anyone.
    ///
    /// Withdraws the published snapshot along the way if it reaches `max_age` first. Going stale
    /// never cuts the wait short — only the stream, or the last receiver leaving, decides when
    /// this returns — because a snapshot goes stale by the clock rather than by anything the feed
    /// is waiting for.
    async fn next_snapshot<U, E>(
        &mut self,
        max_age: Option<Duration>,
        next: impl Future<Output = Option<Result<U, E>>>,
    ) -> Option<Result<U, E>> {
        tokio::pin!(next);
        // Split so the wait can watch the receivers while the deadline still updates what is
        // published.
        let Publisher { tx, published_at, deadline } = self;

        select! {
            biased;
            _ = tx.closed() => None,
            max_age = going_stale(*published_at, max_age, *deadline) => {
                warn!(
                    ?max_age,
                    "snapshot reached its withdrawal deadline"
                );
                withdraw(tx, published_at);

                // Nothing is published any more, so nothing else can go stale before the
                // snapshot this is still waiting for arrives.
                select! {
                    _ = tx.closed() => None,
                    out = &mut next => out,
                }
            }
            out = &mut next => out,
        }
    }

    /// Publishes a snapshot, replacing whatever was being served.
    fn publish(&mut self, snapshot: T) {
        if self.tx.send(Some(snapshot)).is_err() {
            // The last receiver went away between the wait that produced this snapshot and here:
            // it was not published, so there is nothing to announce and nothing whose age to
            // track. The next wait ends the feed.
            return;
        }

        match self.published_at {
            // A consumer that was being served now has something newer; one that was not can
            // start.
            Some(_) => debug!("snapshot refreshed"),
            None => info!("serving a snapshot"),
        }

        self.published_at = Some(Instant::now());
        self.deadline = None;
    }
}

/// Completes once a snapshot published at `published_at` has reached `max_age`, and never if
/// there is no snapshot, or no age for one to reach. Hands back the age it waited out, which the
/// caller reports.
async fn going_stale(
    published_at: Option<Instant>,
    max_age: Option<Duration>,
    source_deadline: Option<Instant>,
) -> Duration {
    let Some(published_at) = published_at else {
        return std::future::pending().await;
    };
    let deadline = match (max_age.map(|age| published_at + age), source_deadline) {
        (Some(configured), Some(source)) => Some(configured.min(source)),
        (Some(configured), None) => Some(configured),
        (None, source) => source,
    };
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    sleep_until(deadline).await;
    deadline.saturating_duration_since(published_at)
}

/// Takes the published snapshot away and stops its clock. An already-empty channel stays quiet:
/// receivers are notified only if there was a snapshot to take.
fn withdraw<T>(tx: &watch::Sender<Option<T>>, published_at: &mut Option<Instant>) {
    *published_at = None;
    tx.send_if_modified(|snapshot| snapshot.take().is_some());
}

impl<T> Drop for Publisher<T> {
    /// Whatever ends a feed ends its snapshot: it gave up, ran out, or its task was dropped
    /// because nobody was reading. Nothing refreshes the published snapshot after that, so it is
    /// withdrawn rather than left standing for consumers to keep using.
    fn drop(&mut self) {
        withdraw(&self.tx, &mut self.published_at);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use futures::{stream, StreamExt};
    use rstest::rstest;
    use tokio::time::sleep;
    use tokio_stream::wrappers::WatchStream;

    use super::{super::expect_to_finish, *};

    #[tokio::test(start_paused = true)]
    async fn source_deadline_does_not_restart_when_a_late_snapshot_is_published() {
        let (publisher, mut receiver) = Publisher::channel();
        let deadline = Instant::now() + Duration::from_secs(2);
        let snapshots = stream::once(async {
            sleep(Duration::from_millis(1500)).await;
            Ok::<_, String>(1u32)
        })
        .chain(stream::pending());
        let task = tokio::spawn(publisher.publishing_with_deadline(
            Some(Duration::from_secs(2)),
            snapshots,
            move |_snapshot| Some(deadline),
        ));
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(1));
        tokio::time::advance(Duration::from_millis(499)).await;
        assert_eq!(*receiver.borrow(), Some(1));
        tokio::time::advance(Duration::from_millis(1)).await;
        receiver.changed().await.unwrap();
        assert!(receiver.borrow_and_update().is_none());
        drop(receiver);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_source_snapshot_is_never_published() {
        let (publisher, receiver) = Publisher::channel();
        let deadline = Instant::now();
        let snapshots = stream::once(async { Ok::<_, String>(1u32) }).chain(stream::pending());
        let task = tokio::spawn(publisher.publishing_with_deadline(
            Some(Duration::from_secs(2)),
            snapshots,
            move |_snapshot| Some(deadline),
        ));
        tokio::task::yield_now().await;
        assert!(receiver.borrow().is_none());
        assert!(!receiver.has_changed().unwrap());
        drop(receiver);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn source_deadline_cannot_extend_the_configured_maximum_age() {
        let (publisher, mut receiver) = Publisher::channel();
        let deadline = Instant::now() + Duration::from_secs(20);
        let snapshots = stream::once(async { Ok::<_, String>(1u32) }).chain(stream::pending());
        let task = tokio::spawn(publisher.publishing_with_deadline(
            Some(Duration::from_secs(2)),
            snapshots,
            move |_snapshot| Some(deadline),
        ));
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(1));
        tokio::time::advance(Duration::from_secs(2)).await;
        receiver.changed().await.unwrap();
        assert!(receiver.borrow_and_update().is_none());
        drop(receiver);
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn next_snapshot_withdraws_at_max_age_and_keeps_waiting() {
        let (mut publisher, mut rx) = Publisher::channel();
        publisher.publish(1);
        assert!(rx.borrow_and_update().is_some(), "a snapshot must be published to go stale");

        let start = Instant::now();
        let next = publisher
            .next_snapshot(Some(Duration::from_millis(100)), async {
                rx.changed()
                    .await
                    .expect("the publisher outlives this wait");
                assert!(rx.borrow_and_update().is_none(), "the change must be the withdrawal");
                assert_eq!(
                    start.elapsed(),
                    Duration::from_millis(100),
                    "the snapshot must be withdrawn once it reaches max_age"
                );

                sleep(Duration::from_millis(200)).await;
                Some(Ok::<u32, String>(2))
            })
            .await;

        assert_eq!(next, Some(Ok(2)), "the snapshot the feed was waiting for still arrives");
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(300),
            "going stale must not cut the wait short: the call returns when the feed answers"
        );
    }

    #[rstest]
    #[case::no_max_age_never_goes_stale(None, Duration::from_secs(3600))]
    #[case::snapshot_arrives_before_max_age(
        Some(Duration::from_millis(100)),
        Duration::from_millis(50)
    )]
    #[tokio::test(start_paused = true)]
    async fn next_snapshot_keeps_a_snapshot_that_has_not_gone_stale(
        #[case] max_age: Option<Duration>,
        #[case] wait: Duration,
    ) {
        let (mut publisher, mut rx) = Publisher::channel();
        publisher.publish(1);
        rx.borrow_and_update();

        publisher
            .next_snapshot(max_age, async move {
                sleep(wait).await;
                Some(Ok::<u32, String>(2))
            })
            .await;

        assert!(rx.borrow().is_some());
        assert!(!rx.has_changed().unwrap(), "the watch must not be disturbed");
    }

    #[tokio::test(start_paused = true)]
    async fn publishing_stops_as_soon_as_the_last_receiver_is_gone() {
        // A stream that never answers, watched by nobody: the feed must not be left waiting on
        // the venue for a snapshot that could reach no one.
        let asked = Arc::new(AtomicBool::new(false));
        let asked_clone = Arc::clone(&asked);
        let snapshots = stream::once(async move {
            asked_clone.store(true, Ordering::SeqCst);
            std::future::pending::<Result<u32, String>>().await
        });

        let (publisher, rx) = Publisher::channel();
        drop(rx);

        expect_to_finish(
            "the feed kept waiting although nobody was reading",
            publisher.publishing(None, snapshots),
        )
        .await
        .expect("no reader left is not a failure of the feed");

        assert!(!asked.load(Ordering::SeqCst), "the stream is not even asked for a snapshot");
    }

    #[tokio::test(start_paused = true)]
    async fn receiver_leaving_after_expiry_stops_a_pending_source() {
        let (mut publisher, mut rx) = Publisher::channel();
        publisher.publish(1);
        rx.borrow_and_update();
        let task = tokio::spawn(
            publisher
                .publishing(Some(Duration::from_secs(1)), stream::pending::<Result<u32, String>>()),
        );
        rx.changed()
            .await
            .expect("the snapshot must expire");
        assert!(rx.borrow_and_update().is_none());
        assert!(!task.is_finished());
        drop(rx);
        expect_to_finish("the expired feed kept waiting without receivers", task)
            .await
            .expect("publisher task panicked")
            .expect("receiver shutdown is not a source error");
    }

    #[tokio::test]
    async fn dropping_the_publisher_withdraws_what_it_published() {
        // Read as a consumer does, through the changes: the publisher owns the sender, so once
        // it is gone the channel is closed and only what it sent on the way out is left.
        let (mut publisher, rx) = Publisher::channel();
        let mut changes = WatchStream::from_changes(rx);
        publisher.publish(1);
        assert_eq!(changes.next().await, Some(Some(1)));

        drop(publisher);

        assert_eq!(changes.next().await, Some(None), "dropping it withdraws what it published");
        assert_eq!(changes.next().await, None, "and ends the stream");
    }

    #[tokio::test]
    async fn dropping_a_publisher_that_published_nothing_leaves_the_watch_quiet() {
        let (publisher, rx) = Publisher::<u32>::channel();
        let mut changes = WatchStream::from_changes(rx);
        drop(publisher);

        assert_eq!(changes.next().await, None, "there was no snapshot to take away");
    }
}
